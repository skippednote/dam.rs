//! Which pages use which assets (M3d·4, §11.4).
//!
//! `connector_asset_refs` has existed since migration 0004 with nothing writing to it. Three things depend on
//! it, and the properties worth proving are the ones where getting it wrong costs real money or tells an
//! operator something false:
//!
//! **A pin expires.** The data is the *site's own*, so a site that has gone quiet — decommissioned, broken
//! module, a token nobody renewed — is indistinguishable from a site that stopped using the asset. If a
//! reference pinned forever, one abandoned integration would hold a library in Standard indefinitely. If it
//! never pinned, a live page would cause a restore storm the first time somebody thawed the original. So the
//! pin holds while the reference is fresh and lapses when it is not.
//!
//! **A full sync has two halves.** Reporting what is used only ever grows the index; something has to say what
//! went away, or a deleted node pins its asset hot forever and every takedown report over-counts.
//!
//! **Two kinds of stale mean different things.** Version drift is a job to run; a missed refresh is a site to
//! go and look at. Both derived, so they cannot disagree with the timestamps under them.
//!
//! **An impact report counts only what is live.** Telling an operator a page exists that does not is worse
//! than telling them nothing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, Utc};
use dam_db::connector_refs::{self, NewRef, STALE_AFTER};
use dam_db::connectors::{self, Kind, NewConnector};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    pool: PgPool,
    site: Uuid,
    other: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    let site = connector(&pool, "Marketing site", "https://www.example.com").await;
    let other = connector(&pool, "Campaign microsite", "https://campaign.example.com").await;
    Fixture {
        _pg: pg,
        pool,
        site,
        other,
    }
}

async fn connector(pool: &PgPool, label: &str, url: &str) -> Uuid {
    let id = Uuid::now_v7();
    let mut conn = pool.acquire().await.expect("conn");
    connectors::register(
        &mut conn,
        &NewConnector {
            id,
            kind: Kind::Drupal,
            label,
            site_url: url,
            remote_version: None,
            api_key_id: None,
            sealed_secret: "v1.k1.n.c",
            asset_group_ids: &[],
            allow_all_groups: true,
            allow_original: false,
            allow_restore: false,
            config: serde_json::json!({}),
        },
    )
    .await
    .expect("register");
    id
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

fn entry<'a>(asset_id: Uuid, entity_id: &'a str, pages: i32) -> NewRef<'a> {
    NewRef {
        asset_id,
        remote_entity_type: "media",
        remote_entity_id: entity_id,
        remote_uuid: None,
        remote_url: Some("https://www.example.com/media/1"),
        usage_count: pages,
        usage_sample: serde_json::json!([{ "url": "/about", "title": "About us" }]),
        synced_version_no: Some(1),
    }
}

#[tokio::test]
async fn reporting_is_an_upsert_on_the_remote_entity_not_the_asset() {
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.expect("conn");
    let first = asset(&f.pool, "harbour").await;
    let second = asset(&f.pool, "quayside").await;
    let now = Utc::now();

    connector_refs::report(&mut conn, f.site, &[entry(first, "42", 3)], now)
        .await
        .expect("report");
    // The same entity, now showing a different asset. One row, updated — not two.
    connector_refs::report(&mut conn, f.site, &[entry(second, "42", 5)], now)
        .await
        .expect("report");

    let rows = connector_refs::for_connector(&mut conn, f.site, 50, now)
        .await
        .expect("refs");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].asset_id, second);
    assert_eq!(rows[0].usage_count, 5);
    // The old asset has no references at all, which is what makes a takedown report about it honest.
    assert!(
        connector_refs::for_asset(&mut conn, first, now)
            .await
            .expect("refs")
            .is_empty()
    );
}

#[tokio::test]
async fn a_fresh_reference_pins_and_a_quiet_site_stops_pinning() {
    // The property that costs money either way. Driven through the *tiering planner's* own query rather than a
    // helper, because that is the one place pinning is decided and a second one would let a dry run disagree
    // with what moves.
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.expect("conn");
    let id = asset(&f.pool, "on-the-homepage").await;
    placement(&f.pool, id, "STANDARD").await;
    let policy = policy(&f.pool).await;

    let now = Utc::now();
    connector_refs::report(&mut conn, f.site, &[entry(id, "42", 12)], now)
        .await
        .expect("report");

    let pinned = pin_reason(&mut conn, &policy, now).await;
    assert_eq!(
        pinned.as_deref(),
        Some("live on 12 page(s) of 'Marketing site'"),
        "a fresh reference pins, and the reason names the site and the count",
    );

    // The same row, a month and a day later. Nothing changed except that the site has not said anything —
    // which is exactly the state an abandoned integration is in.
    let later = now + STALE_AFTER + Duration::days(1);
    assert_eq!(
        pin_reason(&mut conn, &policy, later).await,
        None,
        "a reference nobody has refreshed stops holding the library in Standard",
    );

    // And reporting again brings it back, because that is a site saying it is still using the asset.
    connector_refs::report(&mut conn, f.site, &[entry(id, "42", 12)], later)
        .await
        .expect("report");
    assert!(pin_reason(&mut conn, &policy, later).await.is_some());
}

#[tokio::test]
async fn only_a_live_reference_pins() {
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.expect("conn");
    let id = asset(&f.pool, "harbour").await;
    placement(&f.pool, id, "STANDARD").await;
    let policy = policy(&f.pool).await;
    let now = Utc::now();

    // An entity nobody has placed on a page. A media row existing is not a page existing.
    connector_refs::report(&mut conn, f.site, &[entry(id, "42", 0)], now)
        .await
        .expect("report");
    assert_eq!(
        pin_reason(&mut conn, &policy, now).await,
        None,
        "zero pages"
    );

    connector_refs::report(&mut conn, f.site, &[entry(id, "42", 4)], now)
        .await
        .expect("report");
    assert!(pin_reason(&mut conn, &policy, now).await.is_some());

    // Paused: the site is not rendering anything.
    connectors::set_status(&mut conn, f.site, connectors::Status::Paused)
        .await
        .expect("pause");
    assert_eq!(pin_reason(&mut conn, &policy, now).await, None, "paused");

    // `error` still pins. A failing webhook is not a page going away, and tiering an asset because a delivery
    // attempt got a 500 would be the wrong reaction by a wide margin.
    connectors::set_status(&mut conn, f.site, connectors::Status::Error)
        .await
        .expect("error");
    assert!(
        pin_reason(&mut conn, &policy, now).await.is_some(),
        "an error state is not a page going away",
    );

    // Revoked: terminal, and nothing it ever claimed still holds.
    connectors::set_status(&mut conn, f.site, connectors::Status::Revoked)
        .await
        .expect("revoke");
    assert_eq!(pin_reason(&mut conn, &policy, now).await, None, "revoked");
}

#[tokio::test]
async fn a_sweep_orphans_what_the_site_stopped_reporting() {
    // The other half of a full sync. Without it the index only grows: a deleted node keeps pinning its asset
    // hot and keeps appearing in takedown reports.
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.expect("conn");
    let one = asset(&f.pool, "still-used").await;
    let two = asset(&f.pool, "deleted-node").await;
    placement(&f.pool, two, "STANDARD").await;
    let policy = policy(&f.pool).await;
    let now = Utc::now();

    connector_refs::report(
        &mut conn,
        f.site,
        &[entry(one, "42", 3), entry(two, "43", 7)],
        now,
    )
    .await
    .expect("report");
    assert!(pin_reason(&mut conn, &policy, now).await.is_some());

    // The site's next full sync mentions only entity 42.
    let orphaned = connector_refs::sweep_absent(&mut conn, f.site, "media", &["42"])
        .await
        .expect("sweep");
    assert_eq!(orphaned, 1);
    assert_eq!(
        pin_reason(&mut conn, &policy, now).await,
        None,
        "an orphaned reference describes a page that is gone",
    );

    // The row is kept rather than deleted: an operator asking why something stopped being pinned needs to see
    // that it was once used.
    let rows = connector_refs::for_asset(&mut conn, two, now)
        .await
        .expect("refs");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "orphaned");

    // A sweep of another entity type must not touch these. One integration orphaning another's rows would be a
    // module nobody could safely install beside another.
    connector_refs::report(&mut conn, f.site, &[entry(one, "42", 3)], now)
        .await
        .expect("report");
    let swept = connector_refs::sweep_absent(&mut conn, f.site, "paragraph", &[])
        .await
        .expect("sweep");
    assert_eq!(swept, 0, "a different entity type is a different index");
    assert_eq!(
        connector_refs::for_connector(&mut conn, f.site, 50, now)
            .await
            .expect("refs")
            .iter()
            .filter(|row| row.state == "linked")
            .count(),
        1
    );

    // And a sweep is idempotent: running it twice does not keep reporting work it already did.
    connector_refs::sweep_absent(&mut conn, f.site, "media", &["42"])
        .await
        .expect("sweep");
    assert_eq!(
        connector_refs::sweep_absent(&mut conn, f.site, "media", &["42"])
            .await
            .expect("sweep"),
        0
    );
}

#[tokio::test]
async fn version_drift_and_a_missed_refresh_are_different_facts() {
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.expect("conn");
    let id = asset(&f.pool, "harbour").await;
    let now = Utc::now();

    connector_refs::report(&mut conn, f.site, &[entry(id, "42", 3)], now)
        .await
        .expect("report");
    let rows = connector_refs::for_asset(&mut conn, id, now)
        .await
        .expect("refs");
    assert!(!rows[0].version_drifted);
    assert!(!rows[0].refresh_overdue);

    // A new version in damrs. The site is still rendering version 1 — a job to run, not a site to chase.
    sqlx::query("UPDATE assets SET version_no = 2 WHERE id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .expect("bump");
    let rows = connector_refs::for_asset(&mut conn, id, now)
        .await
        .expect("refs");
    assert!(rows[0].version_drifted, "the site is behind");
    assert!(!rows[0].refresh_overdue, "and it is still reporting");

    // A month later with no word from the site: now both, and they still mean different things.
    let rows = connector_refs::for_asset(&mut conn, id, now + STALE_AFTER + Duration::days(1))
        .await
        .expect("refs");
    assert!(rows[0].version_drifted);
    assert!(rows[0].refresh_overdue);

    // Derived, not stored: nothing wrote `stale` into the column, so the two cannot disagree with the
    // timestamps under them.
    assert_eq!(rows[0].state, "linked");
}

#[tokio::test]
async fn an_impact_report_counts_only_what_is_live() {
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.expect("conn");
    let busy = asset(&f.pool, "on-two-sites").await;
    let quiet = asset(&f.pool, "nowhere").await;
    let now = Utc::now();

    connector_refs::report(
        &mut conn,
        f.site,
        &[entry(busy, "42", 12), entry(busy, "43", 3)],
        now,
    )
    .await
    .expect("report");
    connector_refs::report(&mut conn, f.other, &[entry(busy, "9", 1)], now)
        .await
        .expect("report");

    let impact = connector_refs::impact(&mut conn, &[busy, quiet], now)
        .await
        .expect("impact");
    let found = impact.get(&busy).copied().expect("the busy one");
    assert_eq!(found.sites, 2, "two connected sites");
    assert_eq!(found.entities, 3, "three remote entities");
    assert_eq!(
        found.pages, 16,
        "sixteen places, summed from what each site said"
    );
    // Absent rather than zero: the caller is asking "which of these are in use", and a map of zeroes turns a
    // lookup into a filter.
    assert!(!impact.contains_key(&quiet));

    // Orphan one entity and the numbers move. A report that counted it would tell an operator a page exists
    // that does not.
    connector_refs::sweep_absent(&mut conn, f.site, "media", &["42"])
        .await
        .expect("sweep");
    let after = connector_refs::impact(&mut conn, &[busy], now)
        .await
        .expect("impact");
    let found = after.get(&busy).copied().expect("still in use");
    assert_eq!(found.entities, 2);
    assert_eq!(found.pages, 13);

    // And a quiet site drops out of the report entirely, for the same reason it stops pinning.
    let later = now + STALE_AFTER + Duration::days(1);
    assert!(
        connector_refs::impact(&mut conn, &[busy], later)
            .await
            .expect("impact")
            .is_empty(),
        "a site that has said nothing for a month is not evidence of a live page",
    );

    // An empty request is an empty answer rather than a query.
    assert!(
        connector_refs::impact(&mut conn, &[], now)
            .await
            .expect("impact")
            .is_empty()
    );
}

#[tokio::test]
async fn damrs_can_mark_a_reference_expired_without_the_site_saying_so() {
    // For the states a site cannot report about itself: a licence expiring here, or an asset being
    // unpublished. It will learn through the webhook outbox; this is what the report says meanwhile.
    let f = fixture().await;
    let mut conn = f.pool.acquire().await.expect("conn");
    let id = asset(&f.pool, "expiring").await;
    placement(&f.pool, id, "STANDARD").await;
    let policy = policy(&f.pool).await;
    let now = Utc::now();

    connector_refs::report(&mut conn, f.site, &[entry(id, "42", 5)], now)
        .await
        .expect("report");
    assert!(pin_reason(&mut conn, &policy, now).await.is_some());

    assert_eq!(
        connector_refs::set_state(&mut conn, id, "expired")
            .await
            .expect("expire"),
        1
    );
    assert_eq!(
        pin_reason(&mut conn, &policy, now).await,
        None,
        "an expired reference is not a page anybody should be serving",
    );

    // An orphaned row is not resurrected into another state: the site said it is gone, and damrs saying
    // "expired" on top of that would lose the more specific fact.
    connector_refs::sweep_absent(&mut conn, f.site, "media", &[])
        .await
        .expect("sweep");
    assert_eq!(
        connector_refs::set_state(&mut conn, id, "unpublished")
            .await
            .expect("state"),
        0
    );
}

// ─── fixtures for the tiering path ──────────────────────────────────────────

async fn placement(pool: &PgPool, asset_id: Uuid, class: &str) {
    let pool_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.storage_pools \
           (id, tenant_id, name, driver, bucket, credentials_ref, storage_class, latency_class) \
         VALUES (gen_random_uuid(), NULL, $1, 's3', 'b', 'test', 'STANDARD', 'instant') RETURNING id",
    )
    .bind(format!("pool-{asset_id}"))
    .fetch_one(pool)
    .await
    .expect("pool");
    sqlx::query(
        "INSERT INTO object_placements \
           (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state, placed_at) \
         VALUES ($1, $2, $3, 5000000, 'x', $4, 'present', now() - interval '400 days')",
    )
    .bind(format!("acme/o/{asset_id}"))
    .bind(pool_id)
    .bind(asset_id)
    .bind(class)
    .execute(pool)
    .await
    .expect("placement");
}

async fn policy(pool: &PgPool) -> dam_db::tiering::Policy {
    // A transition needs somewhere to go: `lifecycle_target_present` refuses one without both a target pool
    // and a target class, which is the schema refusing a policy that could never execute.
    let target: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.storage_pools \
           (id, tenant_id, name, driver, bucket, credentials_ref, storage_class, latency_class) \
         VALUES (gen_random_uuid(), NULL, 'glacier-target', 's3', 'b', 'test', 'GLACIER', 'hours') \
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("target pool");
    sqlx::query(
        "INSERT INTO lifecycle_policies \
           (id, name, enabled, applies_to, action, from_storage_class, target_class, \
            target_pool_id, min_age_days) \
         VALUES (gen_random_uuid(), 'cold', true, 'original', 'transition', 'STANDARD', \
                 'GLACIER', $1, 30)",
    )
    .bind(target)
    .execute(pool)
    .await
    .expect("policy");
    let mut conn = pool.acquire().await.expect("conn");
    dam_db::tiering::policies(&mut conn)
        .await
        .expect("policies")
        .into_iter()
        .next()
        .expect("one policy")
}

/// The reason the *planner's own query* gives for pinning, or `None` if it does not.
async fn pin_reason(
    conn: &mut sqlx::PgConnection,
    policy: &dam_db::tiering::Policy,
    now: chrono::DateTime<Utc>,
) -> Option<String> {
    let candidates = dam_db::tiering::candidates(conn, policy, now)
        .await
        .expect("candidates");
    candidates
        .into_iter()
        .find(|candidate| candidate.pinned)
        .and_then(|candidate| candidate.pin_reason)
}
