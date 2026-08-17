//! The two-track migration runner (ARCHITECTURE §5.3).
//!
//! Global and tenant migrations are independent version tracks with independent
//! `_sqlx_migrations` ledgers, one per schema. The counts below were measured
//! during design against a real Postgres; asserting them here turns "the schema is
//! what we think it is" into a build failure rather than a surprise.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;

/// sqlx 0.9 requires a `'static` or explicitly-asserted SQL string. These are all
/// literals or locally-built count queries over a schema name this test controls.
async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("query failed: {sql}\n{e}"))
}

#[tokio::test]
async fn global_migrations_apply_and_land_the_ledger_in_dam_global() {
    let pg = PostgresHarness::start().await.expect("start");
    migrate::global(&pg.url()).await.expect("migrate global");

    assert_eq!(
        count(
            pg.pool(),
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema='dam_global' AND table_type='BASE TABLE' \
               AND table_name <> '_sqlx_migrations'",
        )
        .await,
        14,
        "global table count changed — update this assertion deliberately"
    );

    // The ledger must be inside dam_global, not public. If the bootstrap had not
    // created the schema first, Postgres would silently ignore it in the
    // search_path and the ledger would land elsewhere, making every later run
    // think the database was unmigrated.
    assert_eq!(
        count(
            pg.pool(),
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema='dam_global' AND table_name='_sqlx_migrations'",
        )
        .await,
        1,
        "the sqlx ledger must live in dam_global"
    );
}

#[tokio::test]
async fn tenant_migrations_apply_to_a_named_schema() {
    let pg = PostgresHarness::start().await.expect("start");
    migrate::global(&pg.url()).await.expect("global");
    migrate::tenant(&pg.url(), "t_acme").await.expect("tenant");

    let checks = [
        (
            "BASE TABLE count",
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='t_acme' AND table_type='BASE TABLE' AND table_name <> '_sqlx_migrations'",
            59,
        ),
        (
            "view count",
            "SELECT count(*) FROM information_schema.views WHERE table_schema='t_acme'",
            1,
        ),
        // Excludes the sqlx ledger's own primary-key index, so this counts the
        // indexes the migrations create. The design-time measurement was taken via
        // raw psql, which had no ledger — hence 206 rather than 207.
        (
            "index count",
            "SELECT count(*) FROM pg_indexes WHERE schemaname='t_acme' AND tablename <> '_sqlx_migrations'",
            210,
        ),
        (
            "check constraints",
            "SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace WHERE n.nspname='t_acme' AND c.contype='c'",
            87,
        ),
        (
            "hnsw indexes",
            "SELECT count(*) FROM pg_indexes WHERE schemaname='t_acme' AND indexdef ILIKE '%hnsw%'",
            5,
        ),
        (
            "triggers",
            "SELECT count(*) FROM information_schema.triggers WHERE trigger_schema='t_acme'",
            2,
        ),
        (
            "rules",
            "SELECT count(*) FROM pg_rules WHERE schemaname='t_acme'",
            2,
        ),
    ];
    for (label, sql, expected) in checks {
        assert_eq!(count(pg.pool(), sql).await, expected, "{label} changed");
    }
}

#[tokio::test]
async fn each_tenant_gets_an_independent_ledger() {
    // Two tenants at head must each carry their own migration history. A shared
    // ledger would mean the second tenant's migrations were recorded as applied
    // without running.
    let pg = PostgresHarness::start().await.expect("start");
    migrate::global(&pg.url()).await.expect("global");
    migrate::tenant(&pg.url(), "t_one").await.expect("one");
    migrate::tenant(&pg.url(), "t_two").await.expect("two");

    for schema in ["t_one", "t_two"] {
        let applied = count(
            pg.pool(),
            &format!("SELECT count(*) FROM \"{schema}\"._sqlx_migrations WHERE success"),
        )
        .await;
        assert_eq!(
            applied, 9,
            "{schema} should have 9 tenant migrations applied"
        );
    }
}

#[tokio::test]
async fn migrating_twice_is_a_no_op() {
    let pg = PostgresHarness::start().await.expect("start");
    migrate::global(&pg.url()).await.expect("global 1");
    migrate::global(&pg.url())
        .await
        .expect("global 2 must be idempotent");
    migrate::tenant(&pg.url(), "t_acme")
        .await
        .expect("tenant 1");
    migrate::tenant(&pg.url(), "t_acme")
        .await
        .expect("tenant 2 must be idempotent");
    assert_eq!(
        count(
            pg.pool(),
            "SELECT count(*) FROM t_acme._sqlx_migrations WHERE success"
        )
        .await,
        9
    );
}

#[tokio::test]
async fn tenants_are_isolated_from_each_other() {
    let pg = PostgresHarness::start().await.expect("start");
    migrate::global(&pg.url()).await.expect("global");
    migrate::tenant(&pg.url(), "t_one").await.expect("one");
    migrate::tenant(&pg.url(), "t_two").await.expect("two");

    sqlx::query(r#"INSERT INTO t_one.field_defs (id, key, label, kind) VALUES (gen_random_uuid(), 'brand', 'Brand', 'text')"#)
        .execute(pg.pool())
        .await
        .expect("insert into t_one");

    let in_two: i64 = count(pg.pool(), "SELECT count(*) FROM t_two.field_defs").await;
    assert_eq!(in_two, 0, "a row in one tenant must not appear in another");
}

#[tokio::test]
async fn a_schema_name_that_is_not_a_valid_slug_is_rejected() {
    // Defence in depth. The schema name is interpolated into DDL, so a caller that
    // passes unsanitised input must be refused here rather than trusted to have
    // validated upstream.
    let pg = PostgresHarness::start().await.expect("start");
    for bad in [
        "t_acme; DROP SCHEMA dam_global CASCADE",
        "public",
        "T_Acme",
        "acme",
        "",
    ] {
        let err = migrate::tenant(&pg.url(), bad)
            .await
            .expect_err("must reject a schema name that is not a valid tenant schema");
        assert!(
            err.to_string().to_lowercase().contains("schema"),
            "unhelpful error for {bad:?}: {err}"
        );
    }
}
