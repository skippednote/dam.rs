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
    /// Held so the index directory outlives the fixture — the search router needs one even when no test
    /// searches through the index.
    _indexes: tempfile::TempDir,
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
    // The search router is mounted alongside, because the interesting question about rail *configuration* is
    // whether the rail obeys it — a stored order nothing reads is a settings screen that lies.
    let index_dir = tempfile::tempdir().expect("index dir");
    let app = router(SchemaState {
        global: global.clone(),
    })
    .merge(dam_api::search::router(dam_api::search::SearchState {
        global: global.clone(),
        indexes: std::sync::Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
            index_dir.path(),
        ))),
    }));
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    Fixture {
        _pg: pg,
        _indexes: index_dir,
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
    the_rail_is_ordered_and_switched_off_by_the_tenant(&f).await;
}

#[tokio::test]
async fn the_metadata_type_contract_holds() {
    let f = fixture().await;

    // A vocabulary to draw types from.
    for key in ["description", "print_dpi", "duration_note"] {
        let (status, body) = call(
            &f,
            "POST",
            "/schema/fields",
            &f.key,
            Some(json!({ "key": key, "label": key, "kind": "text" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    types_need_manage_to_edit_and_read_to_list(&f).await;
    a_type_is_created_with_its_fields(&f).await;
    a_type_reports_how_many_assets_use_it(&f).await;
    a_type_naming_an_unknown_field_is_refused(&f).await;
    the_default_moves_rather_than_duplicating(&f).await;
    an_assets_type_can_be_set_and_cleared(&f).await;
    removing_a_type_does_not_strand_its_assets(&f).await;
}

async fn types_need_manage_to_edit_and_read_to_list(f: &Fixture) {
    // Same split as the field routes, for the same reason: a type decides which form an asset gets, so a
    // read-only integration key must be able to render one and never reshape it.
    let (status, body) = call(f, "GET", "/schema/types", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    for (method, path, payload) in [
        (
            "POST",
            "/schema/types",
            Some(json!({ "key": "sneak", "label": "Sneak" })),
        ),
        (
            "PATCH",
            "/schema/types/00000000-0000-4000-8000-000000000000",
            Some(json!({ "label": "x" })),
        ),
        (
            "DELETE",
            "/schema/types/00000000-0000-4000-8000-000000000000",
            None,
        ),
    ] {
        let (status, _) = call(f, method, path, &f.read_only_key, payload).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

async fn a_type_is_created_with_its_fields(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/schema/types",
        &f.key,
        Some(json!({
            "key": "image",
            "label": "Image",
            "applies_to": ["image"],
            "is_default": true,
            // Order is the type's own, and it is not the tenant's field order — which is the point of
            // having a per-type list at all.
            "field_keys": ["print_dpi", "description"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["key"], "image");
    assert_eq!(body["is_default"], true);
    assert_eq!(body["field_keys"][0], "print_dpi");
    assert_eq!(body["field_keys"][1], "description");

    // Amending the field list replaces it wholesale, in the order given: a type's fields are a list, and
    // "add one" against a stale copy would silently drop whatever the client had not seen.
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/schema/types/{}", body["id"].as_str().expect("id")),
        &f.key,
        Some(json!({ "field_keys": ["description", "print_dpi", "duration_note"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["field_keys"].as_array().expect("array").len(), 3);
    assert_eq!(body["field_keys"][0], "description");
}

async fn a_type_reports_how_many_assets_use_it(f: &Fixture) {
    // The number that decides whether removing a type is safe, on the row — same posture as a field's
    // usage count, and for the same reason: an administrator should not have to go and ask.
    let (_, listed) = call(f, "GET", "/schema/types", &f.key, None).await;
    let image = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == "image")
        .expect("image type");
    assert_eq!(image["assets"], 0, "nothing uses it yet");
}

async fn a_type_naming_an_unknown_field_is_refused(f: &Fixture) {
    let (status, body) = call(
        f,
        "POST",
        "/schema/types",
        &f.key,
        Some(json!({ "key": "bogus", "label": "Bogus", "field_keys": ["nonesuch"] })),
    )
    .await;
    // 422: the request names something that does not exist, which is the request's fault.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("nonesuch"),
        "{body}"
    );

    // A duplicate key is a conflict on an existing thing, not a malformed request.
    let (status, body) = call(
        f,
        "POST",
        "/schema/types",
        &f.key,
        Some(json!({ "key": "image", "label": "Image again" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

async fn the_default_moves_rather_than_duplicating(f: &Fixture) {
    let (status, video) = call(
        f,
        "POST",
        "/schema/types",
        &f.key,
        Some(json!({
            "key": "video",
            "label": "Video",
            "applies_to": ["video"],
            "field_keys": ["duration_note"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{video}");

    let (status, _) = call(
        f,
        "PATCH",
        &format!("/schema/types/{}", video["id"].as_str().expect("id")),
        &f.key,
        Some(json!({ "is_default": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Exactly one default, and it moved — two rows claiming the fallback would make an asset's field list
    // depend on row order.
    let (_, listed) = call(f, "GET", "/schema/types", &f.key, None).await;
    let defaults: Vec<&str> = listed
        .as_array()
        .expect("array")
        .iter()
        .filter(|row| row["is_default"] == true)
        .filter_map(|row| row["key"].as_str())
        .collect();
    assert_eq!(defaults, ["video"]);
}

async fn an_assets_type_can_be_set_and_cleared(f: &Fixture) {
    let asset_id = asset_with(&f.acme, "typed", json!({})).await;
    let (_, listed) = call(f, "GET", "/schema/types", &f.key, None).await;
    let image_id = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == "image")
        .and_then(|row| row["id"].as_str())
        .expect("image id")
        .to_owned();

    let (status, body) = call(
        f,
        "PUT",
        &format!("/assets/{asset_id}/metadata-type"),
        &f.key,
        Some(json!({ "metadata_type_id": image_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["metadata_type_id"], image_id);
    // The response carries the form the asset now has, because that is what the client has to redraw — a
    // bare 204 would make the caller guess or re-fetch.
    assert_eq!(body["field_keys"].as_array().expect("array").len(), 3);

    // Cleared with an explicit null, which is different from omitting the member: one says "fall back to
    // the default", the other would say nothing at all.
    let (status, body) = call(
        f,
        "PUT",
        &format!("/assets/{asset_id}/metadata-type"),
        &f.key,
        Some(json!({ "metadata_type_id": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["metadata_type_id"].is_null());
    // And the fallback is the tenant's default, not an empty form.
    assert_eq!(
        body["field_keys"][0], "duration_note",
        "the video default's list"
    );

    // A type that does not exist is a 422 rather than a silent clear: "set this to a type I invented" is a
    // mistake, and treating it as a clear would hide the mistake behind a plausible outcome.
    let (status, _) = call(
        f,
        "PUT",
        &format!("/assets/{asset_id}/metadata-type"),
        &f.key,
        Some(json!({ "metadata_type_id": "00000000-0000-4000-8000-00000000dead" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Read-only cannot set it.
    let (status, _) = call(
        f,
        "PUT",
        &format!("/assets/{asset_id}/metadata-type"),
        &f.read_only_key,
        Some(json!({ "metadata_type_id": image_id })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn removing_a_type_does_not_strand_its_assets(f: &Fixture) {
    let (_, listed) = call(f, "GET", "/schema/types", &f.key, None).await;
    let image_id = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == "image")
        .and_then(|row| row["id"].as_str())
        .expect("image id")
        .to_owned();

    let asset_id = asset_with(&f.acme, "about-to-be-orphaned", json!({})).await;
    let (status, _) = call(
        f,
        "PUT",
        &format!("/assets/{asset_id}/metadata-type"),
        &f.key,
        Some(json!({ "metadata_type_id": image_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The removal is not blocked by the assets referencing it: that is an administrative decision about the
    // schema, and blocking it would make a type unremovable once anything used it.
    let (status, _) = call(
        f,
        "DELETE",
        &format!("/schema/types/{image_id}"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(
        f,
        "GET",
        &format!("/assets/{asset_id}/metadata-type"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["metadata_type_id"].is_null(),
        "the dangling reference was cleared"
    );
    assert!(
        !body["field_keys"].as_array().expect("array").is_empty(),
        "and the asset still has a form, via the fallback: {body}"
    );

    let (status, _) = call(
        f,
        "DELETE",
        &format!("/schema/types/{image_id}"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "removing it twice is a 404");
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

// ─── the refine-search rail (Q.19) ──────────────────────────────────────────

/// The rail is the tenant's to arrange, including the parts that are not fields.
///
/// A library with thirty facetable fields has a rail nobody scrolls to the bottom of, and until this the order
/// was whatever the schema implied. The built-ins are in it too, which is what makes "we do not use ratings"
/// expressible without asking us.
async fn the_rail_is_ordered_and_switched_off_by_the_tenant(f: &Fixture) {
    // Two facetable fields, so there is something to reorder.
    for key in ["brandish", "campaignish"] {
        let (status, body) = call(
            f,
            "POST",
            "/schema/fields",
            &f.key,
            Some(json!({ "key": key, "label": key, "kind": "text", "facetable": true })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // A vocabulary, because a rail entry is not only a field: the whole point of the entry naming its *kind*
    // is that a taxonomy and a field can share a name and still be two entries.
    let vocabulary: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'materials', 'Materials', 'vocabulary') RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("vocabulary");

    let (status, candidates) = call(f, "GET", "/schema/facets", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{candidates}");
    let entries: Vec<&str> = candidates
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|entry| entry["entry"].as_str())
        .collect();
    assert!(entries.contains(&"field:brandish"), "{candidates}");
    assert!(
        entries.contains(&format!("taxonomy:{vocabulary}").as_str()),
        "a vocabulary is an entry too: {candidates}"
    );
    // The four built-ins are entries like any other, which is the point.
    for builtin in [
        "builtin:status",
        "builtin:orientation",
        "builtin:stars",
        "builtin:has",
    ] {
        assert!(
            entries.contains(&builtin),
            "{builtin} is missing: {candidates}"
        );
    }

    // Configure: campaignish first, brandish second, and no ratings at all.
    let (status, body) = call(
        f,
        "PUT",
        "/schema/facets",
        &f.key,
        Some(json!({ "enabled": ["field:campaignish", "field:brandish", "builtin:status"] })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (_, after) = call(f, "GET", "/schema/facets", &f.key, None).await;
    let enabled: Vec<&str> = after
        .as_array()
        .expect("array")
        .iter()
        .filter(|entry| entry["is_enabled"] == json!(true))
        .filter_map(|entry| entry["entry"].as_str())
        .collect();
    assert_eq!(
        enabled,
        vec!["field:campaignish", "field:brandish", "builtin:status"],
        "the tenant's order, and only what they enabled: {after}"
    );
    // The disabled ones are still listed — you cannot re-enable what you cannot see.
    let listed: Vec<&str> = after
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|entry| entry["entry"].as_str())
        .collect();
    assert!(listed.contains(&"builtin:stars"), "{after}");

    // And the rail itself is in that order, with the disabled facet absent rather than empty.
    let (status, rail) = call(f, "GET", "/search/facets", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{rail}");
    let keys: Vec<&str> = rail
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|facet| facet["key"].as_str())
        .collect();
    assert_eq!(
        keys,
        vec!["campaignish", "brandish", "status"],
        "the rail must be the configuration: {rail}"
    );

    // An entry the rail cannot show is refused rather than stored: a typo'd key would be a row matching
    // nothing, silently holding the position an administrator meant for something real.
    let (status, body) = call(
        f,
        "PUT",
        "/schema/facets",
        &f.key,
        Some(json!({ "enabled": ["field:no_such_field"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("no_such_field"), "{body}");
}
