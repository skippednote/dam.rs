//! The eval harness end to end (2.9, G8).
//!
//! The claim 2.9 makes is that "a ranking change reports its effect instead of being argued about". That claim is
//! only true if a run over the real parse → plan → rank path moves when the ranking moves, and does not move when
//! nothing relevant changed. Both halves are here, along with the case that decides whether the number can be
//! trusted at all: a query that never reached the ranker must be *reported*, because a corpus that quietly drops
//! its broken queries scores better the more of it breaks.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::TenantSlug;
use dam_core::fields::FieldKind;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_db::{judgements, migrate, testing::PostgresHarness};
use dam_search::document::AssetDocument;
use dam_search::eval_run::{self, DEFAULT_AT};
use dam_search::schema::IndexSchema;
use dam_search::{IndexPool, PoolConfig};
use sqlx::PgPool;
use uuid::Uuid;

fn unrestricted() -> policy::AccessPredicate {
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

async fn field(pool: &PgPool, key: &str, kind: &str, alias: Option<&str>) {
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, search_alias, display_order) \
         VALUES (gen_random_uuid(), $1, $1, $2, $3, 1)",
    )
    .bind(key)
    .bind(kind)
    .bind(alias)
    .execute(pool)
    .await
    .expect("field def");
}

async fn asset(pool: &PgPool, filename: &str, caption: &str) -> Uuid {
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
        .bind(serde_json::json!({ "caption": caption }))
        .execute(pool)
        .await
        .expect("metadata");
    id
}

/// Indexes assets with the captions given, replacing whatever was there.
async fn index(
    indexes: &IndexPool,
    tenant: &TenantSlug,
    schema: &IndexSchema,
    docs: &[(Uuid, &str, &str)],
) {
    let writer = indexes.writer(tenant, schema).await.expect("writer");
    let mut guard = writer.lock().await;
    guard.delete_all_documents().expect("clear");
    for (id, filename, caption) in docs {
        let document = AssetDocument {
            asset_id: *id,
            filename: (*filename).to_owned(),
            deleted: false,
            group_ids: vec![],
            values: serde_json::json!({ "caption": caption })
                .as_object()
                .expect("an object")
                .clone(),
        };
        guard
            .add_document(document.to_tantivy(schema))
            .expect("add");
    }
    guard.commit().expect("commit");
}

#[tokio::test]
async fn a_ranking_change_moves_the_number() {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    field(&pool, "caption", "text", Some("cap")).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let indexes = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("acme").expect("slug");
    let parse_schema = dam_db::fields::search_schema(&pool)
        .await
        .expect("search schema");
    let index_schema = IndexSchema::new(parse_schema.fields().to_vec());
    assert_eq!(
        parse_schema
            .fields()
            .iter()
            .map(|def| def.kind)
            .collect::<Vec<_>>(),
        vec![FieldKind::Text],
        "the run must plan against the tenant's own definitions, loaded from the tenant's own schema"
    );

    // Two assets, one obviously the better answer for "harbour". The judgements say so.
    let good = asset(&pool, "harbour.jpg", "harbour at dawn").await;
    let poor = asset(
        &pool,
        "field.jpg",
        "a field of wheat, with a harbour far away",
    )
    .await;
    judgements::record(&pool, "harbour", good, 3, None)
        .await
        .expect("judge");
    judgements::record(&pool, "harbour", poor, 0, None)
        .await
        .expect("judge");

    let access = unrestricted();

    // A ranking that agrees with the judgements.
    index(
        &indexes,
        &tenant,
        &index_schema,
        &[
            (good, "harbour.jpg", "harbour harbour harbour at dawn"),
            (
                poor,
                "field.jpg",
                "a field of wheat with lots of other words in it",
            ),
        ],
    )
    .await;
    let corpus = judgements::corpus(&pool).await.expect("corpus");
    let agreeing = eval_run::run(
        &indexes,
        &tenant,
        &index_schema,
        &parse_schema,
        &access,
        corpus.clone(),
        DEFAULT_AT,
    )
    .await
    .expect("run");
    assert!(
        agreeing.is_trustworthy(),
        "nothing was refused, so the run is comparable: {:?}",
        agreeing.refused
    );
    assert_eq!(agreeing.report.scoreable, 1);
    assert_eq!(agreeing.report.mean_ndcg, Some(1.0));
    assert_eq!(agreeing.report.mrr, Some(1.0));

    // Now a ranking that puts the graded-zero asset first. Nothing about the corpus changed; only the index did.
    index(
        &indexes,
        &tenant,
        &index_schema,
        &[
            (
                good,
                "harbour.jpg",
                "a harbour mentioned once among many unrelated words here",
            ),
            (poor, "field.jpg", "harbour harbour harbour harbour"),
        ],
    )
    .await;
    let worse = eval_run::run(
        &indexes,
        &tenant,
        &index_schema,
        &parse_schema,
        &access,
        corpus,
        DEFAULT_AT,
    )
    .await
    .expect("run");

    assert!(
        worse.report.mean_ndcg < agreeing.report.mean_ndcg,
        "the same corpus over a worse ranking must score lower: {:?} vs {:?}",
        worse.report.mean_ndcg,
        agreeing.report.mean_ndcg
    );
    assert_eq!(
        worse.report.mrr,
        Some(0.5),
        "the first relevant result moved from rank 1 to rank 2, which is exactly what MRR is for"
    );
}

#[tokio::test]
async fn a_query_that_cannot_run_is_reported_rather_than_dropped() {
    // The failure mode that makes an eval harness worse than none: a corpus whose queries stop parsing, whose
    // broken queries are skipped, and whose mean therefore *improves*. Refusals are surfaced and
    // `is_trustworthy` is false while any remain.
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    field(&pool, "caption", "text", Some("cap")).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let indexes = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("acme").expect("slug");
    let parse_schema = dam_db::fields::search_schema(&pool)
        .await
        .expect("search schema");
    let index_schema = IndexSchema::new(parse_schema.fields().to_vec());

    let scoreable = asset(&pool, "fine.jpg", "quay at dusk").await;
    let unparseable = asset(&pool, "other.jpg", "unrelated").await;
    judgements::record(&pool, "quay", scoreable, 3, None)
        .await
        .expect("judge");
    // A shorthand key that is not a field. Realistic: a field gets renamed and the corpus still names the old one.
    judgements::record(&pool, "nosuchfield:acme", unparseable, 3, None)
        .await
        .expect("judge");

    index(
        &indexes,
        &tenant,
        &index_schema,
        &[
            (scoreable, "fine.jpg", "quay at dusk"),
            (unparseable, "other.jpg", "unrelated"),
        ],
    )
    .await;

    let run = eval_run::run(
        &indexes,
        &tenant,
        &index_schema,
        &parse_schema,
        &unrestricted(),
        judgements::corpus(&pool).await.expect("corpus"),
        DEFAULT_AT,
    )
    .await
    .expect("run");

    assert_eq!(run.refused.len(), 1, "the broken query must be named");
    assert_eq!(run.refused[0].query_text, "nosuchfield:acme");
    assert!(
        run.refused[0].reason.contains("parse"),
        "and the reason must say what went wrong: {}",
        run.refused[0].reason
    );
    assert_eq!(run.report.scoreable, 1, "the other query still scored");
    assert!(
        !run.is_trustworthy(),
        "a run with a refusal must not be compared against one without — the sample differs"
    );
}

#[tokio::test]
async fn an_empty_corpus_reports_nothing_rather_than_perfection() {
    // 0/0 defaulting to 1.0 is the trap `dam_core::eval` was built to avoid, and it has to survive the trip
    // through this layer too — a run over an unlabelled library must not print a perfect score.
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    field(&pool, "caption", "text", None).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let indexes = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("acme").expect("slug");
    let parse_schema = dam_db::fields::search_schema(&pool)
        .await
        .expect("search schema");
    let index_schema = IndexSchema::new(parse_schema.fields().to_vec());
    index(&indexes, &tenant, &index_schema, &[]).await;

    let run = eval_run::run(
        &indexes,
        &tenant,
        &index_schema,
        &parse_schema,
        &unrestricted(),
        judgements::corpus(&pool).await.expect("corpus"),
        DEFAULT_AT,
    )
    .await
    .expect("run");

    assert_eq!(run.report.scoreable, 0);
    assert_eq!(
        run.report.mean_ndcg, None,
        "no judgements is not a perfect score"
    );
    assert_eq!(run.report.mrr, None);
    assert!(
        !run.is_trustworthy(),
        "an empty corpus cannot gate anything, and must not read as if it could"
    );
}
