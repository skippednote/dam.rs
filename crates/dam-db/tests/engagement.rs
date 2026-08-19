//! Ratings, favourites and watches (Q.5).
//!
//! The storage is three trivial join tables. The substance is entirely in the access rules, and each case here
//! exists because the obvious implementation gets one of them wrong:
//!
//! - **A write to an asset the caller cannot see is refused as "no such asset".** An endpoint that accepts a
//!   rating for any id is an existence oracle; one that answers differently for "hidden" and "absent" is the
//!   same oracle with extra steps.
//! - **A private list is still filtered.** "My favourites" looks like a query needing no access filter, because
//!   the caller owns every row — but access can be withdrawn afterwards, and an unfiltered list would keep
//!   naming the asset.
//! - **The total and the page come from the same predicate.** §7: a count is a disclosure.
//! - **Clearing is deleting.** An average over a table where zero means "no opinion" is wrong in a way that only
//!   shows up once the numbers are on screen.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::engagement::{self, EngagementRefusal, List};
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

/// A plan that sees the whole library.
fn everything() -> Planned {
    Planned::new(Query::All, grants(&[], true), &[]).expect("plan")
}

/// A plan that sees only `group`.
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

async fn group(pool: &PgPool, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("group")
}

#[tokio::test]
async fn the_engagement_contract_holds() {
    let (_pg, pool) = db().await;

    let press = group(&pool, "press").await;
    let embargoed = asset(&pool, "embargoed", None).await;
    let shared = asset(&pool, "shared", Some(press)).await;

    let ada = Uuid::new_v4();
    let grace = Uuid::new_v4();

    a_rating_is_recorded_and_averaged(&pool, shared, ada, grace).await;
    changing_a_rating_replaces_it_rather_than_adding_one(&pool, shared, ada).await;
    a_rating_outside_one_to_five_is_refused(&pool, shared, ada).await;
    unrating_removes_the_opinion_rather_than_scoring_it_zero(&pool, shared, ada, grace).await;
    favouriting_is_idempotent_and_counted(&pool, shared, ada, grace).await;
    watching_is_private_and_has_no_public_count(&pool, shared, ada, grace).await;
    an_asset_the_caller_cannot_see_is_unknown_not_forbidden(&pool, embargoed, ada).await;
    a_private_list_is_still_filtered_by_access(&pool, embargoed, shared, ada, press).await;
    the_total_and_the_page_agree_under_one_predicate(&pool, press, ada).await;
    untouched_assets_still_come_back(&pool, shared, ada).await;
    watchers_are_listed_for_the_sender_that_does_not_exist_yet(&pool, shared, ada, grace).await;
}

async fn a_rating_is_recorded_and_averaged(pool: &PgPool, shared: Uuid, ada: Uuid, grace: Uuid) {
    let state = engagement::rate(c!(pool), shared, ada, 5, &everything())
        .await
        .expect("rate");
    assert_eq!(state.my_stars, Some(5));
    assert_eq!(state.rating_count, 1);
    assert_eq!(state.average_stars, Some(5.0));

    // A second person's rating moves the average and not the caller's own star.
    let state = engagement::rate(c!(pool), shared, grace, 3, &everything())
        .await
        .expect("rate");
    assert_eq!(state.my_stars, Some(3), "the caller is grace now");
    assert_eq!(state.rating_count, 2);
    assert_eq!(state.average_stars, Some(4.0));

    // Ada still sees her own five, which is the point of `my_stars` being per-caller rather than a global field.
    let ada_view = engagement::one(c!(pool), shared, ada, &everything())
        .await
        .expect("read");
    assert_eq!(ada_view.my_stars, Some(5));
    assert_eq!(ada_view.average_stars, Some(4.0));
}

async fn changing_a_rating_replaces_it_rather_than_adding_one(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
) {
    // Changing your mind is the ordinary case, so it is an upsert. If it inserted, the count would climb and the
    // average would be a weighted history of one person's indecision.
    let state = engagement::rate(c!(pool), shared, ada, 1, &everything())
        .await
        .expect("rate");
    assert_eq!(state.my_stars, Some(1));
    assert_eq!(state.rating_count, 2, "still two people: {state:?}");
    assert_eq!(state.average_stars, Some(2.0));
}

async fn a_rating_outside_one_to_five_is_refused(pool: &PgPool, shared: Uuid, ada: Uuid) {
    for bad in [0_i16, 6, -1, i16::MAX] {
        let refusal = engagement::rate(c!(pool), shared, ada, bad, &everything())
            .await
            .expect_err("out of range");
        assert!(
            matches!(refusal, EngagementRefusal::OutOfRange(got) if got == bad),
            "{bad}: {refusal:?}"
        );
    }
    // And the stored value is untouched by the attempts.
    let state = engagement::one(c!(pool), shared, ada, &everything())
        .await
        .expect("read");
    assert_eq!(state.my_stars, Some(1));
}

async fn unrating_removes_the_opinion_rather_than_scoring_it_zero(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
) {
    let state = engagement::unrate(c!(pool), shared, ada, &everything())
        .await
        .expect("unrate");
    assert_eq!(state.my_stars, None);
    // Grace's 3 is the only rating left, so the average is 3 — not 1.5, which is what a zero-scoring
    // implementation would produce.
    assert_eq!(state.rating_count, 1);
    assert_eq!(state.average_stars, Some(3.0));

    // Removing what is not there is not an error: the button is a toggle and a double click is not a fault.
    engagement::unrate(c!(pool), shared, ada, &everything())
        .await
        .expect("idempotent");

    // With nobody rating, the average is absent rather than zero — "unrated" and "everyone hated it" are
    // different facts and a screen must be able to tell them apart.
    let state = engagement::unrate(c!(pool), shared, grace, &everything())
        .await
        .expect("unrate");
    assert_eq!(state.rating_count, 0);
    assert_eq!(state.average_stars, None, "{state:?}");
}

async fn favouriting_is_idempotent_and_counted(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
) {
    let first = engagement::favourite(c!(pool), shared, ada, &everything())
        .await
        .expect("favourite");
    assert!(first.is_favourite);
    assert_eq!(first.favourite_count, 1);

    let again = engagement::favourite(c!(pool), shared, ada, &everything())
        .await
        .expect("idempotent");
    assert_eq!(again.favourite_count, 1, "not two: {again:?}");

    let both = engagement::favourite(c!(pool), shared, grace, &everything())
        .await
        .expect("favourite");
    assert_eq!(both.favourite_count, 2);
    // Grace's own state is hers; the count is the asset's. Conflating them is the bug this asserts against.
    assert!(both.is_favourite);
    let ada_view = engagement::one(c!(pool), shared, ada, &everything())
        .await
        .expect("read");
    assert!(ada_view.is_favourite);
    assert_eq!(ada_view.favourite_count, 2);

    let removed = engagement::unfavourite(c!(pool), shared, grace, &everything())
        .await
        .expect("unfavourite");
    assert!(!removed.is_favourite);
    assert_eq!(removed.favourite_count, 1);

    // Re-favouriting must not move the row. The caller's list is ordered by when they added each asset, so an
    // upsert that touched `created_at` would silently reshuffle it every time somebody clicked a filled star —
    // which is precisely the click that is *not* meant to change anything.
    let later = asset(pool, "favourited-second", None).await;
    engagement::favourite(c!(pool), later, ada, &everything())
        .await
        .expect("favourite");
    let (_, before) = engagement::mine(c!(pool), List::Favourites, ada, &everything(), 10, 0)
        .await
        .expect("mine");
    engagement::favourite(c!(pool), shared, ada, &everything())
        .await
        .expect("again");
    let (_, after) = engagement::mine(c!(pool), List::Favourites, ada, &everything(), 10, 0)
        .await
        .expect("mine");
    assert_eq!(before, after, "re-favouriting reordered the list");
    assert_eq!(after.first(), Some(&later), "newest first: {after:?}");
}

async fn watching_is_private_and_has_no_public_count(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
) {
    let watched = engagement::watch(c!(pool), shared, ada, &everything())
        .await
        .expect("watch");
    assert!(watched.is_watched);

    // Grace watching too changes nothing Ada can observe. There is no watch count on purpose: how many
    // colleagues are watching a file is a fact about the colleagues, and no screen needs it. See DECISIONS.md.
    engagement::watch(c!(pool), shared, grace, &everything())
        .await
        .expect("watch");
    let ada_view = engagement::one(c!(pool), shared, ada, &everything())
        .await
        .expect("read");
    assert!(ada_view.is_watched);
    assert_eq!(
        ada_view.favourite_count, 1,
        "watching is not favouriting: {ada_view:?}"
    );

    let stopped = engagement::unwatch(c!(pool), shared, grace, &everything())
        .await
        .expect("unwatch");
    assert!(!stopped.is_watched);
    // And Ada's watch survives Grace's removal, which a `DELETE ... WHERE asset_id` alone would not.
    let ada_view = engagement::one(c!(pool), shared, ada, &everything())
        .await
        .expect("read");
    assert!(ada_view.is_watched, "{ada_view:?}");
}

async fn an_asset_the_caller_cannot_see_is_unknown_not_forbidden(
    pool: &PgPool,
    embargoed: Uuid,
    ada: Uuid,
) {
    // The scoped caller sees only `press`, and `embargoed` is in no group.
    let press_only = scoped(group(pool, "scope-probe").await);
    let absent = Uuid::new_v4();

    // Asserted one at a time rather than as an array of results: every `c!` holds a pool connection until the
    // end of its statement, and five in one expression exhausts the pool — which surfaces as a timeout that
    // looks nothing like the thing under test.
    fn unknown(label: &str, refusal: &EngagementRefusal, expected: Uuid) {
        assert!(
            matches!(refusal, EngagementRefusal::UnknownAsset(id) if *id == expected),
            "{label}: {refusal:?}"
        );
    }

    // Every write, not just the reads: a favourite on an id you cannot see is a private list of things you are
    // not allowed to know exist, filling in as your access changes.
    unknown(
        "rate",
        &engagement::rate(c!(pool), embargoed, ada, 4, &press_only)
            .await
            .expect_err("hidden"),
        embargoed,
    );
    unknown(
        "favourite",
        &engagement::favourite(c!(pool), embargoed, ada, &press_only)
            .await
            .expect_err("hidden"),
        embargoed,
    );
    unknown(
        "watch",
        &engagement::watch(c!(pool), embargoed, ada, &press_only)
            .await
            .expect_err("hidden"),
        embargoed,
    );
    // Checked here, between the writes and the removals. Putting it at the end made it vacuous: `unrate`,
    // `unfavourite` and `unwatch` delete exactly what `rate`, `favourite` and `watch` had just written, so a
    // visibility check that let all six through left a count of zero behind it. Mutation testing said so.
    assert_eq!(
        engagement_rows(pool, embargoed).await,
        0,
        "a refused write must not write"
    );

    unknown(
        "unrate",
        &engagement::unrate(c!(pool), embargoed, ada, &press_only)
            .await
            .expect_err("hidden"),
        embargoed,
    );
    unknown(
        "unfavourite",
        &engagement::unfavourite(c!(pool), embargoed, ada, &press_only)
            .await
            .expect_err("hidden"),
        embargoed,
    );
    unknown(
        "unwatch",
        &engagement::unwatch(c!(pool), embargoed, ada, &press_only)
            .await
            .expect_err("hidden"),
        embargoed,
    );
    unknown(
        "read",
        &engagement::one(c!(pool), embargoed, ada, &press_only)
            .await
            .expect_err("hidden"),
        embargoed,
    );

    // And an id that genuinely does not exist gets the *same* refusal. A different one would turn the pair into
    // an existence oracle: ask for an id, and the shape of the error tells you whether it is there.
    unknown(
        "absent",
        &engagement::rate(c!(pool), absent, ada, 4, &press_only)
            .await
            .expect_err("absent"),
        absent,
    );

    // And still nothing, after the removals and the read.
    assert_eq!(engagement_rows(pool, embargoed).await, 0);
}

/// Every engagement row for one asset, regardless of who made it.
async fn engagement_rows(pool: &PgPool, asset_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM asset_ratings WHERE asset_id = $1) \
              + (SELECT count(*) FROM asset_favourites WHERE asset_id = $1) \
              + (SELECT count(*) FROM asset_watches WHERE asset_id = $1)",
    )
    .bind(asset_id)
    .fetch_one(pool)
    .await
    .expect("count")
}

async fn a_private_list_is_still_filtered_by_access(
    pool: &PgPool,
    embargoed: Uuid,
    shared: Uuid,
    ada: Uuid,
    press: Uuid,
) {
    // Ada favourites both while she can see both.
    engagement::favourite(c!(pool), embargoed, ada, &everything())
        .await
        .expect("favourite");
    engagement::favourite(c!(pool), shared, ada, &everything())
        .await
        .expect("favourite");

    // Asserted as set membership rather than against a total: the cases share one tenant, so a count written out
    // here is a count that breaks whenever an earlier case gains a fixture — a brittleness that says nothing
    // about the rule under test.
    let (wide_total, wide) =
        engagement::mine(c!(pool), List::Favourites, ada, &everything(), 50, 0)
            .await
            .expect("mine");
    assert_eq!(
        wide_total,
        wide.len() as i64,
        "the page is not truncated here"
    );
    assert!(
        wide.contains(&embargoed) && wide.contains(&shared),
        "{wide:?}"
    );

    // Her access is then narrowed to `press`. The rows are still hers, and the list must not name the one she
    // can no longer see — the row's ownership is not the question §7 asks.
    let (narrow_total, narrow) =
        engagement::mine(c!(pool), List::Favourites, ada, &scoped(press), 50, 0)
            .await
            .expect("mine");
    assert!(
        !narrow.contains(&embargoed),
        "the withdrawn asset is still listed: {narrow:?}"
    );
    assert!(narrow.contains(&shared), "{narrow:?}");
    assert_eq!(
        narrow_total,
        narrow.len() as i64,
        "the total counts the same set the page came from"
    );
    assert!(
        narrow_total < wide_total,
        "narrowing access did not narrow the total: {narrow_total} vs {wide_total}"
    );
}

async fn the_total_and_the_page_agree_under_one_predicate(pool: &PgPool, press: Uuid, ada: Uuid) {
    // Five more visible favourites, read a page at a time. The total must describe the same set the page is drawn
    // from, or a caller paginating a filtered list runs off the end of a number that was never theirs.
    let (before, _) = engagement::mine(c!(pool), List::Favourites, ada, &scoped(press), 50, 0)
        .await
        .expect("baseline");
    for n in 0..5 {
        let id = asset(pool, &format!("page-{n}"), Some(press)).await;
        engagement::favourite(c!(pool), id, ada, &everything())
            .await
            .expect("favourite");
    }
    // Plus one they cannot see, which an unfiltered count would include.
    let hidden = asset(pool, "page-hidden", None).await;
    engagement::favourite(c!(pool), hidden, ada, &everything())
        .await
        .expect("favourite");

    let (total, first) = engagement::mine(c!(pool), List::Favourites, ada, &scoped(press), 2, 0)
        .await
        .expect("page");
    assert_eq!(first.len(), 2);
    assert_eq!(
        total,
        before + 5,
        "the five visible ones counted and the hidden one did not"
    );

    let (again, second) = engagement::mine(c!(pool), List::Favourites, ada, &scoped(press), 2, 2)
        .await
        .expect("page");
    assert_eq!(again, total, "the total does not move between pages");
    assert!(
        second.iter().all(|id| !first.contains(id)),
        "pages do not overlap: {first:?} then {second:?}"
    );

    // A watch list is a different list, and reads through the same code path — so a table name wired to the
    // wrong enum arm would show favourites here.
    let (watch_total, watches) =
        engagement::mine(c!(pool), List::Watches, ada, &scoped(press), 50, 0)
            .await
            .expect("watches");
    assert_ne!(
        watch_total, total,
        "watches and favourites are not the same list"
    );
    assert!(
        watches.iter().all(|id| !first.contains(id)),
        "the watch list is showing favourites: {watches:?}"
    );
}

async fn untouched_assets_still_come_back(pool: &PgPool, shared: Uuid, ada: Uuid) {
    let fresh = asset(pool, "never-touched", None).await;
    let middle = asset(pool, "also-untouched", None).await;

    // Asked for in an order the planner will not reproduce: three ids, with the *touched* one in the middle. Two
    // ids let a reversed implementation pass half the time depending on what the planner returned, which is a
    // coin-flip a test has no business containing.
    let wanted = [fresh, shared, middle];
    let states = engagement::many(c!(pool), &wanted, ada, &everything())
        .await
        .expect("many");

    // In the order asked for, not the planner's: a grid zips these against the ids it sent.
    assert_eq!(
        states.iter().map(|s| s.asset_id).collect::<Vec<_>>(),
        wanted.to_vec()
    );

    // And the untouched one is present with zeroes rather than missing, so a client never has to guess whether
    // an absent row means "no ratings" or "not returned".
    assert_eq!(states[0].rating_count, 0);
    assert_eq!(states[0].average_stars, None);
    assert!(!states[0].is_favourite);

    // A page containing something hidden simply omits it — a grid asking about fifty thumbnails should not fail
    // because one of them turned out to be off-limits.
    let press_only = scoped(group(pool, "many-probe").await);
    let states = engagement::many(c!(pool), &wanted, ada, &press_only)
        .await
        .expect("many");
    assert!(states.is_empty(), "{states:?}");
}

async fn watchers_are_listed_for_the_sender_that_does_not_exist_yet(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
) {
    engagement::watch(c!(pool), shared, grace, &everything())
        .await
        .expect("watch");
    let watching = engagement::watchers(c!(pool), shared)
        .await
        .expect("watchers");

    // Both, and takes no predicate: the notification sender is not a caller and has no grants. It re-checks each
    // watcher's access before telling them anything, which is why this returning everyone is correct rather than
    // a leak — the check is at send time, on the fact that matters then.
    assert!(
        watching.contains(&ada) && watching.contains(&grace),
        "{watching:?}"
    );
}
