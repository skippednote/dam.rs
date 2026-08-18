//! Rebuilding an index from Postgres (2.6's operational half).
//!
//! Postgres is the record and the index is derived, so the property under test is that the derived thing
//! matches the record — including the parts a naive reindex gets wrong: soft-deleted assets, group
//! membership, multivalued fields, and a library larger than one batch.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::TenantSlug;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::{migrate, testing::PostgresHarness};
use dam_search::schema::IndexSchema;
use dam_search::{IndexPool, PoolConfig, reindex};
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

async fn field(pool: &PgPool, key: &str, kind: &str, multivalued: bool) {
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, multivalued, display_order) \
         VALUES (gen_random_uuid(), $1, $1, $2, $3, 1)",
    )
    .bind(key)
    .bind(kind)
    .bind(multivalued)
    .execute(pool)
    .await
    .expect("field def");
}

async fn asset(pool: &PgPool, filename: &str, values: serde_json::Value) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(filename.as_bytes()).to_hex().to_string())
    .bind(filename)
    .execute(pool)
    .await
    .expect("asset");
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(&values)
        .execute(pool)
        .await
        .expect("metadata");
    id
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

async fn found(
    indexes: &IndexPool,
    tenant: &TenantSlug,
    schema: &IndexSchema,
    query: Query,
    predicate: policy::AccessPredicate,
) -> Vec<Uuid> {
    let open = indexes.get(tenant, schema).await.expect("open");
    open.reload().expect("reload");
    let planned = Planned::new(query, predicate, schema.fields()).expect("plan");
    dam_search::query::search(&open, schema, &planned, 100).expect("search")
}

#[tokio::test]
async fn the_index_matches_the_record_including_the_parts_a_naive_rebuild_loses() {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    field(&pool, "caption", "text", false).await;
    field(&pool, "colours", "text", true).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let indexes = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("acme").expect("slug");
    let defs = dam_db::fields::load(&pool).await.expect("defs");
    let schema = IndexSchema::new(defs);

    let restricted = group(&pool, "restricted").await;
    let live = asset(
        &pool,
        "live.jpg",
        serde_json::json!({ "caption": "a lighthouse", "colours": ["red", "blue"] }),
    )
    .await;
    let scoped = asset(
        &pool,
        "scoped.jpg",
        serde_json::json!({ "caption": "a lighthouse" }),
    )
    .await;
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(restricted)
        .bind(scoped)
        .execute(&pool)
        .await
        .expect("membership");
    let removed = asset(
        &pool,
        "removed.jpg",
        serde_json::json!({ "caption": "a lighthouse" }),
    )
    .await;
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(removed)
        .execute(&pool)
        .await
        .expect("soft delete");

    let stats = reindex::tenant(&pool, &indexes, &tenant, &schema, reindex::DEFAULT_BATCH)
        .await
        .expect("reindex");
    assert_eq!(stats.indexed, 3, "every row, tombstones included");
    assert_eq!(
        stats.tombstones, 1,
        "and it says how many were tombstones, so a count larger than the live library is explicable"
    );

    // The tombstone is in the index and invisible to search — which is what makes an undelete a flag flip
    // rather than a reindex.
    let hits = found(
        &indexes,
        &tenant,
        &schema,
        Query::Text("lighthouse".to_owned()),
        access(None),
    )
    .await;
    assert_eq!(
        hits.len(),
        2,
        "the soft-deleted asset must not be a search hit"
    );
    assert!(!hits.contains(&removed));
    assert!(hits.contains(&live) && hits.contains(&scoped));

    // Group membership made the trip, so the access filter can narrow on it.
    let scoped_hits = found(
        &indexes,
        &tenant,
        &schema,
        Query::Text("lighthouse".to_owned()),
        access(Some(&[restricted])),
    )
    .await;
    assert_eq!(
        scoped_hits,
        vec![scoped],
        "an asset's groups must be written into its document or the access filter cannot narrow"
    );

    // A multivalued field's values are all searchable. The bug this guards against was silent: only the
    // first value indexed, and "search does not find my tags" with no error anywhere.
    let blue = found(
        &indexes,
        &tenant,
        &schema,
        Query::Text("blue".to_owned()),
        access(None),
    )
    .await;
    assert_eq!(
        blue,
        vec![live],
        "the second value of a multivalued field must be searchable"
    );
}

#[tokio::test]
async fn a_library_larger_than_one_batch_is_indexed_whole() {
    // The cursor is the part to get wrong. A LIMIT/OFFSET walk over a table taking inserts skips and
    // repeats rows; a cursor keyed on the id cannot.
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    field(&pool, "caption", "text", false).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let indexes = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("acme").expect("slug");
    let schema = IndexSchema::new(dam_db::fields::load(&pool).await.expect("defs"));

    let mut expected = Vec::new();
    for n in 0..25 {
        expected.push(
            asset(
                &pool,
                &format!("batched-{n}.jpg"),
                serde_json::json!({ "caption": "batched sample" }),
            )
            .await,
        );
    }

    // A batch size of four over twenty-five rows: seven round trips, and the last one short.
    let stats = reindex::tenant(&pool, &indexes, &tenant, &schema, 4)
        .await
        .expect("reindex");
    assert_eq!(stats.indexed, 25);

    let mut hits = found(
        &indexes,
        &tenant,
        &schema,
        Query::Text("batched".to_owned()),
        access(None),
    )
    .await;
    hits.sort_unstable();
    expected.sort_unstable();
    assert_eq!(hits, expected, "no row skipped and none indexed twice");
}

#[tokio::test]
async fn reindexing_twice_does_not_double_the_documents() {
    // The index is replaced, not appended to. An additive reindex is the classic way a library's counts
    // start drifting upward every time an operator runs the command.
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    field(&pool, "caption", "text", false).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let indexes = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("acme").expect("slug");
    let schema = IndexSchema::new(dam_db::fields::load(&pool).await.expect("defs"));
    let only = asset(
        &pool,
        "once.jpg",
        serde_json::json!({ "caption": "singular" }),
    )
    .await;

    for _ in 0..3 {
        reindex::tenant(&pool, &indexes, &tenant, &schema, reindex::DEFAULT_BATCH)
            .await
            .expect("reindex");
    }

    assert_eq!(
        found(
            &indexes,
            &tenant,
            &schema,
            Query::Text("singular".to_owned()),
            access(None),
        )
        .await,
        vec![only],
        "three reindexes must leave one document, not three"
    );

    // And a row deleted from Postgres between runs leaves the index — the derived thing follows the record.
    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(only)
        .execute(&pool)
        .await
        .expect("delete");
    reindex::tenant(&pool, &indexes, &tenant, &schema, reindex::DEFAULT_BATCH)
        .await
        .expect("reindex");
    assert!(
        found(
            &indexes,
            &tenant,
            &schema,
            Query::Text("singular".to_owned()),
            access(None),
        )
        .await
        .is_empty(),
        "a hard-deleted asset must leave the index on the next reindex"
    );
}
