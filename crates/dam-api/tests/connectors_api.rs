//! Registering a connected site (M3d·1, §11).
//!
//! `dam_db` proves the secret lifecycle. What lives only here is what registration actually *builds*, and the
//! reason it matters is §11.1's claim: "a misconfigured Drupal view cannot surface an unapproved asset, because
//! the ABAC predicate already excluded it." That is only true if a connector goes through the ordinary
//! predicate — so the interesting test is not that the endpoint returns 201, it is that the key it hands back
//! reads the library through the same filter as everybody else's.
//!
//! - **Registration composes four existing things**: an identity, a membership, a role carrying the asset
//!   groups, and an API key. No connector-shaped authorisation path.
//! - **The key it returns is scoped.** Driven through the real asset listing: a connector registered against
//!   one group sees one asset, and the one it may not see is absent rather than present-and-refused.
//! - **Both secrets are shown once.** Reading the connector back afterwards returns neither, not even the
//!   ciphertext.
//! - **The service account is not a person**: `.invalid`, the reserved TLD that can never receive mail.
//! - **A refused registration leaves nothing behind** — not even the role it had already written.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::connectors::{ConnectorState, router};
use dam_core::Secret;
use dam_core::sealed::SealingKeyring;
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
    /// The whole asset router, so a connector's key can be driven against the real listing.
    assets: axum::Router,
    key: String,
    tenant_id: Uuid,
    /// In the `public` group.
    published: Uuid,
    /// In no group at all.
    internal: Uuid,
    public_group: Uuid,
}

fn keyring() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a test sealing passphrase".to_owned()))
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

    let ada: Uuid = sqlx::query_scalar(
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
    .bind(ada)
    .execute(&global)
    .await
    .expect("membership");

    let published = asset(&acme, "published").await;
    let internal = asset(&acme, "internal").await;
    let public_group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) \
         VALUES (gen_random_uuid(), 'public', 'Public') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(public_group)
        .bind(published)
        .execute(&acme)
        .await
        .expect("member");

    Fixture {
        _pg: pg,
        app: router(ConnectorState {
            global: global.clone(),
            keyring: keyring(),
        }),
        // The real asset router, so a connector's key can be driven against the real listing rather than
        // against an assertion about what the role row says.
        assets: dam_api::assets::router(dam_api::assets::AssetState {
            global: global.clone(),
            delivery: None,
        }),
        key: issue(&global, tenant_id, ada).await,
        global,
        acme,
        tenant_id,
        published,
        internal,
        public_group,
    }
}

async fn issue(global: &PgPool, tenant: Uuid, who: Uuid) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant)
    .bind(who)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
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

async fn registration_builds_the_ordinary_four_things(f: &Fixture) {
    let (status, made) = call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "drupal",
            "label": "  Marketing site  ",
            "site_url": "https://marketing.example.test/",
            "asset_group_ids": [f.public_group],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["connector"]["id"].as_str().expect("id").to_owned();

    assert_eq!(made["connector"]["label"], "Marketing site");
    assert_eq!(
        made["connector"]["site_url"],
        "https://marketing.example.test"
    );
    assert_eq!(made["connector"]["status"], "active");
    assert_eq!(made["connector"]["may_render"], true);
    // Both off unless asked for: a CMS wants renditions, and a page render must never wake Glacier.
    assert_eq!(made["connector"]["allow_original"], false);
    assert_eq!(made["connector"]["allow_restore"], false);

    // Both credentials, once, and said so in the body — a UI that forgets to say it produces a support ticket
    // a week later.
    assert!(
        made["api_key"]
            .as_str()
            .unwrap_or_default()
            .starts_with("damrs_"),
        "{made}"
    );
    assert!(
        !made["signing_secret"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
    assert!(
        made["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("shown only here"),
        "{made}"
    );

    // The four things, none of them new machinery.
    let role: (Vec<String>, Vec<Uuid>, bool) = sqlx::query_as(
        "SELECT permissions, asset_group_ids, all_asset_groups FROM roles WHERE key = $1",
    )
    .bind(format!("connector:{id}"))
    .fetch_one(&f.acme)
    .await
    .expect("role");
    // Read only. Rendering does not go through the API — the remote signs its own URLs — so download is not
    // needed and is not granted.
    assert_eq!(role.0, vec!["asset:read".to_owned()]);
    assert_eq!(role.1, vec![f.public_group]);
    assert!(!role.2);

    let account: (String, Vec<String>, bool) = sqlx::query_as(
        "SELECT i.email, m.role_names, m.is_tenant_admin \
         FROM dam_global.identities i \
         JOIN dam_global.tenant_members m ON m.identity_id = i.id \
         WHERE i.email = $1",
    )
    .bind(format!("connector+{id}@connectors.invalid"))
    .fetch_one(&f.global)
    .await
    .expect("service account");
    // `.invalid` is the reserved TLD: it can never resolve and can never receive a password reset.
    assert!(account.0.ends_with("@connectors.invalid"));
    assert_eq!(account.1, vec![format!("connector:{id}")]);
    assert!(!account.2, "a connector is never a tenant administrator");

    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.api_keys WHERE name = 'connector: Marketing site'",
    )
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(keys, 1);
}

async fn the_key_it_returns_reads_through_the_ordinary_predicate(f: &Fixture) {
    // The property §11.1 actually claims. Not that the endpoint returned 201.
    let (status, made) = call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "wordpress",
            "label": "Scoped site",
            "site_url": "https://scoped.example.test",
            "asset_group_ids": [f.public_group],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let connector_key = made["api_key"].as_str().expect("key").to_owned();

    let (status, page) = call(
        &f.assets,
        "GET",
        "/assets?limit=50",
        Some(&connector_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec![f.published.to_string()], "{page}");
    // Absent, not present-and-refused. A row a connector can see the existence of is a row a misconfigured
    // view can put on a page.
    assert!(!ids.contains(&f.internal.to_string().as_str()));
    assert_eq!(page["total"], 1, "the count is the connector's too");
}

async fn a_connector_with_no_groups_is_refused_rather_than_shown_an_empty_library(f: &Fixture) {
    // Registering before deciding what a site may have is legitimate, and it must fail closed rather than
    // reading "no groups" as "all groups".
    //
    // It fails as a **403**, not as a 200 with nothing in it, and that is the codebase's existing rule rather
    // than anything this endpoint decides: `caller::authorize` refuses any caller whose compiled predicate
    // matches nothing. For a connector it is also the better answer — an empty picker reads as "the DAM has no
    // assets", which sends a site operator looking in the wrong place, while a refusal says the registration
    // is not finished.
    let (status, made) = call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "generic",
            "label": "Not yet decided",
            "site_url": "https://blank.example.test",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["connector"]["all_asset_groups"], false);
    assert!(
        made["connector"]["asset_group_ids"]
            .as_array()
            .expect("groups")
            .is_empty()
    );
    let connector_key = made["api_key"].as_str().expect("key").to_owned();

    let (status, page) = call(
        &f.assets,
        "GET",
        "/assets?limit=50",
        Some(&connector_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{page}");

    // Granting it a group afterwards makes the same key work, which is what makes the refusal a state rather
    // than a dead registration.
    let id = made["connector"]["id"].as_str().expect("id");
    sqlx::query("UPDATE roles SET asset_group_ids = ARRAY[$2] WHERE key = $1")
        .bind(format!("connector:{id}"))
        .bind(f.public_group)
        .execute(&f.acme)
        .await
        .expect("grant");
    let (status, page) = call(
        &f.assets,
        "GET",
        "/assets?limit=50",
        Some(&connector_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 1);
}

async fn allowing_originals_grants_download_and_nothing_more(f: &Fixture) {
    let (status, made) = call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "figma",
            "label": "Design",
            "site_url": "https://design.example.test",
            "all_asset_groups": true,
            "allow_original": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["connector"]["id"].as_str().expect("id");
    assert_eq!(made["connector"]["allow_original"], true);

    let permissions: Vec<String> =
        sqlx::query_scalar("SELECT permissions FROM roles WHERE key = $1")
            .bind(format!("connector:{id}"))
            .fetch_one(&f.acme)
            .await
            .expect("role");
    assert_eq!(
        permissions,
        vec!["asset:read".to_owned(), "asset:download".to_owned()],
        "download, and still not manage"
    );
    assert!(!permissions.iter().any(|one| one.contains("manage")));
}

async fn reading_a_connector_back_returns_no_secret_at_all(f: &Fixture) {
    let (_, listed) = call(&f.app, "GET", "/connectors", Some(&f.key), None).await;
    let rows = listed.as_array().expect("array");
    assert!(!rows.is_empty());
    for row in rows {
        // Not even the ciphertext. The sealed form is not dangerous on its own, but an endpoint an
        // administration screen polls is how it reaches a log, a browser cache and a bug report.
        let rendered = row.to_string();
        assert!(!rendered.contains("signing_secret"), "{rendered}");
        assert!(!rendered.contains("sealed"), "{rendered}");
        assert!(!rendered.contains("v1.k1."), "{rendered}");
        assert!(!rendered.contains("api_key"), "{rendered}");
    }

    let id = rows[0]["id"].as_str().expect("id");
    let (status, one) = call(
        &f.app,
        "GET",
        &format!("/connectors/{id}"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!one.to_string().contains("v1.k1."));
}

async fn a_rotation_returns_a_new_secret_and_leaves_the_key_alone(f: &Fixture) {
    let (_, made) = call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "hubspot",
            "label": "Rotating",
            "site_url": "https://rotating.example.test",
            "all_asset_groups": true,
        })),
    )
    .await;
    let id = made["connector"]["id"].as_str().expect("id").to_owned();
    let first = made["signing_secret"].as_str().expect("secret").to_owned();

    let (status, rotated) = call(
        &f.app,
        "POST",
        &format!("/connectors/{id}/rotate"),
        Some(&f.key),
        Some(json!({ "keep_previous": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let second = rotated["signing_secret"].as_str().expect("secret");
    assert_ne!(second, first);
    assert_eq!(rotated["connector"]["previous_secret_live"], true);
    assert!(rotated["connector"]["secret_rotated_at"].is_string());
    // Two credentials with separate reasons to be replaced. A blank here is what says the key was untouched.
    assert_eq!(rotated["api_key"], "");

    // And the leak case: no grace at all.
    let (status, urgent) = call(
        &f.app,
        "POST",
        &format!("/connectors/{id}/rotate"),
        Some(&f.key),
        Some(json!({ "keep_previous": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{urgent}");
    assert_eq!(urgent["connector"]["previous_secret_live"], false);

    // No default: the two situations want opposite answers, so a body that does not say is refused rather
    // than guessed.
    let (status, _) = call(
        &f.app,
        "POST",
        &format!("/connectors/{id}/rotate"),
        Some(&f.key),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

async fn revoking_is_terminal_and_says_nothing_afterwards(f: &Fixture) {
    let (_, made) = call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "salesforce",
            "label": "Finished with",
            "site_url": "https://gone.example.test",
            "all_asset_groups": true,
        })),
    )
    .await;
    let id = made["connector"]["id"].as_str().expect("id").to_owned();

    let (status, paused) = call(
        &f.app,
        "POST",
        &format!("/connectors/{id}/status"),
        Some(&f.key),
        Some(json!({ "status": "paused" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused["may_render"], false);

    let (status, revoked) = call(
        &f.app,
        "POST",
        &format!("/connectors/{id}/status"),
        Some(&f.key),
        Some(json!({ "status": "revoked" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["status"], "revoked");
    assert_eq!(revoked["may_render"], false);

    // Terminal. One answer for "no such connector" and "already revoked", because there is nothing an
    // operator can do about either and distinguishing them says only that a registration once existed.
    let (status, _) = call(
        &f.app,
        "POST",
        &format!("/connectors/{id}/status"),
        Some(&f.key),
        Some(json!({ "status": "active" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And rotating it is a 409 with a sentence, because that one *is* worth explaining: an operator trying to
    // rotate a revoked connector has misidentified which site they are fixing.
    let (status, refused) = call(
        &f.app,
        "POST",
        &format!("/connectors/{id}/rotate"),
        Some(&f.key),
        Some(json!({ "keep_previous": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("register a new one"),
        "{refused}"
    );
}

async fn a_refused_registration_leaves_nothing_behind(f: &Fixture) {
    call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "drupal",
            "label": "First",
            "site_url": "https://duplicate.example.test",
            "all_asset_groups": true,
        })),
    )
    .await;

    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM roles WHERE key LIKE 'connector:%'")
        .fetch_one(&f.acme)
        .await
        .expect("count");

    let (status, refused) = call(
        &f.app,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "drupal",
            "label": "Second",
            "site_url": "https://duplicate.example.test/",
            "all_asset_groups": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("already connected to"),
        "{refused}"
    );

    // The role the handler had already written is gone. Otherwise a retried registration accumulates orphan
    // roles, each a standing grant naming nothing.
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM roles WHERE key LIKE 'connector:%'")
        .fetch_one(&f.acme)
        .await
        .expect("count");
    assert_eq!(after, before, "a refused registration left a role behind");
}

async fn a_registration_is_refused_before_it_can_be_useless(f: &Fixture) {
    for (body, expected) in [
        (
            json!({ "kind": "sharepoint", "label": "X", "site_url": "https://x.test" }),
            "not a connector kind",
        ),
        (
            json!({ "kind": "drupal", "label": "  ", "site_url": "https://x.test" }),
            "needs a label",
        ),
        (
            json!({ "kind": "drupal", "label": "X", "site_url": "example.test" }),
            "absolute http or https origin",
        ),
        (
            json!({ "kind": "drupal", "label": "X", "site_url": "ftp://example.test" }),
            "absolute http or https origin",
        ),
    ] {
        let (status, refused) = call(&f.app, "POST", "/connectors", Some(&f.key), Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        let reason = refused["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(expected),
            "expected {expected:?}, got {reason:?}"
        );
    }
}

async fn registering_a_site_is_administration(f: &Fixture) {
    let reader: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), 'bob@example.com', 'Bob') RETURNING id",
    )
    .fetch_one(&f.global)
    .await
    .expect("identity");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, all_asset_groups) \
         VALUES (gen_random_uuid(), 'reader', 'Reader', '{asset:read}', true)",
    )
    .execute(&f.acme)
    .await
    .expect("role");
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{reader}', false)",
    )
    .bind(f.tenant_id)
    .bind(reader)
    .execute(&f.global)
    .await
    .expect("membership");
    let reader_key = issue(&f.global, f.tenant_id, reader).await;

    // A connected site is a standing grant of read access to part of the library. Somebody who can read the
    // library must not be able to hand that reach to a server.
    for (method, path, body) in [
        ("GET", "/connectors", None),
        (
            "POST",
            "/connectors",
            Some(json!({ "kind": "drupal", "label": "X", "site_url": "https://y.test" })),
        ),
    ] {
        let (status, _) = call(&f.app, method, path, Some(&reader_key), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

#[tokio::test]
async fn the_connector_contract_holds() {
    let f = fixture().await;

    registration_builds_the_ordinary_four_things(&f).await;
    the_key_it_returns_reads_through_the_ordinary_predicate(&f).await;
    a_connector_with_no_groups_is_refused_rather_than_shown_an_empty_library(&f).await;
    allowing_originals_grants_download_and_nothing_more(&f).await;
    reading_a_connector_back_returns_no_secret_at_all(&f).await;
    a_rotation_returns_a_new_secret_and_leaves_the_key_alone(&f).await;
    revoking_is_terminal_and_says_nothing_afterwards(&f).await;
    a_refused_registration_leaves_nothing_behind(&f).await;
    a_registration_is_refused_before_it_can_be_useless(&f).await;
    registering_a_site_is_administration(&f).await;
}
