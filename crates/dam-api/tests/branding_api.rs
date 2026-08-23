//! Site branding (Q.20d).
//!
//! Four decisions, and two of them are fixes for something that was wrong rather than preferences:
//!
//! - **The application called itself "damrs"** in the nav of every tenant's library — a vendor's name where a
//!   customer's belongs. An empty `site_name` now resolves to the tenant's display name, which they gave us at
//!   provisioning, so a tenant who never opens this screen still sees themselves.
//! - **Every portal defaulted to our blue.** A tenant with six press kits set the same colour six times and
//!   the seventh reverted, with nothing on screen saying why. A portal created without an accent now inherits
//!   the tenant's.
//! - **The accent is interpolated into CSS**, so the format check is the sanitiser. Lowercase six-digit hex
//!   only, refused with a sentence rather than a constraint violation, and normalised on the way in.
//! - **The logo is checked against the caller's own scope.** Otherwise setting it to an id you cannot see
//!   would confirm the asset exists, and would put it on every page of a library whose rules say you may not
//!   read it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::branding::{BrandingState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    app: axum::Router,
    key: String,
    read_only_key: String,
    /// Holds Manage over one group only, and cannot see `hidden`.
    scoped_key: String,
    visible: Uuid,
    hidden: Uuid,
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
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme Corporation', 'acme/', 'active') \
         RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");
    let admin = identity(&global, "ada@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(admin)
    .execute(&global)
    .await
    .expect("membership");

    let visible = asset(&acme, "logo").await;
    let hidden = asset(&acme, "someone-elses-logo").await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) \
         VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(visible)
        .execute(&acme)
        .await
        .expect("member");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped_admin', 'Scoped', '{asset:read,asset:manage}', \
                 ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let curator = identity(&global, "curator@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{scoped_admin}', false)",
    )
    .bind(tenant_id)
    .bind(curator)
    .execute(&global)
    .await
    .expect("membership");

    Fixture {
        _pg: pg,
        app: router(BrandingState {
            global: global.clone(),
            // No delivery: these assets have no rendered thumbnail, so a logo link would be a URL that 404s.
            delivery: None,
        }),
        key: issue(&global, tenant_id, Some(admin), &[]).await,
        read_only_key: issue(&global, tenant_id, Some(admin), &["asset:read"]).await,
        scoped_key: issue(&global, tenant_id, Some(curator), &[]).await,
        global,
        acme,
        visible,
        hidden,
    }
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
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<String>>(),
    )
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/png', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.png"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn call(
    f: &Fixture,
    method: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri("/branding");
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = f
        .app
        .clone()
        .oneshot(
            request
                .body(match &body {
                    Some(value) => Body::from(value.to_string()),
                    None => Body::empty(),
                })
                .expect("request"),
        )
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

async fn an_unset_name_falls_back_to_the_tenants_own(f: &Fixture) {
    let (status, read) = call(f, "GET", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    // The fix: the application used to call itself "damrs" in every tenant's nav. A tenant who has never
    // opened this screen sees the name they gave us at provisioning.
    assert_eq!(read["site_name"], "Acme Corporation");
    // And the form needs to know it is a fallback, or it would pre-fill the field and make the default look
    // like a choice — after which clearing it would be impossible to distinguish from never setting it.
    assert_eq!(read["site_name_is_default"], true);
    assert_eq!(read["accent"], "#2563eb");
    assert!(read["logo_asset_id"].is_null());
    assert!(read["support_email"].is_null());
}

async fn reading_needs_read_and_writing_needs_manage(f: &Fixture) {
    // Read, not Manage: the app shell renders this on every page, so gating it behind Manage would leave a
    // curator looking at a header that says nothing about their own library.
    let (status, _) = call(f, "GET", Some(&f.read_only_key), None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(
        f,
        "PUT",
        Some(&f.read_only_key),
        Some(json!({ "accent": "#123456" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "changing what the whole library is called is administration"
    );

    let (status, _) = call(f, "GET", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn the_accent_is_normalised_and_read_back(f: &Fixture) {
    let (status, saved) = call(
        f,
        "PUT",
        Some(&f.key),
        Some(json!({ "site_name": "  Acme Library  ", "accent": "#FF6600" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    // Lowercased and trimmed, and *read back* rather than echoed — a client shown what it sent would not
    // learn that `#FF6600` became `#ff6600`, and would then compare its own value against a different string.
    assert_eq!(saved["accent"], "#ff6600");
    assert_eq!(saved["site_name"], "Acme Library");
    assert_eq!(saved["site_name_is_default"], false);

    // Clearing it returns to the fallback rather than storing an empty header.
    let (_, cleared) = call(
        f,
        "PUT",
        Some(&f.key),
        Some(json!({ "site_name": "", "accent": "#ff6600" })),
    )
    .await;
    assert_eq!(cleared["site_name"], "Acme Corporation");
    assert_eq!(cleared["site_name_is_default"], true);
}

async fn a_colour_that_is_not_one_is_refused_by_name(f: &Fixture) {
    // This value is interpolated into a stylesheet, so the format check is the sanitiser rather than a
    // nicety — the last case is what it is actually for.
    for refused in [
        "red",
        "#25e",
        "2563eb",
        "#2563ebb",
        "#2563eg",
        "",
        "#000;} body{display:none",
    ] {
        let (status, body) = call(f, "PUT", Some(&f.key), Some(json!({ "accent": refused }))).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{refused:?} should be refused, got {body}"
        );
        let reason = body["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("#rrggbb"),
            "the refusal names the format so it can be fixed in one keystroke: {reason}"
        );
    }

    // And nothing was stored by any of them.
    let (_, read) = call(f, "GET", Some(&f.key), None).await;
    assert_eq!(read["accent"], "#ff6600");
}

async fn a_logo_outside_the_callers_scope_is_refused(f: &Fixture) {
    // Otherwise setting the logo to an id you cannot see confirms the asset exists — and puts it on every page
    // of a library whose own rules say you may not read it.
    let (status, refused) = call(
        f,
        "PUT",
        Some(&f.scoped_key),
        Some(json!({ "accent": "#ff6600", "logo_asset_id": f.hidden })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");

    // The one they can see is fine.
    let (status, saved) = call(
        f,
        "PUT",
        Some(&f.scoped_key),
        Some(json!({ "accent": "#ff6600", "logo_asset_id": f.visible })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["logo_asset_id"], f.visible.to_string());
    // No thumbnail rendered, so no link — rather than a URL that 404s.
    assert!(saved["logo_url"].is_null());

    // Deleting the logo asset costs the logo, not the branding row: `ON DELETE SET NULL`.
    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(f.visible)
        .execute(&f.acme)
        .await
        .expect("delete");
    let (status, read) = call(f, "GET", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert!(read["logo_asset_id"].is_null());
    assert_eq!(
        read["accent"], "#ff6600",
        "and the rest of the branding survives"
    );
}

async fn a_support_address_is_shaped_and_optional(f: &Fixture) {
    let (status, saved) = call(
        f,
        "PUT",
        Some(&f.key),
        Some(json!({ "accent": "#ff6600", "support_email": "  help@acme.test  " })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["support_email"], "help@acme.test", "trimmed");

    // An empty string is an absence, not a value: a form that posts every field would otherwise store "" and
    // put an empty mailto in a portal footer.
    let (_, cleared) = call(
        f,
        "PUT",
        Some(&f.key),
        Some(json!({ "accent": "#ff6600", "support_email": "" })),
    )
    .await;
    assert!(cleared["support_email"].is_null());
}

#[tokio::test]
async fn the_branding_contract_holds() {
    let f = fixture().await;

    an_unset_name_falls_back_to_the_tenants_own(&f).await;
    reading_needs_read_and_writing_needs_manage(&f).await;
    the_accent_is_normalised_and_read_back(&f).await;
    a_colour_that_is_not_one_is_refused_by_name(&f).await;
    a_support_address_is_shaped_and_optional(&f).await;
    // Last: it deletes an asset the earlier cases rely on.
    a_logo_outside_the_callers_scope_is_refused(&f).await;

    assert!(!f.global.is_closed());
}
