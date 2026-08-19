//! The auto-import endpoints (Q.4).
//!
//! `dam_db`'s suite proves the mapping rules and `dam_media`'s proves the extraction. This proves the HTTP
//! contract, and two things that exist only here:
//!
//! - **Every route is Manage, reading included.** No client needs to know a tenant's mappings in order to behave
//!   correctly — a mapping fires on the server during ingest — so there is no reason to widen the read.
//! - **The source list is served.** A screen that made somebody type `exif.artist` from memory would produce
//!   rules that look right in the table and never fire, so the picker is built from the extractor's own names.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::auto_import::{AutoImportState, router};
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
    let app = router(AutoImportState {
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
async fn the_auto_import_endpoints_hold() {
    let f = fixture().await;

    // Two fields to map onto, and one nobody may write to.
    for (key, kind, read_only) in [
        ("photographer", "text", false),
        ("shot_on", "date", false),
        ("ingested_by", "text", true),
    ] {
        sqlx::query(
            "INSERT INTO field_defs (id, key, label, kind, read_only, display_order) \
             VALUES (gen_random_uuid(), $1, $1, $2, $3, 1)",
        )
        .bind(key)
        .bind(kind)
        .bind(read_only)
        .execute(&f.acme)
        .await
        .expect("field");
    }

    reading_needs_manage_not_read(&f).await;
    the_source_list_is_what_the_extractor_produces(&f).await;
    a_mapping_is_created_and_listed(&f).await;
    a_source_the_extractor_cannot_produce_is_422(&f).await;
    a_read_only_field_is_422_and_an_unknown_one_is_404(&f).await;
    the_same_pair_twice_is_409_with_a_reason(&f).await;
    switching_a_mapping_off_returns_the_row_as_stored(&f).await;
    removing_a_mapping_twice_is_404_the_second_time(&f).await;
}

async fn reading_needs_manage_not_read(f: &Fixture) {
    // Unlike upload profiles, which any reader may list: a mapping is a decision about what the tenant's fields
    // mean, and nothing a client does depends on knowing it.
    let (status, _) = call(f, "GET", "/auto-import-mappings", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        f,
        "GET",
        "/auto-import-mappings/sources",
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn the_source_list_is_what_the_extractor_produces(f: &Fixture) {
    let (status, body) = call(f, "GET", "/auto-import-mappings/sources", &f.key, None).await;
    assert_eq!(status, StatusCode::OK);
    let sources: Vec<&str> = body
        .as_array()
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("a string"))
        .collect();
    // From `dam_media` rather than a list written out here, so this cannot pass while the two disagree — which
    // is the whole failure mode the endpoint exists to prevent.
    assert_eq!(sources, dam_media::embedded::sources(), "{body}");
    assert!(sources.contains(&"exif.artist"), "{body}");
}

async fn a_mapping_is_created_and_listed(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/auto-import-mappings",
        &f.key,
        Some(json!({ "source": "exif.artist", "field_key": "photographer" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // The two defaults, both stated in the response rather than left for a client to assume: `overwrite` off is
    // the rule that protects curated values, and `enabled` on is what makes a saved mapping actually apply.
    assert_eq!(body["overwrite"], json!(false), "{body}");
    assert_eq!(body["enabled"], json!(true), "{body}");
    assert_eq!(body["priority"], json!(0), "{body}");

    let (status, body) = call(f, "GET", "/auto-import-mappings", &f.key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body[0]["source"], json!("exif.artist"), "{body}");
}

async fn a_source_the_extractor_cannot_produce_is_422(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/auto-import-mappings",
        &f.key,
        Some(json!({ "source": "EXIF.Artist", "field_key": "photographer" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Named, because the person who typed it is still looking at the form — and the shape is the thing to say.
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("namespace.name")),
        "{body}"
    );
}

async fn a_read_only_field_is_422_and_an_unknown_one_is_404(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/auto-import-mappings",
        &f.key,
        Some(json!({ "source": "exif.software", "field_key": "ingested_by" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("read-only")),
        "{body}"
    );

    // 404 rather than 422: the field picker is built from the same list, so a key that is not there means the
    // field was removed underneath the screen.
    let (status, _) = call(
        f,
        "POST",
        "/auto-import-mappings",
        &f.key,
        Some(json!({ "source": "exif.software", "field_key": "not_a_field" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn the_same_pair_twice_is_409_with_a_reason(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/auto-import-mappings",
        &f.key,
        Some(json!({ "source": "exif.artist", "field_key": "photographer" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("exif.artist")),
        "{body}"
    );
}

async fn switching_a_mapping_off_returns_the_row_as_stored(f: &Fixture) {
    let (_, body) = call(f, "GET", "/auto-import-mappings", &f.key, None).await;
    let id = body[0]["id"].as_str().expect("an id").to_owned();

    let (status, body) = call(
        f,
        "PATCH",
        &format!("/auto-import-mappings/{id}"),
        &f.key,
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], json!(false), "{body}");
    // Read back rather than echoed: `overwrite` was not in the request, and the response says what is stored
    // instead of what the client guessed.
    assert_eq!(body["overwrite"], json!(false), "{body}");

    let (status, body) = call(
        f,
        "PATCH",
        &format!("/auto-import-mappings/{id}"),
        &f.key,
        Some(json!({ "overwrite": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["overwrite"], json!(true), "{body}");
    assert_eq!(
        body["enabled"],
        json!(false),
        "and the switch from the previous request is still off: {body}"
    );

    let (status, _) = call(
        f,
        "PATCH",
        &format!("/auto-import-mappings/{}", Uuid::new_v4()),
        &f.key,
        Some(json!({ "enabled": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn removing_a_mapping_twice_is_404_the_second_time(f: &Fixture) {
    let (_, body) = call(f, "GET", "/auto-import-mappings", &f.key, None).await;
    let id = body[0]["id"].as_str().expect("an id").to_owned();
    let path = format!("/auto-import-mappings/{id}");

    let (status, _) = call(f, "DELETE", &path, &f.key, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(f, "DELETE", &path, &f.key, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "removal is not idempotent by accident"
    );
}
