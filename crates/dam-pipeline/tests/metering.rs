//! The metering chain, end to end (M6c).
//!
//! `dam_db::metering` proves the arithmetic — the level/flow split, the rounding, the idempotent upsert. What
//! this proves is the *wiring*, which is the half that was missing: `dam_global.tenant_usage_daily` has existed
//! since migration 0001 and nothing has ever written a row into it. `enrichment_runs` said in its own comment
//! that its token counters "roll up into dam_global.tenant_usage_daily", and they did not.
//!
//! Two properties, and the second is the one that fails silently:
//!
//! **One pass writes two days**: the one that has ended, and today. Today because an operator watching a spend
//! cap wants the current partial figure rather than one that appears at midnight; yesterday because it is
//! final. Both are upserts, so today's row is replaced on the next pass and becomes final when the day rolls.
//!
//! **Each pass leaves exactly one successor.** `requeue_backfill_collect` documents at length why a re-queue
//! must not carry a dedupe key: the running job still holds it, so `enqueue` resolves the key to the job doing
//! the enqueueing and the chain ends the moment it completes. For metering that failure is entirely invisible —
//! no error, no missing feature, just a billing series that stops on the day of a deploy and a
//! `tenant_usage_daily` whose last row looks like the day the tenant went quiet.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, Utc};
use dam_core::TenantSlug;
use dam_db::{migrate, testing::PostgresHarness};
use dam_store::FakeS3Store;
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    /// Kept so a case can migrate a *second* tenant schema mid-test, which is what provisioning does.
    url: String,
    global: PgPool,
    tenant: PgPool,
    store: FakeS3Store,
    slug: TenantSlug,
    tenant_id: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");
    let (store, _clock) = FakeS3Store::with_test_clock();

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant row");

    Fixture {
        url,
        _pg: pg,
        global,
        tenant,
        store,
        slug: TenantSlug::new("acme").expect("slug"),
        tenant_id,
    }
}

fn context(f: &Fixture) -> dam_pipeline::worker::Context {
    dam_pipeline::worker::Context {
        global: f.global.clone(),
        store: std::sync::Arc::new(f.store.clone()),
        indexes: std::sync::Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
            std::env::temp_dir().join("damrs-metering-index"),
        ))),
        ai: None,
        scanner: None,
        signing_identity: None,
        worker: "metering-test".to_owned(),
        http: reqwest::Client::new(),
    }
}

/// Claims and runs one metering job, bringing a delayed successor due first.
async fn run_one(f: &Fixture, context: &dam_pipeline::worker::Context) -> Option<Uuid> {
    sqlx::query(
        "UPDATE dam_global.jobs SET run_after = now() \
         WHERE kind = $1 AND state = 'queued' AND run_after > now()",
    )
    .bind(dam_pipeline::worker::kind::USAGE_ROLLUP)
    .execute(&f.global)
    .await
    .expect("make the delayed job due");

    let claimed = dam_db::jobs::claim(
        &f.global,
        &context.worker,
        dam_db::jobs::ClaimOptions::default(),
    )
    .await
    .expect("claim");
    let job = claimed
        .iter()
        .find(|job| job.kind == dam_pipeline::worker::kind::USAGE_ROLLUP)?;
    dam_pipeline::worker::handle(context, job)
        .await
        .expect("handle");
    dam_db::jobs::complete(&f.global, job.id)
        .await
        .expect("complete");
    Some(job.id)
}

async fn asset(f: &Fixture, name: &str, bytes: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', $4, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .bind(bytes)
    .execute(&f.tenant)
    .await
    .expect("asset");
    id
}

#[tokio::test]
async fn a_pass_writes_yesterday_and_today_and_leaves_one_successor() {
    let f = fixture().await;
    let context = context(&f);
    let today = Utc::now().date_naive();

    let one = asset(&f, "harbour", 4_000_000).await;
    sqlx::query(
        "INSERT INTO object_placements \
           (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state) \
         VALUES ('k/1', gen_random_uuid(), $1, 4000000, 'x', 'STANDARD', 'present')",
    )
    .bind(one)
    .execute(&f.tenant)
    .await
    .expect("placement");
    // One download yesterday, two today, so the two rows have to differ.
    for days_ago in [1, 0, 0] {
        sqlx::query(
            "INSERT INTO rights_usage (id, asset_id, downloads, source, recorded_at) \
             VALUES (gen_random_uuid(), $1, 1, 'download', now() - ($2 || ' days')::interval)",
        )
        .bind(one)
        .bind(days_ago)
        .execute(&f.tenant)
        .await
        .expect("download");
    }

    // The deduped entry point, as a starting worker calls it.
    dam_pipeline::worker::enqueue_usage_rollup(&f.global, f.tenant_id)
        .await
        .expect("enqueue");
    let ran = run_one(&f, &context).await.expect("a job to run");

    let rows = dam_db::metering::window(&f.global, f.tenant_id, today - Duration::days(1), today)
        .await
        .expect("window");
    assert_eq!(rows.len(), 2, "yesterday and today: {rows:?}");
    assert_eq!(rows[0].totals.downloads, 1, "yesterday");
    assert_eq!(rows[1].totals.downloads, 2, "today");
    // The level is the same in both, because there is one measurement of what is stored.
    assert_eq!(rows[0].totals.asset_count, 1);
    assert_eq!(rows[1].totals.asset_count, 1);
    assert_eq!(
        rows[1].totals.bytes_by_pool,
        serde_json::json!({ "STANDARD": 4_000_000 })
    );

    // And the chain continues. None means it died silently — which for metering means a billing series that
    // stops on the day of a deploy, with nothing anywhere saying so.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs \
         WHERE kind = $1 AND state = 'queued' AND id <> $2",
    )
    .bind(dam_pipeline::worker::kind::USAGE_ROLLUP)
    .bind(ran)
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(
        queued, 1,
        "exactly one successor: none means the chain ended, two means a dedupe key doing the wrong job",
    );
}

#[tokio::test]
async fn a_second_pass_replaces_todays_row_rather_than_doubling_it() {
    let f = fixture().await;
    let context = context(&f);
    let today = Utc::now().date_naive();
    let one = asset(&f, "harbour", 1_000).await;

    dam_pipeline::worker::enqueue_usage_rollup(&f.global, f.tenant_id)
        .await
        .expect("enqueue");
    run_one(&f, &context).await.expect("first pass");

    // Something happens, and the next pass picks it up — replacing today rather than adding to it.
    sqlx::query(
        "INSERT INTO rights_usage (id, asset_id, downloads, source) \
         VALUES (gen_random_uuid(), $1, 1, 'download')",
    )
    .bind(one)
    .execute(&f.tenant)
    .await
    .expect("download");
    run_one(&f, &context).await.expect("second pass");

    let rows = dam_db::metering::window(&f.global, f.tenant_id, today, today)
        .await
        .expect("window");
    assert_eq!(
        rows.len(),
        1,
        "one row per tenant-day, however many passes ran"
    );
    assert_eq!(rows[0].totals.downloads, 1);
}

#[tokio::test]
async fn an_empty_tenant_still_gets_rows() {
    // A gap in a billing series is indistinguishable from a worker that was down, so a tenant with nothing in
    // it has to produce rows of zeroes rather than no rows.
    let f = fixture().await;
    let context = context(&f);
    let today = Utc::now().date_naive();

    dam_pipeline::worker::enqueue_usage_rollup(&f.global, f.tenant_id)
        .await
        .expect("enqueue");
    run_one(&f, &context).await.expect("a job to run");

    let rows = dam_db::metering::window(&f.global, f.tenant_id, today - Duration::days(1), today)
        .await
        .expect("window");
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.totals == dam_db::metering::DayTotals::default())
    );
    assert_eq!(f.slug.as_str(), "acme");
}

#[tokio::test]
async fn a_tenant_created_after_the_worker_started_still_gets_metered() {
    // The bug this closes, found by provisioning three tenants against a running stack, uploading 360 assets
    // between them, and finding `damctl usage` reporting nothing for any of them.
    //
    // The metering chain was started *only* at worker boot. Nothing enqueues one at provision — the function's
    // own doc-comment claimed otherwise and was wrong — so a tenant created while a worker was already running
    // was never metered: no `usage_rollup` job, no `tenant_usage_daily` rows, and since that table is what an
    // operator bills from, an unbilled customer until somebody happened to restart the worker.
    let f = fixture().await;

    // The boot-time pass, as `run` performs it. `acme` gets its chain.
    dam_pipeline::worker::start_missing_metering_chains(&f.global).await;
    assert_eq!(
        live_rollups(&f.global, f.tenant_id).await,
        1,
        "the tenant that existed at boot has a chain"
    );

    // Now a tenant appears, exactly as `damctl provision-tenant` makes one — after the worker is up, and
    // without telling anybody.
    migrate::tenant(&f.url, "t_globex").await.expect("schema");
    let late: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'globex', 't_globex', 'Globex', 'globex/', 'active') RETURNING id",
    )
    .fetch_one(&f.global)
    .await
    .expect("tenant row");
    assert_eq!(
        live_rollups(&f.global, late).await,
        0,
        "the premise: provisioning enqueues nothing, which is what made this silent"
    );

    // The sweep the loop now runs. Before the fix there was no second pass at all.
    dam_pipeline::worker::start_missing_metering_chains(&f.global).await;
    assert_eq!(
        live_rollups(&f.global, late).await,
        1,
        "a tenant provisioned after boot has to acquire a metering chain without anybody asking"
    );

    // And the sweep is idempotent: the tenant that already had one does not acquire a second, or the series
    // would be written twice a day by two chains racing each other.
    dam_pipeline::worker::start_missing_metering_chains(&f.global).await;
    assert_eq!(live_rollups(&f.global, f.tenant_id).await, 1);
    assert_eq!(live_rollups(&f.global, late).await, 1);

    // A suspended tenant is not metered. Suspension stops the bill as well as the access.
    sqlx::query("UPDATE dam_global.tenants SET status = 'suspended' WHERE id = $1")
        .bind(late)
        .execute(&f.global)
        .await
        .expect("suspend");
    sqlx::query("DELETE FROM dam_global.jobs WHERE tenant_id = $1")
        .bind(late)
        .execute(&f.global)
        .await
        .expect("clear its chain");
    dam_pipeline::worker::start_missing_metering_chains(&f.global).await;
    assert_eq!(
        live_rollups(&f.global, late).await,
        0,
        "a suspended tenant must not be handed a fresh chain by the sweep"
    );
}

/// Queued-or-running `usage_rollup` jobs for one tenant — the same condition the dedupe index is partial on.
async fn live_rollups(global: &PgPool, tenant_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs \
         WHERE tenant_id = $1 AND kind = 'usage_rollup' AND state IN ('queued', 'running')",
    )
    .bind(tenant_id)
    .fetch_one(global)
    .await
    .expect("count")
}
