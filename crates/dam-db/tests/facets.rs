//! Faceted search (2.7): counts that respect the access predicate.
//!
//! The property this suite exists for is not "the counts are right" but "the counts do not disclose".
//! §7 says pagination counts alone reveal the existence of assets a caller cannot see, and a facet rail is
//! a pagination count with better presentation: `brand: Acme (5)` shown to someone who may see three of
//! them tells them two exist that they cannot.
//!
//! Two consequences are tested directly. A count is over the access-filtered set. And a value with no
//! visible assets **does not appear at all** — not "appears with count 0", because a zero bucket discloses
//! that the value exists, which for a `client` or `campaign` facet is usually the sensitive part.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::fields::{Constraints, FieldDef, FieldKind};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Comparison, Literal, Planned, Query};
use dam_db::facets::{self, FacetRequest};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn def(key: &str, kind: FieldKind, multivalued: bool, facetable: bool) -> FieldDef {
    FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued,
        required: false,
        read_only: false,
        ai_writable: false,
        facetable,
        constraints: Constraints::default(),
    }
}

fn defs() -> Vec<FieldDef> {
    vec![
        def("brand", FieldKind::Text, false, true),
        def("colours", FieldKind::Text, true, true),
        def("year", FieldKind::Int, false, true),
        // Deliberately not facetable: free text with a bucket per distinct value.
        def("notes", FieldKind::LongText, false, false),
        def("shot_at", FieldKind::Geo, false, true),
    ]
}

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

fn plan(query: Query, groups: Option<&[Uuid]>) -> Planned {
    Planned::new(query, access(groups), &defs()).expect("valid")
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn group(pool: &PgPool, key: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO asset_groups (id, key, label) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(key)
        .execute(pool)
        .await
        .expect("group");
    id
}

async fn asset(
    pool: &PgPool,
    label: &str,
    values: serde_json::Value,
    groups: &[Uuid],
    deleted: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, deleted_at) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1, CASE WHEN $4 THEN now() ELSE NULL END)",
    )
    .bind(id)
    .bind(format!("blake3:{label}"))
    .bind(format!("{label}.jpg"))
    .bind(deleted)
    .execute(pool)
    .await
    .expect("asset");
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(values)
        .execute(pool)
        .await
        .expect("metadata");
    for g in groups {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(g)
            .bind(id)
            .execute(pool)
            .await
            .expect("membership");
    }
    id
}

async fn counts(pool: &PgPool, planned: &Planned, key: &str) -> Vec<(String, i64)> {
    let facets = facets::count(
        pool,
        planned,
        &defs(),
        &[FacetRequest::Field {
            key: key.to_owned(),
            limit: 50,
        }],
    )
    .await
    .expect("count");
    facets[0]
        .buckets
        .iter()
        .map(|b| (b.value.clone(), b.count))
        .collect()
}

// ─── the disclosure properties ──────────────────────────────────────────────

async fn counts_are_over_the_access_filtered_set(pool: &PgPool) {
    // The §7 property. A count that included assets outside the caller's groups would tell them how many
    // exist — which is the disclosure, whatever the row list says.
    let visible = group(pool, "vis1").await;
    let hidden = group(pool, "hid1").await;
    asset(
        pool,
        "a1",
        serde_json::json!({"brand": "Acme"}),
        &[visible],
        false,
    )
    .await;
    asset(
        pool,
        "b1",
        serde_json::json!({"brand": "Acme"}),
        &[hidden],
        false,
    )
    .await;
    asset(
        pool,
        "c1",
        serde_json::json!({"brand": "Acme"}),
        &[hidden],
        false,
    )
    .await;

    let scoped = plan(Query::All, Some(&[visible]));
    assert_eq!(
        counts(pool, &scoped, "brand").await,
        vec![("Acme".to_owned(), 1)],
        "the caller may see one Acme asset, so the count is one — not three"
    );

    let administrator = plan(Query::All, None);
    assert_eq!(
        counts(pool, &administrator, "brand").await,
        vec![("Acme".to_owned(), 3)],
        "and an administrator sees all three, or the test above passed for lack of data"
    );
}

async fn a_value_with_no_visible_assets_does_not_appear_at_all(pool: &PgPool) {
    // Not "appears with count 0". A zero bucket discloses that the value exists, and for a `client` or
    // `campaign` facet that existence is usually the sensitive part — the count is beside the point.
    let visible = group(pool, "vis2").await;
    let hidden = group(pool, "hid2").await;
    asset(
        pool,
        "a2",
        serde_json::json!({"brand": "Visible"}),
        &[visible],
        false,
    )
    .await;
    asset(
        pool,
        "b2",
        serde_json::json!({"brand": "Secret"}),
        &[hidden],
        false,
    )
    .await;

    let scoped = plan(Query::All, Some(&[visible]));
    let values: Vec<String> = counts(pool, &scoped, "brand")
        .await
        .into_iter()
        .map(|(value, _)| value)
        .collect();
    assert!(values.contains(&"Visible".to_owned()));
    assert!(
        !values.contains(&"Secret".to_owned()),
        "an invisible value must produce no bucket, not a zero one: got {values:?}"
    );
}

async fn a_caller_with_no_access_gets_no_buckets(pool: &PgPool) {
    asset(pool, "a3", serde_json::json!({"brand": "Acme"}), &[], false).await;
    let nothing = Planned::new(
        Query::All,
        policy::compile(&Grants::from(vec![]), Action::Read, Utc::now()),
        &defs(),
    )
    .expect("valid");
    assert!(counts(pool, &nothing, "brand").await.is_empty());
}

async fn deleted_assets_do_not_contribute(pool: &PgPool) {
    // Inherited from the shared `push_where` rather than reimplemented, which is the point of routing
    // facets through the same renderer — a second copy of the delete rule is a second thing to forget.
    let g = group(pool, "del4").await;
    asset(
        pool,
        "a4",
        serde_json::json!({"brand": "Four"}),
        &[g],
        false,
    )
    .await;
    asset(pool, "b4", serde_json::json!({"brand": "Four"}), &[g], true).await;

    assert_eq!(
        counts(pool, &plan(Query::All, Some(&[g])), "brand").await,
        vec![("Four".to_owned(), 1)]
    );
}

// ─── counting correctly ─────────────────────────────────────────────────────

async fn a_multivalued_field_counts_each_value_once_per_asset(pool: &PgPool) {
    // Without the array branch a multivalued field facets as zero buckets — the same trap the SQL query
    // renderer had. And the `DISTINCT` matters: an asset whose array repeats a value must count once, or
    // a sloppy import inflates the rail.
    let g = group(pool, "mv5").await;
    asset(
        pool,
        "a5",
        serde_json::json!({"colours": ["red", "blue"]}),
        &[g],
        false,
    )
    .await;
    asset(
        pool,
        "b5",
        serde_json::json!({"colours": ["red", "red", "green"]}),
        &[g],
        false,
    )
    .await;

    let mut got = counts(pool, &plan(Query::All, Some(&[g])), "colours").await;
    got.sort();
    assert_eq!(
        got,
        vec![
            ("blue".to_owned(), 1),
            ("green".to_owned(), 1),
            ("red".to_owned(), 2),
        ],
        "red is on two assets, and the duplicate inside one array must not make it three"
    );
}

async fn a_facet_respects_the_rest_of_the_query(pool: &PgPool) {
    // The behaviour that makes a rail usable: counts narrow as filters are applied. A rail whose numbers
    // never move is a rail nobody trusts.
    let g = group(pool, "q6").await;
    asset(
        pool,
        "a6",
        serde_json::json!({"brand": "Acme", "year": 2020}),
        &[g],
        false,
    )
    .await;
    asset(
        pool,
        "b6",
        serde_json::json!({"brand": "Acme", "year": 2026}),
        &[g],
        false,
    )
    .await;
    asset(
        pool,
        "c6",
        serde_json::json!({"brand": "Globex", "year": 2026}),
        &[g],
        false,
    )
    .await;

    let filtered = Planned::new(
        Query::Field {
            key: "year".to_owned(),
            op: Comparison::Equals(Literal::Int(2026)),
        },
        access(Some(&[g])),
        &defs(),
    )
    .expect("valid");
    let mut got = counts(pool, &filtered, "brand").await;
    got.sort();
    assert_eq!(
        got,
        vec![("Acme".to_owned(), 1), ("Globex".to_owned(), 1)],
        "with year 2026 selected, Acme drops from two to one"
    );
}

async fn numbers_and_booleans_are_counted_as_rendered_text(pool: &PgPool) {
    let g = group(pool, "num7").await;
    asset(pool, "a7", serde_json::json!({"year": 2026}), &[g], false).await;
    asset(pool, "b7", serde_json::json!({"year": 2026}), &[g], false).await;
    asset(pool, "c7", serde_json::json!({"year": 2020}), &[g], false).await;

    assert_eq!(
        counts(pool, &plan(Query::All, Some(&[g])), "year").await,
        vec![("2026".to_owned(), 2), ("2020".to_owned(), 1)],
        "ordered by count descending, which is what a rail shows"
    );
}

async fn an_absent_field_produces_no_bucket(pool: &PgPool) {
    // Not a bucket for `null`. "No brand" is a filter (`Missing`), not a facet value, and a null bucket
    // would sort to the top of most rails.
    let g = group(pool, "abs8").await;
    asset(pool, "a8", serde_json::json!({"year": 1}), &[g], false).await;
    asset(
        pool,
        "b8",
        serde_json::json!({"brand": "Eight"}),
        &[g],
        false,
    )
    .await;

    assert_eq!(
        counts(pool, &plan(Query::All, Some(&[g])), "brand").await,
        vec![("Eight".to_owned(), 1)]
    );
}

async fn the_limit_truncates_and_says_so(pool: &PgPool) {
    // A rail that silently truncates makes "no other brands" and "ninety other brands" look identical,
    // and a user filters on the wrong assumption.
    let g = group(pool, "lim9").await;
    for n in 0..5 {
        asset(
            pool,
            &format!("a9-{n}"),
            serde_json::json!({"brand": format!("Brand{n}")}),
            &[g],
            false,
        )
        .await;
    }

    let facets = facets::count(
        pool,
        &plan(Query::All, Some(&[g])),
        &defs(),
        &[FacetRequest::Field {
            key: "brand".to_owned(),
            limit: 2,
        }],
    )
    .await
    .expect("count");
    assert_eq!(facets[0].buckets.len(), 2);
    assert!(
        facets[0].truncated,
        "five values with a limit of two is truncated"
    );

    let all = facets::count(
        pool,
        &plan(Query::All, Some(&[g])),
        &defs(),
        &[FacetRequest::Field {
            key: "brand".to_owned(),
            limit: 50,
        }],
    )
    .await
    .expect("count");
    assert_eq!(all[0].buckets.len(), 5);
    assert!(!all[0].truncated);
}

// ─── governance ─────────────────────────────────────────────────────────────

async fn a_field_that_is_not_facetable_is_refused(pool: &PgPool) {
    // Governance and a resource guard in one. Faceting a long-text field produces a bucket per distinct
    // value, which on a real library is a response nobody asked for.
    let outcome = facets::count(
        pool,
        &plan(Query::All, None),
        &defs(),
        &[FacetRequest::Field {
            key: "notes".to_owned(),
            limit: 10,
        }],
    )
    .await;
    assert!(outcome.is_err(), "a non-facetable field must be refused");
}

async fn a_geo_field_is_refused_even_when_marked_facetable(pool: &PgPool) {
    // A coordinate has no discrete values to count. Bucketing raw coordinates would produce one bucket per
    // asset, which is the worst possible facet.
    let outcome = facets::count(
        pool,
        &plan(Query::All, None),
        &defs(),
        &[FacetRequest::Field {
            key: "shot_at".to_owned(),
            limit: 10,
        }],
    )
    .await;
    assert!(outcome.is_err());
}

async fn an_unknown_field_is_not_found(pool: &PgPool) {
    let outcome = facets::count(
        pool,
        &plan(Query::All, None),
        &defs(),
        &[FacetRequest::Field {
            key: "nope".to_owned(),
            limit: 10,
        }],
    )
    .await;
    assert!(outcome.is_err());
}

// ─── taxonomy rollup ────────────────────────────────────────────────────────

async fn a_taxonomy_facet_rolls_up_without_double_counting(pool: &PgPool) {
    // The subtle one. An asset tagged with two leaves under the same ancestor must count **once** for that
    // ancestor — otherwise a rollup exceeds the number of assets that exist, and a user sees
    // "Outdoor (7)" over a library of five.
    let g = group(pool, "tax10").await;
    let vocabulary = Uuid::new_v4();
    sqlx::query("INSERT INTO taxonomies (id, key, label) VALUES ($1, 'v10', 'v10')")
        .bind(vocabulary)
        .execute(pool)
        .await
        .expect("taxonomy");

    let outdoor = Uuid::new_v4();
    let beach = Uuid::new_v4();
    let cliff = Uuid::new_v4();
    for (id, path, slug, parent) in [
        (outdoor, "outdoor10", "outdoor10", None),
        (beach, "outdoor10.beach10", "beach10", Some(outdoor)),
        (cliff, "outdoor10.cliff10", "cliff10", Some(outdoor)),
    ] {
        sqlx::query(
            "INSERT INTO taxonomy_terms (id, taxonomy_id, parent_id, path, slug, label) \
             VALUES ($1, $2, $3, text2ltree($4), $5, $5)",
        )
        .bind(id)
        .bind(vocabulary)
        .bind(parent)
        .bind(path)
        .bind(slug)
        .execute(pool)
        .await
        .expect("term");
    }

    let both = asset(pool, "a10", serde_json::json!({}), &[g], false).await;
    let only_beach = asset(pool, "b10", serde_json::json!({}), &[g], false).await;
    let suggested_only = asset(pool, "c10", serde_json::json!({}), &[g], false).await;
    for (asset_id, term, state) in [
        (both, beach, "confirmed"),
        (both, cliff, "confirmed"),
        (only_beach, beach, "confirmed"),
        (suggested_only, beach, "suggested"),
    ] {
        sqlx::query(
            "INSERT INTO asset_tags (asset_id, term_id, state, source) VALUES ($1, $2, $3, 'human')",
        )
        .bind(asset_id)
        .bind(term)
        .bind(state)
        .execute(pool)
        .await
        .expect("tag");
    }

    let facets = facets::count(
        pool,
        &plan(Query::All, Some(&[g])),
        &defs(),
        &[FacetRequest::Taxonomy {
            taxonomy_id: vocabulary,
            limit: 50,
        }],
    )
    .await
    .expect("count");

    let mut got: Vec<(String, i64)> = facets[0]
        .buckets
        .iter()
        .map(|b| (b.value.clone(), b.count))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("beach10".to_owned(), 2),
            ("cliff10".to_owned(), 1),
            ("outdoor10".to_owned(), 2),
        ],
        "outdoor must be 2, not 3: the asset tagged both beach and cliff counts once for their ancestor"
    );

    // And the unreviewed suggestion contributed nothing, matching how search treats it.
    assert!(
        facets[0].buckets.iter().all(|b| b.count <= 2),
        "a suggested tag must not affect facet counts"
    );

    // Each bucket carries its term id, so clicking one produces a query rather than a label match.
    assert!(
        facets[0].buckets.iter().all(|b| b.id.is_some()),
        "a taxonomy bucket must carry its term id"
    );
}

#[tokio::test]
async fn the_facet_invariants_hold() {
    let (_pg, pool) = db().await;

    counts_are_over_the_access_filtered_set(&pool).await;
    a_value_with_no_visible_assets_does_not_appear_at_all(&pool).await;
    a_caller_with_no_access_gets_no_buckets(&pool).await;
    deleted_assets_do_not_contribute(&pool).await;

    a_multivalued_field_counts_each_value_once_per_asset(&pool).await;
    a_facet_respects_the_rest_of_the_query(&pool).await;
    numbers_and_booleans_are_counted_as_rendered_text(&pool).await;
    an_absent_field_produces_no_bucket(&pool).await;
    the_limit_truncates_and_says_so(&pool).await;

    a_field_that_is_not_facetable_is_refused(&pool).await;
    a_geo_field_is_refused_even_when_marked_facetable(&pool).await;
    an_unknown_field_is_not_found(&pool).await;

    a_taxonomy_facet_rolls_up_without_double_counting(&pool).await;
}
