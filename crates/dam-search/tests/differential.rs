//! The differential test §12 asks for, and 0.10 deferred (2.6).
//!
//! §12: the access rules are compiled once and reused by SQL, Tantivy and MCP, *because divergence here is
//! a data leak*. Compiling once is necessary and not sufficient — two renderers can consume the same
//! predicate and still disagree, and the disagreement that matters is the one where Tantivy returns a
//! document SQL would have hidden.
//!
//! So the same corpus is indexed into both back ends, the same [`Planned`] is run through both, and the
//! **id sets must be identical**. Not the order: ranking is Tantivy's job and SQL makes no attempt at it.
//! The set is what carries the security property.
//!
//! Two clauses are deliberately out of scope and are asserted to be *refused* rather than approximated:
//! taxonomy and collection membership are relational and not in the index, and substring matching would
//! disagree with `ILIKE` at the margins. A renderer that silently dropped either would return more than
//! the caller asked for.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::TenantSlug;
use dam_core::fields::{Constraints, FieldDef, FieldKind};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Comparison, Endpoint, Literal, Planned, Query};
use dam_db::{migrate, query_sql, testing::PostgresHarness};
use dam_search::document::AssetDocument;
use dam_search::schema::IndexSchema;
use dam_search::{IndexPool, PoolConfig};
use sqlx::{PgPool, Postgres, QueryBuilder};
use std::collections::BTreeSet;
use tantivy::TantivyDocument;
use tantivy::collector::DocSetCollector;
use tantivy::schema::Value as _;
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
        def("live", FieldKind::Bool, false),
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

/// One asset in the shared corpus.
struct Fixture {
    id: Uuid,
    filename: &'static str,
    deleted: bool,
    groups: Vec<Uuid>,
    values: serde_json::Value,
}

/// The corpus, indexed identically into Postgres and Tantivy.
fn corpus(group_a: Uuid, group_b: Uuid) -> Vec<Fixture> {
    // Ids are fixed rather than random so a failure names the same asset every run.
    let id = |n: u128| Uuid::from_u128(0xd1ff_0000_0000_0000_0000_0000_0000_0000 + n);
    vec![
        Fixture {
            id: id(1),
            filename: "beach-holiday.jpg",
            deleted: false,
            groups: vec![group_a],
            values: serde_json::json!({"brand": "Acme", "year": 2020, "live": true,
                                       "colours": ["red", "blue"]}),
        },
        Fixture {
            id: id(2),
            filename: "mountain-trek.jpg",
            deleted: false,
            groups: vec![group_a, group_b],
            values: serde_json::json!({"brand": "Globex", "year": 2026, "live": false,
                                       "colours": ["green"]}),
        },
        Fixture {
            id: id(3),
            filename: "studio-shot.jpg",
            deleted: false,
            groups: vec![group_b],
            values: serde_json::json!({"brand": "Acme", "year": 2026}),
        },
        Fixture {
            id: id(6),
            // A **multi-word, mixed-case** value, which is the fixture this suite was missing. Every value
            // here used to be a single lowercase word, and that is why a real divergence survived it: under
            // the default tokeniser "Acme Corp" indexed as `acme` + `corp`, so `brand:Acme` matched it in
            // Tantivy and not in SQL — 22 results against 11 on a real corpus.
            filename: "acme-corp-launch.jpg",
            deleted: false,
            groups: vec![group_a],
            values: serde_json::json!({"brand": "Acme Corp", "year": 2024,
                                       "colours": ["Deep Blue", "red"]}),
        },
        Fixture {
            id: id(4),
            // In no group at all: visible to an administrator, invisible to anyone scoped to groups.
            filename: "ungrouped.jpg",
            deleted: false,
            groups: vec![],
            values: serde_json::json!({"brand": "Initech", "year": 1999, "live": true}),
        },
        Fixture {
            id: id(5),
            // Soft-deleted. Excluded by both back ends on every query, which is the kind of clause that
            // is easy to add to one renderer and forget in the other.
            filename: "deleted-beach.jpg",
            deleted: true,
            groups: vec![group_a],
            values: serde_json::json!({"brand": "Acme", "year": 2020}),
        },
    ]
}

async fn seed_postgres(pool: &PgPool, group_a: Uuid, group_b: Uuid, corpus: &[Fixture]) {
    for (id, key) in [(group_a, "a"), (group_b, "b")] {
        sqlx::query("INSERT INTO asset_groups (id, key, label) VALUES ($1, $2, $2)")
            .bind(id)
            .bind(key)
            .execute(pool)
            .await
            .expect("group");
    }

    for asset in corpus {
        sqlx::query(
            "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, deleted_at) \
             VALUES ($1, $2, $3, 'image/jpeg', 10, $1, CASE WHEN $4 THEN now() ELSE NULL END)",
        )
        .bind(asset.id)
        .bind(format!("blake3:{}", asset.filename))
        .bind(asset.filename)
        .bind(asset.deleted)
        .execute(pool)
        .await
        .expect("asset");

        sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
            .bind(asset.id)
            .bind(&asset.values)
            .execute(pool)
            .await
            .expect("metadata");

        for group in &asset.groups {
            sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
                .bind(group)
                .bind(asset.id)
                .execute(pool)
                .await
                .expect("membership");
        }
    }
}

async fn seed_tantivy(
    pool: &IndexPool,
    schema: &IndexSchema,
    tenant: &TenantSlug,
    corpus: &[Fixture],
) {
    let writer = pool.writer(tenant, schema).await.expect("writer");
    let mut guard = writer.lock().await;
    for asset in corpus {
        let document = AssetDocument {
            asset_id: asset.id,
            filename: asset.filename.to_owned(),
            deleted: asset.deleted,
            group_ids: asset.groups.clone(),
            values: asset.values.as_object().expect("an object").clone(),
        };
        guard
            .add_document(document.to_tantivy(schema))
            .expect("add");
    }
    guard.commit().expect("commit");
}

/// The ids SQL returns.
async fn via_sql(pool: &PgPool, planned: &Planned) -> BTreeSet<Uuid> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    query_sql::push_where(&mut builder, planned).expect("render sql");
    builder
        .build_query_scalar::<Uuid>()
        .fetch_all(pool)
        .await
        .expect("sql query")
        .into_iter()
        .collect()
}

/// The ids Tantivy returns.
fn via_tantivy(
    open: &dam_search::pool::OpenIndex,
    schema: &IndexSchema,
    planned: &Planned,
) -> BTreeSet<Uuid> {
    let query = dam_search::query::render(planned, schema).expect("render tantivy");
    let searcher = open.searcher();
    let addresses = searcher
        .search(&query, &DocSetCollector)
        .expect("tantivy search");

    addresses
        .into_iter()
        .map(|address| {
            let doc: TantivyDocument = searcher.doc(address).expect("doc");
            let raw = doc
                .get_first(schema.asset_id())
                .and_then(|value| value.as_str().map(str::to_owned))
                .expect("a stored asset id");
            Uuid::parse_str(&raw).expect("a uuid")
        })
        .collect()
}

#[tokio::test]
async fn both_back_ends_return_identical_sets_for_every_shared_query_shape() {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let sql = pg.pool_for_schema("t_acme").await.expect("pool");

    let group_a = Uuid::from_u128(0xaaaa);
    let group_b = Uuid::from_u128(0xbbbb);
    let fixtures = corpus(group_a, group_b);

    let dir = tempfile::tempdir().expect("tempdir");
    let schema = IndexSchema::new(defs());
    let tenant = TenantSlug::new("acme").expect("slug");
    let index_pool = IndexPool::new(PoolConfig::new(dir.path()));

    seed_postgres(&sql, group_a, group_b, &fixtures).await;
    seed_tantivy(&index_pool, &schema, &tenant, &fixtures).await;
    let open = index_pool.get(&tenant, &schema).await.expect("open");
    open.reload().expect("reload");

    // Each shape is run under an administrator's predicate and under one scoped to a single group, so the
    // access filter is exercised on every shape rather than only on its own case.
    let shapes: Vec<(&str, Query)> = vec![
        ("all", Query::All),
        ("text", Query::Text("beach".to_owned())),
        (
            "equals text",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("Acme".to_owned())),
            },
        ),
        (
            // The case the multi-word fixture exists for: matching one word of a two-word value must return
            // the same set from both back ends, and SQL's jsonb containment compares the whole value.
            "equals one word of a two-word value",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("Acme".to_owned())),
            },
        ),
        (
            "equals a two-word value in full",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("Acme Corp".to_owned())),
            },
        ),
        (
            // Case is part of the value for both back ends: `@>` is case-sensitive, so the index must be too.
            "equals text in the wrong case",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("acme".to_owned())),
            },
        ),
        (
            "equals a two-word value in a multivalued field",
            Query::Field {
                key: "colours".to_owned(),
                op: Comparison::Equals(Literal::Text("Deep Blue".to_owned())),
            },
        ),
        (
            "equals in a multivalued field",
            Query::Field {
                key: "colours".to_owned(),
                op: Comparison::Equals(Literal::Text("blue".to_owned())),
            },
        ),
        (
            "equals int",
            Query::Field {
                key: "year".to_owned(),
                op: Comparison::Equals(Literal::Int(2026)),
            },
        ),
        (
            "equals bool",
            Query::Field {
                key: "live".to_owned(),
                op: Comparison::Equals(Literal::Bool(true)),
            },
        ),
        (
            "not equals",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::NotEquals(Literal::Text("Acme".to_owned())),
            },
        ),
        (
            "exists",
            Query::Field {
                key: "live".to_owned(),
                op: Comparison::Exists,
            },
        ),
        (
            "missing",
            Query::Field {
                key: "live".to_owned(),
                op: Comparison::Missing,
            },
        ),
        (
            "range",
            Query::Field {
                key: "year".to_owned(),
                op: Comparison::Range {
                    lower: Endpoint::Inclusive(Literal::Int(2020)),
                    upper: Endpoint::Unbounded,
                },
            },
        ),
        (
            "and",
            Query::And(vec![
                Query::Field {
                    key: "brand".to_owned(),
                    op: Comparison::Equals(Literal::Text("Acme".to_owned())),
                },
                Query::Field {
                    key: "year".to_owned(),
                    op: Comparison::Equals(Literal::Int(2026)),
                },
            ]),
        ),
        (
            "or",
            Query::Or(vec![
                Query::Field {
                    key: "brand".to_owned(),
                    op: Comparison::Equals(Literal::Text("Globex".to_owned())),
                },
                Query::Field {
                    key: "brand".to_owned(),
                    op: Comparison::Equals(Literal::Text("Initech".to_owned())),
                },
            ]),
        ),
        (
            "not",
            Query::Not(Box::new(Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("Acme".to_owned())),
            })),
        ),
        ("empty and", Query::And(vec![])),
        ("empty or", Query::Or(vec![])),
    ];

    let predicates: Vec<(&str, policy::AccessPredicate)> = vec![
        ("administrator", access(None)),
        ("scoped to group a", access(Some(&[group_a]))),
        ("scoped to group b", access(Some(&[group_b]))),
        ("scoped to both", access(Some(&[group_a, group_b]))),
    ];

    let mut compared = 0usize;
    for (shape_name, shape) in &shapes {
        for (predicate_name, predicate) in &predicates {
            let planned = Planned::new(shape.clone(), predicate.clone(), &defs())
                .unwrap_or_else(|e| panic!("{shape_name} should validate: {e:?}"));

            let from_sql = via_sql(&sql, &planned).await;
            let from_tantivy = via_tantivy(&open, &schema, &planned);

            assert_eq!(
                from_sql, from_tantivy,
                "{shape_name} / {predicate_name}: the two back ends disagree, which is the data leak \
                 §12 exists to prevent.\n  sql:     {from_sql:?}\n  tantivy: {from_tantivy:?}"
            );
            compared += 1;
        }
    }

    assert_eq!(
        compared,
        shapes.len() * predicates.len(),
        "every shape must be compared under every predicate"
    );

    // A guard against the whole thing passing vacuously. If every query returned nothing, the sets would
    // agree perfectly and the test would prove nothing at all — this is the assertion that caught four
    // other tests in this build passing for the wrong reason.
    let everything = Planned::new(Query::All, access(None), &defs()).expect("valid");
    assert_eq!(
        via_sql(&sql, &everything).await.len(),
        5,
        "the corpus must have five visible assets, or the comparisons above are between empty sets"
    );
}

#[tokio::test]
async fn a_soft_deleted_asset_is_invisible_to_both_back_ends() {
    // Stated on its own because it is the clause most easily added to one renderer and forgotten in the
    // other — and a deleted asset reappearing in search is what a customer notices first.
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let sql = pg.pool_for_schema("t_acme").await.expect("pool");

    let group_a = Uuid::from_u128(0xaaaa);
    let group_b = Uuid::from_u128(0xbbbb);
    let fixtures = corpus(group_a, group_b);
    let deleted_id = fixtures
        .iter()
        .find(|f| f.deleted)
        .expect("a deleted fixture")
        .id;

    let dir = tempfile::tempdir().expect("tempdir");
    let schema = IndexSchema::new(defs());
    let tenant = TenantSlug::new("acme").expect("slug");
    let index_pool = IndexPool::new(PoolConfig::new(dir.path()));
    seed_postgres(&sql, group_a, group_b, &fixtures).await;
    seed_tantivy(&index_pool, &schema, &tenant, &fixtures).await;
    let open = index_pool.get(&tenant, &schema).await.expect("open");
    open.reload().expect("reload");

    // Deliberately a query the deleted asset would otherwise match.
    let planned = Planned::new(
        Query::Field {
            key: "brand".to_owned(),
            op: Comparison::Equals(Literal::Text("Acme".to_owned())),
        },
        access(None),
        &defs(),
    )
    .expect("valid");

    let from_sql = via_sql(&sql, &planned).await;
    let from_tantivy = via_tantivy(&open, &schema, &planned);
    assert!(
        !from_sql.contains(&deleted_id),
        "SQL returned the deleted asset"
    );
    assert!(
        !from_tantivy.contains(&deleted_id),
        "Tantivy returned the deleted asset"
    );
    assert!(
        !from_sql.is_empty(),
        "and the query must match something, or the assertions above are vacuous"
    );
}

#[test]
fn clauses_the_index_cannot_answer_are_refused_rather_than_dropped() {
    // The alternative is a filter clause silently ignored, which returns *more* than the caller asked
    // for. For a search over a governed library that is the wrong direction to be wrong in, and it is
    // invisible — the extra rows look like ordinary results.
    let schema = IndexSchema::new(defs());
    for (name, shape) in [
        (
            "taxonomy",
            Query::Term {
                term_id: Uuid::from_u128(1),
                include_descendants: true,
            },
        ),
        ("collection", Query::InCollection(Uuid::from_u128(2))),
        (
            "substring",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Contains("cme".to_owned()),
            },
        ),
    ] {
        let planned = Planned::new(shape, access(None), &defs()).expect("valid");
        let outcome = dam_search::query::render(&planned, &schema);
        assert!(
            matches!(outcome, Err(dam_search::Error::Unsupported(_))),
            "{name} must be refused, not approximated"
        );
    }
}
