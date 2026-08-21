//! Rendering the query IR to SQL (2.4).
//!
//! Two things are under test, and only one of them is about SQL.
//!
//! **The access filter is present for every query shape.** §7 requires it in the query rather than as a
//! post-filter, because pagination counts alone disclose the existence of assets a caller cannot see. A
//! post-filter returns the same rows, so the two are indistinguishable until somebody compares a
//! `count(*)` with the row set — which is exactly what the last case here does, against real rows.
//!
//! The guarantee is structural: `Planned`'s only constructor takes an `AccessPredicate`, so there is no
//! value of that type without one. These tests confirm the renderer honours it for every variant,
//! including the ones where it would be easy to short-circuit — `All`, an empty `Or`, and inside a `Not`.
//!
//! **Nothing is interpolated, and `LIKE` metacharacters are escaped.** A search string is the most
//! attacker-controlled input in the system. The escaping is the part that looks safe and is not: an
//! unescaped `%` turns `contains("50%")` into a prefix match that silently returns far more than asked.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{NaiveDate, Utc};
use dam_core::fields::{Constraints, FieldDef, FieldKind};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Comparison, Endpoint, Literal, Planned, Query};
use dam_db::{migrate, query_sql, testing::PostgresHarness};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

fn def(key: &str, kind: FieldKind, multivalued: bool) -> FieldDef {
    FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued,
        required: false,
        read_only: false,
        ai_writable: false,
        facetable: false,
        constraints: Constraints::default(),
    }
}

fn defs() -> Vec<FieldDef> {
    vec![
        def("brand", FieldKind::Text, false),
        def("colours", FieldKind::Text, true),
        def("year", FieldKind::Int, false),
        def("price", FieldKind::Decimal, false),
        def("shot_on", FieldKind::Date, false),
        def("live", FieldKind::Bool, false),
        def("homepage", FieldKind::Url, false),
    ]
}

/// A predicate that permits everything the action allows.
fn admin_access() -> policy::AccessPredicate {
    policy::compile(
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
    )
}

/// A predicate scoped to `groups`.
fn scoped_access(groups: &[Uuid]) -> policy::AccessPredicate {
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: groups.to_vec(),
            all_asset_groups: false,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Read,
        Utc::now(),
    )
}

/// A predicate that permits nothing.
fn no_access() -> policy::AccessPredicate {
    policy::compile(&Grants::from(vec![]), Action::Read, Utc::now())
}

fn render(query: Query, access: policy::AccessPredicate) -> String {
    let planned = Planned::new(query, access, &defs()).expect("valid query");
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("SELECT 1 WHERE ");
    query_sql::push_where(&mut builder, &planned).expect("render");
    builder.into_sql().as_str().to_owned()
}

// ─── the leak property ──────────────────────────────────────────────────────

#[test]
fn every_query_shape_carries_the_access_filter() {
    // The §7/§12 property, over every variant. The interesting ones are `All` (where a renderer might
    // short-circuit to `true`), an empty `Or` (where it might emit nothing), and anything inside a `Not`
    // (where an access term could get negated if the two trees were mixed).
    let group = Uuid::from_u128(0xabc);
    let shapes = vec![
        ("all", Query::All),
        ("text", Query::Text("beach".to_owned())),
        (
            "field",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("Acme".to_owned())),
            },
        ),
        (
            "term",
            Query::Term {
                term_id: Uuid::from_u128(1),
                include_descendants: true,
            },
        ),
        ("collection", Query::InCollection(Uuid::from_u128(2))),
        ("empty and", Query::And(vec![])),
        ("empty or", Query::Or(vec![])),
        ("not all", Query::Not(Box::new(Query::All))),
        (
            "not text",
            Query::Not(Box::new(Query::Text("beach".to_owned()))),
        ),
        (
            "nested",
            Query::And(vec![
                Query::Or(vec![
                    Query::Text("a".to_owned()),
                    Query::Not(Box::new(Query::All)),
                ]),
                Query::Field {
                    key: "year".to_owned(),
                    op: Comparison::Exists,
                },
            ]),
        ),
    ];

    for (name, shape) in shapes {
        let sql = render(shape, scoped_access(&[group]));
        assert!(
            sql.contains("asset_group_members"),
            "{name}: the access filter must be in the query, not applied after it — got {sql}"
        );
        assert!(sql.contains("deleted_at IS NULL"), "{name}: got {sql}");
        // The access filter must not be inside the negation. If it were, `NOT` would invert it and the
        // query would return precisely the assets the caller may not see.
        let access_at = sql.find("asset_group_members").expect("present");
        if let Some(not_at) = sql.find("NOT (") {
            assert!(
                access_at < not_at,
                "{name}: the access filter appears after a NOT, so it may be negated — {sql}"
            );
        }
    }
}

#[test]
fn a_caller_with_no_access_renders_false_whatever_they_asked_for() {
    // Short-circuited before the user's query is rendered at all. The alternative — rendering the query
    // and relying on the filter to exclude everything — is correct today and one refactor away from not
    // being.
    for shape in [
        Query::All,
        Query::Text("beach".to_owned()),
        Query::Not(Box::new(Query::All)),
    ] {
        let sql = render(shape, no_access());
        assert!(sql.contains("(false)"), "got {sql}");
        assert!(
            !sql.contains("ILIKE"),
            "the user's query should not even be rendered: {sql}"
        );
    }
}

#[test]
fn an_empty_or_renders_false_and_an_empty_and_renders_true() {
    // The dangerous mistake is an empty `Or` rendering as nothing: `WHERE ()` is a syntax error if you
    // are lucky and a dropped filter — every asset in the tenant — if you are not.
    let or = render(Query::Or(vec![]), admin_access());
    assert!(or.contains("(false)"), "got {or}");
    let and = render(Query::And(vec![]), admin_access());
    assert!(and.contains("(true)"), "got {and}");
}

// ─── injection and escaping ─────────────────────────────────────────────────

#[test]
fn no_user_value_is_ever_interpolated() {
    // Injection-proof by construction: the literal goes through `push_bind`, so the statement text holds
    // a placeholder and never a value.
    let hostile = "'; DROP TABLE assets; --";
    let sql = render(
        Query::Field {
            key: "brand".to_owned(),
            op: Comparison::Equals(Literal::Text(hostile.to_owned())),
        },
        admin_access(),
    );
    assert!(!sql.contains("DROP TABLE"), "got {sql}");
    assert!(sql.contains('$'), "expected a bind placeholder in {sql}");
}

#[test]
fn a_text_search_declares_its_like_escape_character() {
    // Escaping the metacharacters is only half of it: without `ESCAPE '\'` Postgres uses no escape
    // character at all in some configurations, and the backslashes we inserted become literal.
    let sql = render(Query::Text("50%".to_owned()), admin_access());
    assert!(sql.contains("ESCAPE"), "got {sql}");
}

// ─── validation ─────────────────────────────────────────────────────────────

#[test]
fn an_unknown_field_is_refused_because_ignoring_it_would_widen_the_results() {
    // The direction matters. A dropped clause in a *filter* returns more than the user asked for, which
    // for a search over a governed library is the wrong way to be wrong.
    let rejected = Planned::new(
        Query::Field {
            key: "brnad".to_owned(),
            op: Comparison::Exists,
        },
        admin_access(),
        &defs(),
    )
    .expect_err("must refuse");
    assert_eq!(rejected[0].code, "unknown_field");
}

#[test]
fn a_literal_of_the_wrong_type_is_refused() {
    let rejected = Planned::new(
        Query::Field {
            key: "year".to_owned(),
            op: Comparison::Equals(Literal::Text("recently".to_owned())),
        },
        admin_access(),
        &defs(),
    )
    .expect_err("must refuse");
    assert_eq!(rejected[0].code, "literal_type");
}

#[test]
fn a_range_on_an_unorderable_field_is_refused() {
    let rejected = Planned::new(
        Query::Field {
            key: "live".to_owned(),
            op: Comparison::Range {
                lower: Endpoint::Inclusive(Literal::Bool(false)),
                upper: Endpoint::Unbounded,
            },
        },
        admin_access(),
        &defs(),
    )
    .expect_err("must refuse");
    let codes: Vec<&str> = rejected.iter().map(|r| r.code).collect();
    assert!(codes.contains(&"not_orderable"), "got {codes:?}");
}

#[test]
fn an_unbounded_range_is_refused_because_it_filters_nothing() {
    let rejected = Planned::new(
        Query::Field {
            key: "year".to_owned(),
            op: Comparison::Range {
                lower: Endpoint::Unbounded,
                upper: Endpoint::Unbounded,
            },
        },
        admin_access(),
        &defs(),
    )
    .expect_err("must refuse");
    assert_eq!(rejected[0].code, "empty_range");
}

#[test]
fn a_substring_match_on_a_number_is_refused() {
    let rejected = Planned::new(
        Query::Field {
            key: "year".to_owned(),
            op: Comparison::Contains("202".to_owned()),
        },
        admin_access(),
        &defs(),
    )
    .expect_err("must refuse");
    assert_eq!(rejected[0].code, "not_textual");
}

#[test]
fn an_over_deep_query_is_refused_before_it_is_rendered() {
    // Both renderers recurse over this tree, so a few kilobytes of nested boolean is a stack overflow
    // rather than a slow query. Refusing at validation is the only place the bound costs nothing.
    let mut query = Query::All;
    for _ in 0..100 {
        query = Query::Not(Box::new(query));
    }
    let rejected = Planned::new(query, admin_access(), &defs()).expect_err("must refuse");
    assert_eq!(rejected[0].code, "too_deep");
}

#[test]
fn an_over_wide_query_is_refused() {
    let children: Vec<Query> = (0..2000)
        .map(|n| Query::InCollection(Uuid::from_u128(n)))
        .collect();
    let rejected =
        Planned::new(Query::Or(children), admin_access(), &defs()).expect_err("must refuse");
    assert_eq!(rejected[0].code, "too_large");
}

#[test]
fn every_broken_clause_is_reported_not_just_the_first() {
    let rejected = Planned::new(
        Query::And(vec![
            Query::Field {
                key: "nope".to_owned(),
                op: Comparison::Exists,
            },
            Query::Field {
                key: "year".to_owned(),
                op: Comparison::Contains("2".to_owned()),
            },
        ]),
        admin_access(),
        &defs(),
    )
    .expect_err("must refuse");
    assert_eq!(rejected.len(), 2, "got {rejected:?}");
}

// ─── against real rows ──────────────────────────────────────────────────────

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn asset_with(pool: &PgPool, label: &str, values: serde_json::Value) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(format!("blake3:{label}"))
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(values)
        .execute(pool)
        .await
        .expect("metadata");
    id
}

/// Runs a planned query and returns the matching asset ids.
async fn run(pool: &PgPool, planned: &Planned) -> Vec<Uuid> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    query_sql::push_where(&mut builder, planned).expect("render");
    builder.push(" ORDER BY assets.id");
    let mut ids: Vec<Uuid> = builder
        .build_query_scalar()
        .fetch_all(pool)
        .await
        .expect("query");
    ids.sort_unstable();
    ids
}

async fn count(pool: &PgPool, planned: &Planned) -> i64 {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT count(*) FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    query_sql::push_where(&mut builder, planned).expect("render");
    builder
        .build_query_scalar()
        .fetch_one(pool)
        .await
        .expect("count")
}

fn plan(query: Query, access: policy::AccessPredicate) -> Planned {
    Planned::new(query, access, &defs()).expect("valid")
}

async fn equality_matches_scalars_and_array_members(pool: &PgPool) {
    // `@>` answers both in one operator. Writing it as `->> =` would be right for the scalar and
    // silently wrong for the array — which is most tag-like fields.
    let scalar = asset_with(pool, "scalar", serde_json::json!({"brand": "Acme"})).await;
    let array = asset_with(
        pool,
        "array",
        serde_json::json!({"colours": ["red", "blue"]}),
    )
    .await;
    asset_with(pool, "other", serde_json::json!({"brand": "Globex"})).await;

    let brand = plan(
        Query::Field {
            key: "brand".to_owned(),
            op: Comparison::Equals(Literal::Text("Acme".to_owned())),
        },
        admin_access(),
    );
    assert_eq!(run(pool, &brand).await, vec![scalar]);

    let colour = plan(
        Query::Field {
            key: "colours".to_owned(),
            op: Comparison::Equals(Literal::Text("blue".to_owned())),
        },
        admin_access(),
    );
    assert_eq!(
        run(pool, &colour).await,
        vec![array],
        "equality must reach inside a multivalued field"
    );
}

async fn not_equals_on_a_multivalued_field_excludes_any_match(pool: &PgPool) {
    // `<>` would compare the whole array, so "not red" would match an asset tagged red *and* blue. This
    // is the case that makes `NOT (@>)` the right rendering.
    let both = asset_with(
        pool,
        "both-ne",
        serde_json::json!({"colours": ["red", "blue"]}),
    )
    .await;
    let neither = asset_with(
        pool,
        "neither-ne",
        serde_json::json!({"colours": ["green"]}),
    )
    .await;

    let query = plan(
        Query::Field {
            key: "colours".to_owned(),
            op: Comparison::NotEquals(Literal::Text("red".to_owned())),
        },
        admin_access(),
    );
    let matched = run(pool, &query).await;
    assert!(matched.contains(&neither));
    assert!(
        !matched.contains(&both),
        "an asset tagged red must not match 'not red' merely because it is also tagged blue"
    );
}

async fn a_numeric_range_compares_as_numbers_not_text(pool: &PgPool) {
    // `'9' > '10'` is true as text and false as a number. Without the cast this test is the one that
    // would fail, and in production it would be a price filter quietly returning the wrong products.
    let nine = asset_with(pool, "nine", serde_json::json!({"year": 9})).await;
    let ten = asset_with(pool, "ten", serde_json::json!({"year": 10})).await;

    let query = plan(
        Query::Field {
            key: "year".to_owned(),
            op: Comparison::Range {
                lower: Endpoint::Exclusive(Literal::Int(9)),
                upper: Endpoint::Unbounded,
            },
        },
        admin_access(),
    );
    let matched = run(pool, &query).await;
    assert!(matched.contains(&ten));
    assert!(!matched.contains(&nine), "9 is not greater than 9");
}

async fn a_date_range_is_inclusive_where_it_says_it_is(pool: &PgPool) {
    let on = asset_with(pool, "on", serde_json::json!({"shot_on": "2026-08-17"})).await;
    let after = asset_with(pool, "after", serde_json::json!({"shot_on": "2026-08-18"})).await;

    let inclusive = plan(
        Query::Field {
            key: "shot_on".to_owned(),
            op: Comparison::Range {
                lower: Endpoint::Inclusive(Literal::Date(
                    NaiveDate::from_ymd_opt(2026, 8, 17).expect("date"),
                )),
                upper: Endpoint::Unbounded,
            },
        },
        admin_access(),
    );
    let matched = run(pool, &inclusive).await;
    assert!(matched.contains(&on) && matched.contains(&after));

    let exclusive = plan(
        Query::Field {
            key: "shot_on".to_owned(),
            op: Comparison::Range {
                lower: Endpoint::Exclusive(Literal::Date(
                    NaiveDate::from_ymd_opt(2026, 8, 17).expect("date"),
                )),
                upper: Endpoint::Unbounded,
            },
        },
        admin_access(),
    );
    let matched = run(pool, &exclusive).await;
    assert!(!matched.contains(&on) && matched.contains(&after));
}

async fn contains_treats_a_percent_as_a_literal_character(pool: &PgPool) {
    // Unescaped, `contains("50%")` becomes a prefix match on "50" and returns "500ml" too — silently
    // returning far more than asked, which is the worst kind of search bug because it looks like it works.
    let exact = asset_with(pool, "fifty", serde_json::json!({"brand": "50% cotton"})).await;
    let decoy = asset_with(pool, "fivehundred", serde_json::json!({"brand": "500ml"})).await;

    let query = plan(
        Query::Field {
            key: "brand".to_owned(),
            op: Comparison::Contains("50%".to_owned()),
        },
        admin_access(),
    );
    let matched = run(pool, &query).await;
    assert!(matched.contains(&exact));
    assert!(
        !matched.contains(&decoy),
        "the percent must be a literal character, not a wildcard"
    );
}

async fn missing_covers_absent_null_and_empty_array(pool: &PgPool) {
    // A user asking for "no brand" means all three. An implementation that only checks absence leaves
    // the cleanup queue permanently non-empty.
    let absent = asset_with(pool, "absent", serde_json::json!({"year": 2026})).await;
    let null = asset_with(pool, "null", serde_json::json!({"brand": null})).await;
    let empty = asset_with(pool, "empty", serde_json::json!({"brand": []})).await;
    let present = asset_with(pool, "present", serde_json::json!({"brand": "Acme"})).await;

    let query = plan(
        Query::Field {
            key: "brand".to_owned(),
            op: Comparison::Missing,
        },
        admin_access(),
    );
    let matched = run(pool, &query).await;
    for id in [absent, null, empty] {
        assert!(matched.contains(&id), "missing must cover {id}");
    }
    assert!(!matched.contains(&present));
}

async fn the_count_matches_the_row_set_so_pagination_cannot_leak(pool: &PgPool) {
    // §7, against real rows. A post-filter returns the same rows as an in-query filter, so the two are
    // indistinguishable *except* by the count — which is why this compares them rather than just
    // checking the row set.
    let group = Uuid::new_v4();
    sqlx::query("INSERT INTO asset_groups (id, key, label) VALUES ($1, 'g', 'g')")
        .bind(group)
        .execute(pool)
        .await
        .expect("group");

    let visible = asset_with(pool, "visible", serde_json::json!({"brand": "Acme"})).await;
    let hidden = asset_with(pool, "hidden", serde_json::json!({"brand": "Acme"})).await;
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(visible)
        .execute(pool)
        .await
        .expect("membership");

    let query = plan(
        Query::Field {
            key: "brand".to_owned(),
            op: Comparison::Equals(Literal::Text("Acme".to_owned())),
        },
        scoped_access(&[group]),
    );

    let rows = run(pool, &query).await;
    let counted = count(pool, &query).await;
    assert!(rows.contains(&visible));
    assert!(
        !rows.contains(&hidden),
        "an asset outside the caller's groups must not be returned"
    );
    assert_eq!(
        counted,
        i64::try_from(rows.len()).expect("small"),
        "the count must match the row set, or pagination discloses assets the caller cannot see"
    );
}

async fn a_taxonomy_query_includes_descendants_and_only_confirmed_tags(pool: &PgPool) {
    // Two properties in one case because they share a fixture. Descendants: clicking a category means
    // its subtree. Confirmed only: a `suggested` AI tag is a proposal in a review queue, and letting one
    // affect results makes unreviewed machine output indistinguishable from a curator's decision.
    let vocabulary = Uuid::new_v4();
    sqlx::query("INSERT INTO taxonomies (id, key, label) VALUES ($1, 'v', 'v')")
        .bind(vocabulary)
        .execute(pool)
        .await
        .expect("taxonomy");

    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    for (id, path, slug, parent_id) in [
        (parent, "outdoorq", "outdoorq", None),
        (child, "outdoorq.beachq", "beachq", Some(parent)),
    ] {
        sqlx::query(
            "INSERT INTO taxonomy_terms (id, taxonomy_id, parent_id, path, slug, label) \
             VALUES ($1, $2, $3, text2ltree($4), $5, $5)",
        )
        .bind(id)
        .bind(vocabulary)
        .bind(parent_id)
        .bind(path)
        .bind(slug)
        .execute(pool)
        .await
        .expect("term");
    }

    let on_child = asset_with(pool, "onchild", serde_json::json!({})).await;
    let suggested = asset_with(pool, "suggested", serde_json::json!({})).await;
    for (asset, term, state) in [
        (on_child, child, "confirmed"),
        (suggested, parent, "suggested"),
    ] {
        sqlx::query(
            "INSERT INTO asset_tags (asset_id, term_id, state, source) VALUES ($1, $2, $3, 'human')",
        )
        .bind(asset)
        .bind(term)
        .bind(state)
        .execute(pool)
        .await
        .expect("tag");
    }

    let with_descendants = plan(
        Query::Term {
            term_id: parent,
            include_descendants: true,
        },
        admin_access(),
    );
    let matched = run(pool, &with_descendants).await;
    assert!(
        matched.contains(&on_child),
        "a tag on a descendant term must match its ancestor"
    );
    assert!(
        !matched.contains(&suggested),
        "an unreviewed suggestion must not affect search results"
    );

    let exact = plan(
        Query::Term {
            term_id: parent,
            include_descendants: false,
        },
        admin_access(),
    );
    assert!(
        !run(pool, &exact).await.contains(&on_child),
        "without descendants, only the term itself matches"
    );
}

async fn a_free_text_search_reaches_array_values(pool: &PgPool) {
    // Without the array branch a text search silently misses every value in a multivalued field, and the
    // symptom is "search does not find my tags" with no error anywhere.
    let in_array = asset_with(
        pool,
        "inarray",
        serde_json::json!({"colours": ["cerulean", "beige"]}),
    )
    .await;
    let query = plan(Query::Text("cerulean".to_owned()), admin_access());
    assert!(run(pool, &query).await.contains(&in_array));
}

#[tokio::test]
async fn the_sql_renderer_invariants_hold() {
    let (_pg, pool) = db().await;
    equality_matches_scalars_and_array_members(&pool).await;
    not_equals_on_a_multivalued_field_excludes_any_match(&pool).await;
    a_numeric_range_compares_as_numbers_not_text(&pool).await;
    a_date_range_is_inclusive_where_it_says_it_is(&pool).await;
    contains_treats_a_percent_as_a_literal_character(&pool).await;
    missing_covers_absent_null_and_empty_array(&pool).await;
    the_count_matches_the_row_set_so_pagination_cannot_leak(&pool).await;
    a_taxonomy_query_includes_descendants_and_only_confirmed_tags(&pool).await;
    a_free_text_search_reaches_array_values(&pool).await;
    a_filename_clause_matches_names_case_insensitively(&pool).await;
    a_wildcard_in_a_filename_stays_a_literal_character(&pool).await;
}

/// Q.16: the filename clause, over the column rather than the index.
async fn a_filename_clause_matches_names_case_insensitively(pool: &PgPool) {
    let camera = asset_with(pool, "DSC_0043", serde_json::json!({})).await;
    let other = asset_with(pool, "DSC_0044", serde_json::json!({})).await;
    let unrelated = asset_with(pool, "harbour", serde_json::json!({})).await;
    // Two names that make the operators tell each other apart. `copy-of-DSC_0044` *contains* the prefix
    // without starting with it, and `DSC_0043.jpg.bak` starts with the exact name without being it — without
    // both, a prefix rendered as a substring and an equality rendered as a prefix return the same rows as the
    // correct ones and the test cannot see the difference.
    let copy = asset_with(pool, "copy-of-DSC_0044", serde_json::json!({})).await;
    let backup = asset_with(pool, "DSC_0043.jpg.bak", serde_json::json!({})).await;

    // A filename is something a person half-remembers, so the case they type is not the case on disk.
    let exact = plan(
        Query::Filename(Comparison::Equals(Literal::Text("dsc_0043.jpg".to_owned()))),
        admin_access(),
    );
    assert_eq!(
        run(pool, &exact).await,
        vec![camera],
        "an equality must not behave like a prefix: `DSC_0043.jpg.bak` starts with this name"
    );

    // The substring is the whole point: `0043` is a token the index does not hold, and it is what somebody
    // reading a filename off a delivery note actually has.
    let substring = plan(
        Query::Filename(Comparison::Contains("0043".to_owned())),
        admin_access(),
    );
    // Both of them: `DSC_0043.jpg.bak` contains the number too, and a substring that quietly stopped at the
    // first match would be the bug this operator exists to avoid.
    let mut containing_number = vec![camera, backup];
    containing_number.sort_unstable();
    assert_eq!(run(pool, &substring).await, containing_number);

    let prefix = plan(
        Query::Filename(Comparison::StartsWith("dsc_00".to_owned())),
        admin_access(),
    );
    let mut expected = vec![camera, other, backup];
    expected.sort_unstable();
    assert_eq!(
        run(pool, &prefix).await,
        expected,
        "a prefix must not behave like a substring: `copy-of-DSC_0044.jpg` contains this and does not \
         start with it"
    );

    // And the substring does reach it, which is the difference between the two operators.
    let anywhere = plan(
        Query::Filename(Comparison::Contains("dsc_0044".to_owned())),
        admin_access(),
    );
    let mut containing = vec![other, copy];
    containing.sort_unstable();
    assert_eq!(run(pool, &anywhere).await, containing);

    // The negation. Everything but that one asset, and the `NOT` has to wrap the whole `ILIKE` — without it
    // the clause reads as the positive one and the result is the exact complement of what was asked for.
    let excluded = plan(
        Query::Filename(Comparison::NotEquals(Literal::Text(
            "dsc_0043.jpg".to_owned(),
        ))),
        admin_access(),
    );
    let found = run(pool, &excluded).await;
    assert!(!found.contains(&camera), "{found:?}");
    assert!(found.contains(&other), "{found:?}");
    assert!(found.contains(&backup), "{found:?}");

    // A list of names, which is what a paste of filenames composes into.
    let listed = plan(
        Query::Or(vec![
            Query::Filename(Comparison::Equals(Literal::Text("dsc_0044.jpg".to_owned()))),
            Query::Filename(Comparison::Equals(Literal::Text("harbour.jpg".to_owned()))),
        ]),
        admin_access(),
    );
    let mut listed_ids = vec![other, unrelated];
    listed_ids.sort_unstable();
    assert_eq!(run(pool, &listed).await, listed_ids);
}

/// A `%` or `_` in a filename is a character, not a pattern.
async fn a_wildcard_in_a_filename_stays_a_literal_character(pool: &PgPool) {
    // `50%-off` is a real filename, and without the ESCAPE clause its `%` would match anything — so searching
    // for it would find every asset whose name starts with "50".
    let discounted = asset_with(pool, "50%-off", serde_json::json!({})).await;
    asset_with(pool, "50-pence", serde_json::json!({})).await;

    let planned = plan(
        Query::Filename(Comparison::StartsWith("50%".to_owned())),
        admin_access(),
    );
    assert_eq!(run(pool, &planned).await, vec![discounted]);
}

// ─── engagement clauses (Q.5b·2) ────────────────────────────────────────────

/// Renders with a named viewer, which `is:` needs.
fn render_as(query: Query, access: policy::AccessPredicate, viewer: Uuid) -> String {
    let planned = Planned::new(query, access, &defs())
        .expect("valid query")
        .viewed_by(viewer);
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("SELECT 1 WHERE ");
    query_sql::push_where(&mut builder, &planned).expect("render");
    builder.into_sql().as_str().to_owned()
}

#[test]
fn a_personal_clause_without_a_viewer_fails_loudly() {
    // Not an empty result. "You have no favourites" and "the code forgot to say who you are" look identical on
    // a screen, and only one of them is a bug somebody can find.
    let planned = Planned::new(
        Query::Mine(dam_core::query::Personal::Favourite),
        admin_access(),
        &defs(),
    )
    .expect("valid query");
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("SELECT 1 WHERE ");
    let outcome = query_sql::push_where(&mut builder, &planned);
    assert!(
        outcome.is_err(),
        "rendered without a viewer: {}",
        builder.into_sql().as_str()
    );
}

#[test]
fn a_personal_clause_binds_the_viewer_and_never_interpolates_it() {
    let viewer = Uuid::new_v4();
    for (state, table) in [
        (dam_core::query::Personal::Favourite, "asset_favourites"),
        (dam_core::query::Personal::Watched, "asset_watches"),
        (dam_core::query::Personal::Rated, "asset_ratings"),
    ] {
        let sql = render_as(Query::Mine(state), admin_access(), viewer);
        assert!(sql.contains(table), "{state:?}: {sql}");
        // The identity is a bound parameter. Interpolated, it would be the one value in the query that comes
        // from the session rather than the request — and the habit is what matters, not this uuid.
        assert!(
            !sql.contains(&viewer.to_string()),
            "{state:?} interpolated the viewer: {sql}"
        );
        // Correlated existence, not a join: a join against a per-person table would multiply asset rows.
        assert!(sql.contains("EXISTS"), "{state:?}: {sql}");
        // And the access filter is still the outer conjunct.
        assert!(sql.contains("deleted_at IS NULL"), "{state:?}: {sql}");
    }

    // Each state reads its own table and no other, so a wire-crossed match arm shows up here.
    let favourites = render_as(
        Query::Mine(dam_core::query::Personal::Favourite),
        admin_access(),
        viewer,
    );
    assert!(!favourites.contains("asset_watches"), "{favourites}");
    assert!(!favourites.contains("asset_ratings"), "{favourites}");
}

#[test]
fn a_rating_comparison_averages_rather_than_joining() {
    let sql = render(
        Query::Rating(Comparison::Range {
            lower: Endpoint::Inclusive(Literal::Int(4)),
            upper: Endpoint::Unbounded,
        }),
        admin_access(),
    );
    assert!(sql.contains("avg(stars)"), "{sql}");
    // A correlated subquery, not a join: joining `asset_ratings` would multiply each asset by its ratings and
    // every count downstream would be wrong.
    assert!(
        sql.contains("SELECT avg(stars) FROM asset_ratings"),
        "{sql}"
    );
    assert!(sql.contains(">="), "{sql}");
    // No viewer needed: an average is the library's shared judgement, not the caller's.
    assert!(sql.contains("deleted_at IS NULL"), "{sql}");
}

#[test]
fn unrated_is_not_the_same_as_rated_low() {
    let missing = render(Query::Rating(Comparison::Missing), admin_access());
    let exists = render(Query::Rating(Comparison::Exists), admin_access());
    assert!(missing.contains("NOT EXISTS"), "{missing}");
    assert!(
        exists.contains("EXISTS") && !exists.contains("NOT EXISTS"),
        "{exists}"
    );

    // `!= 4` must not sweep in the unrated. Their average is null, so a bare `<> 4` would exclude them from
    // *both* sides of the comparison — and the complement of a bucket would be smaller than the library minus
    // the bucket, which is the kind of arithmetic a user notices and cannot explain.
    let not_four = render(
        Query::Rating(Comparison::NotEquals(Literal::Int(4))),
        admin_access(),
    );
    assert!(
        not_four.contains("EXISTS"),
        "unrated assets are not 'not four': {not_four}"
    );
    assert!(not_four.contains("<>"), "{not_four}");
}

#[test]
fn a_rating_outside_one_to_five_is_refused_before_rendering() {
    // Refused rather than clamped: `stars:>=9` is a mistake, and quietly answering `stars:>=5` would answer a
    // question nobody asked.
    for literal in [
        Literal::Int(0),
        Literal::Int(6),
        Literal::Text("four".to_owned()),
    ] {
        let rejections = Planned::new(
            Query::Rating(Comparison::Equals(literal.clone())),
            admin_access(),
            &defs(),
        )
        .expect_err("out of range");
        assert!(
            rejections.iter().any(|r| r.key == "stars"),
            "{literal:?}: {rejections:?}"
        );
    }
    // And a substring operator on a number is refused too.
    let rejections = Planned::new(
        Query::Rating(Comparison::Contains("4".to_owned())),
        admin_access(),
        &defs(),
    )
    .expect_err("not a number comparison");
    assert!(
        rejections.iter().any(|r| r.code == "stars_operator"),
        "{rejections:?}"
    );
}
