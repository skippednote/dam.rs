//! `TenantConn`: the only way to reach a tenant's schema.
//!
//! The invariant is structural, not procedural. `SET LOCAL` outside a transaction is
//! a silent no-op — it warns to a log nobody reads and leaves the path unchanged, so
//! queries land in whatever schema the pooled connection last had. That is a
//! cross-tenant data leak with no error attached to it.
//!
//! So `TenantConn` cannot be constructed except by beginning a transaction. There is
//! no `from_pool`, no `set_schema`, no escape hatch. The compliance-gate suite proved
//! how easy the mistake is (DECISIONS.md, 0.4/0.5/0.6) — this makes it unavailable.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::TenantSlug;
use dam_db::{TenantConn, migrate, testing::PostgresHarness};
use sqlx::PgPool;

async fn two_tenant_db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_one").await.expect("t_one");
    migrate::tenant(&url, "t_two").await.expect("t_two");
    let pool = pg.pool().clone();
    (pg, pool)
}

fn slug(s: &str) -> TenantSlug {
    TenantSlug::new(s).expect("valid slug")
}

#[tokio::test]
async fn queries_inside_resolve_the_tenant_schema_unqualified() {
    let (_pg, pool) = two_tenant_db().await;
    let mut tc = TenantConn::begin(&pool, &slug("one")).await.expect("begin");
    let path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
        .fetch_one(tc.executor())
        .await
        .expect("read search_path");
    // Postgres normalises the quoting away when the identifier does not need it,
    // so compare the first element rather than the literal string we sent.
    let first = path
        .split(',')
        .next()
        .map(|p| p.trim().trim_matches('"'))
        .unwrap_or_default();
    assert_eq!(first, "t_one", "full path was {path}");

    // Unqualified DML must land in the tenant schema.
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'brand', 'Brand', 'text')",
    )
    .execute(tc.executor())
    .await
    .expect("insert unqualified");
    tc.commit().await.expect("commit");

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM t_one.field_defs")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 1);
}

#[tokio::test]
async fn the_search_path_does_not_leak_back_onto_the_pooled_connection() {
    // The whole reason for the transaction requirement. `SET LOCAL` is scoped to the
    // transaction, so the connection returns to the pool clean.
    let (_pg, pool) = two_tenant_db().await;
    // Single connection so the same one is certain to be reused.
    let single = pool.clone();
    {
        let tc = TenantConn::begin(&single, &slug("one"))
            .await
            .expect("begin");
        tc.commit().await.expect("commit");
    }
    let leaked: Option<String> = sqlx::query_scalar("SELECT to_regclass('field_defs')::text")
        .fetch_one(&single)
        .await
        .expect("query after commit");
    assert!(
        leaked.is_none(),
        "search_path leaked back onto the pool: field_defs resolved to {leaked:?}"
    );
}

#[tokio::test]
async fn the_search_path_does_not_leak_after_a_rollback_either() {
    let (_pg, pool) = two_tenant_db().await;
    {
        let mut tc = TenantConn::begin(&pool, &slug("one")).await.expect("begin");
        sqlx::query("INSERT INTO field_defs (id, key, label, kind) VALUES (gen_random_uuid(), 'x', 'X', 'text')")
            .execute(tc.executor())
            .await
            .expect("insert");
        tc.rollback().await.expect("rollback");
    }
    let leaked: Option<String> = sqlx::query_scalar("SELECT to_regclass('field_defs')::text")
        .fetch_one(&pool)
        .await
        .expect("query");
    assert!(leaked.is_none(), "leaked after rollback: {leaked:?}");

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM t_one.field_defs")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 0, "rollback must undo the insert");
}

#[tokio::test]
async fn dropping_without_commit_rolls_back() {
    // A handler that returns early on an error must not half-apply its writes.
    let (_pg, pool) = two_tenant_db().await;
    {
        let mut tc = TenantConn::begin(&pool, &slug("one")).await.expect("begin");
        sqlx::query("INSERT INTO field_defs (id, key, label, kind) VALUES (gen_random_uuid(), 'y', 'Y', 'text')")
            .execute(tc.executor())
            .await
            .expect("insert");
        // No commit, no explicit rollback — just dropped.
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM t_one.field_defs")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 0, "an uncommitted TenantConn must not persist writes");
}

#[tokio::test]
async fn one_tenant_cannot_read_anothers_rows_unqualified() {
    let (_pg, pool) = two_tenant_db().await;
    let mut one = TenantConn::begin(&pool, &slug("one"))
        .await
        .expect("begin one");
    sqlx::query("INSERT INTO field_defs (id, key, label, kind) VALUES (gen_random_uuid(), 'secret', 'Secret', 'text')")
        .execute(one.executor())
        .await
        .expect("insert");
    one.commit().await.expect("commit");

    let mut two = TenantConn::begin(&pool, &slug("two"))
        .await
        .expect("begin two");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM field_defs")
        .fetch_one(two.executor())
        .await
        .expect("count in two");
    assert_eq!(n, 0, "t_two must not see t_one's rows");
    two.commit().await.expect("commit");
}

#[tokio::test]
async fn concurrent_tenant_connections_do_not_interfere() {
    // Two tenants writing at once through the same pool. If search_path were
    // connection-sticky rather than transaction-scoped, these would cross over.
    let (_pg, pool) = two_tenant_db().await;
    let p1 = pool.clone();
    let p2 = pool.clone();

    let a = tokio::spawn(async move {
        for i in 0..10 {
            let mut tc = TenantConn::begin(&p1, &slug("one")).await.expect("begin");
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO field_defs (id, key, label, kind) VALUES (gen_random_uuid(), 'a{i}', 'A', 'text')"
            )))
            .execute(tc.executor())
            .await
            .expect("insert a");
            tc.commit().await.expect("commit a");
        }
    });
    let b = tokio::spawn(async move {
        for i in 0..10 {
            let mut tc = TenantConn::begin(&p2, &slug("two")).await.expect("begin");
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO field_defs (id, key, label, kind) VALUES (gen_random_uuid(), 'b{i}', 'B', 'text')"
            )))
            .execute(tc.executor())
            .await
            .expect("insert b");
            tc.commit().await.expect("commit b");
        }
    });
    a.await.expect("task a");
    b.await.expect("task b");

    let in_one: i64 = sqlx::query_scalar("SELECT count(*) FROM t_one.field_defs")
        .fetch_one(&pool)
        .await
        .expect("count one");
    let in_two: i64 = sqlx::query_scalar("SELECT count(*) FROM t_two.field_defs")
        .fetch_one(&pool)
        .await
        .expect("count two");
    assert_eq!((in_one, in_two), (10, 10), "writes crossed tenants");
}

#[tokio::test]
async fn a_slug_for_a_schema_that_does_not_exist_fails_at_begin() {
    // Better to fail opening the transaction than to run every query against
    // `public` and report confusing "relation does not exist" errors one at a time.
    let (_pg, pool) = two_tenant_db().await;
    let err = TenantConn::begin(&pool, &slug("nosuch"))
        .await
        .expect_err("a missing schema must fail at begin");
    let msg = err.to_string().to_lowercase();
    assert!(msg.contains("schema") || msg.contains("t_nosuch"), "{err}");
}

/// The Rust validator and the database CHECK must agree on every input.
///
/// Two layers enforce the slug shape: `TenantSlug::new` and
/// `dam_global.tenants.slug CHECK (slug ~ '^[a-z][a-z0-9_]{1,38}$')`. A comment
/// claiming they match is worth nothing — the length assertion in dam-core was wrong
/// about the very regex it cited (`{1,38}` is a minimum of one *additional*
/// character, so the floor is two, not one). This feeds the same inputs to both and
/// fails if they ever disagree.
#[tokio::test]
async fn the_rust_validator_and_the_database_check_agree() {
    let (_pg, pool) = two_tenant_db().await;

    let cases = [
        "ab",
        "a",
        "acme",
        "acme_corp",
        "x9",
        "Acme",
        "1acme",
        "_acme",
        "acme-corp",
        "acme corp",
        "acme;DROP",
        "",
        &"a".repeat(39),
        &"a".repeat(40),
        "public",
        "extensions",
    ];

    for case in cases {
        let rust_ok = TenantSlug::new(case).is_ok();

        // Ask Postgres directly whether the CHECK's regex accepts it.
        let db_ok: bool = sqlx::query_scalar("SELECT $1 ~ '^[a-z][a-z0-9_]{1,38}$'")
            .bind(case)
            .fetch_one(&pool)
            .await
            .expect("regex check");

        // Reserved names are refused by Rust but pass the regex; that extra
        // restriction is deliberate (a tenant schema shadowing `extensions` would
        // break every qualified type reference in the tenant migrations), so it is
        // the one permitted asymmetry.
        let reserved = matches!(
            case,
            "public"
                | "pg_catalog"
                | "pg_toast"
                | "information_schema"
                | "extensions"
                | "dam_global"
                | "tenant_template"
        );

        if reserved {
            assert!(!rust_ok, "{case:?}: reserved names must be refused by Rust");
        } else {
            assert_eq!(
                rust_ok, db_ok,
                "{case:?}: Rust says {rust_ok}, the database CHECK says {db_ok}"
            );
        }
    }
}
