//! Versions of an asset (Q.8).
//!
//! `version_group_id`, `version_no`, `is_current` and `replaces_id` have been on `assets` since migration 0001 and
//! nothing ever wrote a second version — so nothing ever *filtered* on `is_current` either. That is the bug this
//! suite is mostly about: every asset is current until a version exists, so every listing looked correct and would
//! have started showing each asset once per version the moment one did.
//!
//! The other properties:
//!
//! - **The unique index makes the swap atomic or impossible.** `UNIQUE (version_group_id) WHERE is_current AND
//!   deleted_at IS NULL` means a group cannot hold two current versions, whatever order a caller writes in.
//! - **A named asset is whatever was named.** Listings show current versions; reading an old one by id works, or
//!   keeping versions would be pointless.
//! - **A history is reachable from any version**, and is filtered by the caller's predicate on every row — a group
//!   can span asset groups, and a sibling must not leak through it.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::versions::{self, VersionRefusal};
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

fn access(groups: &[Uuid], all: bool) -> policy::AccessPredicate {
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

fn everything() -> policy::AccessPredicate {
    access(&[], true)
}

fn planned(predicate: policy::AccessPredicate) -> Planned {
    Planned::new(Query::All, predicate, &[]).expect("plan")
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
async fn the_version_contract_holds() {
    let (_pg, pool) = db().await;

    a_version_supersedes_its_predecessor(&pool).await;
    listings_show_one_row_per_group(&pool).await;
    an_old_version_is_still_readable_by_id(&pool).await;
    a_group_cannot_hold_two_current_versions(&pool).await;
    a_history_is_reachable_from_any_version(&pool).await;
    a_history_is_filtered_by_the_predicate(&pool).await;
    superseding_a_stale_version_is_refused(&pool).await;
    an_earlier_version_can_be_made_current_again(&pool).await;
    a_deleted_version_leaves_the_history(&pool).await;
    an_asset_outside_the_predicate_cannot_be_joined_to_a_group(&pool).await;
}

async fn a_version_supersedes_its_predecessor(pool: &PgPool) {
    let first = asset(pool, "brochure-v1", None).await;
    let second = asset(pool, "brochure-v2", None).await;

    let added = versions::add(c!(pool), first, second, &everything())
        .await
        .expect("add");
    assert_eq!(
        added.version_no, 2,
        "the new version is numbered after the old"
    );
    assert!(added.is_current);
    // `replaces_id` records what it replaced, so a history reads as a chain rather than as a set.
    assert_eq!(added.replaces_id, Some(first));

    // And the group is shared, which is what makes them versions rather than two assets.
    let group: (Uuid, Uuid) = sqlx::query_as(
        "SELECT (SELECT version_group_id FROM assets WHERE id = $1), \
                (SELECT version_group_id FROM assets WHERE id = $2)",
    )
    .bind(first)
    .bind(second)
    .fetch_one(pool)
    .await
    .expect("groups");
    assert_eq!(group.0, group.1);

    let history = versions::history(c!(pool), second, &everything())
        .await
        .expect("history");
    assert_eq!(history.len(), 2);
    // Newest first: a history is read from the top.
    assert_eq!(history[0].asset_id, second);
    assert_eq!(history[1].asset_id, first);
    assert!(!history[1].is_current, "the predecessor was demoted");
}

async fn listings_show_one_row_per_group(pool: &PgPool) {
    // The bug this slice exists to prevent. Before `is_current` was filtered, a group with two versions appeared
    // as two assets in the grid, in search, and in every count — and nothing showed it, because every asset was
    // current until somebody made a version.
    let page = dam_db::assets::page(pool, &everything(), dam_db::assets::Order::Newest, 0, 50)
        .await
        .expect("page");
    let filenames: Vec<&str> = page
        .items
        .iter()
        .map(|item| item.filename.as_str())
        .collect();
    assert!(filenames.contains(&"brochure-v2.jpg"), "{filenames:?}");
    assert!(
        !filenames.contains(&"brochure-v1.jpg"),
        "a superseded version is in the library listing: {filenames:?}"
    );
    // The *total* too, not only the rows: a count that included both would make pagination disagree with itself.
    assert_eq!(page.total as usize, page.items.len());

    // The relational path as well, which is a separate query.
    let matched = dam_db::assets::page_matching(
        pool,
        &planned(everything()),
        dam_db::assets::Order::Newest,
        0,
        50,
    )
    .await
    .expect("page_matching");
    let matched_names: Vec<&str> = matched.items.iter().map(|i| i.filename.as_str()).collect();
    assert!(
        matched_names.contains(&"brochure-v2.jpg"),
        "{matched_names:?}"
    );
    assert!(
        !matched_names.contains(&"brochure-v1.jpg"),
        "the search path shows superseded versions: {matched_names:?}"
    );

    // And the dashboard's asset count, which has to agree with the grid or the page contradicts itself.
    let summary = dam_db::events::summary(c!(pool), &planned(everything()))
        .await
        .expect("summary");
    assert_eq!(summary.assets as usize, page.items.len(), "{summary:?}");
}

async fn an_old_version_is_still_readable_by_id(pool: &PgPool) {
    let history = {
        let current: Uuid =
            sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-v2.jpg'")
                .fetch_one(pool)
                .await
                .expect("current");
        versions::history(c!(pool), current, &everything())
            .await
            .expect("history")
    };
    let old = history
        .iter()
        .find(|version| !version.is_current)
        .expect("a superseded version");

    // Reading by id ignores `is_current`, or keeping versions would be pointless: "give me what we shipped in
    // March" is the entire reason the old row still exists.
    let found = dam_db::assets::detail(pool, &everything(), old.asset_id)
        .await
        .expect("detail");
    assert!(found.is_some(), "a superseded version is unreadable by id");

    // And it is still access-checked, so "readable by id" is not "readable by anybody".
    let elsewhere = group(pool, "elsewhere").await;
    let refused = dam_db::assets::detail(pool, &access(&[elsewhere], false), old.asset_id)
        .await
        .expect("query");
    assert!(refused.is_none(), "an old version escaped the predicate");
}

async fn a_group_cannot_hold_two_current_versions(pool: &PgPool) {
    let current: Uuid =
        sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-v2.jpg'")
            .fetch_one(pool)
            .await
            .expect("current");
    let group_id: Uuid = sqlx::query_scalar("SELECT version_group_id FROM assets WHERE id = $1")
        .bind(current)
        .fetch_one(pool)
        .await
        .expect("group");

    // Straight past the module, to show the database itself refuses it. This is what makes the demote-then-promote
    // order in `add` a convenience rather than the only thing standing between a caller and a corrupt group.
    let refused = sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, version_no, is_current) \
         VALUES (gen_random_uuid(), 'deadbeef', 'sneaky.jpg', 'image/jpeg', 1, $1, 9, true)",
    )
    .bind(group_id)
    .execute(pool)
    .await;
    let message = refused
        .expect_err("two current versions must be impossible")
        .to_string();
    assert!(
        message.contains("assets_current_idx") || message.contains("unique"),
        "the refusal was not the unique index: {message}"
    );
}

async fn a_history_is_reachable_from_any_version(pool: &PgPool) {
    let old: Uuid = sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-v1.jpg'")
        .fetch_one(pool)
        .await
        .expect("old");

    // From the superseded row, not only the current one: somebody looking at March's cut needs to see what
    // replaced it, and making them find the current version first would require knowing the answer already.
    let history = versions::history(c!(pool), old, &everything())
        .await
        .expect("history");
    assert_eq!(history.len(), 2);
    assert!(history.iter().any(|version| version.asset_id == old));
    assert!(history.iter().any(|version| version.is_current));
}

async fn a_history_is_filtered_by_the_predicate(pool: &PgPool) {
    // Two versions in *different* asset groups, which the schema permits: the version group and the access group
    // are unrelated. A history that showed every sibling would leak past the caller's scope through one of them.
    let press = group(pool, "press-versions").await;
    let first = asset(pool, "leaflet-v1", Some(press)).await;
    let second = asset(pool, "leaflet-v2", None).await;
    versions::add(c!(pool), first, second, &everything())
        .await
        .expect("add");

    // Scoped to `press`, which contains only the first version.
    let scoped = access(&[press], false);
    let history = versions::history(c!(pool), first, &scoped)
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        1,
        "a version outside the caller's scope appeared in the history: {history:?}"
    );
    assert_eq!(history[0].asset_id, first);

    // And the caller cannot reach the group at all through the version they cannot see.
    let refusal = versions::history(c!(pool), second, &scoped)
        .await
        .expect_err("not visible");
    assert!(
        matches!(refusal, VersionRefusal::UnknownAsset(id) if id == second),
        "{refusal:?}"
    );
}

async fn superseding_a_stale_version_is_refused(pool: &PgPool) {
    let old: Uuid = sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-v1.jpg'")
        .fetch_one(pool)
        .await
        .expect("old");
    let third = asset(pool, "brochure-v3", None).await;

    // Somebody with a stale screen, adding a version to what they believe is the latest. Refused rather than
    // quietly re-pointing the group, which would discard whatever they had not seen.
    let refusal = versions::add(c!(pool), old, third, &everything())
        .await
        .expect_err("stale");
    assert!(
        matches!(refusal, VersionRefusal::NotCurrent(id) if id == old),
        "{refusal:?}"
    );

    // And nothing moved: the refused call must not have demoted anything.
    let history = versions::history(c!(pool), old, &everything())
        .await
        .expect("history");
    assert_eq!(
        history.len(),
        2,
        "the refused add joined a row anyway: {history:?}"
    );
    assert_eq!(
        history.iter().filter(|version| version.is_current).count(),
        1,
        "{history:?}"
    );
}

async fn an_earlier_version_can_be_made_current_again(pool: &PgPool) {
    let old: Uuid = sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-v1.jpg'")
        .fetch_one(pool)
        .await
        .expect("old");

    let history = versions::restore(c!(pool), old, &everything())
        .await
        .expect("restore");
    let restored = history
        .iter()
        .find(|version| version.asset_id == old)
        .expect("the restored version");
    assert!(restored.is_current);
    // Its number is unchanged: a promotion, not a copy. Duplicating it as version 3 would claim somebody uploaded
    // something they did not.
    assert_eq!(restored.version_no, 1);
    assert_eq!(
        history.iter().filter(|version| version.is_current).count(),
        1,
        "{history:?}"
    );

    // Twice is not an error: "make this current" clicked twice is not a fault.
    versions::restore(c!(pool), old, &everything())
        .await
        .expect("idempotent");

    // The listing follows: the library now shows version 1 and not version 2.
    let page = dam_db::assets::page(pool, &everything(), dam_db::assets::Order::Newest, 0, 50)
        .await
        .expect("page");
    let names: Vec<&str> = page
        .items
        .iter()
        .map(|item| item.filename.as_str())
        .collect();
    assert!(names.contains(&"brochure-v1.jpg"), "{names:?}");
    assert!(!names.contains(&"brochure-v2.jpg"), "{names:?}");
}

async fn a_deleted_version_leaves_the_history(pool: &PgPool) {
    let doomed: Uuid =
        sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-v2.jpg'")
            .fetch_one(pool)
            .await
            .expect("v2");
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(doomed)
        .execute(pool)
        .await
        .expect("soft delete");

    let current: Uuid =
        sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-v1.jpg'")
            .fetch_one(pool)
            .await
            .expect("v1");
    let history = versions::history(c!(pool), current, &everything())
        .await
        .expect("history");
    // A deleted version is not part of a history somebody can act on, and listing it would offer a download the
    // delivery chokepoint refuses.
    assert!(
        history.iter().all(|version| version.asset_id != doomed),
        "a deleted version is still in the history: {history:?}"
    );
    assert_eq!(history.len(), 1);
}

async fn an_asset_outside_the_predicate_cannot_be_joined_to_a_group(pool: &PgPool) {
    let press = group(pool, "join-scope").await;
    let mine = asset(pool, "mine-v1", Some(press)).await;
    let theirs = asset(pool, "theirs-v1", None).await;

    // The caller can see their own asset and not the other. Joining the two would put an asset they cannot see
    // into a group they control — or, read the other way, take one they cannot see and make it theirs.
    let scoped = access(&[press], false);
    let refusal = versions::add(c!(pool), mine, theirs, &scoped)
        .await
        .expect_err("outside scope");
    assert!(
        matches!(refusal, VersionRefusal::UnknownAsset(id) if id == theirs),
        "{refusal:?}"
    );

    // And the reverse direction: superseding something they cannot see.
    let refusal = versions::add(c!(pool), theirs, mine, &scoped)
        .await
        .expect_err("outside scope");
    assert!(
        matches!(refusal, VersionRefusal::UnknownAsset(id) if id == theirs),
        "{refusal:?}"
    );

    // Nothing was joined by either refusal.
    let groups: (Uuid, Uuid) = sqlx::query_as(
        "SELECT (SELECT version_group_id FROM assets WHERE id = $1), \
                (SELECT version_group_id FROM assets WHERE id = $2)",
    )
    .bind(mine)
    .bind(theirs)
    .fetch_one(pool)
    .await
    .expect("groups");
    assert_ne!(groups.0, groups.1, "a refused add joined the groups anyway");
}
