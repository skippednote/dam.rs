//! The oEmbed provider (M3d·3, §11.1).
//!
//! oEmbed assumes a public provider: a consumer fetches `/oembed?url=…` with no credential. A governed asset
//! library cannot work that way — an unauthenticated endpoint that turns an asset id into a filename, a size and
//! a preview URL is an enumeration API for the whole library. So the properties here are mostly about that
//! deviation and about not lying to a consumer:
//!
//! - **It needs a credential**, and the same two as `/browse`.
//! - **An asset the connector cannot see is a 404**, the same answer as an id that never existed.
//! - **A URL from another provider is a 400, not a 404.** The consumer sent something malformed for *this*
//!   provider rather than asking about a resource that might exist, and oEmbed consumers act on the difference.
//! - **An unsupported format is a 501**, which is the spec's status. Answering JSON under an XML content type
//!   would be a lie a consumer cannot detect.
//! - **`cache_age` is below the delivery URL's own lifetime**, or a caching consumer serves a broken image for
//!   most of a day.
//! - **Only an image is a `photo`.** Claiming `video` would need an embeddable player this does not have.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::browse::{BrowseState, ConnectorAuth};
use dam_api::oembed::{OembedState, router};
use dam_connect::browse_token::{self, BrowseClaim};
use dam_core::Secret;
use dam_core::sealed::SealingKeyring;
use dam_core::signed_url::Keyring;
use dam_db::connectors::{self, Kind, NewConnector};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const TENANT: &str = "acme";
const ORIGIN: &str = "https://dam.example.com";

fn sealing() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a test sealing passphrase".to_owned()))
}

struct Fixture {
    _pg: PostgresHarness,
    pool: PgPool,
    group: Uuid,
    app: axum::Router,
    api_key: String,
    connector_id: Uuid,
    secret: String,
    image: Uuid,
    video: Uuid,
    hidden: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("tenant");

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'public', 'Public') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("group");
    let image = asset(&pool, "harbour", "image/jpeg", Some(group)).await;
    let video = asset(&pool, "reel", "video/mp4", Some(group)).await;
    let hidden = asset(&pool, "internal", "image/jpeg", None).await;

    let connector_id = Uuid::now_v7();
    let secret = "the-connector-secret".to_owned();
    let sealed = sealing()
        .seal(
            &Secret::new(secret.clone()),
            &connectors::associated_data(TENANT, connector_id),
        )
        .expect("seal");
    let identity: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, 'Site') RETURNING id",
    )
    .bind(format!("connector+{connector_id}@connectors.invalid"))
    .fetch_one(&pool)
    .await
    .expect("identity");
    let role_key = format!("connector:{connector_id}");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), $1, $1, '{asset:read}', ARRAY[$2], false)",
    )
    .bind(&role_key)
    .bind(group)
    .execute(&pool)
    .await
    .expect("role");
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, ARRAY[$3], false)",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(&role_key)
    .execute(&pool)
    .await
    .expect("membership");
    let key = auth::ApiKey::generate();
    let api_key_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES ($1, $2, $3, 'connector', $4, $5, '{}')",
    )
    .bind(api_key_id)
    .bind(tenant_id)
    .bind(identity)
    .bind(key.prefix())
    .bind(key.hash())
    .execute(&pool)
    .await
    .expect("api key");

    let mut conn = pool.acquire().await.expect("conn");
    connectors::register(
        &mut conn,
        &NewConnector {
            id: connector_id,
            kind: Kind::Drupal,
            label: "Site",
            site_url: "https://www.example.com",
            remote_version: None,
            api_key_id: Some(api_key_id),
            sealed_secret: &sealed,
            asset_group_ids: &[group],
            allow_all_groups: false,
            allow_original: false,
            allow_restore: false,
            config: serde_json::json!({}),
        },
    )
    .await
    .expect("register");
    drop(conn);

    let indexes = Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
        tempfile::tempdir().expect("tempdir").keep(),
    )));
    let search = Arc::new(dam_api::search::SearchState {
        global: pool.clone(),
        indexes,
        delivery: None,
    });
    let delivery = Arc::new(
        dam_api::delivery::DeliveryState::new(
            pool.clone(),
            Arc::new(dam_store::FakeS3Store::with_test_clock().0),
            Keyring::single("k1", Secret::new("a-signing-key".to_owned())),
            tenant_id,
        )
        .with_public_url(Some(ORIGIN.to_owned())),
    );

    Fixture {
        _pg: pg,
        pool: pool.clone(),
        group,
        app: router(OembedState {
            browse: Arc::new(BrowseState {
                search,
                global: pool.clone(),
                connectors: Some(ConnectorAuth {
                    sealing: sealing(),
                    tenant_slug: dam_core::TenantSlug::new(TENANT).expect("slug"),
                }),
            }),
            delivery: Some(delivery),
            public_url: Some(ORIGIN.to_owned()),
        }),
        api_key: key.into_plaintext(),
        connector_id,
        secret,
        image,
        video,
        hidden,
    }
}

async fn asset(pool: &PgPool, name: &str, mime: &str, group: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, width, height, version_group_id) \
         VALUES ($1, $2, $3, $4, 4096, 4000, 3000, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.bin"))
    .bind(mime)
    .execute(pool)
    .await
    .expect("asset");
    // A perpetual worldwide licence, so the rights check is not what refuses anything here.
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual) VALUES ($1, 'ok', 'royalty_free', true)",
    )
    .bind(license_id)
    .execute(pool)
    .await
    .expect("licence");
    sqlx::query(
        "INSERT INTO license_scopes (id, license_id, territories) \
         VALUES (gen_random_uuid(), $1, '{WORLD}')",
    )
    .bind(license_id)
    .execute(pool)
    .await
    .expect("scope");
    sqlx::query("INSERT INTO asset_licenses (asset_id, license_id) VALUES ($1, $2)")
        .bind(id)
        .bind(license_id)
        .execute(pool)
        .await
        .expect("attach");
    // The renditions, with their real `op_hash`es. Delivery resolves name -> profile -> op_hash -> row, so a
    // fixture inventing a hash is a fixture whose derivative is correctly never served.
    if mime.starts_with("image/") {
        for profile in dam_media::profiles::ALL {
            sqlx::query(
                "INSERT INTO derivatives                    (id, asset_id, role, profile, op_hash, object_key, mime, bytes, width, height)                  VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, 'image/jpeg', 5, $6, $7)",
            )
            .bind(id)
            .bind(profile.role)
            .bind(profile.name)
            .bind(profile.op_hash())
            .bind(format!("acme/p/{name}-{}", profile.name))
            .bind(i32::try_from(profile.rendition.width).unwrap_or(0))
            .bind(i32::try_from(profile.rendition.height).unwrap_or(0))
            .execute(pool)
            .await
            .expect("derivative");
        }
    }

    if let Some(group) = group {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(group)
            .bind(id)
            .execute(pool)
            .await
            .expect("member");
    }
    id
}

/// An asset in the connector's group with a licence and *no* derivatives.
async fn asset_without_renditions(f: &Fixture, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, width, height, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, 4000, 3000, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.bin"))
    .execute(&f.pool)
    .await
    .expect("asset");
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual) VALUES ($1, 'ok', 'royalty_free', true)",
    )
    .bind(license_id)
    .execute(&f.pool)
    .await
    .expect("licence");
    sqlx::query(
        "INSERT INTO license_scopes (id, license_id, territories) \
         VALUES (gen_random_uuid(), $1, '{WORLD}')",
    )
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
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(f.group)
        .bind(id)
        .execute(&f.pool)
        .await
        .expect("member");
    id
}

fn token(f: &Fixture) -> String {
    browse_token::sign(
        &Secret::new(f.secret.clone()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
    )
    .expect("sign")
}

async fn call(f: &Fixture, uri: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = f
        .app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
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

fn page_url(id: Uuid) -> String {
    urlencoding(&format!("{ORIGIN}/assets/{id}"))
}

fn urlencoding(value: &str) -> String {
    value.replace(':', "%3A").replace('/', "%2F")
}

async fn an_image_is_a_photo_with_a_signed_url(f: &Fixture) {
    let (status, body) = call(
        f,
        &format!("/oembed?url={}", page_url(f.image)),
        Some(&f.api_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "photo");
    assert_eq!(body["version"], "1.0");
    assert_eq!(body["title"], "harbour.bin");
    assert_eq!(body["provider_name"], "damrs");
    assert_eq!(body["provider_url"], ORIGIN);
    // A real, absolute delivery URL — not a bare token, which is the mistake `sign_preview` documents having
    // made once already.
    let url = body["url"].as_str().unwrap_or_default();
    assert!(url.starts_with(&format!("{ORIGIN}/d/")), "{url}");
    assert!(
        body["width"].is_number() && body["height"].is_number(),
        "{body}"
    );

    // Below the URL's lifetime. A consumer caching for a day would serve a broken image for most of it.
    let cache_age = body["cache_age"].as_u64().unwrap_or_default();
    assert!(cache_age > 0 && cache_age < 15 * 60, "{cache_age}");
}

async fn anything_that_is_not_an_image_is_a_link(f: &Fixture) {
    // Claiming `video` would need an embeddable player this does not have, and a `photo` whose url is an mp4
    // is a broken `<img>` on somebody's page.
    let (status, body) = call(
        f,
        &format!("/oembed?url={}", page_url(f.video)),
        Some(&f.api_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "link");
    assert_eq!(body["title"], "reel.bin");
    // No url, no dimensions — a link says what it is rather than shipping empty fields.
    assert!(body.get("url").is_none() || body["url"].is_null(), "{body}");
    assert!(
        body.get("width").is_none() || body["width"].is_null(),
        "{body}"
    );
}

async fn maxwidth_chooses_a_rendition_rather_than_scaling_one(f: &Fixture) {
    // The renditions are already rendered and already cached. Generating an arbitrary size per oEmbed request
    // would put a render in the path of pasting a URL.
    for (maxwidth, expected) in [
        (256, 256),
        (300, 256),
        (1024, 1024),
        (4000, 2048),
        (100, 256),
    ] {
        let (status, body) = call(
            f,
            &format!("/oembed?url={}&maxwidth={maxwidth}", page_url(f.image)),
            Some(&f.api_key),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["width"], expected,
            "maxwidth={maxwidth} should pick {expected}: {body}"
        );
    }
    // Asking for less than the smallest gets the smallest: `maxwidth` is a layout maximum, and a consumer that
    // cannot scale an image down has other problems.
}

async fn an_asset_the_connector_cannot_see_is_the_same_404_as_one_that_never_existed(f: &Fixture) {
    let (hidden_status, _) = call(
        f,
        &format!("/oembed?url={}", page_url(f.hidden)),
        Some(&f.api_key),
    )
    .await;
    let (invented_status, _) = call(
        f,
        &format!("/oembed?url={}", page_url(Uuid::now_v7())),
        Some(&f.api_key),
    )
    .await;
    assert_eq!(hidden_status, StatusCode::NOT_FOUND);
    assert_eq!(
        invented_status, hidden_status,
        "one answer for both, or the 404 confirms the asset exists"
    );
}

async fn a_url_from_another_provider_is_a_400_not_a_404(f: &Fixture) {
    // oEmbed consumers act on the difference: a 404 means "I know this URL and there is nothing there", a 400
    // means "that is not mine to answer for".
    for url in [
        format!("https://other.example.net/assets/{}", f.image),
        format!("{ORIGIN}/collections/{}", f.image),
        format!("{ORIGIN}/assets/not-a-uuid"),
        format!("{ORIGIN}/assets/{}/extra", f.image),
        "not-a-url".to_owned(),
    ] {
        let (status, body) = call(
            f,
            &format!("/oembed?url={}", urlencoding(&url)),
            Some(&f.api_key),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url}: {body}");
        assert_eq!(body["code"], "not_an_asset_url", "{url}");
    }
}

async fn a_format_this_provider_does_not_emit_is_a_501(f: &Fixture) {
    // The spec's status. Answering JSON under an XML content type would be a lie a consumer cannot detect.
    let (status, body) = call(
        f,
        &format!("/oembed?url={}&format=xml", page_url(f.image)),
        Some(&f.api_key),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
    assert_eq!(body["code"], "unsupported_format");

    // `json`, in any case, is fine — and so is omitting it.
    for format in ["json", "JSON"] {
        let (status, _) = call(
            f,
            &format!("/oembed?url={}&format={format}", page_url(f.image)),
            Some(&f.api_key),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{format}");
    }
}

async fn it_needs_a_credential_and_takes_either_of_the_two(f: &Fixture) {
    // The deviation from the spec, and the reason for it: an unauthenticated provider over a governed library
    // is an enumeration API.
    let (status, _) = call(f, &format!("/oembed?url={}", page_url(f.image)), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A token the site signed, for a browser-side editor plugin.
    let (status, body) = call(
        f,
        &format!("/oembed?url={}&token={}", page_url(f.image), token(f)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "photo");

    // And a forged one is the same 401 — not a 404, which would say the asset exists.
    let forged = browse_token::sign(
        &Secret::new("not-the-secret".to_owned()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
    )
    .expect("sign");
    let (status, _) = call(
        f,
        &format!("/oembed?url={}&token={forged}", page_url(f.image)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn an_asset_whose_rendition_is_not_rendered_yet_is_a_link_not_an_error(f: &Fixture) {
    // An ordinary state, not a fault: a fresh upload has no derivatives for a few seconds, and a reindex or a
    // redefined profile can leave one missing for longer. A consumer that pasted the URL of a real asset with a
    // real title is better served by a card than by a 500 it can do nothing about.
    let bare = asset_without_renditions(f, "just-uploaded").await;
    let (status, body) = call(
        f,
        &format!("/oembed?url={}", page_url(bare)),
        Some(&f.api_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], "link");
    assert_eq!(body["title"], "just-uploaded.bin");
    assert!(body.get("url").is_none() || body["url"].is_null(), "{body}");
}

#[tokio::test]
async fn the_oembed_contract_holds() {
    let f = fixture().await;

    an_image_is_a_photo_with_a_signed_url(&f).await;
    anything_that_is_not_an_image_is_a_link(&f).await;
    maxwidth_chooses_a_rendition_rather_than_scaling_one(&f).await;
    an_asset_the_connector_cannot_see_is_the_same_404_as_one_that_never_existed(&f).await;
    a_url_from_another_provider_is_a_400_not_a_404(&f).await;
    a_format_this_provider_does_not_emit_is_a_501(&f).await;
    it_needs_a_credential_and_takes_either_of_the_two(&f).await;
    an_asset_whose_rendition_is_not_rendered_yet_is_a_link_not_an_error(&f).await;
}
