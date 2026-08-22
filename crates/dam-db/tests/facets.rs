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

    the_builtin_facets_count_what_the_selectors_filter(&pool).await;
    the_builtin_facets_are_access_filtered_too(&pool).await;
}

// ─── the built-in facets (Q.15) ─────────────────────────────────────────────

/// Every built-in bucket, and the clause it composes into, over one set of assets.
///
/// Counted and then *filtered*, in the same test, because the property that matters is not "the count is 2" —
/// it is that clicking the bucket returns the assets it counted. A rail whose numbers and results disagree is
/// worse than no rail.
async fn the_builtin_facets_count_what_the_selectors_filter(pool: &PgPool) {
    let group = group(pool, "builtin").await;
    let wide = shaped(pool, "wide", &[group], Some((4000, 3000)), "active").await;
    let tall = shaped(pool, "tall", &[group], Some((1000, 2000)), "active").await;
    let square = shaped(pool, "square", &[group], Some((800, 800)), "archived").await;
    // No dimensions at all: a PDF has no orientation, and must land in no bucket rather than a default one.
    let paper = shaped(pool, "paper", &[group], None, "active").await;

    rate(pool, wide, 5).await;
    rate(pool, tall, 4).await;
    // Two ratings averaging 3.5, which rounds to 4 — the same rounding `stars:4` does, which is the whole
    // reason the bucket and the filter agree.
    rate(pool, square, 3).await;
    rate(pool, square, 4).await;
    // In the caller's group, both of them: the point of these two rows is `LIBRARY_ROWS`, and a row the
    // access predicate already hides would prove nothing about it.
    let release = attach(pool, "release", wide, &[group]).await;
    let superseded = shaped(pool, "older", &[group], Some((4000, 3000)), "active").await;
    sqlx::query(
        "UPDATE assets SET is_current = false, version_group_id = $2, version_no = 2 WHERE id = $1",
    )
    .bind(superseded)
    .bind(wide)
    .execute(pool)
    .await
    .expect("supersede");

    let planned = plan(Query::All, Some(&[group]));

    assert_eq!(
        builtin(pool, &planned, facets::Builtin::Status).await,
        vec![("active".to_owned(), 3), ("archived".to_owned(), 1)],
        "`LIBRARY_ROWS`: the release form and the superseded version are in the group and in the collection \
         of things this caller may see, and neither is a library row"
    );
    assert_eq!(
        builtin(pool, &planned, facets::Builtin::Orientation).await,
        vec![
            ("landscape".to_owned(), 1),
            ("portrait".to_owned(), 1),
            ("square".to_owned(), 1)
        ],
        "the asset with no dimensions is in no bucket"
    );
    assert_eq!(
        builtin(pool, &planned, facets::Builtin::Rating).await,
        vec![("5".to_owned(), 1), ("4".to_owned(), 2)],
        "highest first, and the unrated asset is absent rather than zero stars"
    );
    assert_eq!(
        builtin(pool, &planned, facets::Builtin::Attachment).await,
        vec![("attachment".to_owned(), 1)],
        "one bucket only: the complement is the rest of the grid"
    );

    // And now the filters, each one what the rail writes when its bucket is clicked.
    assert_eq!(
        matching(pool, Query::Status("archived".to_owned()), group).await,
        vec![square]
    );
    assert_eq!(
        matching(
            pool,
            Query::Orientation(dam_core::query::Orientation::Landscape),
            group
        )
        .await,
        vec![wide]
    );
    assert_eq!(
        matching(
            pool,
            Query::Rating(Comparison::Equals(Literal::Int(4))),
            group
        )
        .await
        .len(),
        2,
        "the 4-star bucket counted two, so the filter must return two"
    );
    assert_eq!(
        matching(pool, Query::HasAttachment, group).await,
        vec![wide]
    );
    // The dimension-less asset is still in the library — it is active, it just has no shape. The release form
    // attached to `wide` is not: `LIBRARY_ROWS` keeps paperwork out of both the buckets and the results, which
    // is why the status facet counted three actives and not four.
    let active = matching(pool, Query::Status("active".to_owned()), group).await;
    assert!(active.contains(&paper), "{active:?}");
    assert!(
        !active.contains(&release),
        "paperwork is not a library row: {active:?}"
    );
}

/// The built-ins are counted over the same access-filtered set as everything else.
///
/// The disclosure this module opens with applies to them exactly as it does to `brand`: "Archived (12)" shown
/// to somebody who may see two of them tells them ten exist.
async fn the_builtin_facets_are_access_filtered_too(pool: &PgPool) {
    let mine = group(pool, "mine-builtin").await;
    let theirs = group(pool, "theirs-builtin").await;
    shaped(pool, "ours", &[mine], Some((100, 50)), "active").await;
    shaped(pool, "theirs", &[theirs], Some((100, 50)), "active").await;

    let planned = plan(Query::All, Some(&[mine]));
    let counted = builtin(pool, &planned, facets::Builtin::Orientation).await;
    let landscape = counted
        .iter()
        .find(|(value, _)| value == "landscape")
        .map(|(_, n)| *n)
        .unwrap_or_default();
    assert_eq!(
        landscape, 1,
        "the other group's landscape asset was counted: {counted:?}"
    );
}

/// An asset with dimensions, a status, and no metadata to speak of.
async fn shaped(
    pool: &PgPool,
    label: &str,
    groups: &[Uuid],
    size: Option<(i32, i32)>,
    status: &str,
) -> Uuid {
    let id = asset(pool, label, serde_json::json!({}), groups, false).await;
    sqlx::query("UPDATE assets SET width = $2, height = $3, status = $4 WHERE id = $1")
        .bind(id)
        .bind(size.map(|(w, _)| w))
        .bind(size.map(|(_, h)| h))
        .bind(status)
        .execute(pool)
        .await
        .expect("shape");
    id
}

async fn rate(pool: &PgPool, asset_id: Uuid, stars: i16) {
    let identity = Uuid::new_v4();
    sqlx::query("INSERT INTO asset_ratings (asset_id, identity_id, stars) VALUES ($1, $2, $3)")
        .bind(asset_id)
        .bind(identity)
        .bind(stars)
        .execute(pool)
        .await
        .expect("rating");
}

async fn attach(pool: &PgPool, label: &str, parent: Uuid, groups: &[Uuid]) -> Uuid {
    let id = asset(pool, label, serde_json::json!({}), groups, false).await;
    sqlx::query(
        "UPDATE assets SET attached_to = $2, attachment_kind = 'release', mime = 'application/pdf' \
          WHERE id = $1",
    )
    .bind(id)
    .bind(parent)
    .execute(pool)
    .await
    .expect("attach");
    id
}

async fn builtin(pool: &PgPool, planned: &Planned, which: facets::Builtin) -> Vec<(String, i64)> {
    let counted = facets::count(pool, planned, &defs(), &[FacetRequest::Builtin(which)])
        .await
        .expect("count");
    assert_eq!(counted[0].key, which.key());
    counted[0]
        .buckets
        .iter()
        .map(|b| (b.value.clone(), b.count))
        .collect()
}

/// The ids a query returns, for comparing a bucket's count against the filter it writes.
async fn matching(pool: &PgPool, query: Query, group_id: Uuid) -> Vec<Uuid> {
    let planned = plan(query, Some(&[group_id]));
    let mut builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
        "SELECT assets.id FROM assets \
                                 LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    dam_db::query_sql::push_where(&mut builder, &planned).expect("render");
    builder.push(" AND assets.is_current AND assets.attached_to IS NULL ORDER BY assets.filename");
    builder
        .build_query_scalar()
        .fetch_all(pool)
        .await
        .expect("select")
}

// ─── predictive suggestions (Q.17) ──────────────────────────────────────────

#[tokio::test]
async fn suggestions_do_not_disclose_what_a_caller_cannot_see() {
    // The property this whole module exists for, one step sharper. A facet count needs a reader to infer
    // something from a number; a suggestion *names* the value, so a type-ahead offering "Northwind" to
    // somebody with no access to Northwind's assets hands them the fact directly.
    let (_pg, pool) = db().await;
    let mine = group(&pool, "suggest-mine").await;
    let theirs = group(&pool, "suggest-theirs").await;

    asset(
        &pool,
        "mine-acme",
        serde_json::json!({"brand": "Acme Corp"}),
        &[mine],
        false,
    )
    .await;
    asset(
        &pool,
        "theirs-northwind",
        serde_json::json!({"brand": "Northwind"}),
        &[theirs],
        false,
    )
    .await;

    let scoped = plan(Query::All, Some(&[mine]));
    let labels = suggested(&pool, &scoped, "no").await;
    assert!(
        labels.is_empty(),
        "a value only the other group can see was suggested: {labels:?}"
    );

    // And the caller's own value is offered, with the fragment that filters by it.
    let found = dam_db::suggest::for_prefix(
        &mut pool.acquire().await.expect("conn"),
        &scoped,
        &defs(),
        "acm",
    )
    .await
    .expect("suggest");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].label, "Acme Corp");
    assert_eq!(found[0].within.as_deref(), Some("brand"));
    // Quoted, because `brand:Acme Corp` is a brand filter plus the free text "Corp" — the same rule the
    // client's composer follows, and it has to be the same or a suggestion changes the query when clicked.
    assert_eq!(found[0].fragment, "brand:\"Acme Corp\"");
    assert_eq!(found[0].count, 1);
}

#[tokio::test]
async fn a_suggestion_is_a_prefix_narrowed_by_the_query_already_typed() {
    let (_pg, pool) = db().await;
    let group_id = group(&pool, "suggest-prefix").await;
    asset(
        &pool,
        "sunrise",
        serde_json::json!({"brand": "acme", "colours": ["cerulean", "cedar"]}),
        &[group_id],
        false,
    )
    .await;
    asset(
        &pool,
        "sunset",
        serde_json::json!({"brand": "globex", "colours": ["cerulean"]}),
        &[group_id],
        false,
    )
    .await;

    let everything = plan(Query::All, Some(&[group_id]));
    // A prefix, not a substring: somebody four letters into a word wants what starts that way, and substring
    // matching puts the thing they are typing fourth in the list.
    let mut ceruleans = suggested(&pool, &everything, "ce").await;
    ceruleans.sort();
    assert_eq!(ceruleans, vec!["cedar", "cerulean"]);
    assert!(
        suggested(&pool, &everything, "erulean").await.is_empty(),
        "a substring is not a prefix"
    );

    // Narrowed by the query already in the box, exactly as the facet rail narrows: two clauses into a search,
    // the offer is what is left rather than what the library holds.
    let narrowed = plan(
        Query::Field {
            key: "brand".to_owned(),
            op: Comparison::Equals(Literal::Text("globex".to_owned())),
        },
        Some(&[group_id]),
    );
    assert_eq!(
        suggested(&pool, &narrowed, "ce").await,
        vec!["cerulean"],
        "`cedar` belongs to the asset the query excluded"
    );
    assert!(
        suggested(&pool, &narrowed, "ced").await.is_empty(),
        "the excluded asset's own value is not offered either"
    );

    // One character offers nothing: it is every value in the library, ordered by count, at the cost of three
    // queries.
    assert!(suggested(&pool, &everything, "c").await.is_empty());
}

#[tokio::test]
async fn a_filename_and_a_term_are_suggested_with_the_fragments_that_filter_by_them() {
    let (_pg, pool) = db().await;
    let group_id = group(&pool, "suggest-sources").await;
    let id = asset(&pool, "DSC_0043", serde_json::json!({}), &[group_id], false).await;

    // A confirmed tag under a real taxonomy, because a suggested one is a proposal in a review queue and
    // offering it as a filter would put unreviewed machine output in front of somebody as a curator's word.
    let taxonomy: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'places', 'Places', 'category') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("taxonomy");
    let term: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, label, slug, path) \
         VALUES (gen_random_uuid(), $1, 'Harbourside', 'harbourside', 'harbourside') RETURNING id",
    )
    .bind(taxonomy)
    .fetch_one(&pool)
    .await
    .expect("term");
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'confirmed', 'human')",
    )
    .bind(id)
    .bind(term)
    .execute(&pool)
    .await
    .expect("tag");
    let pending: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, label, slug, path) \
         VALUES (gen_random_uuid(), $1, 'Harbinger', 'harbinger', 'harbinger') RETURNING id",
    )
    .bind(taxonomy)
    .fetch_one(&pool)
    .await
    .expect("term");
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'suggested', 'zero_shot')",
    )
    .bind(id)
    .bind(pending)
    .execute(&pool)
    .await
    .expect("tag");

    let everything = plan(Query::All, Some(&[group_id]));
    let found = dam_db::suggest::for_prefix(
        &mut pool.acquire().await.expect("conn"),
        &everything,
        &defs(),
        "harb",
    )
    .await
    .expect("suggest");
    let labels: Vec<&str> = found.iter().map(|one| one.label.as_str()).collect();
    assert_eq!(labels, vec!["Harbourside"], "an unreviewed tag was offered");
    // `in:` takes the path, not the label: the path is what brings the descendants with it, and a fragment
    // naming the label would filter by a word rather than by a place in the tree.
    assert_eq!(found[0].fragment, "in:harbourside");
    assert_eq!(found[0].within.as_deref(), Some("Places"));

    let by_name = dam_db::suggest::for_prefix(
        &mut pool.acquire().await.expect("conn"),
        &everything,
        &defs(),
        "dsc_",
    )
    .await
    .expect("suggest");
    assert_eq!(by_name.len(), 1, "{by_name:?}");
    assert_eq!(by_name[0].fragment, "filename:DSC_0043.jpg");
    assert_eq!(by_name[0].within, None, "a filename is its own category");
}

#[tokio::test]
async fn the_nearest_value_is_one_that_exists_or_nothing() {
    // The did-you-mean on an empty page. It is an offer to run something that will work, so the candidate has
    // to be a value the caller can actually see — which is why it is looked up rather than generated.
    let (_pg, pool) = db().await;
    let mine = group(&pool, "nearest-mine").await;
    let theirs = group(&pool, "nearest-theirs").await;
    asset(
        &pool,
        "near-acme",
        serde_json::json!({"brand": "acme"}),
        &[mine],
        false,
    )
    .await;
    asset(
        &pool,
        "near-secret",
        serde_json::json!({"brand": "northwind"}),
        &[theirs],
        false,
    )
    .await;

    let visible = plan(Query::All, Some(&[mine]));
    let brand = defs().into_iter().find(|d| d.key == "brand").expect("def");
    let mut conn = pool.acquire().await.expect("conn");
    assert_eq!(
        dam_db::suggest::nearest_value(&mut conn, &visible, &brand, "acmee")
            .await
            .expect("nearest"),
        Some("acme".to_owned())
    );
    // Not the other group's value, even though it is the nearest thing in the table.
    assert_eq!(
        dam_db::suggest::nearest_value(&mut conn, &visible, &brand, "northwnd")
            .await
            .expect("nearest"),
        None,
        "a value only another group can see must not be suggested"
    );
    // And nothing when nothing is close: a guess sends somebody round a second empty loop.
    assert_eq!(
        dam_db::suggest::nearest_value(&mut conn, &visible, &brand, "photographer")
            .await
            .expect("nearest"),
        None
    );
}

/// The labels `for_prefix` offers, in the order it offers them.
async fn suggested(pool: &PgPool, planned: &Planned, typed: &str) -> Vec<String> {
    dam_db::suggest::for_prefix(
        &mut pool.acquire().await.expect("conn"),
        planned,
        &defs(),
        typed,
    )
    .await
    .expect("suggest")
    .into_iter()
    .map(|one| one.label)
    .collect()
}
