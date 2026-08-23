//! Fleet metering (M6c).
//!
//! Four things here are easy to get wrong, and each has a case:
//!
//! **A level is not a flow.** `downloads`, `restores` and the token counters are things that happened between
//! one midnight and the next, and the rows carry their own timestamps. `asset_count` and `bytes_by_pool` are
//! how much is *stored*, which `object_placements` only knows as of now. So a day old enough that its level can
//! no longer be observed is refused, rather than silently recorded with today's storage against it — which
//! would draw a flat cost curve out of one number repeated.
//!
//! **Re-running corrects, it does not accumulate.** A retry, a manual re-run and a lease that lapsed mid-pass
//! all have to converge on the same row. A metering job that added would turn a retry into an invoice.
//!
//! **AI cost is `numeric(12, 4)` cents and must be rounded once.** Truncating per row loses all of a cheap
//! enrichment, and a million cheap enrichments is the whole bill.
//!
//! **This one is deliberately not scoped.** Every other count in this codebase runs through a caller's
//! predicate because §7 says a count is a disclosure. A bill narrowed to what one reader can see is not a bill,
//! so there is no predicate in this module's signatures at all — and nothing tenant-facing reads it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, NaiveDate, Utc};
use dam_db::metering::{self, Refusal};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    tenant_id: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("tenant pool");
    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");
    Fixture {
        _pg: pg,
        global,
        acme,
        tenant_id,
    }
}

async fn asset(pool: &PgPool, name: &str, bytes: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', $4, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .bind(bytes)
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn placement(pool: &PgPool, asset_id: Uuid, class: &str, bytes: i64, state: &str) {
    sqlx::query(
        "INSERT INTO object_placements \
         (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state) \
         VALUES ($1, gen_random_uuid(), $2, $3, 'x', $4, $5)",
    )
    .bind(format!("k/{}", Uuid::new_v4()))
    .bind(asset_id)
    .bind(bytes)
    .bind(class)
    .bind(state)
    .execute(pool)
    .await
    .expect("placement");
}

async fn download(pool: &PgPool, asset_id: Uuid, days_ago: i64) {
    sqlx::query(
        "INSERT INTO rights_usage (id, asset_id, downloads, source, recorded_at) \
         VALUES (gen_random_uuid(), $1, 1, 'download', now() - ($2 || ' days')::interval)",
    )
    .bind(asset_id)
    .bind(days_ago)
    .execute(pool)
    .await
    .expect("download");
}

async fn enrichment(pool: &PgPool, asset_id: Uuid, cents: &str, days_ago: i64) {
    sqlx::query(
        "INSERT INTO enrichment_runs \
         (id, asset_id, pipeline, state, input_tokens, output_tokens, cached_tokens, \
          est_cost_cents, started_at) \
         VALUES (gen_random_uuid(), $1, 'describe', 'succeeded', 100, 20, 5, $2::numeric, \
                 now() - ($3 || ' days')::interval)",
    )
    .bind(asset_id)
    .bind(cents)
    .bind(days_ago)
    .execute(pool)
    .await
    .expect("enrichment run");
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

#[tokio::test]
async fn a_flow_belongs_to_its_day_and_a_level_is_the_same_in_both() {
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("conn");
    let one = asset(&f.acme, "harbour", 4_000_000).await;
    placement(&f.acme, one, "STANDARD", 4_000_000, "present").await;

    download(&f.acme, one, 1).await;
    download(&f.acme, one, 1).await;
    download(&f.acme, one, 0).await;

    let yesterday = metering::measure(&mut conn, today() - Duration::days(1), today())
        .await
        .expect("yesterday");
    let now = metering::measure(&mut conn, today(), today())
        .await
        .expect("today");

    // The flow splits by day.
    assert_eq!(yesterday.downloads, 2);
    assert_eq!(now.downloads, 1);
    // The level does not: it is what is stored, and there is one measurement of that.
    assert_eq!(yesterday.asset_count, 1);
    assert_eq!(now.asset_count, 1);
    assert_eq!(yesterday.bytes_by_pool, now.bytes_by_pool);
}

#[tokio::test]
async fn a_day_whose_storage_can_no_longer_be_observed_is_refused() {
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("conn");
    let one = asset(&f.acme, "harbour", 1_000).await;
    download(&f.acme, one, 40).await;

    // The flows for that day are perfectly readable. The level is not, and recording today's storage against
    // last month would be worse than recording nothing — a cost curve that looks flat because it is the same
    // number written forty times.
    let refused = metering::measure(&mut conn, today() - Duration::days(40), today()).await;
    match refused {
        Err(Refusal::LevelUnobservable { day, today: when }) => {
            assert_eq!(day, today() - Duration::days(40));
            assert_eq!(when, today());
            // And the refusal says what the problem is, because "cannot measure" is not actionable.
            let said = Refusal::LevelUnobservable { day, today: when }.to_string();
            assert!(
                said.contains("object_placements only knows what is stored now"),
                "{said}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Yesterday and today are both fine, which is the whole shape the job runs in.
    assert!(
        metering::measure(&mut conn, today() - Duration::days(1), today())
            .await
            .is_ok()
    );
    assert!(metering::measure(&mut conn, today(), today()).await.is_ok());
}

#[tokio::test]
async fn bytes_are_grouped_by_storage_class_and_only_what_is_present() {
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("conn");
    let one = asset(&f.acme, "one", 1_000).await;
    let two = asset(&f.acme, "two", 2_000).await;

    placement(&f.acme, one, "STANDARD", 1_000, "present").await;
    placement(&f.acme, two, "STANDARD", 2_000, "present").await;
    placement(&f.acme, two, "DEEP_ARCHIVE", 9_000, "present").await;
    // Mid-upload: not stored yet, so not on a bill.
    placement(&f.acme, one, "STANDARD", 500_000, "uploading").await;
    // A scrub finding, not a line item.
    placement(&f.acme, one, "GLACIER", 700_000, "missing").await;

    let day = metering::measure(&mut conn, today(), today())
        .await
        .expect("measure");
    assert_eq!(
        day.bytes_by_pool,
        serde_json::json!({ "STANDARD": 3000, "DEEP_ARCHIVE": 9000 })
    );
    assert_eq!(day.asset_count, 2, "assets, not placements");
}

#[tokio::test]
async fn cheap_enrichments_are_rounded_once_rather_than_truncated_each() {
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("conn");
    let one = asset(&f.acme, "harbour", 1_000).await;

    // Ten runs at 0.4 cents. Truncated per row that is zero; summed then rounded it is four — and a library
    // doing a million of these is the difference between a bill and nothing.
    for _ in 0..10 {
        enrichment(&f.acme, one, "0.4000", 0).await;
    }

    let day = metering::measure(&mut conn, today(), today())
        .await
        .expect("measure");
    assert_eq!(day.est_cost_cents, 4);
    assert_eq!(day.ai_input_tokens, 1_000);
    assert_eq!(day.ai_output_tokens, 200);
    assert_eq!(day.ai_cached_tokens, 50);

    // Yesterday saw none of it.
    let yesterday = metering::measure(&mut conn, today() - Duration::days(1), today())
        .await
        .expect("measure");
    assert_eq!(yesterday.est_cost_cents, 0);
    assert_eq!(yesterday.ai_input_tokens, 0);
}

#[tokio::test]
async fn writing_the_same_day_twice_replaces_rather_than_adds() {
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("conn");
    let one = asset(&f.acme, "harbour", 1_000).await;
    placement(&f.acme, one, "STANDARD", 1_000, "present").await;
    download(&f.acme, one, 0).await;

    let first = metering::measure(&mut conn, today(), today())
        .await
        .expect("measure");
    metering::upsert(&f.global, f.tenant_id, today(), &first)
        .await
        .expect("first write");
    // Twice, as a retried job does.
    metering::upsert(&f.global, f.tenant_id, today(), &first)
        .await
        .expect("second write");

    let rows = metering::window(&f.global, f.tenant_id, today(), today())
        .await
        .expect("window");
    assert_eq!(rows.len(), 1, "one row per tenant-day");
    assert_eq!(rows[0].totals.downloads, 1, "not two");

    // And a later, larger measurement replaces the earlier one rather than being added to it.
    download(&f.acme, one, 0).await;
    let again = metering::measure(&mut conn, today(), today())
        .await
        .expect("measure");
    metering::upsert(&f.global, f.tenant_id, today(), &again)
        .await
        .expect("third write");
    let rows = metering::window(&f.global, f.tenant_id, today(), today())
        .await
        .expect("window");
    assert_eq!(rows[0].totals.downloads, 2);
}

#[tokio::test]
async fn a_window_reads_days_in_order_and_only_this_tenants() {
    let f = fixture().await;
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'other', 't_other', 'Other', 'other/', 'active') RETURNING id",
    )
    .fetch_one(&f.global)
    .await
    .expect("other tenant");

    for back in 0..3 {
        let day = today() - Duration::days(back);
        metering::upsert(
            &f.global,
            f.tenant_id,
            day,
            &metering::DayTotals {
                downloads: back + 1,
                ..Default::default()
            },
        )
        .await
        .expect("write");
        // The other tenant's numbers, which must never appear in this one's window — D2, arriving as
        // arithmetic rather than as a query error.
        metering::upsert(
            &f.global,
            other,
            day,
            &metering::DayTotals {
                downloads: 1_000,
                ..Default::default()
            },
        )
        .await
        .expect("write");
    }

    let rows = metering::window(&f.global, f.tenant_id, today() - Duration::days(2), today())
        .await
        .expect("window");
    assert_eq!(rows.len(), 3);
    assert!(
        rows.windows(2).all(|pair| pair[0].day < pair[1].day),
        "oldest first"
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.totals.downloads)
            .collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    assert!(rows.iter().all(|row| row.tenant_id == f.tenant_id));

    // A window with nothing in it is empty rather than an error: a tenant provisioned this morning has no
    // history, and that is a fact rather than a fault.
    let empty = metering::window(
        &f.global,
        f.tenant_id,
        today() - Duration::days(400),
        today() - Duration::days(390),
    )
    .await
    .expect("window");
    assert!(empty.is_empty());
}

#[tokio::test]
async fn an_empty_tenant_meters_as_zeroes_rather_than_as_nothing() {
    // A gap in a billing series is indistinguishable from a worker that was down, so a tenant with no assets
    // must still produce a row.
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("conn");

    let day = metering::measure(&mut conn, today(), today())
        .await
        .expect("measure");
    assert_eq!(day, metering::DayTotals::default());
    // Including the object: an empty map, not null, so a consumer can read it without a branch.
    assert_eq!(day.bytes_by_pool, serde_json::json!({}));

    metering::upsert(&f.global, f.tenant_id, today(), &day)
        .await
        .expect("write");
    let rows = metering::window(&f.global, f.tenant_id, today(), today())
        .await
        .expect("window");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].totals.asset_count, 0);
}
