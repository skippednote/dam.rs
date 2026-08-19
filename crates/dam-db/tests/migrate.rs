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
            // 61 since 0015: `metadata_types` and `metadata_type_fields`.
            // 62 since 0017: `upload_profiles`; 63 since 0018: `auto_import_mappings`; 66 since 0019 added the
            // three engagement tables — separate because a rating aggregates, a favourite is a private list and
            // a watch is a standing request, and one table would carry a null column per unused role.
            // 68 since 0020: `asset_comments` and its routing table; 69 since 0021 added the default partition
            // that keeps an event write from failing outside January 2026 — see the migration.
            // 70 since 0023: `conversions`, the tenant's named download formats.
            70,
        ),
        (
            "view count",
            "SELECT count(*) FROM information_schema.views WHERE table_schema='t_acme'",
            2,
        ),
        // Excludes the sqlx ledger's own primary-key index, so this counts the
        // indexes the migrations create. The design-time measurement was taken via
        // raw psql, which had no ledger — hence 206 rather than 207.
        (
            "index count",
            "SELECT count(*) FROM pg_indexes WHERE schemaname='t_acme' AND tablename <> '_sqlx_migrations'",
            // 213 since 0012: taxonomy_terms gains a live-terms index and a supersession index.
            // 219 since 0015: metadata_types gains a primary key, a key index and the one-default partial
            // index; metadata_type_fields a composite primary key and a field lookup; assets a partial index
            // on its type.
            // 223 since 0017: upload_profiles gains a primary key, a key index and its one-default partial
            // index, and assets a partial index on the profile. (0016 is net zero — it swapped a unique slug
            // index for a non-unique lookup on the same columns.)
            // 233 since 0019: three primary keys and four more indexes — each engagement table is read from both
            // ends (by asset for the aggregate, by identity for the person's own list) and a composite key can only
            // serve one of those.
            //
            // 226 since 0018: auto_import_mappings gains a primary key, its source/field unique index and the
            // partial resolution index.
            //
            // 247 since 0023: `conversions` gains a primary key, the unique index behind its key, and the partial
            // offer index that the download dialog's one read uses.
            248,
        ),
        (
            "check constraints",
            "SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace WHERE n.nspname='t_acme' AND c.contype='c'",
            // 91 since 0014, which added the `upload_sessions.content_hash` shape check. 90 came from 0012:
            // a superseded term must be deprecated, and cannot supersede itself.
            // 100 since 0022: an attachment's kind is constrained, and two constraints keep the pair coherent —
            // both columns set or neither, and nothing attached to itself.
            //
            // 112 since 0023: a conversion's key shape, its reserved-name exclusion, label and description
            // lengths, media class, both dimensions, format, quality, fit, background and permission shape are
            // each constrained in the column — the constraints *are* the specification for a usable recipe, so
            // the Rust layer reports which one refused rather than restating them.
            //
            // 97 since 0021: the events default partition inherits its parent's `actor_kind` check, for the same
            // reason it inherits the indexes.
            //
            // 96 since 0020: a comment's body length, visibility and status are all constrained in the column, so
            // none of the three can hold a value this module cannot read back.
            //
            // 93 since 0019: `asset_ratings.stars` is range-checked in the column, so a rating outside 1–5 cannot
            // exist even if a future caller skips the Rust check.
            //
            // 92 since 0018: `auto_import_mappings.source` is shape-checked, because a mapping's left-hand side is
            // free text and a malformed one would silently never match.
            112,
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
        // Against the embedded count rather than a literal: a literal has to be edited for every new
        // migration, and a number edited by rote is a number nobody checks.
        assert_eq!(
            applied,
            i64::try_from(migrate::tenant_migration_count()).expect("a plausible migration count"),
            "{schema} should have every tenant migration applied"
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
        i64::try_from(migrate::tenant_migration_count()).expect("a plausible migration count")
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
