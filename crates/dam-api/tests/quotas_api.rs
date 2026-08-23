//! Where a tenant stands against its caps (G19).
//!
//! `dam_db::quotas` proves the arithmetic, the level/flow distinction and the stamps. What lives only here:
//!
//! - **Reading a cap needs `Manage`.** A cap is a commercial fact about the account, and somebody who can upload
//!   should not learn how close the library is to a limit they cannot change.
//! - **A cap cannot be *set* through the API at all.** A tenant raising its own limit is not a feature; that is
//!   a `damctl` command.
//! - **Only configured caps appear.** An absent one is not a cap of zero, and listing it with a limit of nothing
//!   would read as exhausted — contradicting the enforcement, which allows it.
//! - **Every row says whether its number is a level or a flow**, because "1.2 TB" means what exists for one and
//!   what happened this month for the other.
//! - **The period is a calendar month**, echoed so a screen can label it rather than guessing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::quotas::{QuotaState, router};
use dam_db::{auth, migrate, quotas, testing::PostgresHarness};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    app: axum::Router,
    tenant_id: Uuid,
    /// Holds `Manage`.
    admin_key: String,
    /// Holds `asset:read` only.
    reader_key: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");

    let ada = identity(&global, "ada@example.com").await;
    member(&global, tenant_id, ada, "{}", true).await;
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, all_asset_groups) \
         VALUES (gen_random_uuid(), 'reader', 'Reader', '{asset:read}', true)",
    )
    .execute(&acme)
    .await
    .expect("role");
    let bob = identity(&global, "bob@example.com").await;
    member(&global, tenant_id, bob, "{reader}", false).await;

    Fixture {
        _pg: pg,
        app: router(QuotaState {
            global: global.clone(),
        }),
        admin_key: issue(&global, tenant_id, ada).await,
        reader_key: issue(&global, tenant_id, bob).await,
        global,
        tenant_id,
    }
}

async fn identity(pool: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("identity")
}

async fn member(pool: &PgPool, tenant: Uuid, who: Uuid, roles: &str, admin: bool) {
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3::text[], $4)",
    )
    .bind(tenant)
    .bind(who)
    .bind(roles)
    .bind(admin)
    .execute(pool)
    .await
    .expect("membership");
}

async fn issue(pool: &PgPool, tenant: Uuid, who: Uuid) -> String {
    let key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant)
    .bind(who)
    .bind(key.prefix())
    .bind(key.hash())
    .execute(pool)
    .await
    .expect("key");
    key.into_plaintext()
}

async fn get(f: &Fixture, method: &str, path: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = f
        .app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn cap(f: &Fixture, key: &str, limit: i64, hard: bool) {
    let mut conn = f.global.acquire().await.expect("connection");
    quotas::set(
        &mut conn,
        f.tenant_id,
        key,
        &quotas::Quota {
            limit_value: limit,
            warn_at_fraction: 0.8,
            enforcement: if hard {
                quotas::Enforcement::Hard
            } else {
                quotas::Enforcement::Soft
            },
        },
    )
    .await
    .expect("cap");
}

#[tokio::test]
async fn the_quota_contract_holds() {
    let f = fixture().await;
    let period = quotas::month_start(chrono::Utc::now());

    // Nothing configured is an empty list, not a list of zeroes. A screen showing every possible key at zero
    // would read as a tenant who had exhausted everything.
    let (status, body) = get(&f, "GET", "/quotas", Some(&f.admin_key)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["period_start"], period.to_string());
    assert!(
        body["quotas"].as_array().expect("quotas").is_empty(),
        "{body}"
    );

    // A level and a flow, each past a different line.
    cap(&f, quotas::STORAGE_BYTES, 1_000, false).await;
    cap(&f, quotas::AI_SPEND, 500, true).await;
    let mut conn = f.global.acquire().await.expect("connection");
    quotas::observe(&mut conn, f.tenant_id, quotas::STORAGE_BYTES, period, 900)
        .await
        .expect("observe");
    quotas::charge(
        &mut conn,
        f.tenant_id,
        quotas::AI_SPEND,
        period,
        600 * quotas::MICRO,
    )
    .await
    .expect("charge");

    let (status, body) = get(&f, "GET", "/quotas", Some(&f.admin_key)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["quotas"].as_array().expect("quotas");
    assert_eq!(rows.len(), 2, "{body}");

    let spend = rows
        .iter()
        .find(|row| row["quota_key"] == quotas::AI_SPEND)
        .expect("ai spend");
    assert_eq!(spend["used"], 600);
    assert_eq!(spend["limit_value"], 500);
    assert_eq!(spend["standing"], "refused");
    assert_eq!(spend["enforcement"], "hard");
    // A flow: cents spent *this month*, not cents that exist.
    assert_eq!(spend["is_level"], false);
    assert!(spend["exceeded_at"].is_string(), "{spend}");
    assert!(
        spend["warned_at"].is_string(),
        "crossing hard implies crossing warned"
    );

    let storage = rows
        .iter()
        .find(|row| row["quota_key"] == quotas::STORAGE_BYTES)
        .expect("storage");
    assert_eq!(storage["used"], 900);
    assert_eq!(storage["standing"], "warned");
    assert_eq!(storage["enforcement"], "soft");
    // A level: bytes that exist. A screen has to say which, or a month's egress reads as a library's size.
    assert_eq!(storage["is_level"], true);
    // The line itself, so a bar can be drawn at the real fraction rather than an invented 80%.
    assert_eq!(storage["warn_at_fraction"], 0.8);
    assert!(storage["warned_at"].is_string());
    assert!(
        storage["exceeded_at"].is_null(),
        "a soft cap warns rather than exceeding"
    );

    // Reading a cap is administration: somebody who can upload should not learn how close the library is to a
    // limit they cannot change.
    let (status, _) = get(&f, "GET", "/quotas", Some(&f.reader_key)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get(&f, "GET", "/quotas", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And there is no way to set one. A tenant raising its own limit is not a feature — that is `damctl`.
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let (status, _) = get(&f, method, "/quotas", Some(&f.admin_key)).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} /quotas must not exist",
        );
    }

    // Dropping back under a cap changes the standing and keeps the stamps: the tenant was over, and that stays
    // answerable.
    quotas::observe(&mut conn, f.tenant_id, quotas::STORAGE_BYTES, period, 10)
        .await
        .expect("observe");
    let (_, body) = get(&f, "GET", "/quotas", Some(&f.admin_key)).await;
    let storage = body["quotas"]
        .as_array()
        .expect("quotas")
        .iter()
        .find(|row| row["quota_key"] == quotas::STORAGE_BYTES)
        .expect("storage");
    assert_eq!(storage["standing"], "allowed");
    assert_eq!(storage["used"], 10);
    assert!(
        storage["warned_at"].is_string(),
        "the crossing is still recorded"
    );
}
