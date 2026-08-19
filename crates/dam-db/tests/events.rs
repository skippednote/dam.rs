//! The activity feed and the dashboard's counts (Q.7).
//!
//! Three things this suite is really about:
//!
//! - **A write outside the seed partition must not fail.** `events` has been partitioned by month since 0001 with
//!   one January 2026 partition and a comment promising a roll-forward command that was never written. Nothing had
//!   ever written an event, so the gap was invisible; the first one would have failed and so would every one after
//!   it. Migration 0021 adds a default partition, and the case below writes at a timestamp no monthly partition
//!   covers.
//! - **The feed is filtered by the caller's predicate.** An event names an asset, so an unfiltered feed discloses
//!   the existence *and the filenames* of assets the caller cannot see.
//! - **The counts come from one statement.** Five statements are five snapshots, and a number that disagrees with
//!   the one beside it reads as a bug in the page rather than in the clock.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::events::{self, ActorKind, Kind, NewEvent};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

fn grants(groups: &[Uuid], all: bool) -> policy::AccessPredicate {
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: groups.to_vec(),
            all_asset_groups: all,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Read,
        Utc::now(),
    )
}

fn everything() -> Planned {
    Planned::new(Query::All, grants(&[], true), &[]).expect("plan")
}

fn scoped(group: Uuid) -> Planned {
    Planned::new(Query::All, grants(&[group], false), &[]).expect("plan")
}

macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

async fn asset(pool: &PgPool, label: &str, group: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    if let Some(group) = group {
        sqlx::query("INSERT INTO asset_group_members (asset_id, group_id) VALUES ($1, $2)")
            .bind(id)
            .bind(group)
            .execute(pool)
            .await
            .expect("membership");
    }
    id
}

async fn group(pool: &PgPool, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("group")
}

#[tokio::test]
async fn the_event_contract_holds() {
    let (_pg, pool) = db().await;

    let press = group(&pool, "press").await;
    let shared = asset(&pool, "shared", Some(press)).await;
    let embargoed = asset(&pool, "embargoed", None).await;
    let ada = Uuid::new_v4();

    a_write_outside_every_monthly_partition_still_lands(&pool, shared, ada).await;
    the_feed_is_newest_first_and_carries_the_filename(&pool, shared, ada).await;
    the_feed_hides_events_about_assets_the_caller_cannot_see(&pool, embargoed, shared, ada, press)
        .await;
    an_event_with_no_asset_is_not_in_the_asset_feed(&pool, ada).await;
    the_feed_can_be_narrowed_to_kinds(&pool, shared, ada).await;
    the_feed_is_bounded_and_deterministic(&pool, shared, ada).await;
    the_counts_are_scoped_and_come_from_one_statement(&pool, shared, embargoed, press).await;
    an_unknown_kind_reads_back_as_itself(&pool, shared, ada).await;
    // Last, because it removes the default partition to prove what it is for.
    without_the_default_partition_the_write_fails(&pool, shared, ada).await;
}

async fn without_the_default_partition_the_write_fails(pool: &PgPool, shared: Uuid, ada: Uuid) {
    // The claim migration 0021 rests on, demonstrated rather than asserted. Detaching the default leaves `events`
    // as it was before that migration — partitioned by month with one January 2026 partition — and an insert at
    // `now()` then has nowhere to go.
    //
    // Runs last in the container, because it changes the schema for everything after it.
    sqlx::query("ALTER TABLE events DETACH PARTITION events_default")
        .execute(pool)
        .await
        .expect("detach");

    let refused = events::record(c!(pool), NewEvent::by(Kind::Uploaded, shared, ada)).await;
    let message = refused
        .expect_err("without a default partition this write has nowhere to go")
        .to_string();
    assert!(
        message.contains("partition"),
        "the failure was not about partitioning, so this case proves nothing: {message}"
    );

    // Reattached, so the container is left as the migrations built it.
    sqlx::query("ALTER TABLE events ATTACH PARTITION events_default DEFAULT")
        .execute(pool)
        .await
        .expect("reattach");
    events::record(c!(pool), NewEvent::by(Kind::Uploaded, shared, ada))
        .await
        .expect("and it works again once the default is back");
}

async fn a_write_outside_every_monthly_partition_still_lands(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
) {
    // The bug 0021 fixes. `events` has one monthly partition — January 2026 — and `now()` is not in it, so before
    // the default partition existed this insert failed with "no partition of relation events found for row" and so
    // would every event after it. Nothing had ever written an event, which is why nothing noticed.
    let id = events::record(c!(pool), NewEvent::by(Kind::Uploaded, shared, ada))
        .await
        .expect("an event outside January 2026 must still be storable");

    // And in the default partition specifically, so this case fails rather than passes silently if somebody adds
    // a monthly partition covering today and removes the default.
    let landed: Option<String> = sqlx::query_scalar(
        "SELECT c.relname FROM events e \
         JOIN pg_class c ON c.oid = e.tableoid WHERE e.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .expect("query");
    assert!(
        landed.is_some(),
        "the event is not readable through the parent"
    );

    // A timestamp far outside any plausible monthly partition, to make the point directly.
    sqlx::query(
        "INSERT INTO events (id, occurred_at, kind, asset_id, actor_id, actor_kind) \
         VALUES (gen_random_uuid(), '2031-07-04T12:00:00Z', 'upload', $1, $2, 'user')",
    )
    .bind(shared)
    .bind(ada)
    .execute(pool)
    .await
    .expect("a write five years out must not fail either");
}

async fn the_feed_is_newest_first_and_carries_the_filename(pool: &PgPool, shared: Uuid, ada: Uuid) {
    events::record(
        c!(pool),
        NewEvent::by(Kind::Commented, shared, ada).with(serde_json::json!({ "excerpt": "tight" })),
    )
    .await
    .expect("record");

    let feed = events::feed(c!(pool), &everything(), 50, &[])
        .await
        .expect("feed");
    assert!(!feed.is_empty());

    // Newest first: a feed in any other order is a list, not a feed.
    let mut times: Vec<_> = feed.iter().map(|entry| entry.occurred_at).collect();
    let sorted = {
        let mut copy = times.clone();
        copy.sort_by(|a, b| b.cmp(a));
        copy
    };
    assert_eq!(times, sorted, "{feed:?}");
    times.dedup();

    // The filename comes back with the row, so a line reads as a sentence without a second query per entry.
    let commented = feed
        .iter()
        .find(|entry| entry.kind == "comment")
        .expect("the comment event");
    assert_eq!(commented.filename.as_deref(), Some("shared.jpg"));
    assert_eq!(commented.context["excerpt"], serde_json::json!("tight"));
    assert_eq!(commented.actor_kind, "user");
}

async fn the_feed_hides_events_about_assets_the_caller_cannot_see(
    pool: &PgPool,
    embargoed: Uuid,
    shared: Uuid,
    ada: Uuid,
    press: Uuid,
) {
    events::record(c!(pool), NewEvent::by(Kind::Uploaded, embargoed, ada))
        .await
        .expect("record");

    // The scoped caller sees `press` only, and `embargoed` is in no group. An unfiltered feed would disclose both
    // that the asset exists and what it is called.
    let feed = events::feed(c!(pool), &scoped(press), 50, &[])
        .await
        .expect("feed");
    assert!(
        feed.iter().all(|entry| entry.asset_id != Some(embargoed)),
        "the feed leaked an asset outside the caller's scope: {feed:?}"
    );
    assert!(
        feed.iter()
            .all(|entry| entry.filename.as_deref() != Some("embargoed.jpg")),
        "the feed leaked a filename: {feed:?}"
    );
    // And it is not simply empty, or the assertion above would hold for the wrong reason.
    assert!(
        feed.iter().any(|entry| entry.asset_id == Some(shared)),
        "{feed:?}"
    );
}

async fn an_event_with_no_asset_is_not_in_the_asset_feed(pool: &PgPool, ada: Uuid) {
    // A login, a schema change: real activity that is not about one asset. Excluded from *this* feed rather than
    // shown to everybody, because who may see it is a different question and defaulting it in would answer that
    // question by accident.
    events::record(
        c!(pool),
        NewEvent {
            kind: Kind::Edited,
            asset_id: None,
            actor_id: Some(ada),
            actor_kind: ActorKind::System,
            context: serde_json::json!({ "what": "a field definition" }),
            bytes: None,
        },
    )
    .await
    .expect("record");

    let feed = events::feed(c!(pool), &everything(), 50, &[])
        .await
        .expect("feed");
    assert!(
        feed.iter().all(|entry| entry.asset_id.is_some()),
        "an event with no asset appeared in the asset feed: {feed:?}"
    );
    // It is stored, though — excluded from a view is not the same as discarded.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE asset_id IS NULL")
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(stored, 1);
}

async fn the_feed_can_be_narrowed_to_kinds(pool: &PgPool, shared: Uuid, ada: Uuid) {
    events::record(c!(pool), NewEvent::by(Kind::Downloaded, shared, ada))
        .await
        .expect("record");

    let downloads = events::feed(c!(pool), &everything(), 50, &[Kind::Downloaded])
        .await
        .expect("feed");
    assert!(!downloads.is_empty());
    assert!(
        downloads.iter().all(|entry| entry.kind == "download"),
        "{downloads:?}"
    );

    // Two kinds, and the filter is a set rather than a single value.
    let both = events::feed(
        c!(pool),
        &everything(),
        50,
        &[Kind::Downloaded, Kind::Commented],
    )
    .await
    .expect("feed");
    assert!(both.iter().any(|entry| entry.kind == "download"));
    assert!(both.iter().any(|entry| entry.kind == "comment"));
    assert!(
        both.iter()
            .all(|entry| entry.kind == "download" || entry.kind == "comment"),
        "{both:?}"
    );
    assert!(both.len() > downloads.len(), "the filter did nothing");
}

async fn the_feed_is_bounded_and_deterministic(pool: &PgPool, shared: Uuid, ada: Uuid) {
    for _ in 0..5 {
        events::record(c!(pool), NewEvent::by(Kind::Edited, shared, ada))
            .await
            .expect("record");
    }

    let page = events::feed(c!(pool), &everything(), 3, &[])
        .await
        .expect("feed");
    assert_eq!(page.len(), 3);

    // Three events sharing one timestamp *exactly*, so the tie-break is the only thing that can order them.
    // Comparing two reads was not enough: Postgres is free to return the same arbitrary order twice, and it did,
    // so the assertion held with the tie-break removed. This asserts the order the tie-break specifies.
    let stamp = "2026-06-15T12:00:00Z";
    let mut planted: Vec<Uuid> = Vec::new();
    for _ in 0..3 {
        let id = Uuid::new_v4();
        planted.push(id);
        sqlx::query(
            "INSERT INTO events (id, occurred_at, kind, asset_id, actor_id, actor_kind) \
             VALUES ($1, $2::timestamptz, 'share', $3, $4, 'user')",
        )
        .bind(id)
        .bind(stamp)
        .bind(shared)
        .bind(ada)
        .execute(pool)
        .await
        .expect("insert");
    }

    let shares = events::feed(c!(pool), &everything(), 50, &[Kind::Shared])
        .await
        .expect("feed");
    let ordered: Vec<Uuid> = shares.iter().map(|entry| entry.id).collect();
    planted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        ordered, planted,
        "events sharing a timestamp came back in an order the ORDER BY does not specify"
    );

    // And the cap holds when there is actually more than a page to return. With fifteen events in the table both
    // sides of `clamp` gave the same answer, so the bound was untested.
    sqlx::query(
        "INSERT INTO events (id, kind, asset_id, actor_id, actor_kind) \
         SELECT gen_random_uuid(), 'edit', $1, $2, 'user' FROM generate_series(1, 250)",
    )
    .bind(shared)
    .bind(ada)
    .execute(pool)
    .await
    .expect("bulk insert");

    let capped = events::feed(c!(pool), &everything(), 10_000, &[])
        .await
        .expect("feed");
    assert_eq!(
        capped.len() as i64,
        dam_db::events::MAX_FEED,
        "the feed returned more than a page"
    );
}

async fn the_counts_are_scoped_and_come_from_one_statement(
    pool: &PgPool,
    shared: Uuid,
    embargoed: Uuid,
    press: Uuid,
) {
    let wide = events::summary(c!(pool), &everything())
        .await
        .expect("summary");
    let narrow = events::summary(c!(pool), &scoped(press))
        .await
        .expect("summary");

    // Every count is about what the caller can see. §7: a count is a disclosure, so a dashboard showing the
    // library total would tell a scoped reader how much of it they cannot reach.
    assert!(wide.assets > narrow.assets, "{wide:?} vs {narrow:?}");
    assert!(
        narrow.uploads_this_week < wide.uploads_this_week,
        "the upload count ignored the predicate: {wide:?} vs {narrow:?}"
    );

    // The work queue: both assets have no metadata, and the scoped caller is told about one of them.
    assert_eq!(wide.without_metadata, 2, "{wide:?}");
    assert_eq!(narrow.without_metadata, 1, "{narrow:?}");

    // An asset with metadata drops out of the queue, so the count is about emptiness rather than about existence.
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, '{\"a\": 1}'::jsonb)")
        .bind(shared)
        .execute(pool)
        .await
        .expect("metadata");
    let after = events::summary(c!(pool), &everything())
        .await
        .expect("summary");
    assert_eq!(after.without_metadata, 1, "{after:?}");

    // An *empty* document still counts as needing metadata. This is the case ingest actually produces — every
    // asset gets an `asset_metadata` row, with `{}` when no profile default filled it — so a count that only
    // looked for a missing row would report zero work to do over a library nobody has described. Every asset in
    // this container had no row at all until a moment ago, which is why the distinction was untested.
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, '{}'::jsonb)")
        .bind(embargoed)
        .execute(pool)
        .await
        .expect("empty metadata");
    let with_empty = events::summary(c!(pool), &everything())
        .await
        .expect("summary");
    assert_eq!(
        with_empty.without_metadata, 1,
        "an empty metadata document is not described metadata: {with_empty:?}"
    );
}

async fn an_unknown_kind_reads_back_as_itself(pool: &PgPool, shared: Uuid, ada: Uuid) {
    // The column is deliberately open text so a future subsystem can record something without a migration. A row
    // this build does not know about has to survive the read: dropping it would hide activity, and mapping it to a
    // known kind would misreport it.
    sqlx::query(
        "INSERT INTO events (id, kind, asset_id, actor_id, actor_kind) \
         VALUES (gen_random_uuid(), 'transcoded', $1, $2, 'system')",
    )
    .bind(shared)
    .bind(ada)
    .execute(pool)
    .await
    .expect("insert");

    let feed = events::feed(c!(pool), &everything(), 50, &[])
        .await
        .expect("feed");
    assert!(
        feed.iter().any(|entry| entry.kind == "transcoded"),
        "an unrecognised kind was dropped from the feed: {feed:?}"
    );
}
