//! The download endpoint (Q.11c).
//!
//! The DAM's own users had no way to take a copy: only the share portal minted delivery URLs. What this suite
//! pins is the contract of the one that now exists.
//!
//! - **Download, not Read**, and the asset gate before any question about formats.
//! - **A format not yet rendered is 202 with the render queued**, not a dead URL and not a synchronous wait.
//!   Two people choosing it at the same moment is *one* job, which is what the dedupe key is for.
//! - **Rights are evaluated at issue as well as at delivery**, so a link that would fail never reaches an email.
//!   The refusal carries the verdict's codes, because the caller has already been shown the asset.
//! - **A format the caller has no permission for is a 403 naming the permission**, and one that does not exist
//!   is a 404 — the deliberate departure from the asset rule, for the reason in DECISIONS.md.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, TimeZone, Utc};
use dam_api::delivery::DeliveryState;
use dam_api::downloads::{DownloadState, router};
use dam_core::Secret;
use dam_core::signed_url::Keyring;
use dam_db::{auth, migrate, testing::PostgresHarness};
use dam_store::{BlobStore, FakeS3Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    acme: PgPool,
    global: PgPool,
    tenant_id: Uuid,
    /// Download, and `conversion:print`.
    printer_key: String,
    /// Download, no conversion permissions.
    downloader_key: String,
    /// Read only.
    read_only_key: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let tenant_id = provision(&global, "acme").await;
    let read_only_key = person_key(&global, "acme", "rita@example.com", &["reader"]).await;
    let downloader_key = person_key(&global, "acme", "dee@example.com", &["downloader"]).await;
    let printer_key = person_key(&global, "acme", "pat@example.com", &["printer"]).await;

    for (role, permissions) in [
        ("reader", vec!["asset:read"]),
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

    // A real signer over a fake store, and a fixed clock — the delivery suite's note applies here too: with the
    // handler reading the wall clock, a token minted a second before the fixture's `now()` is in the future.
    let store: Arc<dyn BlobStore> = Arc::new(FakeS3Store::with_test_clock().0);
    let keyring = Keyring::single("k1", Secret::new("a-signing-key".to_owned()));
    let clock = Arc::new(dam_core::TestClock::new());
    clock.set(now());
    let delivery = Arc::new(
        DeliveryState::new(acme.clone(), store, keyring, tenant_id).with_clock(clock.clone()),
    );

    let app = router(DownloadState {
        global: global.clone(),
        delivery: Some(delivery),
    });

    Fixture {
        _pg: pg,
        app,
        acme,
        global,
        tenant_id,
        printer_key,
        downloader_key,
        read_only_key,
    }
}

async fn provision(global: &PgPool, slug: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), $1, 't_' || $1, $1, $1 || '/', 'active') RETURNING id",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant")
}

async fn person_key(global: &PgPool, slug: &str, email: &str, roles: &[&str]) -> String {
    let tenant_id: Uuid = sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
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
         VALUES ($1, $2, $3, false)",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(roles.iter().map(|r| (*r).to_owned()).collect::<Vec<String>>())
    .execute(global)
    .await
    .expect("membership");

    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn asset(f: &Fixture, label: &str, mime: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, $4, 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.bin"))
    .bind(mime)
    .execute(&f.acme)
    .await
    .expect("asset");
    id
}

/// A perpetual worldwide licence, without which every download is refused as unlicensed.
async fn licence(f: &Fixture, asset_id: Uuid) {
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual) \
         VALUES ($1, 'worldwide', 'royalty_free', true)",
    )
    .bind(license_id)
    .execute(&f.acme)
    .await
    .expect("licence");
    sqlx::query(
        "INSERT INTO license_scopes (id, license_id, territories) \
         VALUES (gen_random_uuid(), $1, '{WORLD}')",
    )
    .bind(license_id)
    .execute(&f.acme)
    .await
    .expect("scope");
    sqlx::query("INSERT INTO asset_licenses (asset_id, license_id) VALUES ($1, $2)")
        .bind(asset_id)
        .bind(license_id)
        .execute(&f.acme)
        .await
        .expect("attach");
}

/// A conversion, with a recipe and an optional required permission.
///
/// The recipe is a parameter because the cache key *is* the recipe: two conversions defined identically share one
/// rendered object, which is a property this suite asserts rather than trips over.
async fn conversion(
    f: &Fixture,
    key: &str,
    size: i32,
    format: &str,
    permission: Option<&str>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO conversions \
         (id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
          required_permission) \
         VALUES (gen_random_uuid(), $1, $1, 'A format.', 'image', $2, $2, $3, 82, 'contain', $4) \
         RETURNING id",
    )
    .bind(key)
    .bind(size)
    .bind(format)
    .bind(permission)
    .fetch_one(&f.acme)
    .await
    .expect("conversion")
}

/// Records a rendered derivative for a conversion, as the worker would.
async fn rendered(f: &Fixture, asset_id: Uuid, key: &str) {
    let row = dam_db::conversions::by_key(&mut f.acme.acquire().await.expect("conn"), key)
        .await
        .expect("read")
        .expect("present");
    // The conversion's own `op_hash`, because delivery resolves key -> recipe -> hash -> row. A fixture
    // inventing a hash would record a derivative that is correctly never found.
    sqlx::query(
        "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
         VALUES (gen_random_uuid(), $1, 'rendition', $2, $3, $4, 'image/jpeg', 5)",
    )
    .bind(asset_id)
    .bind(key)
    .bind(row.op_hash().expect("renderable"))
    .bind(format!("acme/r/{asset_id}-{key}"))
    .execute(&f.acme)
    .await
    .expect("derivative");
}

async fn call(f: &Fixture, path: &str, key: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
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

/// How many render jobs are queued for this asset and format.
async fn queued(f: &Fixture, asset_id: Uuid, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs \
         WHERE tenant_id = $1 AND kind = 'render_conversion' AND dedupe_key = $2",
    )
    .bind(f.tenant_id)
    .bind(format!("conversion:{asset_id}:{key}"))
    .fetch_one(&f.global)
    .await
    .expect("count")
}

#[tokio::test]
async fn the_download_http_contract_holds() {
    let f = fixture().await;
    let photograph = asset(&f, "harbour", "image/jpeg").await;
    licence(&f, photograph).await;
    let unlicensed = asset(&f, "orphan", "image/jpeg").await;
    let document = asset(&f, "brochure", "application/pdf").await;
    licence(&f, document).await;

    conversion(&f, "web-2048", 2048, "jpeg", None).await;
    conversion(&f, "print-full", 4096, "png", Some("conversion:print")).await;
    // Defined identically to `web-2048` — a second name for the same recipe, which a tenant does when one
    // audience calls it "web" and another calls it "email".
    conversion(&f, "email-2048", 2048, "jpeg", None).await;

    downloading_is_download_not_read(&f, photograph).await;
    the_original_is_a_signed_url(&f, photograph).await;
    an_unrendered_format_is_accepted_and_queued(&f, photograph).await;
    a_second_request_does_not_queue_a_second_render(&f, photograph).await;
    a_rendered_format_is_a_url(&f, photograph).await;
    two_names_for_one_recipe_share_the_render(&f, photograph).await;
    a_format_needing_a_permission_names_it(&f, photograph).await;
    an_unknown_format_is_absent(&f, photograph).await;
    a_format_for_another_class_is_refused(&f, document).await;
    an_unlicensed_download_is_refused_with_its_reasons(&f, unlicensed).await;
}

async fn downloading_is_download_not_read(f: &Fixture, photograph: Uuid) {
    let (status, _) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.read_only_key,
        json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "somebody who may only look can take a copy"
    );
}

async fn the_original_is_a_signed_url(f: &Fixture, photograph: Uuid) {
    // No `format` in the body: "download" without further information means the original.
    let (status, body) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.downloader_key,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("ready"), "{body}");
    assert_eq!(body["format"], json!("original"), "{body}");
    // A URL, not a bare token: a token is not something a client can fetch.
    assert!(
        body["url"].as_str().is_some_and(|url| url.contains("/d/")),
        "{body}"
    );
}

async fn an_unrendered_format_is_accepted_and_queued(f: &Fixture, photograph: Uuid) {
    assert_eq!(queued(f, photograph, "web-2048").await, 0, "nothing yet");

    let (status, body) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.downloader_key,
        json!({ "format": "web-2048" }),
    )
    .await;
    // 202, not 404: the format exists and its bytes do not exist *yet*. A dead URL would be the alternative,
    // and a synchronous render would hold the connection for however long a large source takes.
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], json!("rendering"), "{body}");
    assert_eq!(body["url"], Value::Null, "{body}");
    assert_eq!(
        queued(f, photograph, "web-2048").await,
        1,
        "the render was not queued, so the client would poll forever"
    );
}

async fn a_second_request_does_not_queue_a_second_render(f: &Fixture, photograph: Uuid) {
    // Twenty people choosing the same format is one render. The dedupe key is built from the asset and the key
    // the caller named, which is why it collapses this case.
    let (status, _) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.printer_key,
        json!({ "format": "web-2048" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        queued(f, photograph, "web-2048").await,
        1,
        "a second request queued a second render of the same thing"
    );
}

async fn a_rendered_format_is_a_url(f: &Fixture, photograph: Uuid) {
    rendered(f, photograph, "web-2048").await;

    let (status, body) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.downloader_key,
        json!({ "format": "web-2048" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("ready"), "{body}");
    assert_eq!(body["format"], json!("web-2048"), "{body}");
    assert!(
        body["url"].as_str().is_some_and(|url| url.contains("/d/")),
        "{body}"
    );
}

async fn two_names_for_one_recipe_share_the_render(f: &Fixture, photograph: Uuid) {
    // `email-2048` has never been rendered and is immediately ready, because the cache key is the recipe and
    // `web-2048` already produced those exact bytes. This is what the shared `tenant_op_hash` buys: a tenant
    // with four names for one size stores one object rather than four.
    let (status, body) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.downloader_key,
        json!({ "format": "email-2048" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an identical recipe re-rendered instead of sharing: {body}"
    );
    assert_eq!(body["format"], json!("email-2048"), "{body}");
    assert_eq!(
        queued(f, photograph, "email-2048").await,
        0,
        "a render was queued for bytes that already exist"
    );
}

async fn a_format_needing_a_permission_names_it(f: &Fixture, photograph: Uuid) {
    // Named, not hidden. A conversion is tenant configuration: that a print format exists says nothing about
    // anybody's library, and a person refused it is better served by knowing what to ask for.
    let (status, body) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.downloader_key,
        json!({ "format": "print-full" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("conversion:print")),
        "the refusal does not say which permission: {body}"
    );

    // And it works for somebody who holds it.
    let (allowed, body) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.printer_key,
        json!({ "format": "print-full" }),
    )
    .await;
    assert_eq!(allowed, StatusCode::ACCEPTED, "{body}");
}

async fn an_unknown_format_is_absent(f: &Fixture, photograph: Uuid) {
    let (status, _) = call(
        f,
        &format!("/assets/{photograph}/download"),
        &f.printer_key,
        json!({ "format": "not-a-format" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And an asset that does not exist answers the same way, whatever format is named — the asset rule, which
    // this module keeps for assets even while departing from it for formats.
    let (nowhere, _) = call(
        f,
        &format!("/assets/{}/download", Uuid::new_v4()),
        &f.printer_key,
        json!({ "format": "web-2048" }),
    )
    .await;
    assert_eq!(nowhere, StatusCode::NOT_FOUND);
}

async fn a_format_for_another_class_is_refused(f: &Fixture, document: Uuid) {
    // 422: the request named two real things that do not go together. Queueing it would queue a job whose only
    // possible outcome is failure, and the person would poll a render that can never finish.
    let (status, body) = call(
        f,
        &format!("/assets/{document}/download"),
        &f.printer_key,
        json!({ "format": "web-2048" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("document")),
        "{body}"
    );
    assert_eq!(
        queued(f, document, "web-2048").await,
        0,
        "a render was queued for a class it cannot apply to"
    );

    // The original is still downloadable: nothing about a format mismatch stops somebody taking the file.
    let (original, body) = call(
        f,
        &format!("/assets/{document}/download"),
        &f.printer_key,
        json!({}),
    )
    .await;
    assert_eq!(original, StatusCode::OK, "{body}");
}

async fn an_unlicensed_download_is_refused_with_its_reasons(f: &Fixture, unlicensed: Uuid) {
    // Refused at *issue*, not only at delivery. A URL that looks valid in an email and fails when somebody
    // clicks it puts the error in front of the person who cannot act on it.
    let (status, body) = call(
        f,
        &format!("/assets/{unlicensed}/download"),
        &f.printer_key,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let reason = body["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("rights"),
        "the refusal does not say it is a rights decision: {body}"
    );
    // The codes travel: a customer who cannot download their own asset needs to know why.
    assert!(
        reason.contains("no_license"),
        "the refusal carries no reason codes: {body}"
    );
}
