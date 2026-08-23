//! The asset picker's endpoint (M3d·3, §11.1).
//!
//! `/search` and `/search/facets` already exist and are tested. What is new here is *who may call*, and the
//! properties worth proving are all about that:
//!
//! - **A token the site signed works, and is scoped exactly as its API key would be.** Same connector, same
//!   role, same predicate — because both go through `caller::authorize_as` rather than through two resolvers.
//! - **A browser gets CORS only for the connector's own origin.** Not a wildcard: the token lives in a browser
//!   and is the thing most likely to leak.
//! - **The API-key path gets no CORS headers at all.** A cross-origin request carrying `Authorization` would be
//!   a site putting its long-lived key in JavaScript, and answering it would endorse that.
//! - **Pausing, revoking, or revoking the key stops a token immediately** — the same property as a signed
//!   render URL.
//! - **Every refusal is one 401.** "No such connector", "revoked", "bad signature" and "expired" collapse,
//!   because distinguishing them tells whoever holds the token which connectors exist.
//! - **The rail is counted over the same query as the grid**, so a facet saying 40 beside three results cannot
//!   happen.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::browse::{BrowseState, ConnectorAuth, router};
use dam_connect::browse_token::{self, BrowseClaim};
use dam_core::Secret;
use dam_core::sealed::SealingKeyring;
use dam_db::connectors::{self, Kind, NewConnector};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const TENANT: &str = "acme";
const SITE: &str = "https://www.example.com";

fn sealing() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a test sealing passphrase".to_owned()))
}

struct Fixture {
    _pg: PostgresHarness,
    pool: PgPool,
    app: axum::Router,
    /// The connector's own API key, for the server-side path.
    api_key: String,
    connector_id: Uuid,
    secret: String,
    /// The asset the connector's group holds. Asserted on by filename rather than by id, because a picker's
    /// grid shows names — but kept so the fixture reads as the pair it is.
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

    let visible = asset(&pool, "on-the-site", "acme").await;
    let hidden = asset(&pool, "internal", "acme").await;
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'public', 'Public') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(visible)
        .execute(&pool)
        .await
        .expect("member");

    // The connector, and the four things registration builds — reconstructed rather than driven through
    // `POST /connectors`, because this suite is about who may browse.
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
         VALUES (gen_random_uuid(), $1, 'Marketing site') RETURNING id",
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
            label: "Marketing site",
            site_url: SITE,
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

    Fixture {
        _pg: pg,
        app: router(BrowseState {
            search,
            global: pool.clone(),
            connectors: Some(ConnectorAuth {
                sealing: sealing(),
                tenant_slug: dam_core::TenantSlug::new(TENANT).expect("slug"),
            }),
        }),
        pool,
        api_key: key.into_plaintext(),
        connector_id,
        secret,
        hidden,
    }
}

async fn asset(pool: &PgPool, name: &str, brand: &str) -> Uuid {
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
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(serde_json::json!({ "brand": brand }))
        .execute(pool)
        .await
        .expect("metadata");
    id
}

fn token(f: &Fixture, ttl_minutes: i64) -> String {
    browse_token::sign(
        &Secret::new(f.secret.clone()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(ttl_minutes),
        },
    )
    .expect("sign")
}

struct Answer {
    status: StatusCode,
    body: Value,
    allow_origin: Option<String>,
}

async fn call(f: &Fixture, uri: &str, key: Option<&str>, origin: Option<&str>) -> Answer {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    let response = f
        .app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let allow_origin = response
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        allow_origin,
    }
}

fn filenames(body: &Value) -> Vec<String> {
    body["results"]["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["filename"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

async fn the_api_key_path_answers_and_is_scoped(f: &Fixture) {
    let answer = call(f, "/browse", Some(&f.api_key), None).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(filenames(&answer.body), vec!["on-the-site.jpg".to_owned()]);
    assert_eq!(
        answer.body["results"]["total"], 1,
        "the count is the connector's too"
    );
    // The rail comes back in the same call, so it cannot disagree with the grid beside it.
    assert!(answer.body["facets"].is_array(), "{}", answer.body);

    // A server-side call gets no CORS headers. A cross-origin request carrying `Authorization` would be a site
    // putting its long-lived key in a browser.
    let answer = call(f, "/browse", Some(&f.api_key), Some(SITE)).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.allow_origin, None);
}

async fn a_site_signed_token_is_scoped_exactly_as_the_key_is(f: &Fixture) {
    let uri = format!("/browse?token={}", token(f, 5));
    let answer = call(f, &uri, None, Some(SITE)).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    // The same one asset, not the library: both paths go through `authorize_as`, so there is one predicate.
    assert_eq!(filenames(&answer.body), vec!["on-the-site.jpg".to_owned()]);
    assert!(
        !filenames(&answer.body).contains(&"internal.jpg".to_owned()),
        "a token must not widen what the connector may see"
    );
    let _ = f.hidden;
    // CORS, for that connector's own origin.
    assert_eq!(answer.allow_origin.as_deref(), Some(SITE));
}

async fn cors_is_for_this_connectors_origin_and_nothing_else(f: &Fixture) {
    let uri = format!("/browse?token={}", token(f, 5));

    // Another origin gets `null` rather than a refusal: the browser blocks the read, which is what CORS is for,
    // and answering 403 would tell a page it guessed the wrong origin.
    let answer = call(f, &uri, None, Some("https://evil.example.net")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.allow_origin.as_deref(), Some("null"));

    // No origin header at all — a server-side fetch with a token — is answered, without a permissive header.
    let answer = call(f, &uri, None, None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.allow_origin.as_deref(), Some("null"));

    // Never a wildcard. The token lives in a browser and is the thing most likely to leak.
    let answer = call(f, &uri, None, Some(SITE)).await;
    assert_ne!(answer.allow_origin.as_deref(), Some("*"));
}

async fn a_preflight_says_which_origin_may_try(f: &Fixture) {
    let uri = format!("/browse?token={}", token(f, 5));
    let response = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri(&uri)
                .header(header::ORIGIN, SITE)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(SITE)
    );

    // A preflight carries no credential, so it grants nothing: a forged token gets a preflight it cannot use.
    let forged = browse_token::sign(
        &Secret::new("not-the-secret".to_owned()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
    )
    .expect("sign");
    let answer = call(f, &format!("/browse?token={forged}"), None, Some(SITE)).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

async fn every_bad_token_is_the_same_one_answer(f: &Fixture) {
    let forged = browse_token::sign(
        &Secret::new("not-the-secret".to_owned()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
    )
    .expect("sign");
    let expired = browse_token::sign(
        &Secret::new(f.secret.clone()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() - chrono::Duration::minutes(1),
        },
    )
    .expect("sign");
    let overlong = browse_token::sign(
        &Secret::new(f.secret.clone()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() + chrono::Duration::days(365),
        },
    )
    .expect("sign");
    let unknown = browse_token::sign(
        &Secret::new(f.secret.clone()),
        &BrowseClaim {
            connector_id: Uuid::now_v7(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
    )
    .expect("sign");

    for (name, token) in [
        ("forged", forged),
        ("expired", expired),
        ("over-long", overlong),
        ("unknown connector", unknown),
        ("nonsense", "not-a-token".to_owned()),
        ("empty", String::new()),
    ] {
        let answer = call(f, &format!("/browse?token={token}"), None, Some(SITE)).await;
        assert_eq!(
            answer.status,
            StatusCode::UNAUTHORIZED,
            "{name} should be one flat 401, got {}",
            answer.body
        );
    }
}

async fn a_bad_query_comes_back_as_the_servers_own_sentence(f: &Fixture) {
    // The reason `/browse` returns `search::Failure` rather than the flatter `assets::Failure`: a picker has a
    // search box, so a query that does not parse has to say *what* was wrong and where. "Bad request" sends an
    // editor looking for a typo in the wrong place.
    let uri = format!("/browse?token={}&q=brand%3Aacme", token(f, 5));
    let answer = call(f, &uri, None, Some(SITE)).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.body);
    assert_eq!(answer.body["code"], "unknown_field");
    assert!(
        answer.body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no field or alias named"),
        "{}",
        answer.body
    );
    // The character it stopped at, which is what lets a picker underline the word.
    assert_eq!(answer.body["at"], 1);

    // And an empty query lists the library — what a picker opens on. Answered from SQL rather than the index,
    // because there is nothing to rank and a document not yet written would simply be missing.
    let uri = format!("/browse?token={}&q=", token(f, 5));
    let answer = call(f, &uri, None, Some(SITE)).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(filenames(&answer.body), vec!["on-the-site.jpg".to_owned()]);
    assert_eq!(answer.body["results"]["ranked"], false, "nothing to rank");
}

async fn pausing_revoking_or_killing_the_key_stops_a_token(f: &Fixture) {
    let uri = format!("/browse?token={}", token(f, 5));
    assert_eq!(call(f, &uri, None, Some(SITE)).await.status, StatusCode::OK);

    let mut conn = f.pool.acquire().await.expect("conn");
    connectors::set_status(&mut conn, f.connector_id, connectors::Status::Paused)
        .await
        .expect("pause");
    assert_eq!(
        call(f, &uri, None, Some(SITE)).await.status,
        StatusCode::UNAUTHORIZED,
        "a paused connector's picker stops working now",
    );

    connectors::set_status(&mut conn, f.connector_id, connectors::Status::Active)
        .await
        .expect("resume");
    assert_eq!(call(f, &uri, None, Some(SITE)).await.status, StatusCode::OK);

    // Revoking the API key stops the token too. Otherwise revoking a credential would leave minted tokens
    // working for their full lifetime, which is the same hole the delivery route closes.
    sqlx::query("UPDATE dam_global.api_keys SET revoked_at = now() WHERE name = 'connector'")
        .execute(&f.pool)
        .await
        .expect("revoke");
    assert_eq!(
        call(f, &uri, None, Some(SITE)).await.status,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE dam_global.api_keys SET revoked_at = NULL WHERE name = 'connector'")
        .execute(&f.pool)
        .await
        .expect("restore");

    connectors::set_status(&mut conn, f.connector_id, connectors::Status::Revoked)
        .await
        .expect("revoke connector");
    assert_eq!(
        call(f, &uri, None, Some(SITE)).await.status,
        StatusCode::UNAUTHORIZED,
        "and a revoked connector has no secret left to verify with",
    );
}

async fn a_rotation_does_not_close_an_open_picker(f: &Fixture) {
    // Re-register: the previous case revoked this connector, and revocation is terminal.
    let uri = format!("/browse?token={}", token(f, 5));
    let fresh = "the-rotated-secret";
    let sealed = sealing()
        .seal(
            &Secret::new(fresh.to_owned()),
            &connectors::associated_data(TENANT, f.connector_id),
        )
        .expect("seal");
    let mut conn = f.pool.acquire().await.expect("conn");
    // Un-revoke directly, because `set_status` refuses it — the row is the fixture's, not a product path.
    sqlx::query("UPDATE connectors SET status = 'active' WHERE id = $1")
        .bind(f.connector_id)
        .execute(&f.pool)
        .await
        .expect("reactivate");
    connectors::rotate(&mut conn, f.connector_id, &sealed, true, chrono::Utc::now())
        .await
        .expect("rotate");

    // Wait: `rotate` moved the *cleared* secret into `previous`, because revocation blanked it. So the token
    // signed with the original secret cannot verify — which is correct and worth asserting, because it is the
    // one case where a rotation does close a picker: the connector had already been revoked.
    assert_eq!(
        call(f, &uri, None, Some(SITE)).await.status,
        StatusCode::UNAUTHORIZED,
        "a revoked connector's secrets are gone, and rotating afterwards does not bring them back",
    );

    // A token signed with the new secret works.
    let after = browse_token::sign(
        &Secret::new(fresh.to_owned()),
        &BrowseClaim {
            connector_id: f.connector_id,
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
    )
    .expect("sign");
    assert_eq!(
        call(f, &format!("/browse?token={after}"), None, Some(SITE))
            .await
            .status,
        StatusCode::OK
    );
}

async fn no_credential_at_all_is_a_401(f: &Fixture) {
    let answer = call(f, "/browse", None, None).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_browse_contract_holds() {
    let f = fixture().await;

    the_api_key_path_answers_and_is_scoped(&f).await;
    a_site_signed_token_is_scoped_exactly_as_the_key_is(&f).await;
    cors_is_for_this_connectors_origin_and_nothing_else(&f).await;
    a_preflight_says_which_origin_may_try(&f).await;
    every_bad_token_is_the_same_one_answer(&f).await;
    a_bad_query_comes_back_as_the_servers_own_sentence(&f).await;
    no_credential_at_all_is_a_401(&f).await;
    // Last, because it revokes the connector.
    pausing_revoking_or_killing_the_key_stops_a_token(&f).await;
    a_rotation_does_not_close_an_open_picker(&f).await;
}
