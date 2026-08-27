//! Share management and the recipient portal.
//!
//! The properties with weight here: the management routes hold the same predicate discipline as every other
//! write (an asset outside the caller's scope is 404, a read-only key gets nothing), and the portal — the one
//! unauthenticated surface in the product — hands out **nothing but what `issue_for_share` signs**, so rights
//! and revocation hold on it exactly as they hold on delivery, because it *is* delivery.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::delivery::DeliveryState;
use dam_api::shares::{ShareState, router};
use dam_core::Secret;
use dam_core::signed_url::Keyring;
use dam_db::{auth, migrate, testing::PostgresHarness};
use dam_store::{BlobStore, FakeS3Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    /// The delivery tenant's schema pool — what the portal reads.
    pool: PgPool,
    /// The control plane, for the identity an order is placed by.
    global: PgPool,
    globex: PgPool,
    key: String,
    read_only_key: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    migrate::tenant(&url, "t_globex").await.expect("globex");
    let global = pg.pool().clone();
    let pool = pg.pool_for_schema("t_acme").await.expect("acme pool");
    let globex = pg.pool_for_schema("t_globex").await.expect("globex pool");

    let key = provision(&global, "acme", "a@example.com").await;
    provision(&global, "globex", "b@example.com").await;
    let read_only_key = scoped_key(&global, "acme").await;

    let tenant_id: Uuid =
        sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = 'acme'")
            .fetch_one(&global)
            .await
            .expect("tenant id");

    let store: Arc<dyn BlobStore> = Arc::new(FakeS3Store::with_test_clock().0);
    let delivery = Arc::new(DeliveryState::new(
        pool.clone(),
        pool.clone(),
        store,
        Keyring::single("k1", Secret::new("a-signing-key".to_owned())),
        tenant_id,
        dam_core::TenantSlug::new("acme").expect("a slug"),
    ));

    let app = router(ShareState {
        global: global.clone(),
        delivery,
    });

    Fixture {
        _pg: pg,
        app,
        pool,
        global: global.clone(),
        globex,
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

async fn scoped_key(global: &PgPool, slug: &str) -> String {
    let (tenant_id, identity_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, m.identity_id FROM dam_global.tenants t \
         JOIN dam_global.tenant_members m ON m.tenant_id = t.id WHERE t.slug = $1",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant and member");
    issue(global, tenant_id, identity_id, &["asset:read"]).await
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

/// An asset with a real web-2048 derivative row and the bytes behind it, licensed worldwide.
async fn licensed_asset(f: &Fixture, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    let content_hash = blake3::hash(label.as_bytes()).to_hex().to_string();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(&content_hash)
    .bind(format!("{label}.jpg"))
    .execute(&f.pool)
    .await
    .expect("asset");

    let profile = dam_media::profiles::by_name("web-2048").expect("a built-in profile");
    sqlx::query(
        "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, 'image/jpeg', 5)",
    )
    .bind(id)
    .bind(profile.role)
    .bind(profile.name)
    .bind(profile.op_hash())
    .bind(format!("acme/p/{label}-2048"))
    .execute(&f.pool)
    .await
    .expect("derivative");

    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual) \
         VALUES ($1, 'worldwide', 'royalty_free', true)",
    )
    .bind(license_id)
    .execute(&f.pool)
    .await
    .expect("licence");
    sqlx::query("INSERT INTO license_scopes (id, license_id, territories) VALUES (gen_random_uuid(), $1, '{WORLD}')")
        .bind(license_id)
        .execute(&f.pool)
        .await
        .expect("scope");
    sqlx::query("INSERT INTO asset_licenses (asset_id, license_id) VALUES ($1, $2)")
        .bind(id)
        .bind(license_id)
        .execute(&f.pool)
        .await
        .expect("attach");
    id
}

/// An asset with a derivative but no licence at all — `rights_state` unknown, which denies distribution.
async fn unlicensed_asset(f: &Fixture, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(&f.pool)
    .await
    .expect("asset");
    id
}

async fn send(app: &axum::Router, request: Request<Body>) -> axum::http::Response<Body> {
    app.clone().oneshot(request).await.expect("router")
}

fn authed(method: &str, uri: &str, key: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {key}"));
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    builder
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .expect("request")
}

fn public(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

async fn json(response: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Creates a share through the API and returns (id, portal token).
async fn share(f: &Fixture, body: Value) -> (Uuid, String) {
    let response = send(&f.app, authed("POST", "/shares", &f.key, Some(body))).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(response).await;
    (
        created["id"].as_str().expect("id").parse().expect("uuid"),
        created["token"].as_str().expect("token").to_owned(),
    )
}

// ─── management ─────────────────────────────────────────────────────────────

async fn a_read_only_key_cannot_manage_shares(f: &Fixture) {
    let target = licensed_asset(f, "ro-share").await;
    for (label, request) in [
        (
            "create",
            authed(
                "POST",
                "/shares",
                &f.read_only_key,
                Some(json!({"asset_id": target})),
            ),
        ),
        ("list", authed("GET", "/shares", &f.read_only_key, None)),
        (
            "revoke",
            authed(
                "DELETE",
                &format!("/shares/{}", Uuid::new_v4()),
                &f.read_only_key,
                None,
            ),
        ),
    ] {
        assert_eq!(
            send(&f.app, request).await.status(),
            StatusCode::FORBIDDEN,
            "{label} must require manage"
        );
    }
}

async fn sharing_an_asset_outside_the_callers_scope_is_404(f: &Fixture) {
    // Sharing is distribution: the predicate applies exactly as it does to a write, and 404 rather than 403
    // so the caller learns nothing about whether the id exists.
    let theirs = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, 'theirs.jpg', 'image/jpeg', 10, $1)",
    )
    .bind(theirs)
    .bind(blake3::hash(b"not-yours").to_hex().to_string())
    .execute(&f.globex)
    .await
    .expect("their asset");

    let response = send(
        &f.app,
        authed("POST", "/shares", &f.key, Some(json!({"asset_id": theirs}))),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

async fn the_list_reports_liveness_so_a_reader_does_not_rederive_it(f: &Fixture) {
    let target = licensed_asset(f, "list-live").await;
    let (live_id, _) = share(f, json!({"asset_id": target})).await;
    let (revoked_id, _) = share(f, json!({"asset_id": target})).await;
    assert_eq!(
        send(
            &f.app,
            authed("DELETE", &format!("/shares/{revoked_id}"), &f.key, None)
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );

    let rows = json(send(&f.app, authed("GET", "/shares", &f.key, None)).await).await;
    let rows = rows.as_array().expect("a list");
    let by_id = |id: Uuid| {
        rows.iter()
            .find(|row| row["id"].as_str() == Some(id.to_string().as_str()))
            .expect("row present")
    };
    assert_eq!(by_id(live_id)["live"], true);
    assert_eq!(by_id(revoked_id)["live"], false);
    assert_eq!(by_id(revoked_id)["revoked"], true);
    assert_eq!(
        by_id(live_id)["filename"],
        "list-live.jpg",
        "the row names what is shared"
    );
}

// ─── the portal ─────────────────────────────────────────────────────────────

async fn the_portal_serves_a_licensed_share_without_any_credential(f: &Fixture) {
    let target = licensed_asset(f, "portal-ok").await;
    let (_, token) = share(f, json!({"asset_id": target})).await;

    let response = send(&f.app, public(&format!("/share/{token}"), json!({}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let view = json(response).await;
    assert_eq!(view["filename"], "portal-ok.jpg");
    assert!(
        view["preview_url"]
            .as_str()
            .expect("a preview URL")
            .contains("/d/"),
        "the preview is a real delivery URL: {view}"
    );
    assert_eq!(view["download_allowed"], true);
}

async fn a_dead_token_says_why_but_never_what(f: &Fixture) {
    let response = send(
        &f.app,
        public(&format!("/share/{}", "ab".repeat(32)), json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json(response).await;
    assert!(
        body["reason"]
            .as_str()
            .expect("a reason")
            .contains("no such share"),
        "{body}"
    );
}

async fn a_passcode_gates_the_portal_and_the_download_alike(f: &Fixture) {
    let target = licensed_asset(f, "portal-pass").await;
    let (_, token) = share(f, json!({"asset_id": target, "passcode": "spring2026"})).await;

    let bare = send(&f.app, public(&format!("/share/{token}"), json!({}))).await;
    assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);
    assert!(
        json(bare).await["reason"]
            .as_str()
            .expect("reason")
            .contains("required"),
        "a missing passcode says to look for one"
    );

    let wrong = send(
        &f.app,
        public(&format!("/share/{token}"), json!({"passcode": "wrong"})),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert!(
        json(wrong).await["reason"]
            .as_str()
            .expect("reason")
            .contains("not correct"),
        "a wrong passcode says to re-read it — the two are different mistakes"
    );

    let right = send(
        &f.app,
        public(
            &format!("/share/{token}"),
            json!({"passcode": "spring2026"}),
        ),
    )
    .await;
    assert_eq!(right.status(), StatusCode::OK);

    // The download route runs the same gate: a passcode checked on view but not on download is no gate.
    let download_bare = send(
        &f.app,
        public(&format!("/share/{token}/download"), json!({})),
    )
    .await;
    assert_eq!(download_bare.status(), StatusCode::UNAUTHORIZED);
}

async fn downloads_consume_the_limit_and_a_refusal_does_not(f: &Fixture) {
    let target = licensed_asset(f, "portal-limit").await;
    let (_, token) = share(f, json!({"asset_id": target, "max_downloads": 1})).await;

    let first = send(
        &f.app,
        public(&format!("/share/{token}/download"), json!({})),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let body = json(first).await;
    assert!(body["url"].as_str().expect("url").contains("/d/"));
    assert_eq!(body["downloads_remaining"], 0);

    let second = send(
        &f.app,
        public(&format!("/share/{token}/download"), json!({})),
    )
    .await;
    assert_eq!(
        second.status(),
        StatusCode::NOT_FOUND,
        "the limit is spent; the link is exhausted"
    );
    assert!(
        json(second).await["reason"]
            .as_str()
            .expect("reason")
            .contains("download limit")
    );
}

async fn an_unlicensed_asset_shares_its_name_but_never_its_bytes(f: &Fixture) {
    // The design in one case: a share is a door, not a skeleton key. The recipient sees what was shared and
    // is told why nothing can be delivered — rights refuse, at the same chokepoint as everywhere else.
    let target = unlicensed_asset(f, "portal-unlicensed").await;
    let (_, token) = share(f, json!({"asset_id": target})).await;

    let view = json(send(&f.app, public(&format!("/share/{token}"), json!({}))).await).await;
    assert_eq!(view["filename"], "portal-unlicensed.jpg");
    assert!(
        view["preview_url"].is_null(),
        "no pixels without a licence: {view}"
    );
    assert!(
        view["preview_unavailable"]
            .as_str()
            .expect("a reason")
            .contains("not licensed")
    );

    let download = send(
        &f.app,
        public(&format!("/share/{token}/download"), json!({})),
    )
    .await;
    assert_eq!(download.status(), StatusCode::FORBIDDEN);

    // And the refusal spent nothing: a recipient's downloads are not consumed by bytes they never received.
    let count: i32 = sqlx::query_scalar(
        "SELECT download_count FROM share_links ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&f.pool)
    .await
    .expect("count");
    assert_eq!(count, 0);
}

async fn revoking_kills_the_portal_immediately(f: &Fixture) {
    let target = licensed_asset(f, "portal-revoked").await;
    let (id, token) = share(f, json!({"asset_id": target})).await;
    assert_eq!(
        send(&f.app, public(&format!("/share/{token}"), json!({})))
            .await
            .status(),
        StatusCode::OK
    );

    send(
        &f.app,
        authed("DELETE", &format!("/shares/{id}"), &f.key, None),
    )
    .await;

    let after = send(&f.app, public(&format!("/share/{token}"), json!({}))).await;
    assert_eq!(after.status(), StatusCode::NOT_FOUND);
    assert!(
        json(after).await["reason"]
            .as_str()
            .expect("reason")
            .contains("revoked"),
        "the recipient is told to ask for a new link, not to re-type this one"
    );
}

async fn a_eula_gated_share_fails_closed_until_the_flow_exists(f: &Fixture) {
    // The column exists; the acceptance machinery does not. Enforcing nothing while the flag reads as
    // protection would be worse than the missing feature — so the portal refuses, on view and on download.
    let target = licensed_asset(f, "portal-eula").await;
    let (id, token) = share(f, json!({"asset_id": target})).await;
    sqlx::query("UPDATE share_links SET requires_eula = true WHERE id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .expect("set the flag");

    for uri in [
        format!("/share/{token}"),
        format!("/share/{token}/download"),
    ] {
        let response = send(&f.app, public(&uri, json!({}))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert!(
            json(response).await["reason"]
                .as_str()
                .expect("reason")
                .contains("not available yet"),
            "the refusal says the gap is ours, not the recipient's"
        );
    }
}

#[tokio::test]
async fn the_share_contract_holds() {
    let f = fixture().await;
    a_read_only_key_cannot_manage_shares(&f).await;
    sharing_an_asset_outside_the_callers_scope_is_404(&f).await;
    the_list_reports_liveness_so_a_reader_does_not_rederive_it(&f).await;
    the_portal_serves_a_licensed_share_without_any_credential(&f).await;
    a_dead_token_says_why_but_never_what(&f).await;
    a_passcode_gates_the_portal_and_the_download_alike(&f).await;
    downloads_consume_the_limit_and_a_refusal_does_not(&f).await;
    an_unlicensed_asset_shares_its_name_but_never_its_bytes(&f).await;
    revoking_kills_the_portal_immediately(&f).await;
    a_eula_gated_share_fails_closed_until_the_flow_exists(&f).await;
    an_order_pickup_renders_the_whole_set(&f).await;
    a_pickup_download_records_the_declared_use(&f).await;
    an_asset_share_is_not_a_pickup(&f).await;
    a_pickup_refuses_an_asset_that_is_not_in_it(&f).await;
    one_unlicensed_item_does_not_deny_the_rest(&f).await;
}

// ─── order pickups (Q.13d) ──────────────────────────────────────────────────

/// An order and its pickup share, made the way fulfilment makes them, returning (order id, token).
async fn order_pickup(f: &Fixture, assets: &[Uuid], channel: Option<&str>) -> (Uuid, String) {
    let requester: Uuid = sqlx::query_scalar("SELECT id FROM dam_global.identities LIMIT 1")
        .fetch_one(&f.global)
        .await
        .expect("an identity");
    let order = dam_db::orders::place(
        &mut f.pool.acquire().await.expect("conn"),
        &dam_db::orders::NewOrder {
            requested_by: requester,
            purpose: "The spring brochure.".to_owned(),
            channel: channel.map(str::to_owned),
            territory: channel.map(|_| "GB".to_owned()),
            conversion_key: None,
            include_metadata: false,
            recipients: vec!["agency@example.com".to_owned()],
            asset_ids: assets.to_vec(),
        },
        &everything(),
    )
    .await
    .expect("place");
    dam_db::orders::approve(
        &mut f.pool.acquire().await.expect("conn"),
        order.id,
        requester,
        None,
        &everything(),
        14,
        chrono::Utc::now(),
    )
    .await
    .expect("approve");

    let share = dam_db::shares::create_on(
        &mut f.pool.acquire().await.expect("conn"),
        &dam_db::shares::ShareSpec {
            kind: "order",
            target_id: Some(order.id),
            search_query: None,
            passcode: None,
            expires_at: None,
            max_downloads: Some(10),
            allow_original: true,
            requires_eula: false,
            created_by: Some(requester),
        },
    )
    .await
    .expect("share");
    dam_db::orders::mark_ready(
        &mut f.pool.acquire().await.expect("conn"),
        order.id,
        share.id,
    )
    .await
    .expect("ready");
    (order.id, share.token().to_owned())
}

fn everything() -> dam_core::policy::AccessPredicate {
    dam_core::policy::compile(
        &dam_core::policy::Grants::from(vec![dam_core::policy::Grant {
            permissions: vec!["asset:read".to_owned(), "asset:download".to_owned()],
            asset_group_ids: vec![],
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        dam_core::policy::Action::Download,
        chrono::Utc::now(),
    )
}

async fn an_order_pickup_renders_the_whole_set(f: &Fixture) {
    // The case the portal could not answer before: `kind = 'order'` points at a set, and until this slice that
    // was "this link shares something this portal cannot show yet".
    let one = licensed_asset(f, "pickup-one").await;
    let two = licensed_asset(f, "pickup-two").await;
    let (_, token) = order_pickup(f, &[one, two], Some("print")).await;

    let response = send(&f.app, public(&format!("/share/{token}/set"), json!({}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert!(
        body["reference"]
            .as_str()
            .is_some_and(|reference| reference.starts_with("ORD-")),
        "{body}"
    );
    // The reason travels to the recipient, who is usually the reason it was asked for.
    assert!(
        body["purpose"]
            .as_str()
            .is_some_and(|p| p.contains("brochure")),
        "{body}"
    );
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "{body}");
    // A rights-checked preview per item, and the names as ordered.
    assert!(
        items.iter().all(|item| item["preview_url"].is_string()),
        "{body}"
    );
    assert!(
        items.iter().all(|item| item["filename"].is_string()),
        "{body}"
    );

    // The single-asset portal answers this token by pointing at the right route rather than 404ing blankly, and
    // never by reading an order id as an asset id.
    let wrong = send(&f.app, public(&format!("/share/{token}"), json!({}))).await;
    assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
    let reason = json(wrong).await;
    assert!(
        reason["reason"]
            .as_str()
            .is_some_and(|r| r.contains("order pickup")),
        "{reason}"
    );
}

async fn a_pickup_download_records_the_declared_use(f: &Fixture) {
    // The loop Q.12 opened, closed: a pickup's download lands in the ledger as a *declared* use, because the
    // requester named the channel when they placed the order and an approver agreed to it.
    let one = licensed_asset(f, "pickup-ledger").await;
    let (order_id, token) = order_pickup(f, &[one], Some("print")).await;

    let response = send(
        &f.app,
        public(&format!("/share/{token}/items/{one}/download"), json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert!(
        body["url"].as_str().is_some_and(|url| url.contains("/d/")),
        "{body}"
    );

    let rows = dam_db::usage::for_asset(
        &mut f.pool.acquire().await.expect("conn"),
        one,
        &everything(),
        10,
    )
    .await
    .expect("ledger");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].channel.as_deref(), Some("print"));
    assert_eq!(rows[0].territory.as_deref(), Some("GB"));
    assert!(
        rows[0].declared,
        "a pickup download was recorded as an undeclared use: {rows:?}"
    );
    // Attributed to the person who asked and stated the use. The recipient has no identity here, so recording
    // them is not an option and recording nobody would lose the only accountable party.
    assert!(rows[0].recorded_by.is_some(), "{rows:?}");

    // And the order is collected — idempotently, so a second file is not a second collection.
    let state: String = sqlx::query_scalar("SELECT state FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&f.pool)
        .await
        .expect("state");
    assert_eq!(state, "collected");
}

async fn an_asset_share_is_not_a_pickup(f: &Fixture) {
    // The two kinds must not be confusable. An asset share arriving at the set route is refused *as not a
    // pickup* rather than answered with a generic absence — the message is the only observable difference, since
    // both are 404s, and a recipient debugging a link deserves the true reason.
    let one = licensed_asset(f, "not-a-pickup").await;
    let (_, token) = share(f, json!({ "asset_id": one })).await;

    let response = send(&f.app, public(&format!("/share/{token}/set"), json!({}))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json(response).await;
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not an order pickup")),
        "an asset share at the set route is answered as though the pickup vanished: {body}"
    );

    // And the item-download route likewise: it is for pickups, and an asset share has no items. Asserted on the
    // *reason* as well, for the same purpose as above — without the kind check the order lookup refuses anyway,
    // so the message is the whole difference, and "this pickup is no longer available" would be a lie about a
    // link that was never a pickup.
    let item = send(
        &f.app,
        public(&format!("/share/{token}/items/{one}/download"), json!({})),
    )
    .await;
    assert_eq!(item.status(), StatusCode::NOT_FOUND);
    let body = json(item).await;
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not an order pickup")),
        "{body}"
    );
}

async fn a_pickup_refuses_an_asset_that_is_not_in_it(f: &Fixture) {
    // A pickup is an agreement about a specific set. Letting a recipient name any id would turn one approval
    // into a key to the library, so the id is checked against the order's items.
    let inside = licensed_asset(f, "pickup-inside").await;
    let outside = licensed_asset(f, "pickup-outside").await;
    let (_, token) = order_pickup(f, &[inside], None).await;

    let response = send(
        &f.app,
        public(
            &format!("/share/{token}/items/{outside}/download"),
            json!({}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // And nothing was consumed or recorded for it.
    let rows = dam_db::usage::for_asset(
        &mut f.pool.acquire().await.expect("conn"),
        outside,
        &everything(),
        10,
    )
    .await
    .expect("ledger");
    assert!(rows.is_empty(), "{rows:?}");
}

async fn one_unlicensed_item_does_not_deny_the_rest(f: &Fixture) {
    // An order of forty where two are unlicensed is a pickup of thirty-eight. Collapsing that into one refusal
    // would deny the recipient what they were entitled to because of somebody else's paperwork.
    let good = licensed_asset(f, "pickup-good").await;
    let bad = unlicensed_asset(f, "pickup-bad").await;
    let (_, token) = order_pickup(f, &[good, bad], None).await;

    let response = send(&f.app, public(&format!("/share/{token}/set"), json!({}))).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "{body}");
    let refused = items
        .iter()
        .find(|item| item["preview_url"].is_null())
        .expect("the unlicensed one");
    assert!(
        refused["preview_unavailable"]
            .as_str()
            .is_some_and(|why| why.contains("not licensed")),
        "{body}"
    );
    // Its name is still listed — the order is a record of what was asked for — and the other one is collectable.
    assert!(refused["filename"].is_string(), "{body}");
    let allowed = send(
        &f.app,
        public(&format!("/share/{token}/items/{good}/download"), json!({})),
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::OK);

    // The unlicensed one is refused at the item route too, with the reason rather than a flat 404.
    let denied = send(
        &f.app,
        public(&format!("/share/{token}/items/{bad}/download"), json!({})),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    // And that refusal wrote *nothing* to the ledger. This is the whole reason rights are evaluated before the
    // record rather than left to the mint: a denied attempt is not a download, and recording one would make a
    // licence cap count refusals against the licence that refused them. Mutation testing found this gap — with
    // the pre-check removed the mint still refuses, so the only visible difference is the phantom ledger row.
    let rows = dam_db::usage::for_asset(
        &mut f.pool.acquire().await.expect("conn"),
        bad,
        &everything(),
        10,
    )
    .await
    .expect("ledger");
    assert!(
        rows.is_empty(),
        "a refused pickup download is in the ledger: {rows:?}"
    );
}
