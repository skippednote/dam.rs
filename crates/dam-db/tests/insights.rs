//! Insights over a real library (M6c).
//!
//! Five queries, and the risk in each is a different one:
//!
//! **The date spine.** A chart drawn from only the days that had events has no holes — it draws a straight line
//! across a quiet week, which is a lie about the shape. So a quiet day must come back as a row of zeroes.
//!
//! **Downloads come from the ledger, not the feed.** `rights_usage` records every download including one taken
//! through a share link; `events` records only downloads by somebody with an identity, because `actor_id` is an
//! identity and a share token is not one. A "most downloaded" list built on the feed would omit exactly the
//! downloads a rights manager most wants to see. The case that catches it is a link download with no actor.
//!
//! **The ledger holds three kinds of row.** Connector usage reports and manual print runs live in the same
//! table and answer "where is this in use". Summing them with downloads gives a number that is neither.
//!
//! **Never-downloaded means ever, not in the window.** An asset taken once two years ago has a different
//! answer from one taken never, and the whole point of the list is the second.
//!
//! **Every count runs through the caller's predicate.** A scoped reader's graph is their own — §7 says a count
//! is a disclosure — which also means two people legitimately see different charts.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_db::insights;
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn access(groups: Option<&[Uuid]>) -> policy::AccessPredicate {
    let (ids, all) = match groups {
        Some(ids) => (ids.to_vec(), false),
        None => (vec![], true),
    };
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: ids,
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

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

/// An asset, created `age_days` ago so the never-downloaded ordering means something.
async fn asset(pool: &PgPool, name: &str, mime: &str, bytes: i64, age_days: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, created_at) \
         VALUES ($1, $2, $3, $4, $5, $1, now() - ($6 || ' days')::interval)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.{}", extension(mime)))
    .bind(mime)
    .bind(bytes)
    .bind(age_days)
    .execute(pool)
    .await
    .expect("asset");
    id
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "video/mp4" => "mp4",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// A download in the ledger, `days_ago` ago. `who` is `None` for a share-link download.
async fn download(pool: &PgPool, asset_id: Uuid, days_ago: i64, who: Option<Uuid>) {
    sqlx::query(
        "INSERT INTO rights_usage (id, asset_id, downloads, source, recorded_by, recorded_at) \
         VALUES (gen_random_uuid(), $1, 1, 'download', $2, now() - ($3 || ' days')::interval)",
    )
    .bind(asset_id)
    .bind(who)
    .bind(days_ago)
    .execute(pool)
    .await
    .expect("download");
}

/// A ledger row that is *not* a download: a connector's usage report.
async fn connector_usage(pool: &PgPool, asset_id: Uuid, impressions: i64) {
    sqlx::query(
        "INSERT INTO rights_usage (id, asset_id, impressions, source) \
         VALUES (gen_random_uuid(), $1, $2, 'connector')",
    )
    .bind(asset_id)
    .bind(impressions)
    .execute(pool)
    .await
    .expect("connector usage");
}

async fn event(pool: &PgPool, kind: &str, asset_id: Uuid, actor: Option<Uuid>, days_ago: i64) {
    sqlx::query(
        "INSERT INTO events (id, occurred_at, kind, asset_id, actor_id, actor_kind) \
         VALUES (gen_random_uuid(), now() - ($1 || ' days')::interval, $2, $3, $4, 'user')",
    )
    .bind(days_ago)
    .bind(kind)
    .bind(asset_id)
    .bind(actor)
    .execute(pool)
    .await
    .expect("event");
}

async fn group(pool: &PgPool, key: &str, members: &[Uuid]) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("group");
    for member in members {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(id)
            .bind(member)
            .execute(pool)
            .await
            .expect("member");
    }
    id
}

#[tokio::test]
async fn a_quiet_day_is_a_row_of_zeroes() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let one = asset(&pool, "harbour", "image/jpeg", 4_000_000, 30).await;

    // Activity today and six days ago, nothing between.
    event(&pool, "upload", one, None, 6).await;
    event(&pool, "comment", one, None, 0).await;
    download(&pool, one, 0, None).await;

    let days = insights::series(&mut conn, &access(None), 7)
        .await
        .expect("series");

    // Seven rows for seven days, not two rows for the two days that had something.
    assert_eq!(days.len(), 7, "the spine, not the data: {days:?}");
    assert_eq!(days[0].uploads, 1, "six days ago");
    assert_eq!(days[6].comments, 1, "today");
    assert_eq!(days[6].downloads, 1);
    // The quiet middle, which is the whole point.
    assert!(
        days[1..6].iter().all(|day| day.uploads == 0
            && day.downloads == 0
            && day.edits == 0
            && day.comments == 0
            && day.shares == 0),
        "{days:?}"
    );
    // And in order, oldest first, so a chart's x-axis needs no sorting.
    assert!(days.windows(2).all(|pair| pair[0].day < pair[1].day));
}

#[tokio::test]
async fn a_share_link_download_counts_and_a_connector_report_does_not() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let one = asset(&pool, "harbour", "image/jpeg", 4_000_000, 30).await;
    let ada = Uuid::new_v4();

    download(&pool, one, 1, Some(ada)).await;
    // No actor: taken through a share link by somebody outside the tenant. The event feed cannot hold this —
    // `actor_id` is an identity — which is why the ledger is the source.
    download(&pool, one, 1, None).await;
    // A connector's usage report and nothing else. Same table, different question.
    connector_usage(&pool, one, 50_000).await;

    let days = insights::series(&mut conn, &access(None), 7)
        .await
        .expect("series");
    let total: i64 = days.iter().map(|day| day.downloads).sum();
    assert_eq!(total, 2, "two downloads, and not the impression report");

    let top = insights::most_downloaded(&mut conn, &access(None), 7, 10)
        .await
        .expect("top");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].count, 2);
    assert_eq!(top[0].filename, "harbour.jpg");
    assert!(top[0].last_at.is_some());
}

#[tokio::test]
async fn never_downloaded_means_ever_and_leads_with_the_oldest() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let ancient = asset(&pool, "ancient", "image/jpeg", 1_000, 900).await;
    let old = asset(&pool, "old", "image/jpeg", 1_000, 400).await;
    let recent = asset(&pool, "recent", "image/jpeg", 1_000, 2).await;
    let taken_long_ago = asset(&pool, "taken", "image/jpeg", 1_000, 800).await;

    // Outside any window a chart would ask for — and still enough to keep it off this list, because the
    // question is "has anybody ever used it".
    download(&pool, taken_long_ago, 700, None).await;

    let unused = insights::never_downloaded(&mut conn, &access(None), 10)
        .await
        .expect("unused");
    let names: Vec<&str> = unused.iter().map(|row| row.filename.as_str()).collect();
    assert_eq!(names, vec!["ancient.jpg", "old.jpg", "recent.jpg"]);
    assert!(
        unused
            .iter()
            .all(|row| row.count == 0 && row.last_at.is_none())
    );
    let _ = (ancient, old, recent);

    // And the count is of all of them, not of the page. A capped list of unused assets reads as the whole
    // problem; on the dev library it was twenty rows of a much larger number.
    assert_eq!(
        insights::never_downloaded_count(&mut conn, &access(None))
            .await
            .expect("count"),
        3
    );
    let capped = insights::never_downloaded(&mut conn, &access(None), 1)
        .await
        .expect("capped");
    assert_eq!(capped.len(), 1, "the page is capped");
    assert_eq!(
        insights::never_downloaded_count(&mut conn, &access(None))
            .await
            .expect("count"),
        3,
        "and the count is not"
    );
}

#[tokio::test]
async fn the_library_is_summarised_by_class_not_by_mime_type() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    asset(&pool, "a", "image/jpeg", 4_000_000, 1).await;
    asset(&pool, "b", "image/jpeg", 6_000_000, 1).await;
    asset(&pool, "c", "video/mp4", 800_000_000, 1).await;
    asset(&pool, "d", "application/pdf", 200_000, 1).await;
    asset(&pool, "e", "application/zip", 5_000, 1).await;

    let classes = insights::by_class(&mut conn, &access(None))
        .await
        .expect("classes");
    // Largest by bytes first, because that is the question a storage bill asks.
    assert_eq!(classes[0].class, "video");
    assert_eq!(classes[0].assets, 1);
    assert_eq!(classes[0].bytes, 800_000_000);
    let images = classes
        .iter()
        .find(|one| one.class == "image")
        .expect("image");
    assert_eq!(images.assets, 2);
    assert_eq!(images.bytes, 10_000_000);
    assert!(classes.iter().any(|one| one.class == "document"));
    // Anything unclassified is `other` rather than absent: a class nobody expected still costs money.
    assert!(classes.iter().any(|one| one.class == "other"));
}

#[tokio::test]
async fn contributors_are_counted_but_never_their_downloads() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let one = asset(&pool, "harbour", "image/jpeg", 1_000, 5).await;
    let ada = Uuid::new_v4();
    let bob = Uuid::new_v4();

    event(&pool, "upload", one, Some(ada), 3).await;
    event(&pool, "upload", one, Some(ada), 2).await;
    event(&pool, "comment", one, Some(ada), 1).await;
    event(&pool, "edit", one, Some(bob), 1).await;
    // A download by Ada, in both places it could be recorded. Neither should reach this list: a person's
    // download history reads as surveillance, and the per-asset ledger already answers "who took this".
    download(&pool, one, 1, Some(ada)).await;
    event(&pool, "download", one, Some(ada), 1).await;
    // And a system event with no actor, which must not become a row with a nil uuid.
    event(&pool, "upload", one, None, 1).await;

    let people = insights::contributors(&mut conn, &access(None), 7, 10)
        .await
        .expect("contributors");
    assert_eq!(people.len(), 2, "{people:?}");
    assert_eq!(people[0].identity_id, ada);
    assert_eq!(people[0].uploads, 2);
    assert_eq!(people[0].comments, 1);
    assert_eq!(people[0].edits, 0);
    assert_eq!(people[1].identity_id, bob);
    assert_eq!(people[1].edits, 1);
    // No download column exists to be filled, which is the assertion: the struct has three counts.
}

#[tokio::test]
async fn every_number_is_the_readers_own() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let mine = asset(&pool, "mine", "image/jpeg", 1_000, 10).await;
    let theirs = asset(&pool, "theirs", "video/mp4", 9_000, 10).await;
    let ada = Uuid::new_v4();

    let scoped = group(&pool, "mine", &[mine]).await;

    download(&pool, mine, 1, None).await;
    download(&pool, theirs, 1, None).await;
    download(&pool, theirs, 1, None).await;
    event(&pool, "upload", mine, Some(ada), 1).await;
    event(&pool, "upload", theirs, Some(ada), 1).await;

    let wide = access(None);
    let narrow = access(Some(&[scoped]));

    // The series: three downloads for a reader who can see everything, one for the scoped reader. Not "three,
    // of which you may open one" — that would tell them exactly how much they cannot reach.
    let all: i64 = insights::series(&mut conn, &wide, 7)
        .await
        .expect("wide")
        .iter()
        .map(|day| day.downloads)
        .sum();
    let ours: i64 = insights::series(&mut conn, &narrow, 7)
        .await
        .expect("narrow")
        .iter()
        .map(|day| day.downloads)
        .sum();
    assert_eq!((all, ours), (3, 1));

    // The top list: the invisible asset is absent, not present with a count.
    let top = insights::most_downloaded(&mut conn, &narrow, 7, 10)
        .await
        .expect("top");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].filename, "mine.jpg");

    // The class breakdown: no `video` row at all for the scoped reader, so the storage total is theirs too.
    let classes = insights::by_class(&mut conn, &narrow)
        .await
        .expect("classes");
    assert_eq!(classes.len(), 1);
    assert_eq!(classes[0].class, "image");
    assert_eq!(classes[0].bytes, 1_000);

    // And the contributor counts. Ada uploaded two, and this reader sees one of them — which is exactly why
    // this list is not a performance measure.
    let people = insights::contributors(&mut conn, &narrow, 7, 10)
        .await
        .expect("people");
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].uploads, 1);

    // The unused list, from the other direction: an asset the reader cannot see is not their problem.
    let unused = insights::never_downloaded(&mut conn, &narrow, 10)
        .await
        .expect("unused");
    assert!(unused.is_empty(), "{unused:?}");
    // The count too. A scoped total is the one number on this surface where a leak would be a single integer
    // saying how many assets exist that the reader cannot see.
    assert_eq!(
        insights::never_downloaded_count(&mut conn, &narrow)
            .await
            .expect("count"),
        0
    );
    assert_eq!(
        insights::never_downloaded_count(&mut conn, &wide)
            .await
            .expect("count"),
        0,
        "both assets here have been downloaded"
    );
}

#[tokio::test]
async fn a_reader_who_can_see_nothing_gets_zeroes_rather_than_everything() {
    // The failure `access::push_asset_filter` exists to prevent, arriving through this module: a predicate that
    // matches nothing must render as a false condition, not as an omitted filter.
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let one = asset(&pool, "harbour", "image/jpeg", 1_000, 10).await;
    download(&pool, one, 1, None).await;
    event(&pool, "upload", one, Some(Uuid::new_v4()), 1).await;

    let nothing = access(Some(&[]));
    let days = insights::series(&mut conn, &nothing, 7)
        .await
        .expect("series");
    assert_eq!(days.len(), 7, "the spine is still drawn");
    assert!(
        days.iter()
            .all(|day| day.downloads == 0 && day.uploads == 0)
    );
    assert!(
        insights::most_downloaded(&mut conn, &nothing, 7, 10)
            .await
            .expect("top")
            .is_empty()
    );
    assert!(
        insights::by_class(&mut conn, &nothing)
            .await
            .expect("classes")
            .is_empty()
    );
    assert!(
        insights::contributors(&mut conn, &nothing, 7, 10)
            .await
            .expect("people")
            .is_empty()
    );
    assert!(
        insights::never_downloaded(&mut conn, &nothing, 10)
            .await
            .expect("unused")
            .is_empty()
    );
    assert_eq!(
        insights::never_downloaded_count(&mut conn, &nothing)
            .await
            .expect("count"),
        0
    );
}

#[tokio::test]
async fn a_window_and_a_row_count_are_bounded_rather_than_trusted() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let one = asset(&pool, "harbour", "image/jpeg", 1_000, 1).await;
    for _ in 0..3 {
        download(&pool, one, 0, None).await;
    }

    // A year, not ten. `events` is partitioned by month, and an unbounded window is a scan of every partition
    // ever created — asked for by a caller who typed a big number into a query string.
    let days = insights::series(&mut conn, &access(None), 100_000)
        .await
        .expect("series");
    assert_eq!(
        i64::try_from(days.len()).unwrap_or(i64::MAX),
        insights::MAX_DAYS
    );

    // And zero is a day, not an empty chart: clamping the low end matters as much as the high one.
    let single = insights::series(&mut conn, &access(None), 0)
        .await
        .expect("series");
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].downloads, 3);

    let top = insights::most_downloaded(&mut conn, &access(None), 7, 100_000)
        .await
        .expect("top");
    assert_eq!(top.len(), 1, "clamped, and still returns what there is");
}
