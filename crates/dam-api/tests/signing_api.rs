//! The C2PA signing-identity endpoints (G10·3b management surface).
//!
//! `dam_db::signing_identities` proves the storage; `dam_media::provenance` proves the cert/key validation.
//! What only exists here is the HTTP contract:
//!
//! - **The private key goes in and never comes out.** The list view returns the public certificate and never
//!   the sealed key, and this suite reads the raw body to prove the sealed text is absent.
//! - **An unusable certificate is refused on the way in**, not at the thousandth derivative: a chain that
//!   cannot build a signer, and an empty field, are both 422.
//! - **Configuration is not readable by everybody**: a read-only key cannot list identities.
//! - **Standing down when there is nothing** is a 404, not a silent success.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::signing::{SigningState, router};
use dam_core::Secret;
use dam_core::sealed::SealingKeyring;
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

/// A distinctive sealed-key placeholder: the list view must never echo it.
const SEALED_PLACEHOLDER: &str = "SEALED-KEY-MUST-NOT-LEAK-0xdeadbeef";

struct Fixture {
    _pg: PostgresHarness,
    acme: PgPool,
    app: axum::Router,
    /// A tenant admin: Manage.
    key: String,
    /// `asset:read` only.
    read_only_key: String,
}

fn keyring() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a test sealing passphrase".to_owned()))
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");
    let identity: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), 'ada@example.com', 'Ada') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("identity");
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(identity)
    .execute(&global)
    .await
    .expect("membership");

    let key = issue(&global, tenant_id, Some(identity), &[]).await;
    let read_only_key = issue(&global, tenant_id, Some(identity), &["asset:read"]).await;

    let app = router(SigningState {
        global: global.clone(),
        keyring: keyring(),
    });

    Fixture {
        _pg: pg,
        acme,
        app,
        key,
        read_only_key,
    }
}

async fn issue(global: &PgPool, tenant: Uuid, identity: Option<Uuid>, scopes: &[&str]) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, $5)",
    )
    .bind(tenant)
    .bind(identity)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .bind(
        scopes
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<String>>(),
    )
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {key}"));
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(match &body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        })
        .expect("request");
    let response = f.app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn json_call(
    f: &Fixture,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, text) = call(f, method, path, key, body).await;
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Inserts an already-sealed identity straight into the tenant schema, bypassing the API — so the list and
/// stand-down contracts can be exercised without a real certificate to install.
async fn seed_identity(f: &Fixture, label: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO signing_identities \
         (id, label, cert_pem, sealed_key, sealing_key_id, algorithm, timestamp_authority) \
         VALUES ($1, $2, $3, $4, 'k1', 'es256', NULL)",
    )
    .bind(id)
    .bind(label)
    .bind("-----BEGIN CERTIFICATE-----\npublic-and-fine\n-----END CERTIFICATE-----")
    .bind(SEALED_PLACEHOLDER)
    .execute(&f.acme)
    .await
    .expect("seed identity");
    id
}

#[tokio::test]
async fn configuration_is_not_readable_by_everybody() {
    let f = fixture().await;
    let (status, _) = call(&f, "GET", "/signing-identities", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_empty_tenant_lists_nothing_and_has_nothing_to_stand_down() {
    let f = fixture().await;
    let (status, list) = json_call(&f, "GET", "/signing-identities", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().map(Vec::len), Some(0));

    let (status, _) = call(&f, "DELETE", "/signing-identities/active", &f.key, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_certificate_that_cannot_sign_is_refused_on_the_way_in() {
    let f = fixture().await;

    // Not a certificate or a key: refused before anything is stored.
    let (status, body) = json_call(
        &f,
        "POST",
        "/signing-identities",
        &f.key,
        Some(json!({
            "label": "garbage",
            "cert_pem": "-----BEGIN CERTIFICATE-----\nnope\n-----END CERTIFICATE-----",
            "private_key_pem": "-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----",
            "algorithm": "es256",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Empty fields: the same 422, said plainly.
    let (status, _) = json_call(
        &f,
        "POST",
        "/signing-identities",
        &f.key,
        Some(json!({
            "label": "empty",
            "cert_pem": "",
            "private_key_pem": "",
            "algorithm": "es256",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Nothing was stored either way.
    let (_, list) = json_call(&f, "GET", "/signing-identities", &f.key, None).await;
    assert_eq!(list.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn an_installed_identity_lists_without_its_key_and_stands_down() {
    let f = fixture().await;
    seed_identity(&f, "acme-2026").await;

    let (status, text) = call(&f, "GET", "/signing-identities", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        !text.contains(SEALED_PLACEHOLDER),
        "the sealed key must never appear in a response: {text}"
    );
    let list: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["label"], "acme-2026");
    assert_eq!(list[0]["algorithm"], "es256");
    assert_eq!(list[0]["is_active"], true);
    assert!(
        list[0]["cert_pem"]
            .as_str()
            .unwrap()
            .contains("BEGIN CERTIFICATE")
    );
    assert!(list[0].get("sealed_key").is_none(), "no key field at all");

    // Stand it down: 204, then the row is no longer active, and a second stand-down is a 404.
    let (status, _) = call(&f, "DELETE", "/signing-identities/active", &f.key, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list) = json_call(&f, "GET", "/signing-identities", &f.key, None).await;
    assert_eq!(list[0]["is_active"], false, "kept, but no longer signing");

    let (status, _) = call(&f, "DELETE", "/signing-identities/active", &f.key, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
