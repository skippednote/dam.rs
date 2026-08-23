//! Taxonomy lifecycle: move, merge, deprecate (2.2).
//!
//! A taxonomy is not a fixed thing. Customers rename, reparent, and eventually discover that two of
//! their terms were always the same term. What they must never do is change what an existing asset
//! means — and the obvious implementation of all three operations does exactly that.
//!
//! The rule TASKS.md names is the one everything else here serves: **a deprecated term stays resolvable,
//! so old assets keep their meaning.** "Outdoor" tagged in 2019 still means outdoor after the vocabulary
//! is reorganised in 2026, and an id stored outside this database — a saved search, a Drupal field, an
//! API client's cache — keeps working after a merge.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::taxonomy::{self, Error as TaxonomyError};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

/// One connection out of the pool.
///
/// These functions take a connection rather than a pool because in production they run inside a tenant
/// transaction — the `search_path` that makes `taxonomy_terms` mean `t_acme.taxonomy_terms` is set on that
/// transaction, which is also what makes a half-applied merge impossible. The tests borrow one per statement,
/// which is a superset of what any single call needs.
async fn held(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("acquire")
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn taxonomy_named(pool: &PgPool, key: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO taxonomies (id, key, label) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(key)
        .execute(pool)
        .await
        .expect("taxonomy");
    id
}

/// A term at `path`, with `parent` when it has one.
async fn term(pool: &PgPool, taxonomy_id: Uuid, path: &str, parent: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let slug = path.rsplit('.').next().expect("a leaf segment");
    sqlx::query(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, parent_id, path, slug, label) \
         VALUES ($1, $2, $3, text2ltree($4), $5, $5)",
    )
    .bind(id)
    .bind(taxonomy_id)
    .bind(parent)
    .bind(path)
    .bind(slug)
    .execute(pool)
    .await
    .expect("term");
    id
}

async fn tag(pool: &PgPool, term_id: Uuid, label: &str) -> Uuid {
    let asset_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(asset_id)
    .bind(format!("blake3:{label}"))
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'confirmed', 'human')",
    )
    .bind(asset_id)
    .bind(term_id)
    .execute(pool)
    .await
    .expect("tag");
    asset_id
}

async fn path_of(pool: &PgPool, term_id: Uuid) -> String {
    sqlx::query_scalar::<_, String>("SELECT path::text FROM taxonomy_terms WHERE id = $1")
        .bind(term_id)
        .fetch_one(pool)
        .await
        .expect("path")
}

async fn terms_on(pool: &PgPool, asset_id: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar("SELECT term_id FROM asset_tags WHERE asset_id = $1 ORDER BY term_id")
        .bind(asset_id)
        .fetch_all(pool)
        .await
        .expect("tags")
}

// ─── the rule the task names ────────────────────────────────────────────────

async fn a_deprecated_term_stays_resolvable_and_keeps_its_assets(pool: &PgPool) {
    // The whole point. Deleting the term would take its tags with it via ON DELETE CASCADE, so
    // "retiring a term" would silently untag every asset that used it — and nobody would notice until
    // a search came back empty.
    let vocabulary = taxonomy_named(pool, "v1").await;
    let outdoor = term(pool, vocabulary, "outdoor", None).await;
    let asset = tag(pool, outdoor, "beach").await;

    taxonomy::deprecate(&mut *held(pool).await, outdoor)
        .await
        .expect("deprecate");

    let resolved = taxonomy::resolve(&mut *held(pool).await, outdoor)
        .await
        .expect("resolve")
        .expect("a deprecated term must still resolve");
    assert_eq!(resolved.id, outdoor);
    assert!(resolved.deprecated_at.is_some());
    assert_eq!(
        resolved.effective_id, outdoor,
        "with no successor, a deprecated term still means itself"
    );
    assert_eq!(
        terms_on(pool, asset).await,
        vec![outdoor],
        "deprecating must not touch existing tags"
    );
}

async fn a_deprecated_term_is_excluded_from_the_assignable_set(pool: &PgPool) {
    // Resolvable is not the same as offerable. A picker that keeps showing retired terms means the
    // vocabulary never actually gets cleaned up, which is the reason someone deprecated it.
    let vocabulary = taxonomy_named(pool, "v2").await;
    let live = term(pool, vocabulary, "live2", None).await;
    let retired = term(pool, vocabulary, "retired2", None).await;
    taxonomy::deprecate(&mut *held(pool).await, retired)
        .await
        .expect("deprecate");

    let assignable = taxonomy::assignable(&mut *held(pool).await, vocabulary)
        .await
        .expect("assignable");
    let ids: Vec<Uuid> = assignable.iter().map(|t| t.id).collect();
    assert!(ids.contains(&live));
    assert!(
        !ids.contains(&retired),
        "a deprecated term must not be offered for new assignment"
    );
}

async fn deprecating_a_parent_with_live_children_is_refused(pool: &PgPool) {
    // Otherwise the tree ends up with active terms hanging under a retired ancestor, which makes
    // "everything under Outdoor" return terms whose parent has been retired — a state no rollup query
    // can render sensibly. Refusing names the problem; cascading silently would retire terms the
    // operator did not ask about.
    let vocabulary = taxonomy_named(pool, "v3").await;
    let parent = term(pool, vocabulary, "parent3", None).await;
    let child = term(pool, vocabulary, "parent3.child3", Some(parent)).await;

    let refused = taxonomy::deprecate(&mut *held(pool).await, parent)
        .await
        .expect_err("must refuse while a child is live");
    assert!(
        matches!(refused, TaxonomyError::HasLiveChildren { .. }),
        "got {refused:?}"
    );

    // Retire the child first and the parent becomes retirable, which is the order an operator would
    // work in anyway.
    taxonomy::deprecate(&mut *held(pool).await, child)
        .await
        .expect("child");
    taxonomy::deprecate(&mut *held(pool).await, parent)
        .await
        .expect("parent");
}

// ─── merge ──────────────────────────────────────────────────────────────────

async fn merging_moves_the_assets_and_leaves_the_old_id_resolvable(pool: &PgPool) {
    // A merge says "these were always the same thing". The assets move to the survivor, and the old
    // term is retired pointing at it — so an id held outside this database still resolves to something
    // meaningful instead of 404ing.
    let vocabulary = taxonomy_named(pool, "v4").await;
    let dog = term(pool, vocabulary, "dog4", None).await;
    let dogs = term(pool, vocabulary, "dogs4", None).await;
    let asset = tag(pool, dog, "spaniel").await;

    taxonomy::merge(&mut *held(pool).await, dog, dogs)
        .await
        .expect("merge");

    assert_eq!(
        terms_on(pool, asset).await,
        vec![dogs],
        "the asset must now be tagged with the surviving term"
    );

    let resolved = taxonomy::resolve(&mut *held(pool).await, dog)
        .await
        .expect("resolve")
        .expect("the merged-away id must still resolve");
    assert_eq!(resolved.id, dog);
    assert_eq!(
        resolved.effective_id, dogs,
        "resolution must follow the merge to the survivor"
    );
    assert!(resolved.deprecated_at.is_some());
}

async fn merging_an_asset_tagged_with_both_terms_does_not_conflict(pool: &PgPool) {
    // `asset_tags` is keyed on (asset_id, term_id), so an asset already carrying both terms would make
    // a naive UPDATE violate the primary key and fail the whole merge. The duplicate is dropped rather
    // than the merge being abandoned — the asset already has the meaning.
    let vocabulary = taxonomy_named(pool, "v5").await;
    let dog = term(pool, vocabulary, "dog5", None).await;
    let dogs = term(pool, vocabulary, "dogs5", None).await;

    let asset = tag(pool, dog, "both5").await;
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'confirmed', 'human')",
    )
    .bind(asset)
    .bind(dogs)
    .execute(pool)
    .await
    .expect("second tag");

    taxonomy::merge(&mut *held(pool).await, dog, dogs)
        .await
        .expect("a doubly-tagged asset must not fail the merge");
    assert_eq!(terms_on(pool, asset).await, vec![dogs]);
}

async fn merging_across_taxonomies_is_refused(pool: &PgPool) {
    // It would change what the asset means, not just which term carries it: a colour term standing in
    // for a category is not a consolidation, it is a corruption — and it would then facet under the
    // wrong vocabulary.
    let categories = taxonomy_named(pool, "cat6").await;
    let colours = taxonomy_named(pool, "col6").await;
    let outdoor = term(pool, categories, "outdoor6", None).await;
    let red = term(pool, colours, "red6", None).await;

    let refused = taxonomy::merge(&mut *held(pool).await, outdoor, red)
        .await
        .expect_err("must refuse a cross-taxonomy merge");
    assert!(
        matches!(refused, TaxonomyError::DifferentTaxonomies { .. }),
        "got {refused:?}"
    );
}

async fn a_merge_chain_resolves_to_the_end_and_a_cycle_is_refused(pool: &PgPool) {
    // Vocabularies get cleaned up repeatedly, so A→B→C is ordinary and resolution has to walk it.
    // A cycle would make that walk non-terminating, which is a hung request rather than a wrong answer.
    let vocabulary = taxonomy_named(pool, "v7").await;
    let a = term(pool, vocabulary, "a7", None).await;
    let b = term(pool, vocabulary, "b7", None).await;
    let c = term(pool, vocabulary, "c7", None).await;

    taxonomy::merge(&mut *held(pool).await, a, b)
        .await
        .expect("a into b");
    taxonomy::merge(&mut *held(pool).await, b, c)
        .await
        .expect("b into c");

    let resolved = taxonomy::resolve(&mut *held(pool).await, a)
        .await
        .expect("resolve")
        .expect("present");
    assert_eq!(
        resolved.effective_id, c,
        "a two-hop chain must resolve to the surviving term"
    );

    let refused = taxonomy::merge(&mut *held(pool).await, c, a)
        .await
        .expect_err("closing the loop must be refused");
    assert!(
        matches!(refused, TaxonomyError::WouldCycle { .. }),
        "got {refused:?}"
    );
}

async fn merging_into_a_deprecated_term_is_refused(pool: &PgPool) {
    // The survivor has to be a term someone may actually use. Merging into a retired one produces a
    // vocabulary where the "current" answer is itself retired, and the operator would have to merge
    // again to find out.
    let vocabulary = taxonomy_named(pool, "v8").await;
    let a = term(pool, vocabulary, "a8", None).await;
    let retired = term(pool, vocabulary, "retired8", None).await;
    taxonomy::deprecate(&mut *held(pool).await, retired)
        .await
        .expect("deprecate");

    let refused = taxonomy::merge(&mut *held(pool).await, a, retired)
        .await
        .expect_err("must refuse a deprecated survivor");
    assert!(
        matches!(refused, TaxonomyError::TargetDeprecated { .. }),
        "got {refused:?}"
    );
}

// ─── move ───────────────────────────────────────────────────────────────────

async fn moving_a_term_reparents_its_whole_subtree(pool: &PgPool) {
    // The operation ltree exists for, and the one that is wrong in most hand-written versions: updating
    // only the moved term leaves its descendants pointing at a path that no longer exists, so every
    // ancestor query below it silently returns nothing.
    let vocabulary = taxonomy_named(pool, "v9").await;
    let outdoor = term(pool, vocabulary, "outdoor9", None).await;
    let indoor = term(pool, vocabulary, "indoor9", None).await;
    let beach = term(pool, vocabulary, "outdoor9.beach9", Some(outdoor)).await;
    let sand = term(pool, vocabulary, "outdoor9.beach9.sand9", Some(beach)).await;

    taxonomy::move_term(&mut *held(pool).await, beach, Some(indoor))
        .await
        .expect("move");

    assert_eq!(path_of(pool, beach).await, "indoor9.beach9");
    assert_eq!(
        path_of(pool, sand).await,
        "indoor9.beach9.sand9",
        "a descendant's path must move with its ancestor, or every rollup below it breaks"
    );
    let parent: Option<Uuid> =
        sqlx::query_scalar("SELECT parent_id FROM taxonomy_terms WHERE id = $1")
            .bind(beach)
            .fetch_one(pool)
            .await
            .expect("parent");
    assert_eq!(parent, Some(indoor));
}

async fn moving_a_term_to_the_root_is_allowed(pool: &PgPool) {
    let vocabulary = taxonomy_named(pool, "v10").await;
    let outdoor = term(pool, vocabulary, "outdoor10", None).await;
    let beach = term(pool, vocabulary, "outdoor10.beach10", Some(outdoor)).await;

    taxonomy::move_term(&mut *held(pool).await, beach, None)
        .await
        .expect("move");
    assert_eq!(path_of(pool, beach).await, "beach10");
}

async fn moving_a_term_under_its_own_descendant_is_refused(pool: &PgPool) {
    // It would detach the subtree from the tree entirely: the new parent's path is computed from the
    // term being moved, so the result is a cycle with no root. Refusing beats producing a taxonomy
    // whose terms are unreachable from any query.
    let vocabulary = taxonomy_named(pool, "v11").await;
    let outdoor = term(pool, vocabulary, "outdoor11", None).await;
    let beach = term(pool, vocabulary, "outdoor11.beach11", Some(outdoor)).await;

    let refused = taxonomy::move_term(&mut *held(pool).await, outdoor, Some(beach))
        .await
        .expect_err("must refuse");
    assert!(
        matches!(refused, TaxonomyError::WouldCycle { .. }),
        "got {refused:?}"
    );

    // And under itself, which is the same mistake one level shorter.
    let refused = taxonomy::move_term(&mut *held(pool).await, outdoor, Some(outdoor))
        .await
        .expect_err("must refuse");
    assert!(matches!(refused, TaxonomyError::WouldCycle { .. }));
}

async fn moving_a_term_across_taxonomies_is_refused(pool: &PgPool) {
    let a = taxonomy_named(pool, "a12").await;
    let b = taxonomy_named(pool, "b12").await;
    let term_a = term(pool, a, "x12", None).await;
    let term_b = term(pool, b, "y12", None).await;

    let refused = taxonomy::move_term(&mut *held(pool).await, term_a, Some(term_b))
        .await
        .expect_err("must refuse");
    assert!(
        matches!(refused, TaxonomyError::DifferentTaxonomies { .. }),
        "got {refused:?}"
    );
}

async fn one_slug_under_two_parents_is_allowed_and_makes_the_move_guard_reachable(pool: &PgPool) {
    // This case used to assert the opposite, and the inversion is the point.
    //
    // `taxonomy_terms_slug_idx` was UNIQUE on `(taxonomy_id, slug)`, which made two terms in one taxonomy
    // unable to share a leaf label — and therefore made a path collision in `move_term` unreachable. That was
    // recorded as a modelling limit rather than a bug, with the `PathTaken` guard kept deliberately "so
    // relaxing this index later does not silently produce a half-applied UPDATE".
    //
    // Migration 0016 relaxed it, because a category tree needs "Yellow" under both Exterior and Interior.
    // Uniqueness on `(taxonomy_id, path)` still forbids two *siblings* sharing a slug, which is the rule that
    // matters. So the guard is now reachable, and this asserts both halves: the same slug under two parents is
    // accepted, and moving one onto the other is refused by name.
    let vocabulary = taxonomy_named(pool, "v13").await;
    let outdoor = term(pool, vocabulary, "outdoor13", None).await;
    let indoor = term(pool, vocabulary, "indoor13", None).await;
    term(pool, vocabulary, "outdoor13.beach13", Some(outdoor)).await;

    let sibling = sqlx::query(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, parent_id, path, slug, label) \
         VALUES (gen_random_uuid(), $1, $2, text2ltree('indoor13.beach13'), 'beach13', 'beach13')",
    )
    .bind(vocabulary)
    .bind(indoor)
    .execute(pool)
    .await;
    assert!(
        sibling.is_ok(),
        "the same slug under a different parent is a different path and must be allowed: {sibling:?}"
    );

    // Now the collision the guard exists for: moving `indoor13.beach13` under `outdoor13`, where a
    // `beach13` already sits. Refused by name, so a rewrite of N rows never half-applies.
    let moving: Uuid = sqlx::query_scalar(
        "SELECT id FROM taxonomy_terms WHERE taxonomy_id = $1 AND path = text2ltree('indoor13.beach13')",
    )
    .bind(vocabulary)
    .fetch_one(pool)
    .await
    .expect("the term to move");
    let refused = taxonomy::move_term(&mut *held(pool).await, moving, Some(outdoor))
        .await
        .expect_err("the destination path is taken");
    assert!(
        matches!(&refused, TaxonomyError::PathTaken { path } if path == "outdoor13.beach13"),
        "got {refused:?}"
    );

    // And nothing moved.
    let still: String = sqlx::query_scalar("SELECT path::text FROM taxonomy_terms WHERE id = $1")
        .bind(moving)
        .fetch_one(pool)
        .await
        .expect("path");
    assert_eq!(still, "indoor13.beach13");
}

async fn a_refused_move_leaves_the_subtree_exactly_as_it_was(pool: &PgPool) {
    // The transaction boundary, asserted on the failure path. A move rewrites N rows, so a refusal that
    // came after some of them had been written would leave a taxonomy no ancestor query renders
    // correctly — and the refusal would look like nothing happened.
    let vocabulary = taxonomy_named(pool, "v13b").await;
    let outdoor = term(pool, vocabulary, "outdoor13b", None).await;
    let beach = term(pool, vocabulary, "outdoor13b.beach13b", Some(outdoor)).await;
    let sand = term(pool, vocabulary, "outdoor13b.beach13b.sand13b", Some(beach)).await;

    // Refused as a cycle: the new parent is inside the subtree being moved.
    let refused = taxonomy::move_term(&mut *held(pool).await, outdoor, Some(sand))
        .await
        .expect_err("must refuse");
    assert!(
        matches!(refused, TaxonomyError::WouldCycle { .. }),
        "got {refused:?}"
    );

    assert_eq!(path_of(pool, outdoor).await, "outdoor13b");
    assert_eq!(path_of(pool, beach).await, "outdoor13b.beach13b");
    assert_eq!(path_of(pool, sand).await, "outdoor13b.beach13b.sand13b");
}

async fn moving_a_deprecated_term_is_refused(pool: &PgPool) {
    // Reorganising a retired term changes the shape of history for no benefit: nothing new can be
    // assigned to it, and its path is what old rollup queries were written against.
    let vocabulary = taxonomy_named(pool, "v14").await;
    let retired = term(pool, vocabulary, "retired14", None).await;
    let live = term(pool, vocabulary, "live14", None).await;
    taxonomy::deprecate(&mut *held(pool).await, retired)
        .await
        .expect("deprecate");

    let refused = taxonomy::move_term(&mut *held(pool).await, retired, Some(live))
        .await
        .expect_err("must refuse");
    assert!(
        matches!(refused, TaxonomyError::Deprecated { .. }),
        "got {refused:?}"
    );
}

// ─── resolution edges ───────────────────────────────────────────────────────

async fn an_unknown_term_resolves_to_absent_rather_than_erroring(pool: &PgPool) {
    // A stored id whose term was hard-deleted with its taxonomy. The caller needs to distinguish
    // "retired, here is where it went" from "gone", and an error for the second would make an ordinary
    // stale reference look like a fault.
    assert!(
        taxonomy::resolve(&mut *held(pool).await, Uuid::new_v4())
            .await
            .expect("resolve")
            .is_none()
    );
}

async fn a_live_term_resolves_to_itself(pool: &PgPool) {
    let vocabulary = taxonomy_named(pool, "v15").await;
    let live = term(pool, vocabulary, "live15", None).await;
    let resolved = taxonomy::resolve(&mut *held(pool).await, live)
        .await
        .expect("resolve")
        .expect("present");
    assert_eq!(resolved.effective_id, live);
    assert!(resolved.deprecated_at.is_none());
}

#[tokio::test]
async fn the_taxonomy_lifecycle_invariants_hold() {
    let (_pg, pool) = db().await;

    a_deprecated_term_stays_resolvable_and_keeps_its_assets(&pool).await;
    a_deprecated_term_is_excluded_from_the_assignable_set(&pool).await;
    deprecating_a_parent_with_live_children_is_refused(&pool).await;

    merging_moves_the_assets_and_leaves_the_old_id_resolvable(&pool).await;
    merging_an_asset_tagged_with_both_terms_does_not_conflict(&pool).await;
    merging_across_taxonomies_is_refused(&pool).await;
    a_merge_chain_resolves_to_the_end_and_a_cycle_is_refused(&pool).await;
    merging_into_a_deprecated_term_is_refused(&pool).await;

    moving_a_term_reparents_its_whole_subtree(&pool).await;
    moving_a_term_to_the_root_is_allowed(&pool).await;
    moving_a_term_under_its_own_descendant_is_refused(&pool).await;
    moving_a_term_across_taxonomies_is_refused(&pool).await;
    one_slug_under_two_parents_is_allowed_and_makes_the_move_guard_reachable(&pool).await;
    a_refused_move_leaves_the_subtree_exactly_as_it_was(&pool).await;
    moving_a_deprecated_term_is_refused(&pool).await;

    an_unknown_term_resolves_to_absent_rather_than_erroring(&pool).await;
    a_live_term_resolves_to_itself(&pool).await;
}
