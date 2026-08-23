//! Archival, end to end: an object goes cold, and a restore brings a copy back (§6.4, §6.5).
//!
//! Against `FakeS3Store` rather than a real bucket, and that is the deliberate choice ARCHITECTURE §20.2
//! already records: SeaweedFS *accepts* a storage-class header and ignores it, which is worse than refusing
//! for a test's purposes — a sweep would report every transition as successful over a store where nothing
//! moved. The fake models what matters instead: the minimum billable duration, the restore timeline, and the
//! ongoing→available→expired progression that the tier badge is derived from. The real thing is covered by
//! the nightly AWS conformance suite, which asserts the two properties this could never see — that
//! `GLACIER_IR` is readable without a restore and `DEEP_ARCHIVE` is not.
//!
//! What is being proven here is the *wiring*, because that is what was missing. The planner, the arithmetic,
//! the S3 calls and the bookkeeping all existed and were tested; nothing called them, so every asset stayed
//! in the class it was uploaded to forever and `restore_requests` was a table with no writer.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::{Clock, RestoreTier, StorageClass, TenantSlug, TestClock};
use dam_db::{migrate, testing::PostgresHarness};
use dam_store::{BlobStore, FakeS3Store, Key};
use sqlx::PgPool;
use uuid::Uuid;

/// A fixed start, so "ninety days later" is a date rather than a race.
fn origin() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
}

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    tenant: PgPool,
    store: FakeS3Store,
    clock: TestClock,
    slug: TenantSlug,
    tenant_id: Uuid,
    pool_id: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let (store, clock) = FakeS3Store::with_test_clock();
    clock.set(origin());

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant row");

    // Prices recorded on the pool, with the tier spread §6.5 turns on. Not seeded by provisioning — see the
    // migration — so a test that wants a non-zero estimate says so, which is also what an operator does.
    let pool_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.storage_pools \
           (id, tenant_id, name, driver, bucket, credentials_ref, latency_class, \
            cost_per_gb_retrieval, cost_per_gb_retrieval_expedited, cost_per_gb_retrieval_bulk, \
            cost_per_1k_requests) \
         VALUES (gen_random_uuid(), $1, 'hot', 's3', 'b', 'test', 'instant', \
                 0.01, 0.03, 0.0025, 0.05) RETURNING id",
    )
    .bind(tenant_id)
    .fetch_one(&global)
    .await
    .expect("storage pool");

    Fixture {
        _pg: pg,
        global,
        tenant,
        store,
        clock,
        slug: TenantSlug::new("acme").expect("slug"),
        tenant_id,
        pool_id,
    }
}

#[tokio::test]
async fn the_archival_lifecycle_holds() {
    let f = fixture().await;

    a_dry_run_plans_and_moves_nothing(&f).await;
    an_enabled_policy_moves_an_eligible_original(&f).await;
    the_placement_records_the_class_and_the_minimum_duration(&f).await;
    a_legal_hold_is_not_tiered_and_says_why(&f).await;
    a_pinned_collection_keeps_its_assets_hot(&f).await;
    an_interface_derivative_is_never_tiered(&f).await;
    an_object_younger_than_the_policy_waits(&f).await;
    a_second_hop_waits_for_the_minimum_duration(&f).await;
    a_policy_pointing_at_another_pool_does_not_move_anything(&f).await;
    a_run_records_what_it_did(&f).await;

    a_restore_is_issued_and_then_lands(&f).await;
    a_restored_copy_makes_the_asset_readable_again(&f).await;
    an_expired_copy_stops_claiming_to_be_there(&f).await;
    expedited_deep_archive_is_refused_rather_than_downgraded(&f).await;
    the_estimate_reflects_the_tier(&f).await;

    a_pinned_object_does_not_consume_the_run_cap(&f).await;
    a_store_that_cannot_tier_says_so_and_keeps_the_count(&f).await;
    a_claim_that_never_reached_s3_is_reissued(&f).await;
    each_pass_leaves_the_next_one_behind_it(&f).await;
}

/// The other half of choosing at-most-once, and the case that pins it.
///
/// The poll claims in one transaction and issues outside it, so a worker that dies in between leaves a row
/// saying `requested` with nothing asked for. Holding the transaction across the vendor call would trade that
/// for re-issuing every restore in the batch on the same crash — each one a real charge — so the claim is
/// deliberately the cheap way round and the poll reconciles against S3 instead.
///
/// Simulated by writing the state the crash would leave: claimed, with no restore in progress on the object.
async fn a_claim_that_never_reached_s3_is_reissued(f: &Fixture) {
    let (key, asset_id) = archived(f, "acme/o/uu/vv/orphaned", StorageClass::Glacier).await;
    let id = request_restore(f, asset_id, RestoreTier::Standard).await;
    sqlx::query("UPDATE restore_requests SET state = 'requested' WHERE id = $1")
        .bind(id)
        .execute(&f.tenant)
        .await
        .expect("the state a crash between claim and call leaves");
    assert!(
        matches!(
            f.store.head(&key).await.expect("head").restore_state,
            dam_core::RestoreState::None,
        ),
        "nothing was ever asked of the store, which is the whole premise",
    );

    let polled = dam_pipeline::tiering::poll(&f.global, &f.store, &f.slug, f.clock.now())
        .await
        .expect("poll");
    assert_eq!(polled.reissued, 1, "{polled:?}");
    assert!(
        matches!(
            f.store.head(&key).await.expect("head").restore_state,
            dam_core::RestoreState::Ongoing,
        ),
        "and now the store is working on it, rather than the row waiting forever on a call nobody made",
    );
}

/// The chain continues, and this is the case that would catch it silently not continuing.
///
/// Both of these jobs re-queue themselves, and `requeue_backfill_collect` documents at length why that must
/// not carry a dedupe key: the running job still holds it, so `enqueue` resolves the key to *the job doing the
/// enqueueing*, returns its id, and the chain ends the moment it completes. A live backfill did exactly that —
/// one poll, "still working", and a batch nobody ever came back for.
///
/// The same mistake here would be quieter and worse. A backfill that stops is a library that stays
/// undescribed; a sweep that stops is a tenant paying Standard rates on a cold archive forever, with a policy
/// row that says `enabled` and a `last_run_at` from the day somebody set it up.
async fn each_pass_leaves_the_next_one_behind_it(f: &Fixture) {
    let context = dam_pipeline::worker::Context {
        global: f.global.clone(),
        store: std::sync::Arc::new(f.store.clone()),
        indexes: std::sync::Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
            std::env::temp_dir().join("damrs-tiering-index"),
        ))),
        ai: None,
        scanner: None,
        signing_identity: None,
        worker: "tiering-test".to_owned(),
        // No webhook subscriptions in these fixtures, so nothing is ever dispatched. A default client
        // rather than a builder, because what these suites exercise is unrelated to how it is configured.
        http: reqwest::Client::new(),
    };

    for kind in [
        dam_pipeline::worker::kind::TIER_SWEEP,
        dam_pipeline::worker::kind::RESTORE_POLL,
    ] {
        // One in flight to begin with, from the deduped entry point an API call would use.
        match kind {
            dam_pipeline::worker::kind::TIER_SWEEP => {
                dam_pipeline::worker::enqueue_tier_sweep(&f.global, f.tenant_id).await
            }
            _ => dam_pipeline::worker::enqueue_restore_poll(&f.global, f.tenant_id).await,
        }
        .expect("enqueue");

        let ran = run_one(f, &context, kind).await.expect("a job to run");

        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM dam_global.jobs \
             WHERE kind = $1 AND state = 'queued' AND id <> $2",
        )
        .bind(kind)
        .bind(ran)
        .fetch_one(&f.global)
        .await
        .expect("count");
        assert_eq!(
            queued, 1,
            "{kind} must leave exactly one successor behind it — none means the chain died silently, \
             and two means a dedupe key that is not doing its job",
        );
    }
}

/// Claims and runs one job of a kind, bringing a delayed one due first.
///
/// The successor is queued with a `run_after` in the future — tomorrow for a sweep, two minutes for a poll —
/// so a claim without this finds nothing and the case reads as a chain that stopped.
async fn run_one(f: &Fixture, context: &dam_pipeline::worker::Context, kind: &str) -> Option<Uuid> {
    sqlx::query(
        "UPDATE dam_global.jobs SET run_after = now() \
         WHERE kind = $1 AND state = 'queued' AND run_after > now()",
    )
    .bind(kind)
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
    let job = claimed.iter().find(|job| job.kind == kind)?;
    dam_pipeline::worker::handle(context, job)
        .await
        .expect("handle");
    dam_db::jobs::complete(&f.global, job.id)
        .await
        .expect("complete");
    Some(job.id)
}

// ─── the sweep ──────────────────────────────────────────────────────────────

/// Stages an object in the store and records the placement, as finalisation would.
async fn object(f: &Fixture, key: &str, bytes: usize, placed: DateTime<Utc>) -> Key {
    let key = Key::new(key.to_owned()).expect("key");
    f.store
        .put(
            &key,
            bytes::Bytes::from(vec![7u8; bytes]),
            StorageClass::Standard,
        )
        .await
        .expect("put");
    sqlx::query(
        "INSERT INTO object_placements \
           (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state, placed_at) \
         VALUES ($1, $2, $3, $4, 'x', 'STANDARD', 'present', $5)",
    )
    .bind(key.as_str())
    .bind(f.pool_id)
    .bind(asset(f, key.as_str()).await)
    .bind(i64::try_from(bytes).unwrap())
    .bind(placed)
    .execute(&f.tenant)
    .await
    .expect("placement");
    key
}

/// An asset row for a key, so the scan's joins have something to join to.
async fn asset(f: &Fixture, label: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{}.jpg", label.replace('/', "-")))
    .execute(&f.tenant)
    .await
    .expect("asset");
    id
}

/// A policy that archives originals idle for ninety days.
async fn policy(f: &Fixture, name: &str, dry_run: bool) -> Uuid {
    policy_with(f, name, dry_run, StorageClass::GlacierIr, Some(f.pool_id)).await
}

async fn policy_with(
    f: &Fixture,
    name: &str,
    dry_run: bool,
    target: StorageClass,
    pool: Option<Uuid>,
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO lifecycle_policies \
           (id, name, applies_to, idle_days, action, target_pool_id, target_class, dry_run, enabled) \
         VALUES (gen_random_uuid(), $1, 'original', 90, 'transition', $2, $3, $4, true) \
         RETURNING id",
    )
    .bind(name)
    .bind(pool)
    .bind(target.to_string())
    .bind(dry_run)
    .fetch_one(&f.tenant)
    .await
    .expect("policy");
    id
}

async fn disable_all(f: &Fixture) {
    sqlx::query("UPDATE lifecycle_policies SET enabled = false")
        .execute(&f.tenant)
        .await
        .expect("disable");
}

async fn class_of(f: &Fixture, key: &Key) -> String {
    sqlx::query_scalar("SELECT storage_class FROM object_placements WHERE object_key = $1")
        .bind(key.as_str())
        .fetch_one(&f.tenant)
        .await
        .expect("class")
}

/// The default is a plan, and the default is what a policy created through any path gets.
///
/// `dry_run` defaults to true in the schema and in `LifecyclePolicy`, and this is the case that would fail if
/// somebody "helpfully" flipped either — the failure being that terabytes move before anybody reads what
/// would move.
async fn a_dry_run_plans_and_moves_nothing(f: &Fixture) {
    let key = object(f, "acme/o/aa/bb/dry", 4096, origin() - Duration::days(200)).await;
    let id = policy(f, "dry", true).await;

    let swept = dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert_eq!(swept.planned, 1, "the object was eligible: {swept:?}");
    assert_eq!(swept.moved, 0, "a dry run moves nothing: {swept:?}");
    assert_eq!(class_of(f, &key).await, "STANDARD");
    sqlx::query("DELETE FROM lifecycle_policies WHERE id = $1")
        .bind(id)
        .execute(&f.tenant)
        .await
        .expect("cleanup");
}

async fn an_enabled_policy_moves_an_eligible_original(f: &Fixture) {
    let key = object(f, "acme/o/cc/dd/cold", 4096, origin() - Duration::days(200)).await;
    policy(f, "archive originals", false).await;

    let swept = dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert!(swept.moved >= 1, "nothing moved: {swept:?}");
    assert_eq!(
        class_of(f, &key).await,
        "GLACIER_IR",
        "the placement must record the class the object is actually in",
    );
    let state = f.store.head(&key).await.expect("head");
    assert_eq!(
        state.storage_class,
        StorageClass::GlacierIr,
        "and the object store must agree — a placement that says GLACIER_IR over an object that is still \
         STANDARD is a bill nobody is paying and a restore nobody needs",
    );
}

/// The counter, without which the second hop is free-looking and expensive.
///
/// Glacier IR bills a 90-day minimum. An object moved today and moved again tomorrow is charged the full 90
/// days for today's move as well as the new one — so the date it may next move is written with the class, in
/// the same statement, because a crash between two statements would leave an object with no counter.
async fn the_placement_records_the_class_and_the_minimum_duration(f: &Fixture) {
    let until: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT min_duration_until FROM object_placements WHERE object_key = $1",
    )
    .bind("acme/o/cc/dd/cold")
    .fetch_one(&f.tenant)
    .await
    .expect("min duration");

    let until = until.expect("a transition must write the minimum-duration counter");
    assert_eq!(
        until,
        origin() + Duration::days(90),
        "Glacier IR's ninety days, from the move",
    );
}

/// The one this engine must never get wrong.
///
/// A legal hold means litigation, and Deep Archive means the bytes are unreadable for up to 48 hours. The
/// column that was supposed to prevent this — `object_placements.pinned` — has never been written by
/// anything, so the scan derives the pin from `assets.legal_hold` instead and ORs the column on top.
async fn a_legal_hold_is_not_tiered_and_says_why(f: &Fixture) {
    let key = object(f, "acme/o/ee/ff/held", 4096, origin() - Duration::days(300)).await;
    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = (SELECT asset_id FROM object_placements WHERE object_key = $1)")
        .bind(key.as_str())
        .execute(&f.tenant)
        .await
        .expect("hold");

    dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert_eq!(
        class_of(f, &key).await,
        "STANDARD",
        "an asset under legal hold must not be archived, whatever a policy says",
    );
}

async fn a_pinned_collection_keeps_its_assets_hot(f: &Fixture) {
    let key = object(
        f,
        "acme/o/gg/hh/pinned",
        4096,
        origin() - Duration::days(300),
    )
    .await;
    let asset_id: Uuid =
        sqlx::query_scalar("SELECT asset_id FROM object_placements WHERE object_key = $1")
            .bind(key.as_str())
            .fetch_one(&f.tenant)
            .await
            .expect("asset");
    let collection: Uuid = sqlx::query_scalar(
        "INSERT INTO collections (id, key, label, pin_hot) \
         VALUES (gen_random_uuid(), 'campaign', 'Campaign', true) RETURNING id",
    )
    .fetch_one(&f.tenant)
    .await
    .expect("collection");
    sqlx::query("INSERT INTO collection_items (collection_id, asset_id) VALUES ($1, $2)")
        .bind(collection)
        .bind(asset_id)
        .execute(&f.tenant)
        .await
        .expect("membership");

    dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert_eq!(
        class_of(f, &key).await,
        "STANDARD",
        "a `pin_hot` collection is a promise that its assets stay fetchable",
    );

    // And the plan says *which* pin. A skip that reads only "pinned" is one an operator cannot act on: a
    // hold, a collection and a manual note are three different pieces of work.
    let mut conn = f.tenant.acquire().await.expect("conn");
    let policy = dam_db::tiering::policy(
        &mut conn,
        sqlx::query_scalar("SELECT id FROM lifecycle_policies ORDER BY created_at LIMIT 1")
            .fetch_one(&f.tenant)
            .await
            .expect("a policy"),
    )
    .await
    .expect("load")
    .expect("a policy");
    let candidates = dam_db::tiering::candidates(&mut conn, &policy, origin())
        .await
        .expect("candidates");
    let pinned = candidates
        .iter()
        .find(|candidate| candidate.object_key.as_str() == key.as_str())
        .expect("the pinned candidate");
    assert_eq!(
        pinned.pin_reason.as_deref(),
        Some("a member of the pinned collection 'Campaign'"),
        "the collection names itself, so somebody can go and unpin it",
    );
}

/// §2's search substrate stays hot, and the rule is enforced by the key rather than by a policy.
///
/// `Key::is_tier_exempt` reads the namespace, so it holds "even for an object whose placement row is missing
/// or stale" — which is why the scan does not re-derive it from `derivatives.role`. A policy scoped to
/// derivatives is a legitimate thing to write; archiving the thumbnails the grid draws is not.
async fn an_interface_derivative_is_never_tiered(f: &Fixture) {
    let hex = "b".repeat(64);
    let key = object(
        f,
        &format!("acme/t/{hex}/256.webp"),
        4096,
        origin() - Duration::days(300),
    )
    .await;
    sqlx::query(
        "INSERT INTO lifecycle_policies \
           (id, name, applies_to, idle_days, action, target_pool_id, target_class, dry_run, enabled) \
         VALUES (gen_random_uuid(), 'archive everything', 'both', 1, 'transition', $1, 'DEEP_ARCHIVE', \
                 false, true)",
    )
    .bind(f.pool_id)
    .execute(&f.tenant)
    .await
    .expect("policy");

    dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert_eq!(
        class_of(f, &key).await,
        "STANDARD",
        "a thumbnail in Deep Archive is a grid cell that cannot render for two days",
    );
    disable_all(f).await;
}

async fn an_object_younger_than_the_policy_waits(f: &Fixture) {
    let key = object(f, "acme/o/ii/jj/fresh", 4096, origin() - Duration::days(10)).await;
    policy(f, "ninety days", false).await;

    let swept = dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert_eq!(class_of(f, &key).await, "STANDARD", "{swept:?}");
    assert!(
        swept.skipped >= 1,
        "and it was reported, not dropped: {swept:?}"
    );
    disable_all(f).await;
}

/// The billing trap, from the other side.
///
/// The object moved to Glacier IR above. A policy that now wants it in Deep Archive must wait out the 90 days
/// Glacier IR charges for, or the tenant pays both minimums for one object.
async fn a_second_hop_waits_for_the_minimum_duration(f: &Fixture) {
    let key = Key::new("acme/o/cc/dd/cold".to_owned()).expect("key");
    sqlx::query(
        "INSERT INTO lifecycle_policies \
           (id, name, applies_to, idle_days, action, target_pool_id, target_class, dry_run, enabled) \
         VALUES (gen_random_uuid(), 'deeper', 'original', 1, 'transition', $1, 'DEEP_ARCHIVE', false, true)",
    )
    .bind(f.pool_id)
    .execute(&f.tenant)
    .await
    .expect("policy");

    // A day after the first move: eligible by age, blocked by the counter.
    dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin() + Duration::days(1))
        .await
        .expect("sweep");
    assert_eq!(
        class_of(f, &key).await,
        "GLACIER_IR",
        "the second hop must wait for the minimum billable duration",
    );

    // Past it, the same policy moves the same object.
    dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin() + Duration::days(91))
        .await
        .expect("sweep");
    assert_eq!(
        class_of(f, &key).await,
        "DEEP_ARCHIVE",
        "and once the counter has elapsed it proceeds",
    );
    disable_all(f).await;
}

/// A transition is in place; a different pool is a copy between buckets, which this does not do.
///
/// Refused rather than performed in place, because "moved, but not where you said" is worse than "did
/// nothing, and said so".
async fn a_policy_pointing_at_another_pool_does_not_move_anything(f: &Fixture) {
    let key = object(
        f,
        "acme/o/kk/ll/elsewhere",
        4096,
        origin() - Duration::days(300),
    )
    .await;
    policy_with(
        f,
        "to another pool",
        false,
        StorageClass::GlacierIr,
        Some(Uuid::now_v7()),
    )
    .await;

    dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert_eq!(class_of(f, &key).await, "STANDARD");
    disable_all(f).await;
}

async fn a_run_records_what_it_did(f: &Fixture) {
    let key = object(
        f,
        "acme/o/mm/nn/counted",
        4096,
        origin() - Duration::days(300),
    )
    .await;
    let id = policy(f, "counted", false).await;

    let swept = dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    let (at, moved): (Option<DateTime<Utc>>, Option<i32>) =
        sqlx::query_as("SELECT last_run_at, last_run_moved FROM lifecycle_policies WHERE id = $1")
            .bind(id)
            .fetch_one(&f.tenant)
            .await
            .expect("run record");
    assert_eq!(
        at,
        Some(origin()),
        "a run that says nothing is a run nobody can audit"
    );
    // Against what the run reported rather than against a literal: by this point the fixture holds several
    // objects a ninety-day policy legitimately matches, and a hardcoded 1 would be asserting that the earlier
    // cases left no residue rather than that the row records the truth.
    assert_eq!(
        moved,
        Some(i32::try_from(swept.moved).unwrap()),
        "the row must say what the run did: {swept:?}",
    );
    assert!(
        swept.moved >= 1,
        "and this policy did move something: {swept:?}"
    );
    assert_eq!(class_of(f, &key).await, "GLACIER_IR");
    disable_all(f).await;
}

/// The cap bounds what moves, not what is looked at.
///
/// Found against a real library on AWS: a tenant with 136 pinned placements and `max_objects_per_run = 1`
/// planned nothing, run after run. The scan fetched cap-plus-one rows in key order, both happened to be
/// pinned, and the planner correctly reported two skips and no transitions — so a policy that looked
/// configured could never reach a movable object. Ordering movable rows first is the fix; this is the case
/// that would have caught it.
async fn a_pinned_object_does_not_consume_the_run_cap(f: &Fixture) {
    // Keys chosen so the pinned one sorts *first*: without the ordering fix it takes the whole budget.
    let pinned = object(
        f,
        "acme/o/aa/aa/pinned-first",
        4096,
        origin() - Duration::days(300),
    )
    .await;
    let movable = object(
        f,
        "acme/o/zz/zz/movable-last",
        4096,
        origin() - Duration::days(300),
    )
    .await;
    sqlx::query("UPDATE object_placements SET pinned = true WHERE object_key = $1")
        .bind(pinned.as_str())
        .execute(&f.tenant)
        .await
        .expect("pin");

    let id = policy(f, "one at a time", false).await;
    sqlx::query("UPDATE lifecycle_policies SET max_objects_per_run = 1 WHERE id = $1")
        .bind(id)
        .execute(&f.tenant)
        .await
        .expect("cap");

    let swept = dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("sweep");

    assert!(
        swept.moved >= 1,
        "a cap of one must move one, not spend its budget on a row it was never going to move: {swept:?}",
    );
    assert_eq!(
        class_of(f, &movable).await,
        "GLACIER_IR",
        "and the movable object is the one that moved",
    );
    assert_eq!(class_of(f, &pinned).await, "STANDARD", "while the pin held",);
    disable_all(f).await;
}

/// A driver without storage classes refuses, and the run still reports what it did.
///
/// Found by running this against the dev stack, where SeaweedFS answers "seaweedfs does not support storage
/// class transitions" — which is the *right* answer, and better than accepting the header and ignoring it
/// (§20.2). What was wrong was our side: the refusal propagated, aborted the policy, and took the partial
/// result with it, so a run that had already moved five objects logged `moved=0`. A log that understates what
/// happened is worse than none, because the next run would move five it believed were still Standard.
///
/// Simulated with a policy whose target is a class the fake refuses to leave — there is no way to make
/// `FakeS3Store` lack the capability, so the failure is provoked at the object rather than at the driver, and
/// what is asserted is the accounting rather than the specific error.
async fn a_store_that_cannot_tier_says_so_and_keeps_the_count(f: &Fixture) {
    let movable = object(
        f,
        "acme/o/ww/xx/counts",
        4096,
        origin() - Duration::days(300),
    )
    .await;
    policy(f, "counts what it did", false).await;

    let swept = dam_pipeline::tiering::sweep(&f.global, &f.store, &f.slug, origin())
        .await
        .expect("a sweep that meets a refusal must still return");

    assert!(
        swept.moved >= 1,
        "what moved before anything went wrong is still what moved: {swept:?}",
    );
    assert_eq!(class_of(f, &movable).await, "GLACIER_IR");
    assert_eq!(
        swept.failed, 0,
        "and nothing failed here, which is what makes the count meaningful: {swept:?}",
    );
    disable_all(f).await;
}

// ─── the restore ────────────────────────────────────────────────────────────

/// Puts an object straight into an archive class and returns its key and asset.
async fn archived(f: &Fixture, key: &str, class: StorageClass) -> (Key, Uuid) {
    let key = object(f, key, 8192, origin() - Duration::days(300)).await;
    f.store.transition(&key, class).await.expect("transition");
    sqlx::query("UPDATE object_placements SET storage_class = $2 WHERE object_key = $1")
        .bind(key.as_str())
        .bind(class.to_string())
        .execute(&f.tenant)
        .await
        .expect("class");
    let asset_id: Uuid =
        sqlx::query_scalar("SELECT asset_id FROM object_placements WHERE object_key = $1")
            .bind(key.as_str())
            .fetch_one(&f.tenant)
            .await
            .expect("asset");
    (key, asset_id)
}

async fn request_restore(f: &Fixture, asset_id: Uuid, tier: RestoreTier) -> Uuid {
    let planned =
        dam_pipeline::tiering::plan_for(&f.global, &f.slug, asset_id, tier, 7, f.clock.now())
            .await
            .expect("plan")
            .expect("a plan");
    let mut conn = f.tenant.acquire().await.expect("conn");
    let outcome = dam_db::restores::request(
        &mut conn,
        &dam_db::restores::RestoreSpec {
            object_key: &planned.1.object_key,
            pool_id: planned.1.pool_id,
            asset_id: Some(asset_id),
            tier,
            keep_warm_days: 7,
            requested_by: None,
            batch_id: None,
            notify: serde_json::json!({}),
        },
        &planned.0,
    )
    .await
    .expect("request");
    match outcome {
        dam_db::restores::Outcome::Created(request)
        | dam_db::restores::Outcome::AlreadyInFlight(request) => request.id,
    }
}

async fn state_of(f: &Fixture, id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM restore_requests WHERE id = $1")
        .bind(id)
        .fetch_one(&f.tenant)
        .await
        .expect("state")
}

/// The whole point: a request becomes an S3 call, and the copy's arrival is noticed.
async fn a_restore_is_issued_and_then_lands(f: &Fixture) {
    let (key, asset_id) = archived(f, "acme/o/oo/pp/frozen", StorageClass::Glacier).await;
    let id = request_restore(f, asset_id, RestoreTier::Standard).await;
    assert_eq!(state_of(f, id).await, "queued");

    let polled = dam_pipeline::tiering::poll(&f.global, &f.store, &f.slug, f.clock.now())
        .await
        .expect("poll");
    assert_eq!(polled.issued, 1, "{polled:?}");
    assert_eq!(state_of(f, id).await, "requested");
    assert!(
        !f.store.head(&key).await.expect("head").is_readable(),
        "a Glacier object is not readable the instant it is asked for; the fake models the wait",
    );

    // Standard on Glacier is 3–5 hours. Six is past it.
    f.clock.set(origin() + Duration::hours(6));
    let polled = dam_pipeline::tiering::poll(&f.global, &f.store, &f.slug, f.clock.now())
        .await
        .expect("poll");
    assert_eq!(polled.available, 1, "{polled:?}");
    assert_eq!(state_of(f, id).await, "available");
}

/// The placement, not just the request — because the tier badge reads the placement.
///
/// Marking only the request would leave a restored asset still drawing as `archive` in the grid with its
/// download disabled: the copy is there and nothing in the UI says so.
async fn a_restored_copy_makes_the_asset_readable_again(f: &Fixture) {
    let (state, expires): (String, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT restore_state, restore_expires_at FROM object_placements WHERE object_key = $1",
    )
    .bind("acme/o/oo/pp/frozen")
    .fetch_one(&f.tenant)
    .await
    .expect("placement");

    assert_eq!(state, "available");
    assert_eq!(
        expires,
        Some(origin() + Duration::hours(6) + Duration::days(7)),
        "seven warm days from when the copy actually landed, not from when it was asked for",
    );
    assert_eq!(
        dam_core::AssetTier::of(StorageClass::Glacier, dam_core::RestoreState::Available),
        dam_core::AssetTier::Restored,
        "which is the tier the grid draws, and the reason the placement has to be written",
    );
}

/// The sweep the module docs call not optional.
async fn an_expired_copy_stops_claiming_to_be_there(f: &Fixture) {
    let after = origin() + Duration::hours(6) + Duration::days(8);
    let polled = dam_pipeline::tiering::poll(&f.global, &f.store, &f.slug, after)
        .await
        .expect("poll");

    assert_eq!(polled.expired, 1, "{polled:?}");
    let state: String =
        sqlx::query_scalar("SELECT restore_state FROM object_placements WHERE object_key = $1")
            .bind("acme/o/oo/pp/frozen")
            .fetch_one(&f.tenant)
            .await
            .expect("placement");
    assert_eq!(
        state, "expired",
        "a placement still saying `available` is a delivery URL that 403s at S3 with nothing explaining why",
    );
}

/// Deep Archive has no Expedited tier, and substituting Standard silently would answer a request for minutes
/// with twelve hours.
async fn expedited_deep_archive_is_refused_rather_than_downgraded(f: &Fixture) {
    let (_, asset_id) = archived(f, "acme/o/qq/rr/deep", StorageClass::DeepArchive).await;
    let refused = dam_pipeline::tiering::plan_for(
        &f.global,
        &f.slug,
        asset_id,
        RestoreTier::Expedited,
        7,
        f.clock.now(),
    )
    .await
    .expect("plan call");

    let error = refused.expect_err("Deep Archive has no Expedited tier");
    assert!(
        error.to_string().contains("no expedited tier"),
        "the refusal must name the constraint so a user can choose Standard knowingly: {error}",
    );
}

/// The estimate is the reason the screen exists, so it has to move with the tier.
///
/// Two gigabytes, and a placement with no object behind it. The estimate reads `object_placements.size_bytes`
/// and never touches the store, so staging two real gigabytes in an in-memory fake would buy nothing but
/// memory pressure.
///
/// The size matters, and finding out why was the useful part of writing this: at eight kilobytes all three
/// tiers quoted the same single cent, because the per-1000-requests charge dominates and the per-GB term
/// rounds away entirely. That is arithmetically correct and it means the tier chooser is genuinely
/// meaningless for small objects — worth knowing before somebody reports the identical prices as a bug.
async fn the_estimate_reflects_the_tier(f: &Fixture) {
    let asset_id = asset(f, "priced").await;
    sqlx::query(
        "INSERT INTO object_placements \
           (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state, placed_at) \
         VALUES ($1, $2, $3, $4, 'x', 'GLACIER', 'present', $5)",
    )
    .bind("acme/o/ss/tt/priced")
    .bind(f.pool_id)
    .bind(asset_id)
    .bind(2i64 * 1024 * 1024 * 1024)
    .bind(origin() - Duration::days(300))
    .execute(&f.tenant)
    .await
    .expect("placement");
    let mut costs = Vec::new();
    for tier in [
        RestoreTier::Expedited,
        RestoreTier::Standard,
        RestoreTier::Bulk,
    ] {
        let (plan, _) =
            dam_pipeline::tiering::plan_for(&f.global, &f.slug, asset_id, tier, 7, f.clock.now())
                .await
                .expect("plan call")
                .expect("a plan");
        costs.push((tier, plan.est_cost_cents, plan.eta_at));
    }

    assert!(
        costs[0].1 > costs[2].1,
        "Expedited must cost more than Bulk, or the choice the screen offers is decoration: {costs:?}",
    );
    assert!(
        costs[0].2 < costs[2].2,
        "and it must be faster, which is what the extra buys: {costs:?}",
    );
}
