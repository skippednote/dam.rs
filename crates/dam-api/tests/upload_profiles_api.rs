//! The upload-profile endpoints (Q.3b).
//!
//! `dam_db`'s suite proves the validation and merge rules. This proves the HTTP contract, and the split that
//! only exists here: **listing profiles needs Read, editing them needs Manage**. Reading is deliberately open,
//! because the uploader has to render the profile picker and the required-field rule before it can upload
//! anything — a client that could not read profiles could not honour them.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::upload_profiles::{UploadProfileState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    acme: PgPool,
    key: String,
    read_only_key: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let key = provision(&global, "acme", "a@example.com").await;
    let read_only_key = scoped_key(&global, "acme", &["asset:read"]).await;
    let app = router(UploadProfileState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        app,
        acme,
        key,
        read_only_key,
    }
}

async fn provision(global: &PgPool, slug: &str, email: &str) -> String {
    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), $1, 't_' || $1, $1, $1 || '/', 'active') RETURNING id",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant");
    let identity: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(global)
    .await
    .expect("identity");
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(identity)
    .execute(global)
    .await
    .expect("membership");
    issue(global, tenant_id, identity, &[]).await
}

async fn scoped_key(global: &PgPool, slug: &str, scopes: &[&str]) -> String {
    let (tenant_id, identity_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, m.identity_id FROM dam_global.tenants t \
         JOIN dam_global.tenant_members m ON m.tenant_id = t.id WHERE t.slug = $1",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant and member");
    issue(global, tenant_id, identity_id, scopes).await
}

async fn issue(global: &PgPool, tenant: Uuid, identity: Uuid, scopes: &[&str]) -> String {
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
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json");
    let request = match &body {
        Some(json) => request.body(Body::from(json.to_string())).expect("request"),
        None => request.body(Body::empty()).expect("request"),
    };
    let response = f.app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_upload_profile_http_contract_holds() {
    let f = fixture().await;

    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, display_order) \
         VALUES (gen_random_uuid(), 'credit', 'Credit', 'text', 1)",
    )
    .execute(&f.acme)
    .await
    .expect("field");

    listing_needs_read_and_editing_needs_manage(&f).await;
    a_profile_is_created_and_listed(&f).await;
    invalid_defaults_are_refused_with_the_field_named(&f).await;
    the_fallback_moves_when_another_claims_it(&f).await;
    a_profile_is_removed_without_taking_its_assets(&f).await;
}

async fn listing_needs_read_and_editing_needs_manage(f: &Fixture) {
    // Read, deliberately: the uploader must render the picker and the required-field rule before it can
    // upload anything, so a client that could not list profiles could not honour them.
    let (status, body) = call(f, "GET", "/upload-profiles", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.is_array());

    for (method, path, payload) in [
        (
            "POST",
            "/upload-profiles".to_owned(),
            Some(json!({ "key": "sneak", "label": "Sneak" })),
        ),
        (
            "PATCH",
            format!("/upload-profiles/{}", Uuid::new_v4()),
            Some(json!({ "label": "x" })),
        ),
        (
            "DELETE",
            format!("/upload-profiles/{}", Uuid::new_v4()),
            None,
        ),
    ] {
        let (status, _) = call(f, method, &path, &f.read_only_key, payload).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

async fn a_profile_is_created_and_listed(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/upload-profiles",
        &f.key,
        Some(json!({
            "key": "press",
            "label": "Press delivery",
            "defaults": { "credit": "Acme Press Office" },
            "require_complete": true,
            "ai_tags_enabled": false,
            "is_default": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["key"], "press");
    assert_eq!(body["require_complete"], true);
    // Off, and it has to survive the round trip: enrichment reads this to decide whether an asset from this
    // intake may be machine-tagged at all.
    assert_eq!(body["ai_tags_enabled"], false);
    assert_eq!(body["defaults"]["credit"], "Acme Press Office");

    let (_, listed) = call(f, "GET", "/upload-profiles", &f.key, None).await;
    let press = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == "press")
        .expect("press is listed");
    assert_eq!(press["is_default"], true);

    // Omitting `ai_tags_enabled` means on. Asserted because the *documented* default is the surprising half:
    // a profile that silently disabled enrichment would be a bad thing to inherit by leaving a field out.
    let (status, minimal) = call(
        f,
        "POST",
        "/upload-profiles",
        &f.key,
        Some(json!({ "key": "minimal", "label": "Minimal" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{minimal}");
    assert_eq!(
        minimal["ai_tags_enabled"], true,
        "tagging is on unless a profile turns it off: {minimal}"
    );
    assert_eq!(
        minimal["require_complete"], false,
        "and the strict rule is opt-in"
    );

    // A duplicate key is a conflict on something that exists, not a malformed request.
    let (status, _) = call(
        f,
        "POST",
        "/upload-profiles",
        &f.key,
        Some(json!({ "key": "press", "label": "Again" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

async fn invalid_defaults_are_refused_with_the_field_named(f: &Fixture) {
    // 422 with the field named, so the form can put the error where the value was typed. A profile whose
    // defaults were accepted and then failed at upload time would break every intake from that source, and
    // the person who could fix it would never see why.
    let (status, body) = call(
        f,
        "POST",
        "/upload-profiles",
        &f.key,
        Some(json!({
            "key": "typo",
            "label": "Typo",
            "defaults": { "creditt": "Acme" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("creditt"),
        "the refusal names the field: {body}"
    );
}

async fn the_fallback_moves_when_another_claims_it(f: &Fixture) {
    let (status, second) = call(
        f,
        "POST",
        "/upload-profiles",
        &f.key,
        Some(json!({ "key": "studio", "label": "Studio", "is_default": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");

    let (_, listed) = call(f, "GET", "/upload-profiles", &f.key, None).await;
    let defaults: Vec<&str> = listed
        .as_array()
        .expect("array")
        .iter()
        .filter(|row| row["is_default"] == true)
        .filter_map(|row| row["key"].as_str())
        .collect();
    assert_eq!(defaults, ["studio"], "exactly one fallback, and it moved");

    // Amending can also claim it, and the same rule holds.
    let press_id = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == "press")
        .and_then(|row| row["id"].as_str())
        .expect("press id")
        .to_owned();
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/upload-profiles/{press_id}"),
        &f.key,
        Some(json!({ "is_default": true, "label": "Press deliveries" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["label"], "Press deliveries");
    assert_eq!(body["is_default"], true);

    let (_, listed) = call(f, "GET", "/upload-profiles", &f.key, None).await;
    let defaults: Vec<&str> = listed
        .as_array()
        .expect("array")
        .iter()
        .filter(|row| row["is_default"] == true)
        .filter_map(|row| row["key"].as_str())
        .collect();
    assert_eq!(defaults, ["press"]);

    // Amending with defaults that do not validate is refused too, not only on create. Otherwise a profile
    // could be edited into a state that breaks every upload from that source, with the person who could fix it
    // never seeing why.
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/upload-profiles/{press_id}"),
        &f.key,
        Some(json!({ "defaults": { "creditt": "Acme" } })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("creditt"),
        "{body}"
    );

    // And the refused edit changed nothing: the label from the accepted amendment above still stands.
    let (_, listed) = call(f, "GET", "/upload-profiles", &f.key, None).await;
    let press = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == "press")
        .expect("press");
    assert_eq!(press["label"], "Press deliveries");
    assert_eq!(press["defaults"]["credit"], "Acme Press Office");

    // Amending something absent is a 404: nothing is wrong with the request.
    let (status, _) = call(
        f,
        "PATCH",
        &format!("/upload-profiles/{}", Uuid::new_v4()),
        &f.key,
        Some(json!({ "label": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn a_profile_is_removed_without_taking_its_assets(f: &Fixture) {
    let (status, created) = call(
        f,
        "POST",
        "/upload-profiles",
        &f.key,
        Some(json!({ "key": "temporary", "label": "Temporary" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().expect("id").to_owned();

    let asset = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, upload_profile_id) \
         VALUES ($1, $2, 'arrived.jpg', 'image/jpeg', 10, $1, $3::uuid)",
    )
    .bind(asset)
    .bind(blake3::hash(b"arrived").to_hex().to_string())
    .bind(&id)
    .execute(&f.acme)
    .await
    .expect("asset");

    let (status, _) = call(f, "DELETE", &format!("/upload-profiles/{id}"), &f.key, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The asset survives with its reference cleared: removing a profile is a decision about *future* intakes,
    // and it must neither be blocked by nor destroy what already arrived under it.
    let still: Option<Uuid> =
        sqlx::query_scalar("SELECT upload_profile_id FROM assets WHERE id = $1")
            .bind(asset)
            .fetch_one(&f.acme)
            .await
            .expect("row");
    assert!(still.is_none());

    let (status, _) = call(f, "DELETE", &format!("/upload-profiles/{id}"), &f.key, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "removing it twice is a 404");
}
