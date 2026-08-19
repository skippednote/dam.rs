//! The conversion endpoints (Q.11b).
//!
//! `dam_db`'s suite proves the model — the cache key, the withdrawal rule, the constraints. What only exists here
//! is the HTTP contract, and four things that are decisions about the *interface*:
//!
//! - **Administration is Manage; asking what an asset can be had as is Download.** Not Read: what formats exist
//!   for an asset is a question about taking a copy of it, so somebody who may only look never sees the list.
//! - **The asset gate is first.** An asset outside the caller's scope is 404 before any conversion is considered,
//!   because the reverse order answers "may I have this asset" through the shape of "which formats exist".
//! - **A format the caller may not use is absent from the offer, and named on a direct request.** Deliberately
//!   unlike the asset rule, where hidden and absent collapse: a conversion is tenant configuration and discloses
//!   nothing about anybody's library, so telling somebody which permission a format needs is more useful than
//!   pretending it does not exist.
//! - **A refused recipe is 422 naming the constraint**, a taken key is 409. The database is the specification;
//!   the message says which rule refused rather than "invalid".

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::conversions::{ConversionState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    acme: PgPool,
    /// A tenant admin, with a person behind it: Manage and Download both.
    key: String,
    /// `asset:read` and nothing else, to separate Read from Download and from Manage.
    read_only_key: String,
    /// Download but not Manage, and no conversion permissions.
    downloader_key: String,
    /// Download plus `conversion:print`, so a restricted format has somebody who may use it.
    printer_key: String,
    /// A person who may download only what is in `group`.
    scoped_key: String,
    group: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let key = provision(&global, "acme", "ada@example.com").await;
    let read_only_key = plain_key(&global, "acme", &["asset:read"]).await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'visible', 'Visible') \
         RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");

    // Three roles, differing only in what they carry: this is the axis the whole permission story runs along,
    // so each is a real role row rather than a key scope.
    for (role, permissions) in [
        ("downloader", vec!["asset:read", "asset:download"]),
        (
            "printer",
            vec!["asset:read", "asset:download", "conversion:print"],
        ),
    ] {
        sqlx::query(
            "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
             VALUES (gen_random_uuid(), $1, $1, $2, '{}', true)",
        )
        .bind(role)
        .bind(
            permissions
                .iter()
                .map(|p| (*p).to_owned())
                .collect::<Vec<String>>(),
        )
        .execute(&acme)
        .await
        .expect("role");
    }
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped_downloader', 'Scoped', '{asset:read,asset:download}', \
                 ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");

    let downloader_key =
        person_key(&global, "acme", "dee@example.com", &["downloader"], false).await;
    let printer_key = person_key(&global, "acme", "pat@example.com", &["printer"], false).await;
    let scoped_key = person_key(
        &global,
        "acme",
        "scoped@example.com",
        &["scoped_downloader"],
        false,
    )
    .await;

    let app = router(ConversionState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        app,
        acme,
        key,
        read_only_key,
        downloader_key,
        printer_key,
        scoped_key,
        group,
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
    let identity = identity(global, email).await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(identity)
    .execute(global)
    .await
    .expect("membership");
    issue(global, tenant_id, Some(identity), &[]).await
}

async fn identity(global: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(global)
    .await
    .expect("identity")
}

async fn plain_key(global: &PgPool, slug: &str, scopes: &[&str]) -> String {
    let (tenant_id, identity_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, m.identity_id FROM dam_global.tenants t \
         JOIN dam_global.tenant_members m ON m.tenant_id = t.id WHERE t.slug = $1 \
         ORDER BY m.identity_id LIMIT 1",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant and member");
    issue(global, tenant_id, Some(identity_id), scopes).await
}

async fn person_key(
    global: &PgPool,
    slug: &str,
    email: &str,
    roles: &[&str],
    admin: bool,
) -> String {
    let tenant_id: Uuid = sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
        .bind(slug)
        .fetch_one(global)
        .await
        .expect("tenant");
    let identity = identity(global, email).await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(roles.iter().map(|r| (*r).to_owned()).collect::<Vec<String>>())
    .bind(admin)
    .execute(global)
    .await
    .expect("membership");
    issue(global, tenant_id, Some(identity), &[]).await
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

/// An asset, optionally in the scoped group, with a chosen mime and tier.
async fn asset(f: &Fixture, label: &str, mime: &str, tier: &str, in_group: bool) -> Uuid {
    let id = Uuid::new_v4();
    let content_hash = blake3::hash(label.as_bytes()).to_hex().to_string();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, $4, 10, $1)",
    )
    .bind(id)
    .bind(&content_hash)
    .bind(format!("{label}.jpg"))
    .bind(mime)
    .execute(&f.acme)
    .await
    .expect("asset");

    // The tier is not a column: it is derived from the warmest *present* placement. So a cold asset means a
    // placement in a cold class, which is also the only way this test exercises the same derivation the grid
    // and the detail panel use.
    if tier != "none" {
        let class = if tier == "cold" {
            "GLACIER"
        } else {
            "STANDARD"
        };
        sqlx::query(
            "INSERT INTO object_placements \
             (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state) \
             VALUES ($1, gen_random_uuid(), $2, 10, 'x', $3, 'present')",
        )
        .bind(format!("acme/original/{content_hash}"))
        .bind(id)
        .bind(class)
        .execute(&f.acme)
        .await
        .expect("placement");
    }
    if in_group {
        sqlx::query("INSERT INTO asset_group_members (asset_id, group_id) VALUES ($1, $2)")
            .bind(id)
            .bind(f.group)
            .execute(&f.acme)
            .await
            .expect("membership");
    }
    id
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

fn recipe(key: &str) -> Value {
    json!({
        "key": key,
        "label": "Web JPEG",
        "description": "Sized for a web page, and small enough to email.",
        "max_width": 2048,
        "max_height": 2048,
        "format": "jpeg",
        "quality": 82
    })
}

#[tokio::test]
async fn the_conversion_http_contract_holds() {
    let f = fixture().await;
    let photograph = asset(&f, "harbour", "image/jpeg", "hot", true).await;
    let archived = asset(&f, "cold", "image/jpeg", "cold", true).await;
    let document = asset(&f, "brochure", "application/pdf", "hot", true).await;
    let elsewhere = asset(&f, "hidden", "image/jpeg", "hot", false).await;

    administration_is_manage(&f).await;
    a_taken_key_is_a_conflict(&f).await;
    an_unrenderable_recipe_names_the_constraint(&f).await;
    the_options_are_download_not_read(&f, photograph).await;
    a_restricted_format_is_absent_rather_than_refused(&f, photograph).await;
    a_class_with_no_formats_says_so(&f, document).await;
    a_cold_original_is_not_offered(&f, archived).await;
    an_asset_outside_the_scope_is_absent(&f, elsewhere).await;
    a_withdrawn_format_leaves_the_offer(&f, photograph).await;
    a_redefinition_keeps_the_key(&f).await;
}

async fn administration_is_manage(f: &Fixture) {
    // The set of formats and the recipes behind them are configuration. A reader has no business with the
    // quality setting, and — more to the point — with which permission each format needs.
    let (refused, _) = call(f, "GET", "/conversions", &f.read_only_key, None).await;
    assert_eq!(refused, StatusCode::FORBIDDEN);
    let (also_refused, _) = call(f, "GET", "/conversions", &f.downloader_key, None).await;
    assert_eq!(
        also_refused,
        StatusCode::FORBIDDEN,
        "somebody who may download may administer the format list"
    );

    let (created, body) = call(f, "POST", "/conversions", &f.key, Some(recipe("web-2048"))).await;
    assert_eq!(created, StatusCode::CREATED, "{body}");
    assert_eq!(body["key"], json!("web-2048"), "{body}");
    assert_eq!(body["is_active"], json!(true), "{body}");

    let (listed, list) = call(f, "GET", "/conversions", &f.key, None).await;
    assert_eq!(listed, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().expect("array").len(), 1, "{list}");

    // Creating is Manage too, and a downloader is not an administrator.
    let (denied, _) = call(
        f,
        "POST",
        "/conversions",
        &f.downloader_key,
        Some(recipe("sneaky")),
    )
    .await;
    assert_eq!(denied, StatusCode::FORBIDDEN);
}

async fn a_taken_key_is_a_conflict(f: &Fixture) {
    // 409, not 422: the request is well formed and the world already contains that name. The distinction tells
    // a client whether to show a field error or to say "that name is in use".
    let (status, body) = call(f, "POST", "/conversions", &f.key, Some(recipe("web-2048"))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("web-2048")),
        "the conflict does not say which name: {body}"
    );
}

async fn an_unrenderable_recipe_names_the_constraint(f: &Fixture) {
    // The CHECK constraints are the specification for a usable recipe, so the refusal carries the constraint's
    // own name. An administrator told only "invalid" has to guess which of eight fields was wrong.
    let mut absurd = recipe("absurd");
    absurd["quality"] = json!(500);
    let (status, body) = call(f, "POST", "/conversions", &f.key, Some(absurd)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("quality")),
        "the refusal does not name the field: {body}"
    );

    // And a key is required on create, because it is the name a download request carries — a generated one
    // would be a URL nobody chose.
    let mut anonymous = recipe("ignored");
    anonymous["key"] = Value::Null;
    let (missing, body) = call(f, "POST", "/conversions", &f.key, Some(anonymous)).await;
    assert_eq!(missing, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

async fn the_options_are_download_not_read(f: &Fixture, photograph: Uuid) {
    let path = format!("/assets/{photograph}/download-options");

    // Read is not enough. What an asset can be had as is a question about taking a copy of it.
    let (refused, _) = call(f, "GET", &path, &f.read_only_key, None).await;
    assert_eq!(refused, StatusCode::FORBIDDEN);

    let (status, body) = call(f, "GET", &path, &f.downloader_key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["original_available"], json!(true), "{body}");
    assert_eq!(body["media_class"], json!("image"), "{body}");
    let offered = body["conversions"].as_array().expect("array");
    assert_eq!(offered.len(), 1, "{body}");
    // The description travels: it is the reason this table exists rather than a list of sizes.
    assert!(
        offered[0]["description"]
            .as_str()
            .is_some_and(|text| text.contains("web page")),
        "{body}"
    );
}

async fn a_restricted_format_is_absent_rather_than_refused(f: &Fixture, photograph: Uuid) {
    let mut print = recipe("print-full");
    print["label"] = json!("Print PNG");
    print["description"] = json!("Full size, lossless, for print.");
    print["format"] = json!("png");
    print["required_permission"] = json!("conversion:print");
    print["sort_order"] = json!(5);
    let (created, body) = call(f, "POST", "/conversions", &f.key, Some(print)).await;
    assert_eq!(created, StatusCode::CREATED, "{body}");

    let path = format!("/assets/{photograph}/download-options");

    // Absent for somebody without the permission. A list of things you cannot have is a worse answer than a
    // shorter list.
    let (_, plain) = call(f, "GET", &path, &f.downloader_key, None).await;
    let keys = offered_keys(&plain);
    assert!(!keys.contains(&"print-full".to_owned()), "{plain}");

    // Present for somebody with it, and in the configured order rather than alphabetical.
    let (_, privileged) = call(f, "GET", &path, &f.printer_key, None).await;
    let keys = offered_keys(&privileged);
    assert_eq!(
        keys,
        vec!["web-2048".to_owned(), "print-full".to_owned()],
        "{privileged}"
    );

    // And the permission is never named in this answer: every format here is one the caller may use, so
    // mentioning the gate would be telling them about one they have already passed.
    for row in privileged["conversions"].as_array().expect("array") {
        assert_eq!(row["required_permission"], Value::Null, "{privileged}");
    }
}

async fn a_class_with_no_formats_says_so(f: &Fixture, document: Uuid) {
    // An empty list plus the class, rather than an empty list alone: a client can then say "no formats are
    // configured for documents" instead of showing a blank panel that looks broken.
    let (status, body) = call(
        f,
        "GET",
        &format!("/assets/{document}/download-options"),
        &f.printer_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["media_class"], json!("document"), "{body}");
    assert_eq!(body["conversions"], json!([]), "{body}");
    // The original is still there. Nothing about a missing conversion set stops somebody taking the file.
    assert_eq!(body["original_available"], json!(true), "{body}");
}

async fn a_cold_original_is_not_offered(f: &Fixture, archived: Uuid) {
    // Offering it would be offering a download that fails minutes after the person chose it.
    let (status, body) = call(
        f,
        "GET",
        &format!("/assets/{archived}/download-options"),
        &f.printer_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["original_available"], json!(false), "{body}");
    // The list is unaffected, and that is what it means: these are the formats the tenant *offers*, not a
    // promise that each one's bytes exist this second. A conversion already rendered is its own object and does
    // not tier; one that has not been rendered comes from the original, so for this asset it needs the same
    // restore. Distinguishing those is the download route's job, and that route does not exist yet.
    assert!(
        !body["conversions"].as_array().expect("array").is_empty(),
        "{body}"
    );
}

async fn an_asset_outside_the_scope_is_absent(f: &Fixture, elsewhere: Uuid) {
    // The asset rule, unchanged: 404, and the same 404 an asset that does not exist gets. Only the
    // *conversion* half of this module departs from that, and it departs because a format is configuration.
    let (status, _) = call(
        f,
        "GET",
        &format!("/assets/{elsewhere}/download-options"),
        &f.scoped_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (nowhere, _) = call(
        f,
        "GET",
        &format!("/assets/{}/download-options", Uuid::new_v4()),
        &f.scoped_key,
        None,
    )
    .await;
    assert_eq!(nowhere, StatusCode::NOT_FOUND);
}

async fn a_withdrawn_format_leaves_the_offer(f: &Fixture, photograph: Uuid) {
    let (_, list) = call(f, "GET", "/conversions", &f.key, None).await;
    let id = list
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == json!("print-full"))
        .expect("present")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let (status, body) = call(
        f,
        "PATCH",
        &format!("/conversions/{id}/active"),
        &f.key,
        Some(json!({ "is_active": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_active"], json!(false), "{body}");

    let (_, offered) = call(
        f,
        "GET",
        &format!("/assets/{photograph}/download-options"),
        &f.printer_key,
        None,
    )
    .await;
    assert!(
        !offered_keys(&offered).contains(&"print-full".to_owned()),
        "{offered}"
    );

    // Still in administration, or nobody could put it back.
    let (_, admin) = call(f, "GET", "/conversions", &f.key, None).await;
    assert!(
        admin
            .as_array()
            .expect("array")
            .iter()
            .any(|row| row["key"] == json!("print-full")),
        "{admin}"
    );

    call(
        f,
        "PATCH",
        &format!("/conversions/{id}/active"),
        &f.key,
        Some(json!({ "is_active": true })),
    )
    .await;
}

async fn a_redefinition_keeps_the_key(f: &Fixture) {
    let (_, list) = call(f, "GET", "/conversions", &f.key, None).await;
    let id = list
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == json!("web-2048"))
        .expect("present")["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let mut renamed = recipe("web-1024");
    renamed["max_width"] = json!(1024);
    renamed["max_height"] = json!(1024);
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/conversions/{id}"),
        &f.key,
        Some(renamed),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A delivery token carries the key, so a link sent last week must keep resolving. The body's key field is
    // ignored rather than refused: one request type serves create and redefine, and two nearly identical bodies
    // is how they drift.
    assert_eq!(body["key"], json!("web-2048"), "{body}");
    assert_eq!(body["max_width"], json!(1024), "{body}");

    // And an unknown id is 404 rather than a silent no-op: an administrator whose request quietly did nothing
    // goes on believing it worked.
    let (missing, _) = call(
        f,
        "PATCH",
        &format!("/conversions/{}", Uuid::new_v4()),
        &f.key,
        Some(recipe("whatever")),
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
}

/// The keys in a download-options body, in order.
fn offered_keys(body: &Value) -> Vec<String> {
    body["conversions"]
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["key"].as_str().expect("key").to_owned())
        .collect()
}
