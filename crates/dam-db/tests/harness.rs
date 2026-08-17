//! The container harness itself, tested before anything is built on it.
//!
//! Every later test in this workspace stands on this, so a harness that silently
//! hands back a half-bootstrapped database would make every downstream failure
//! misleading. These assertions are deliberately about the *preconditions* the
//! migration runner assumes (ARCHITECTURE §5.3), not about the migrations.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_db::testing::PostgresHarness;

#[tokio::test]
async fn harness_hands_back_a_usable_pool() {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(pg.pool())
        .await
        .expect("query");
    assert_eq!(one, 1);
}

#[tokio::test]
async fn bootstrap_creates_the_three_schemas() {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT nspname FROM pg_namespace \
         WHERE nspname IN ('dam_global', 'extensions', 'tenant_template') ORDER BY nspname",
    )
    .fetch_all(pg.pool())
    .await
    .expect("query schemas");
    assert_eq!(names, vec!["dam_global", "extensions", "tenant_template"]);
}

#[tokio::test]
async fn bootstrap_installs_the_extensions_into_the_extensions_schema() {
    // Extensions are database-scoped, not schema-scoped, so they are installed
    // once into `extensions` and referenced schema-qualified from every tenant
    // schema. If they landed in `public` instead, `extensions.vector(1152)` in the
    // tenant migrations would fail with a confusing type error.
    let pg = PostgresHarness::start().await.expect("start postgres");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT e.extname, n.nspname FROM pg_extension e \
         JOIN pg_namespace n ON n.oid = e.extnamespace \
         WHERE e.extname IN ('vector', 'ltree', 'pgcrypto') ORDER BY e.extname",
    )
    .fetch_all(pg.pool())
    .await
    .expect("query extensions");
    assert_eq!(
        rows,
        vec![
            ("ltree".to_owned(), "extensions".to_owned()),
            ("pgcrypto".to_owned(), "extensions".to_owned()),
            ("vector".to_owned(), "extensions".to_owned()),
        ]
    );
}

#[tokio::test]
async fn the_vector_type_is_usable_schema_qualified() {
    // The specific thing 0003 depends on. Proving it here means a pgvector image
    // change surfaces as a harness failure rather than a migration failure.
    let pg = PostgresHarness::start().await.expect("start postgres");
    sqlx::query("CREATE TABLE probe (id int, v extensions.vector(3))")
        .execute(pg.pool())
        .await
        .expect("vector column must be creatable schema-qualified");
    sqlx::query("CREATE INDEX ON probe USING hnsw (v extensions.vector_cosine_ops)")
        .execute(pg.pool())
        .await
        .expect("hnsw index with a qualified opclass must build");
}

#[tokio::test]
async fn two_harnesses_are_independent() {
    // Suites run in parallel, so each harness must get its own container on its
    // own port. A shared container would make test order significant.
    let a = PostgresHarness::start().await.expect("start a");
    let b = PostgresHarness::start().await.expect("start b");
    assert_ne!(a.port(), b.port(), "harnesses must not share a container");

    sqlx::query("CREATE TABLE only_in_a (x int)")
        .execute(a.pool())
        .await
        .expect("create in a");
    let leaked: Option<String> = sqlx::query_scalar("SELECT to_regclass('only_in_a')::text")
        .fetch_one(b.pool())
        .await
        .expect("query b");
    assert!(leaked.is_none(), "table leaked across harnesses");
}
