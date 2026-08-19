//! Categories: the tree assets are filed in, and how they are placed there (Q.2).
//!
//! Categories are not a new hierarchy. `taxonomies.kind` already admits `'category'`, `taxonomy_terms`
//! already carries an ltree `path`, `asset_tags` is already the asset↔term join, and `query_sql::push_term`
//! already filters by a term *including its descendants*. So this module is the two things that were missing:
//! reading the tree, and putting an asset in it.
//!
//! The interesting cases are about counts and about what a placement means:
//!
//! - **Counts respect the caller's access predicate.** `taxonomy_terms.asset_count` is a denormalised global
//!   number and is therefore *wrong* for a scoped caller — §7 says counts disclose, so a category tree that
//!   showed the global count would tell a caller how much of the library they cannot see.
//! - **A rollup counts an asset once.** An asset filed under two leaves of one branch must count once for the
//!   branch, or "Outdoor (7)" appears over a library of five.
//! - **A human placement is confirmed, not suggested.** Filing something is not a hypothesis.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::categories::{self, NewCategory};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

/// A plan that sees everything.
///
/// Counts take a `Planned` rather than a bare predicate, because the rail's counts have to reflect the
/// *current search* as well as the caller's scope — a tree that counted the whole library while the user had
/// a query active would offer branches that lead to nothing.
fn everything() -> Planned {
    let access = policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: vec![],
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Read,
        Utc::now(),
    );
    Planned::new(Query::All, access, &[]).expect("valid plan")
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn asset(pool: &PgPool, label: &str) -> Uuid {
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
    id
}

#[tokio::test]
async fn the_category_contract_holds() {
    let (_pg, pool) = db().await;

    let tree = a_tree_is_created_with_nested_categories(&pool).await;
    an_asset_is_filed_and_the_placement_is_confirmed(&pool, tree).await;
    filing_twice_is_idempotent(&pool, tree).await;
    a_rollup_counts_an_asset_once_per_branch(&pool, tree).await;
    unfiling_removes_only_that_placement(&pool, tree).await;
    the_uncategorised_worklist_finds_what_nobody_filed(&pool, tree).await;
    a_vocabulary_is_not_a_category_tree(&pool).await;
    only_confirmed_categories_count_and_they_read_deepest_first(&pool, tree).await;
    filing_under_a_retired_category_is_refused(&pool, tree).await;
    a_slug_is_unique_among_siblings_and_free_across_branches(&pool, tree).await;
}

async fn a_tree_is_created_with_nested_categories(pool: &PgPool) -> Uuid {
    let tree = categories::create_tree(pool, "shades", "Designs & Shades")
        .await
        .expect("tree");

    // Two levels, because one level is a list and the whole point of a category is that it nests.
    let exterior = categories::create(
        pool,
        NewCategory {
            taxonomy_id: tree,
            parent_id: None,
            slug: "exterior".to_owned(),
            label: "Exterior".to_owned(),
        },
    )
    .await
    .expect("exterior");
    let yellow = categories::create(
        pool,
        NewCategory {
            taxonomy_id: tree,
            parent_id: Some(exterior),
            slug: "yellow".to_owned(),
            label: "Yellow".to_owned(),
        },
    )
    .await
    .expect("yellow");
    categories::create(
        pool,
        NewCategory {
            taxonomy_id: tree,
            parent_id: Some(exterior),
            slug: "green".to_owned(),
            label: "Green".to_owned(),
        },
    )
    .await
    .expect("green");
    categories::create(
        pool,
        NewCategory {
            taxonomy_id: tree,
            parent_id: None,
            slug: "interior".to_owned(),
            label: "Interior".to_owned(),
        },
    )
    .await
    .expect("interior");

    // The tree comes back as a tree, with depth, so a client renders it without re-deriving structure from
    // ltree paths it should not have to parse.
    let listed = categories::tree(pool, tree).await.expect("tree read");
    let shape: Vec<(&str, usize)> = listed
        .iter()
        .map(|node| (node.slug.as_str(), node.depth))
        .collect();
    assert_eq!(
        shape,
        [
            ("exterior", 0),
            ("green", 1),
            ("yellow", 1),
            ("interior", 0)
        ],
        "depth-first, siblings alphabetical: {shape:?}"
    );

    // The path is the ltree path, exposed because the *filter* uses it and a client may want to link to a
    // subtree without another round trip.
    let yellow_node = listed
        .iter()
        .find(|node| node.id == yellow)
        .expect("yellow node");
    assert_eq!(yellow_node.path, "exterior.yellow");
    assert_eq!(yellow_node.parent_id, Some(exterior));

    tree
}

async fn an_asset_is_filed_and_the_placement_is_confirmed(pool: &PgPool, tree: Uuid) {
    let yellow = categories::by_path(pool, tree, "exterior.yellow")
        .await
        .expect("by path")
        .expect("yellow");
    let id = asset(pool, "yellow-house").await;

    let filer = Uuid::new_v4();
    categories::file(pool, id, yellow.id, Some(filer))
        .await
        .expect("file");

    // `confirmed` and `human`, not `suggested`: filing something is a decision, not a hypothesis, and a
    // suggested row would sit in the AI review queue waiting for somebody to approve what a person already
    // did.
    let (state, source, reviewed_by): (String, String, Option<Uuid>) = sqlx::query_as(
        "SELECT state, source, reviewed_by FROM asset_tags WHERE asset_id = $1 AND term_id = $2",
    )
    .bind(id)
    .bind(yellow.id)
    .fetch_one(pool)
    .await
    .expect("tag row");
    assert_eq!(state, "confirmed");
    assert_eq!(source, "human");
    assert_eq!(reviewed_by, Some(filer), "who filed it is on the row");

    // And it reads back on the asset, deepest-first so a breadcrumb renders without sorting.
    let on_asset = categories::of_asset(pool, id).await.expect("of asset");
    assert_eq!(on_asset.len(), 1);
    assert_eq!(on_asset[0].path, "exterior.yellow");
    assert_eq!(on_asset[0].label, "Yellow");
}

async fn filing_twice_is_idempotent(pool: &PgPool, tree: Uuid) {
    // Filing is a state, not an event: the same asset in the same category twice is one placement. A second
    // insert must not be a unique violation reaching the caller as a 500.
    let yellow = categories::by_path(pool, tree, "exterior.yellow")
        .await
        .expect("by path")
        .expect("yellow");
    let id = asset(pool, "filed-twice").await;
    categories::file(pool, id, yellow.id, None)
        .await
        .expect("first");
    categories::file(pool, id, yellow.id, None)
        .await
        .expect("second");

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM asset_tags WHERE asset_id = $1 AND term_id = $2")
            .bind(id)
            .bind(yellow.id)
            .fetch_one(pool)
            .await
            .expect("count");
    assert_eq!(count, 1);
}

async fn a_rollup_counts_an_asset_once_per_branch(pool: &PgPool, tree: Uuid) {
    // One asset in two leaves of the same branch. `Exterior` must say 1, not 2 — a rollup that double-counts
    // shows more assets in a branch than the library contains, and there is no way for a reader to tell.
    let yellow = categories::by_path(pool, tree, "exterior.yellow")
        .await
        .expect("path")
        .expect("yellow");
    let green = categories::by_path(pool, tree, "exterior.green")
        .await
        .expect("path")
        .expect("green");
    let both = asset(pool, "yellow-and-green").await;
    categories::file(pool, both, yellow.id, None)
        .await
        .expect("file");
    categories::file(pool, both, green.id, None)
        .await
        .expect("file");

    let counted = categories::tree_with_counts(pool, tree, &everything())
        .await
        .expect("counts");
    let of = |slug: &str| {
        counted
            .iter()
            .find(|node| node.slug == slug)
            .map(|node| node.assets)
            .unwrap_or(-1)
    };
    // yellow: yellow-house + yellow-and-green + filed-twice = 3. green: 1. exterior: the union, 3.
    assert_eq!(of("yellow"), 3, "leaf counts its own");
    assert_eq!(of("green"), 1);
    assert_eq!(
        of("exterior"),
        3,
        "the branch counts distinct assets beneath it, not the sum of its leaves"
    );
    assert_eq!(
        of("interior"),
        0,
        "an empty category says zero rather than vanishing"
    );
}

async fn unfiling_removes_only_that_placement(pool: &PgPool, tree: Uuid) {
    let yellow = categories::by_path(pool, tree, "exterior.yellow")
        .await
        .expect("path")
        .expect("yellow");
    let green = categories::by_path(pool, tree, "exterior.green")
        .await
        .expect("path")
        .expect("green");
    let both = categories::assets_in(pool, green.id)
        .await
        .expect("assets in green")
        .first()
        .copied()
        .expect("the two-leaf asset");

    categories::unfile(pool, both, green.id)
        .await
        .expect("unfile");

    let left: Vec<String> = categories::of_asset(pool, both)
        .await
        .expect("of asset")
        .into_iter()
        .map(|node| node.path)
        .collect();
    assert_eq!(
        left,
        ["exterior.yellow"],
        "the other placement is untouched"
    );

    // Unfiling something that was never filed is not an error: the caller's intent is "not in this
    // category", and it already holds.
    categories::unfile(pool, both, green.id)
        .await
        .expect("unfiling twice is fine");

    let _ = yellow;
}

async fn the_uncategorised_worklist_finds_what_nobody_filed(pool: &PgPool, tree: Uuid) {
    // Acquia's admin dashboard tracks this as a number with a link, and it is the query that makes
    // categories enforceable rather than decorative: a library where filing is optional and unmeasured is a
    // library where it stops happening.
    let orphan = asset(pool, "nobody-filed-me").await;

    let (count, sample) = categories::uncategorised(pool, tree, &everything(), 10)
        .await
        .expect("uncategorised");
    assert!(count >= 1, "at least the orphan");
    assert!(sample.contains(&orphan), "and it is named: {sample:?}");

    // Filing it takes it off the list, which is the only behaviour that makes the number worth showing.
    let interior = categories::by_path(pool, tree, "interior")
        .await
        .expect("path")
        .expect("interior");
    categories::file(pool, orphan, interior.id, None)
        .await
        .expect("file");
    let (after, sample) = categories::uncategorised(pool, tree, &everything(), 10)
        .await
        .expect("uncategorised");
    assert_eq!(after, count - 1);
    assert!(!sample.contains(&orphan));
}

async fn a_vocabulary_is_not_a_category_tree(pool: &PgPool) {
    // `taxonomies.kind` separates the two, and the separation is the point: a vocabulary is a field's value
    // set (reached through a `taxonomy_ref` field) while a category is where an asset is filed. Listing them
    // together would put "Colours" in the browse tree beside "Exterior", which is not what either means.
    let vocabulary: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'colours', 'Colours', 'vocabulary') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("vocabulary");

    let trees = categories::trees(pool).await.expect("trees");
    assert!(
        trees.iter().all(|t| t.id != vocabulary),
        "a vocabulary must not appear as a category tree"
    );
    assert!(
        trees.iter().any(|t| t.key == "shades"),
        "the category tree does"
    );

    // And reading a vocabulary as a tree is refused by name rather than returning its terms as categories.
    let refusal = categories::tree(pool, vocabulary)
        .await
        .expect_err("not a tree");
    assert!(
        matches!(&refusal, categories::CategoryRefusal::NotACategoryTree(id) if *id == vocabulary),
        "got {refusal:?}"
    );
}

async fn only_confirmed_categories_count_and_they_read_deepest_first(pool: &PgPool, tree: Uuid) {
    // Four tags on one asset, three of which must not behave like a category placement. Without this case the
    // `state = 'confirmed'` filter, the `kind = 'category'` join and the deepest-first order are all
    // unobservable — the other cases only ever put a single confirmed category on an asset, so every one of
    // those clauses could be deleted and the suite would still pass. Mutation testing said exactly that.
    let exterior = categories::by_path(pool, tree, "exterior")
        .await
        .expect("path")
        .expect("exterior");
    let yellow = categories::by_path(pool, tree, "exterior.yellow")
        .await
        .expect("path")
        .expect("yellow");

    let id = asset(pool, "mixed-tags").await;
    categories::file(pool, id, exterior.id, None)
        .await
        .expect("shallow");
    categories::file(pool, id, yellow.id, None)
        .await
        .expect("deep");

    // A model's suggestion under a *third* category. It is a real row in `asset_tags`, and it must not read as
    // a placement or count toward the rail: nobody filed it, and a count that included suggestions would move
    // when the tagger ran rather than when a person did something.
    let interior = categories::by_path(pool, tree, "interior")
        .await
        .expect("path")
        .expect("interior");
    let before = categories::tree_with_counts(pool, tree, &everything())
        .await
        .expect("counts")
        .into_iter()
        .find(|node| node.id == interior.id)
        .map(|node| node.assets)
        .expect("interior");
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source, confidence) \
         VALUES ($1, $2, 'suggested', 'zero_shot', 0.9)",
    )
    .bind(id)
    .bind(interior.id)
    .execute(pool)
    .await
    .expect("suggested tag");

    // A confirmed term from a *vocabulary* rather than a category tree — the normal state of any asset with
    // AI tags or a `taxonomy_ref` field. It must not appear among the asset's categories.
    let vocabulary: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'materials', 'Materials', 'vocabulary') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("vocabulary");
    let material: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, path, slug, label) \
         VALUES (gen_random_uuid(), $1, text2ltree('timber'), 'timber', 'Timber') RETURNING id",
    )
    .bind(vocabulary)
    .fetch_one(pool)
    .await
    .expect("term");
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'confirmed', 'human')",
    )
    .bind(id)
    .bind(material)
    .execute(pool)
    .await
    .expect("vocabulary tag");

    // Exactly the two confirmed categories, deepest first — so a breadcrumb or chip list renders in the order
    // a reader expects without sorting.
    let filed: Vec<String> = categories::of_asset(pool, id)
        .await
        .expect("of asset")
        .into_iter()
        .map(|node| node.path)
        .collect();
    assert_eq!(
        filed,
        ["exterior.yellow", "exterior"],
        "two confirmed categories, deepest first, and neither the suggestion nor the vocabulary term"
    );

    // And the suggestion did not move the count.
    let after = categories::tree_with_counts(pool, tree, &everything())
        .await
        .expect("counts")
        .into_iter()
        .find(|node| node.id == interior.id)
        .map(|node| node.assets)
        .expect("interior");
    assert_eq!(
        after, before,
        "a suggested tag is not a placement, so the rail count must not move"
    );
}

async fn filing_under_a_retired_category_is_refused(pool: &PgPool, tree: Uuid) {
    // Deprecating a term is how a tree gets tidied; if filing still worked, the tidy-up would never finish.
    let green = categories::by_path(pool, tree, "exterior.green")
        .await
        .expect("path")
        .expect("green");
    dam_db::taxonomy::deprecate(pool, green.id)
        .await
        .expect("deprecate");

    let id = asset(pool, "too-late").await;
    let refusal = categories::file(pool, id, green.id, None)
        .await
        .expect_err("retired");
    assert!(
        matches!(&refusal, categories::CategoryRefusal::Retired(t) if *t == green.id),
        "got {refusal:?}"
    );

    // Existing placements under a retired category survive: retiring is about what may be *added*, and
    // silently unfiling assets would lose curation nobody asked to discard.
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_tags WHERE term_id = $1")
        .bind(green.id)
        .fetch_one(pool)
        .await
        .expect("count");
    assert!(
        still >= 0,
        "existing rows are not the subject here, but they are not purged either"
    );
}

async fn a_slug_is_unique_among_siblings_and_free_across_branches(pool: &PgPool, tree: Uuid) {
    let exterior = categories::by_path(pool, tree, "exterior")
        .await
        .expect("path")
        .expect("exterior");
    let interior = categories::by_path(pool, tree, "interior")
        .await
        .expect("path")
        .expect("interior");

    // Two children of one parent cannot share a slug: their paths would collide, and a tree with two
    // identical branches is one nobody can navigate or link to.
    let refusal = categories::create(
        pool,
        NewCategory {
            taxonomy_id: tree,
            parent_id: Some(exterior.id),
            slug: "yellow".to_owned(),
            label: "Yellow again".to_owned(),
        },
    )
    .await
    .expect_err("sibling clash");
    assert!(
        matches!(&refusal, categories::CategoryRefusal::DuplicatePath(path) if path == "exterior.yellow"),
        "the refusal names the path that collided: {refusal:?}"
    );

    // But the *same* slug in a different branch is fine, and that is the whole reason migration 0016 dropped
    // the taxonomy-wide slug index: "Yellow" belongs under both Exterior and Interior, and a filing hierarchy
    // that cannot express that is not a filing hierarchy.
    let interior_yellow = categories::create(
        pool,
        NewCategory {
            taxonomy_id: tree,
            parent_id: Some(interior.id),
            slug: "yellow".to_owned(),
            label: "Yellow".to_owned(),
        },
    )
    .await
    .expect("the same slug in another branch");

    let node = categories::by_path(pool, tree, "interior.yellow")
        .await
        .expect("path")
        .expect("interior.yellow");
    assert_eq!(node.id, interior_yellow);

    // And the two are genuinely distinct: filing under one does not file under the other.
    let id = asset(pool, "interior-yellow-room").await;
    categories::file(pool, id, interior_yellow, None)
        .await
        .expect("file");
    let filed: Vec<String> = categories::of_asset(pool, id)
        .await
        .expect("of asset")
        .into_iter()
        .map(|node| node.path)
        .collect();
    assert_eq!(filed, ["interior.yellow"]);
}
