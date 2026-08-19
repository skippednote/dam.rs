//! The schema-administration endpoints (F.11b·2).
//!
//! `dam_db`'s `schema_admin` suite proves the refusals; this proves the HTTP contract. Two properties it is
//! the only place to prove: that **editing the schema needs Manage while reading it needs only Read** — a
//! read-only integration key must not be able to redefine the tenant's fields — and that the consequences
//! the database computes (the value count, the reindex flag, the newly-incomplete count) actually reach the
//! caller, because an administrator who is not told is an administrator who finds out from a support ticket.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::schema::{SchemaState, router};
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

    let key = provision(&global, "acme", "a@example.com").await;
    let read_only_key = scoped_key(&global, "acme", &["asset:read"]).await;
    let app = router(SchemaState {
        global: global.clone(),
    });
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn asset_with(pool: &PgPool, label: &str, values: Value) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(values)
        .execute(pool)
        .await
        .expect("metadata");
    id
}

#[tokio::test]
async fn the_schema_admin_contract_holds() {
    let f = fixture().await;

    reading_the_schema_needs_only_read(&f).await;
    editing_the_schema_needs_manage(&f).await;
    a_definition_is_created_and_listed(&f).await;
    a_refusal_names_what_to_fix(&f).await;
    an_amendment_reports_its_consequences(&f).await;
    a_locked_kind_is_a_conflict_not_a_bad_request(&f).await;
    a_removal_says_what_goes_dark(&f).await;
    a_reorder_takes_the_whole_list(&f).await;
}

async fn reading_the_schema_needs_only_read(f: &Fixture) {
    let (status, body) = call(f, "GET", "/schema/fields", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_array(), "a list, even when empty: {body}");
}

async fn editing_the_schema_needs_manage(f: &Fixture) {
    // The property this file exists for. An integration key handed to a website build has `asset:read`,
    // and the blast radius of a schema edit is every form, facet and search in the tenant — so the same
    // key must not be able to define, amend, remove or reorder.
    for (method, path, body) in [
        (
            "POST",
            "/schema/fields",
            Some(json!({ "key": "sneaky", "label": "Sneaky", "kind": "text" })),
        ),
        (
            "PATCH",
            "/schema/fields/anything",
            Some(json!({ "label": "Renamed" })),
        ),
        ("DELETE", "/schema/fields/anything", None),
        ("PUT", "/schema/fields/order", Some(json!({ "keys": [] }))),
    ] {
        let (status, _) = call(f, method, path, &f.read_only_key, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} must require manage"
        );
    }

    // And nothing landed.
    let (_, body) = call(f, "GET", "/schema/fields", &f.key, None).await;
    let keys: Vec<&str> = body
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|def| def["key"].as_str())
        .collect();
    assert!(!keys.contains(&"sneaky"), "a refused define must not land");
}

async fn a_definition_is_created_and_listed(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/schema/fields",
        &f.key,
        Some(json!({
            "key": "brand",
            "label": "Brand",
            "kind": "text",
            "facetable": true,
            "search_alias": "bra",
            "validation": { "max_length": 40 }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["key"], "brand");
    assert_eq!(body["facetable"], true);
    // The count is on every response, because it is the number that decides whether an edit is safe.
    assert_eq!(body["assets_with_values"], 0);

    let (_, listed) = call(f, "GET", "/schema/fields", &f.key, None).await;
    let brand = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|def| def["key"] == "brand")
        .expect("brand is listed");
    assert_eq!(brand["search_alias"], "bra");
    assert_eq!(brand["searchable"], true);
}

async fn a_refusal_names_what_to_fix(f: &Fixture) {
    // A duplicate key is a conflict on an existing thing, not a malformed request: the fix is to pick
    // another key, and the status says which of those two situations it is.
    let (status, body) = call(
        f,
        "POST",
        "/schema/fields",
        &f.key,
        Some(json!({ "key": "brand", "label": "Brand", "kind": "text" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("brand"),
        "the refusal names the key: {body}"
    );

    // A key that cannot be spelled in a search query or a JSON path is refused as the request's fault.
    let (status, body) = call(
        f,
        "POST",
        "/schema/fields",
        &f.key,
        Some(json!({ "key": "Brand Name", "label": "Brand Name", "kind": "text" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        !body["reason"].as_str().unwrap_or_default().is_empty(),
        "the refusal says what is wrong with the key: {body}"
    );

    // An unknown kind, same class — and named, because "kind is invalid" leaves an administrator guessing
    // which of fourteen spellings this build wanted.
    let (status, body) = call(
        f,
        "POST",
        "/schema/fields",
        &f.key,
        Some(json!({ "key": "colour", "label": "Colour", "kind": "swatch" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("swatch"),
        "{body}"
    );
}

async fn an_amendment_reports_its_consequences(f: &Fixture) {
    asset_with(&f.acme, "branded", json!({ "brand": "acme" })).await;
    asset_with(&f.acme, "unbranded", json!({})).await;

    let (status, body) = call(
        f,
        "PATCH",
        "/schema/fields/brand",
        &f.key,
        Some(json!({ "label": "Brand name", "facetable": false, "required": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["label"], "Brand name");
    // Facetable changed, so the facet counts in the index are now stale — and the caller is told rather
    // than left to notice that a filter rail stopped matching.
    assert_eq!(body["reindex_required"], true);
    // One asset has no brand, so one asset's next metadata write will now fail. Saying so here is the
    // difference between an informed decision and a support ticket.
    assert_eq!(body["assets_now_incomplete"], 1);
    assert_eq!(body["assets_with_values"], 1);

    // Amending something absent is a 404, not a 422: there is nothing wrong with the request.
    let (status, _) = call(
        f,
        "PATCH",
        "/schema/fields/nonesuch",
        &f.key,
        Some(json!({ "label": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn a_locked_kind_is_a_conflict_not_a_bad_request(f: &Fixture) {
    // The request is perfectly well-formed; it is the *state* that refuses it, which is what 409 means.
    // And the count is in the message, because "you cannot" without "because 40,000 assets" is not a
    // decision an administrator can make.
    let (status, body) = call(
        f,
        "PATCH",
        "/schema/fields/brand",
        &f.key,
        Some(json!({ "kind": "int" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let reason = body["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains('1'),
        "the refusal carries the count: {body}"
    );
}

async fn a_removal_says_what_goes_dark(f: &Fixture) {
    let (status, body) = call(f, "DELETE", "/schema/fields/brand", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["assets_with_values"], 1);
    assert_eq!(body["reindex_required"], true);

    // Gone from the catalogue…
    let (_, listed) = call(f, "GET", "/schema/fields", &f.key, None).await;
    assert!(
        listed
            .as_array()
            .expect("array")
            .iter()
            .all(|def| def["key"] != "brand")
    );

    // …and the values are still in the database, which is what makes this recoverable.
    let kept: i64 =
        sqlx::query_scalar("SELECT count(*) FROM asset_metadata WHERE values ? 'brand'")
            .fetch_one(&f.acme)
            .await
            .expect("count");
    assert_eq!(kept, 1, "a removal must not delete data");

    let (status, _) = call(f, "DELETE", "/schema/fields/brand", &f.key, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "removing it twice is a 404");
}

async fn a_reorder_takes_the_whole_list(f: &Fixture) {
    for (key, label) in [("campaign", "Campaign"), ("stylist", "Stylist")] {
        let (status, _) = call(
            f,
            "POST",
            "/schema/fields",
            &f.key,
            Some(json!({ "key": key, "label": label, "kind": "text" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let (_, listed) = call(f, "GET", "/schema/fields", &f.key, None).await;
    let keys: Vec<String> = listed
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|def| def["key"].as_str().map(str::to_owned))
        .collect();
    let mut reversed = keys.clone();
    reversed.reverse();

    let (status, _) = call(
        f,
        "PUT",
        "/schema/fields/order",
        &f.key,
        Some(json!({ "keys": reversed })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, listed) = call(f, "GET", "/schema/fields", &f.key, None).await;
    let after: Vec<String> = listed
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|def| def["key"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(after, reversed, "the list order is the display order");

    // A partial list is a 422: the client's copy of the schema is stale, and applying it would move
    // fields it never showed anybody.
    let (status, _) = call(
        f,
        "PUT",
        "/schema/fields/order",
        &f.key,
        Some(json!({ "keys": [reversed[0].clone()] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
