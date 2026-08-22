//! Ranked retrieval (2.9's other half).
//!
//! The differential suite next door asserts that SQL and Tantivy return the same *set*, and says explicitly that
//! order is out of scope there because ranking is Tantivy's job. This is that job: [`dam_search::query::search`]
//! returns ids **best first**, which is the input the eval harness scores. A set cannot be scored — nDCG is a
//! function of position — so without an ordered accessor the harness has nothing to measure and a ranking change
//! stays a matter of opinion.
//!
//! No Postgres here. The ordering is entirely Tantivy's, and pulling a container in to prove it would only make
//! the suite slower.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::TenantSlug;
use dam_core::fields::{Constraints, FieldDef, FieldKind};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_search::document::AssetDocument;
use dam_search::schema::IndexSchema;
use dam_search::{IndexPool, PoolConfig};
use uuid::Uuid;

fn def(key: &str, kind: FieldKind) -> FieldDef {
    FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued: false,
        required: false,
        read_only: false,
        ai_writable: false,
        facetable: false,
        constraints: Constraints::default(),
    }
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

struct Fixture {
    id: Uuid,
    filename: &'static str,
    caption: &'static str,
    groups: Vec<Uuid>,
}

async fn indexed(corpus: &[Fixture]) -> (tempfile::TempDir, IndexPool, IndexSchema, TenantSlug) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = IndexPool::new(PoolConfig::new(dir.path()));
    let schema = IndexSchema::new(vec![def("caption", FieldKind::Text)]);
    let tenant = TenantSlug::new("acme").expect("slug");

    let writer = pool.writer(&tenant, &schema).await.expect("writer");
    let mut guard = writer.lock().await;
    for asset in corpus {
        let document = AssetDocument {
            asset_id: asset.id,
            filename: asset.filename.to_owned(),
            deleted: false,
            group_ids: asset.groups.clone(),
            values: serde_json::json!({ "caption": asset.caption })
                .as_object()
                .expect("an object")
                .clone(),
        };
        guard
            .add_document(document.to_tantivy(&schema))
            .expect("add");
    }
    guard.commit().expect("commit");
    drop(guard);

    (dir, pool, schema, tenant)
}

#[tokio::test]
async fn results_come_back_best_first() {
    // Three documents mentioning "harbour" with decreasing emphasis. BM25 puts the one that is mostly about
    // harbours above the one that mentions it once in passing, and *that* is the signal the eval harness scores.
    // Returning a set instead would make every ranking indistinguishable.
    let strong = Uuid::from_u128(1);
    let middling = Uuid::from_u128(2);
    let weak = Uuid::from_u128(3);
    let irrelevant = Uuid::from_u128(4);

    let corpus = vec![
        Fixture {
            id: strong,
            filename: "harbour.jpg",
            caption: "harbour harbour harbour at dawn",
            groups: vec![],
        },
        Fixture {
            id: middling,
            filename: "boats.jpg",
            caption: "boats in the harbour harbour",
            groups: vec![],
        },
        Fixture {
            id: weak,
            filename: "city.jpg",
            caption: "a city with a harbour somewhere behind the long avenue of trees and shops",
            groups: vec![],
        },
        Fixture {
            id: irrelevant,
            filename: "mountain.jpg",
            caption: "a mountain range",
            groups: vec![],
        },
    ];
    let (_dir, pool, schema, tenant) = indexed(&corpus).await;
    let open = pool.get(&tenant, &schema).await.expect("open");
    open.reload().expect("reload");

    let planned = Planned::new(
        Query::Text("harbour".to_owned()),
        access(None),
        &[def("caption", FieldKind::Text)],
    )
    .expect("plan");

    let ranked = dam_search::query::search(&open, &schema, &planned, 10).expect("search");
    assert_eq!(
        ranked,
        vec![strong, middling, weak],
        "ranked best first, and the document that never mentions harbour is not a hit at all"
    );
}

#[tokio::test]
async fn the_limit_keeps_the_best_and_drops_the_rest() {
    // nDCG@10 means the harness asks for ten. A limit that truncated arbitrarily rather than by score would make
    // the metric depend on segment layout instead of on the ranking.
    let best = Uuid::from_u128(11);
    let second = Uuid::from_u128(12);
    let third = Uuid::from_u128(13);
    let corpus = vec![
        Fixture {
            id: third,
            filename: "c.jpg",
            caption: "estuary mentioned once among many other unrelated words in a long caption",
            groups: vec![],
        },
        Fixture {
            id: best,
            filename: "a.jpg",
            caption: "estuary estuary estuary",
            groups: vec![],
        },
        Fixture {
            id: second,
            filename: "b.jpg",
            caption: "estuary estuary",
            groups: vec![],
        },
    ];
    let (_dir, pool, schema, tenant) = indexed(&corpus).await;
    let open = pool.get(&tenant, &schema).await.expect("open");
    open.reload().expect("reload");
    let planned = Planned::new(
        Query::Text("estuary".to_owned()),
        access(None),
        &[def("caption", FieldKind::Text)],
    )
    .expect("plan");

    assert_eq!(
        dam_search::query::search(&open, &schema, &planned, 2).expect("search"),
        vec![best, second],
        "a limit of two keeps the two best, not the two first indexed"
    );
}

#[tokio::test]
async fn the_access_predicate_still_applies_to_a_ranked_search() {
    // The ordered accessor is a second consumer of the same predicate, and §12's warning is that a second
    // consumer is where divergence appears. A ranked search that ignored the filter would leak by ranking.
    let mine = Uuid::from_u128(21);
    let theirs = Uuid::from_u128(22);
    let my_group = Uuid::from_u128(0xaaaa);
    let their_group = Uuid::from_u128(0xbbbb);
    let corpus = vec![
        Fixture {
            id: theirs,
            filename: "theirs.jpg",
            caption: "quay quay quay quay",
            groups: vec![their_group],
        },
        Fixture {
            id: mine,
            filename: "mine.jpg",
            caption: "quay",
            groups: vec![my_group],
        },
    ];
    let (_dir, pool, schema, tenant) = indexed(&corpus).await;
    let open = pool.get(&tenant, &schema).await.expect("open");
    open.reload().expect("reload");

    // `theirs` scores far higher, so if the filter were dropped it would come back first — the failure is loud
    // rather than a subtle count difference.
    let planned = Planned::new(
        Query::Text("quay".to_owned()),
        access(Some(&[my_group])),
        &[def("caption", FieldKind::Text)],
    )
    .expect("plan");
    assert_eq!(
        dam_search::query::search(&open, &schema, &planned, 10).expect("search"),
        vec![mine],
        "the better-scoring document belongs to another group and must not be ranked into the results"
    );

    // And a predicate that matches nothing returns nothing rather than everything.
    let nothing = Planned::new(
        Query::Text("quay".to_owned()),
        access(Some(&[])),
        &[def("caption", FieldKind::Text)],
    )
    .expect("plan");
    assert!(
        dam_search::query::search(&open, &schema, &nothing, 10)
            .expect("search")
            .is_empty()
    );
}

#[tokio::test]
async fn a_clause_the_index_cannot_answer_is_still_refused() {
    // The ordered accessor must not become a way around `render`'s refusals. Dropping a taxonomy clause would
    // return more than the caller asked for, and doing it on the ranked path would be no better than doing it on
    // the set path.
    let corpus = vec![Fixture {
        id: Uuid::from_u128(31),
        filename: "x.jpg",
        caption: "anything",
        groups: vec![],
    }];
    let (_dir, pool, schema, tenant) = indexed(&corpus).await;
    let open = pool.get(&tenant, &schema).await.expect("open");
    open.reload().expect("reload");

    let planned = Planned::new(
        Query::Term {
            term_id: Uuid::from_u128(0xbeef),
            include_descendants: true,
        },
        access(None),
        &[def("caption", FieldKind::Text)],
    )
    .expect("plan");

    assert!(matches!(
        dam_search::query::search(&open, &schema, &planned, 10),
        Err(dam_search::Error::Unsupported(_))
    ));
}
