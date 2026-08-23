//! A connected site renders its own URLs (M3d·2, §11.3).
//!
//! "Rendering never blocks on damrs": the remote signs transform URLs in PHP from the shared secret, so a damrs
//! outage degrades to stale-but-working pages rather than white screens. That means damrs verifies tokens signed
//! with a key it holds but did not use — and a site that signs its own URLs can sign *anything*.
//!
//! So the whole of this suite is about what happens after the signature checks out. Every case here is a way the
//! signing secret would otherwise be a bypass of what §11 claims the connector enforces:
//!
//! - **A preview purpose is refused.** `Purpose::InternalPreview` skips the rights check, deliberately, because
//!   an unlicensed asset is the normal state of a fresh upload and gating the grid on the distribution verdict
//!   makes a correct DAM unusable. A connector is a customer's public website. This is the bound that matters
//!   most: without it, a site signs `purpose=preview` and every licence check on every page is gone.
//! - **Originals are refused unless allowed**, and **assets outside the connector's groups are refused**, which
//!   is what makes §11.1's "a misconfigured Drupal view cannot surface an unapproved asset" true — a site that
//!   knows an id can sign for it whether or not it was ever shown one.
//! - **A cold original becomes the proxy, not a 202.** §11.1: "a page render must never trigger a Glacier
//!   restore." Refusing would blank an image on a live page for a reason the site cannot act on.
//! - **Pausing, revoking, or revoking the key stops URLs already signed.** The same property as a revoked
//!   share: the signature records what was asked for, never that it is still allowed.
//! - **A rotation does not break the site**, and once the window closes the old secret stops working.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_api::delivery::{self, CONNECTOR_KEY_PREFIX, DeliveryState};
use dam_core::Secret;
use dam_core::sealed::SealingKeyring;
use dam_core::signed_url::{DeliveryClaim, Keyring, Purpose};
use dam_db::connectors::{self, Kind, NewConnector};
use dam_db::{migrate, testing::PostgresHarness};
use dam_store::{BlobStore, FakeS3Store, Key};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const TENANT: &str = "acme";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

fn sealing() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a test sealing passphrase".to_owned()))
}

struct Fixture {
    _pg: PostgresHarness,
    pool: PgPool,
    store: FakeS3Store,
    app: axum::Router,
    state: DeliveryState,
    tenant_id: Uuid,
    clock: Arc<dam_core::TestClock>,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");

    let store = FakeS3Store::with_test_clock().0;
    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("tenant");

    let clock = Arc::new(dam_core::TestClock::new());
    clock.set(now());
    let state = DeliveryState::new(
        pool.clone(),
        Arc::new(store.clone()) as Arc<dyn BlobStore>,
        Keyring::single("k1", Secret::new("the-server-signing-key".to_owned())),
        tenant_id,
    )
    .with_clock(clock.clone())
    .with_connector_auth(sealing(), dam_core::TenantSlug::new(TENANT).expect("slug"));
    let app = delivery::router(state.clone());

    Fixture {
        _pg: pg,
        pool,
        store,
        app,
        state,
        tenant_id,
        clock,
    }
}

/// A connected site and its plaintext signing secret — what the remote would hold.
struct Site {
    id: Uuid,
    secret: String,
}

async fn connect(f: &Fixture, label: &str, options: Connect<'_>) -> Site {
    let id = Uuid::now_v7();
    let secret = format!("secret-for-{label}");
    let sealed = sealing()
        .seal(
            &Secret::new(secret.clone()),
            &connectors::associated_data(TENANT, id),
        )
        .expect("seal");

    // The scoping goes through the ordinary machinery — an identity, a membership, a role — exactly as
    // `POST /connectors` builds it. Reconstructed here rather than called, because this suite is about
    // delivery and driving registration would couple the two.
    let identity: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $2) RETURNING id",
    )
    .bind(format!("connector+{id}@connectors.invalid"))
    .bind(label)
    .fetch_one(&f.pool)
    .await
    .expect("identity");
    let role_key = format!("connector:{id}");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), $1, $1, '{asset:read}', $2, $3)",
    )
    .bind(&role_key)
    .bind(options.groups.to_vec())
    .bind(options.all_groups)
    .execute(&f.pool)
    .await
    .expect("role");
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, ARRAY[$3], false)",
    )
    .bind(f.tenant_id)
    .bind(identity)
    .bind(&role_key)
    .execute(&f.pool)
    .await
    .expect("membership");

    let api_key = dam_db::auth::ApiKey::generate();
    let api_key_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES ($1, $2, $3, $4, $5, $6, '{asset:read}')",
    )
    .bind(api_key_id)
    .bind(f.tenant_id)
    .bind(identity)
    .bind(label)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(&f.pool)
    .await
    .expect("api key");

    let mut conn = f.pool.acquire().await.expect("conn");
    connectors::register(
        &mut conn,
        &NewConnector {
            id,
            kind: Kind::Drupal,
            label,
            site_url: &format!("https://{label}.example.test"),
            remote_version: None,
            api_key_id: Some(api_key_id),
            sealed_secret: &sealed,
            asset_group_ids: options.groups,
            allow_all_groups: options.all_groups,
            allow_original: options.allow_original,
            allow_restore: options.allow_restore,
            config: serde_json::json!({}),
        },
    )
    .await
    .expect("register");

    Site { id, secret }
}

#[derive(Default)]
struct Connect<'a> {
    groups: &'a [Uuid],
    all_groups: bool,
    allow_original: bool,
    allow_restore: bool,
}

/// Signs a claim as the *site* would: with the connector's secret, under its own key id.
fn site_signs(site: &Site, asset_id: Uuid, transform: &str) -> String {
    signed_as(
        site,
        &site.secret,
        asset_id,
        transform,
        Purpose::Distribution,
        None,
    )
}

fn signed_as(
    site: &Site,
    secret: &str,
    asset_id: Uuid,
    transform: &str,
    purpose: Purpose,
    share_link_id: Option<Uuid>,
) -> String {
    let key_id = format!("{CONNECTOR_KEY_PREFIX}{}", site.id);
    dam_core::signed_url::sign(
        &Keyring::single(key_id.clone(), Secret::new(secret.to_owned())),
        &DeliveryClaim {
            purpose,
            asset_id,
            transform: transform.to_owned(),
            channel: "web".to_owned(),
            territory: "WORLD".to_owned(),
            identity_id: None,
            share_link_id,
            // Long, as a CMS would: a render URL cached in a page has to outlive the request that built it.
            expires_at: now() + Duration::days(1),
            key_id,
        },
    )
    .expect("sign")
}

/// An asset with an original object and a `web-2048` proxy, both present in the store.
async fn asset(f: &Fixture, label: &str, group: Option<Uuid>) -> Uuid {
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

    // No bytes are staged. Delivery mints a presigned URL and redirects; whether the object is there is the
    // store's answer to the browser, not this handler's — which is what the existing delivery suite relies on
    // too, and it keeps these cases about the bounds rather than about a fake store's contents.
    let profile = dam_media::profiles::by_name("web-2048").expect("a built-in profile");
    let derivative = Key::new(format!("acme/p/{label}-2048")).expect("key");
    sqlx::query(
        "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, 'image/jpeg', 5)",
    )
    .bind(id)
    .bind(profile.role)
    .bind(profile.name)
    .bind(profile.op_hash())
    .bind(derivative.as_str())
    .execute(&f.pool)
    .await
    .expect("derivative");

    if let Some(group) = group {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(group)
            .bind(id)
            .execute(&f.pool)
            .await
            .expect("group member");
    }

    // A perpetual worldwide licence, so the rights check is not what refuses anything here.
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
    id
}

async fn group(f: &Fixture, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(key)
    .fetch_one(&f.pool)
    .await
    .expect("group")
}

async fn archive_the_original(f: &Fixture, asset_id: Uuid, class: &str) {
    let content_hash: String = sqlx::query_scalar("SELECT content_hash FROM assets WHERE id = $1")
        .bind(asset_id)
        .fetch_one(&f.pool)
        .await
        .expect("hash");
    let pool_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.storage_pools \
           (id, tenant_id, name, driver, bucket, credentials_ref, storage_class, latency_class) \
         VALUES (gen_random_uuid(), NULL, $1, 's3', 'b', 'test', $2, 'hours') RETURNING id",
    )
    .bind(format!("cold-{asset_id}"))
    .bind(class)
    .fetch_one(&f.pool)
    .await
    .expect("pool");
    sqlx::query(
        "INSERT INTO object_placements \
           (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state) \
         VALUES ($1, $2, $3, 10, 'x', $4, 'present')",
    )
    .bind(format!(
        "acme/o/{}/{}",
        &content_hash[..2],
        &content_hash[2..4]
    ))
    .bind(pool_id)
    .bind(asset_id)
    .bind(class)
    .execute(&f.pool)
    .await
    .expect("placement");
}

async fn get(f: &Fixture, token: &str) -> axum::http::Response<Body> {
    f.app
        .clone()
        .oneshot(
            Request::get(format!("/d/{token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router")
}

async fn status(f: &Fixture, token: &str) -> StatusCode {
    get(f, token).await.status()
}

#[tokio::test]
async fn a_site_signed_url_is_served_and_every_bound_holds() {
    let f = fixture().await;
    let public = group(&f, "public").await;
    let private = group(&f, "private").await;
    let site = connect(
        &f,
        "marketing",
        Connect {
            groups: &[public],
            ..Default::default()
        },
    )
    .await;
    let shown = asset(&f, "on-the-homepage", Some(public)).await;
    let hidden = asset(&f, "not-for-the-site", Some(private)).await;

    // The happy path: a URL the site signed itself, with no API call in the render path.
    assert_eq!(
        status(&f, &site_signs(&site, shown, "web-2048")).await,
        StatusCode::FOUND,
        "a site-signed proxy URL is served",
    );

    // The bound that matters most. `InternalPreview` skips the rights check, so a site signing one would
    // remove every licence check from every page it renders.
    assert_eq!(
        status(
            &f,
            &signed_as(
                &site,
                &site.secret,
                shown,
                "web-2048",
                Purpose::InternalPreview,
                None
            )
        )
        .await,
        StatusCode::NOT_FOUND,
        "a connector must not claim an internal preview",
    );

    // A share's authority belongs to the share.
    assert_eq!(
        status(
            &f,
            &signed_as(
                &site,
                &site.secret,
                shown,
                "web-2048",
                Purpose::Distribution,
                Some(Uuid::new_v4()),
            )
        )
        .await,
        StatusCode::NOT_FOUND,
        "a connector must not claim a share link",
    );

    // `allow_original` is off, so the master is refused however the URL was signed.
    assert_eq!(
        status(&f, &site_signs(&site, shown, "original")).await,
        StatusCode::NOT_FOUND,
        "a site that may not have masters cannot sign for one",
    );

    // The §11.1 property. The site knows this id — a Drupal editor could have pasted it — and signing for it
    // must not be enough.
    assert_eq!(
        status(&f, &site_signs(&site, hidden, "web-2048")).await,
        StatusCode::NOT_FOUND,
        "an asset outside the connector's groups is not renderable by it",
    );

    // And one connector's secret does not sign another's URLs, even for an asset both may see.
    let other = connect(
        &f,
        "other",
        Connect {
            groups: &[public],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        status(
            &f,
            &signed_as(
                &site,
                &other.secret,
                shown,
                "web-2048",
                Purpose::Distribution,
                None
            )
        )
        .await,
        StatusCode::NOT_FOUND,
        "another connector's secret must not verify under this connector's id",
    );
}

#[tokio::test]
async fn a_cold_original_becomes_the_proxy_rather_than_a_wait() {
    let f = fixture().await;
    let site = connect(
        &f,
        "marketing",
        Connect {
            all_groups: true,
            allow_original: true,
            allow_restore: false,
            ..Default::default()
        },
    )
    .await;
    let id = asset(&f, "in-deep-archive", None).await;
    archive_the_original(&f, id, "DEEP_ARCHIVE").await;

    // §11.1: "a page render must never trigger a Glacier restore." A 202 with an ETA is the right answer for a
    // person who asked for a master and the wrong one for an `<img>` tag, and refusing would blank an image on
    // a live page for a reason the site cannot act on.
    let response = get(&f, &site_signs(&site, id, "original")).await;
    assert_eq!(response.status(), StatusCode::FOUND, "served, not 202");
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    // The proxy's object, not the original's. `acme/p/...` versus `acme/o/...`.
    assert!(
        location.contains("in-deep-archive-2048"),
        "expected the proxy, got {location}"
    );

    // With `allow_restore` on it is the ordinary answer again: an operator who asked for this wants the wait
    // and the cost estimate, not a silent substitution.
    let restoring = connect(
        &f,
        "restoring",
        Connect {
            all_groups: true,
            allow_original: true,
            allow_restore: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        status(&f, &site_signs(&restoring, id, "original")).await,
        StatusCode::ACCEPTED,
        "a connector that may restore gets the 202 with the ETA",
    );
}

#[tokio::test]
async fn pausing_revoking_or_killing_the_key_stops_urls_already_signed() {
    let f = fixture().await;
    let site = connect(
        &f,
        "marketing",
        Connect {
            all_groups: true,
            ..Default::default()
        },
    )
    .await;
    let id = asset(&f, "live", None).await;
    let token = site_signs(&site, id, "web-2048");
    assert_eq!(status(&f, &token).await, StatusCode::FOUND);

    // The same property as a revoked share: a signature records what was asked for, never that it is still
    // allowed. A paused connector's pages go blank *now*, which is what pausing is for.
    let mut conn = f.pool.acquire().await.expect("conn");
    connectors::set_status(&mut conn, site.id, connectors::Status::Paused)
        .await
        .expect("pause");
    assert_eq!(status(&f, &token).await, StatusCode::NOT_FOUND);

    connectors::set_status(&mut conn, site.id, connectors::Status::Active)
        .await
        .expect("resume");
    assert_eq!(status(&f, &token).await, StatusCode::FOUND, "and back");

    // An error state still renders: whatever went wrong is not a reason to blank somebody's home page.
    connectors::set_status(&mut conn, site.id, connectors::Status::Error)
        .await
        .expect("error");
    assert_eq!(status(&f, &token).await, StatusCode::FOUND);

    // Revoking the *API key* also stops the URLs. Otherwise revoking a connector's credential would leave its
    // render URLs working for as long as the site kept signing them, which is indefinitely.
    connectors::set_status(&mut conn, site.id, connectors::Status::Active)
        .await
        .expect("resume");
    sqlx::query("UPDATE dam_global.api_keys SET revoked_at = now() WHERE name = 'marketing'")
        .execute(&f.pool)
        .await
        .expect("revoke key");
    assert_eq!(status(&f, &token).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_rotation_does_not_break_the_site_and_the_window_eventually_closes() {
    let f = fixture().await;
    let site = connect(
        &f,
        "marketing",
        Connect {
            all_groups: true,
            ..Default::default()
        },
    )
    .await;
    let id = asset(&f, "live", None).await;
    // A URL the site signed before it heard about the rotation. Long-lived, as a cached page's would be.
    let old_token = site_signs(&site, id, "web-2048");

    let fresh = "the-rotated-secret";
    let sealed = sealing()
        .seal(
            &Secret::new(fresh.to_owned()),
            &connectors::associated_data(TENANT, site.id),
        )
        .expect("seal");
    let mut conn = f.pool.acquire().await.expect("conn");
    connectors::rotate(&mut conn, site.id, &sealed, true, now())
        .await
        .expect("rotate");

    // Both work during the window: the site has not deployed yet, and the DAM already has.
    assert_eq!(
        status(&f, &old_token).await,
        StatusCode::FOUND,
        "the superseded secret still verifies inside the window",
    );
    let new_token = signed_as(&site, fresh, id, "web-2048", Purpose::Distribution, None);
    assert_eq!(status(&f, &new_token).await, StatusCode::FOUND);

    // Past the window, only the current one. Enforced by the clock rather than by clearing a column — the
    // superseded secret is still in the row.
    f.clock
        .set(now() + connectors::SECRET_GRACE + Duration::hours(1));
    assert_eq!(
        status(&f, &old_token).await,
        StatusCode::NOT_FOUND,
        "the window closed",
    );
    // The new token has expired too by then, so re-sign at the later clock to prove the *secret* still works.
    let later = dam_core::signed_url::sign(
        &Keyring::single(
            format!("{CONNECTOR_KEY_PREFIX}{}", site.id),
            Secret::new(fresh.to_owned()),
        ),
        &DeliveryClaim {
            purpose: Purpose::Distribution,
            asset_id: id,
            transform: "web-2048".to_owned(),
            channel: "web".to_owned(),
            territory: "WORLD".to_owned(),
            identity_id: None,
            share_link_id: None,
            expires_at: now() + connectors::SECRET_GRACE + Duration::days(1),
            key_id: format!("{CONNECTOR_KEY_PREFIX}{}", site.id),
        },
    )
    .expect("sign");
    assert_eq!(status(&f, &later).await, StatusCode::FOUND);

    // A leak rotation closes it immediately instead.
    f.clock.set(now());
    let after_leak = "the-emergency-secret";
    let sealed = sealing()
        .seal(
            &Secret::new(after_leak.to_owned()),
            &connectors::associated_data(TENANT, site.id),
        )
        .expect("seal");
    connectors::rotate(&mut conn, site.id, &sealed, false, now())
        .await
        .expect("rotate");
    assert_eq!(
        status(&f, &new_token).await,
        StatusCode::NOT_FOUND,
        "a leak rotation stops the superseded secret at once",
    );
}

#[tokio::test]
async fn a_token_naming_a_connector_that_does_not_exist_is_the_same_flat_refusal() {
    let f = fixture().await;
    let site = connect(
        &f,
        "marketing",
        Connect {
            all_groups: true,
            ..Default::default()
        },
    )
    .await;
    let id = asset(&f, "live", None).await;

    // An invented connector id, an unparseable one, and a token signed with the *server's* key but claiming a
    // connector id. All the same 404 an unsigned token gets: distinguishing them would say which connectors
    // exist and what state each is in.
    let invented = Site {
        id: Uuid::new_v4(),
        secret: site.secret.clone(),
    };
    assert_eq!(
        status(&f, &site_signs(&invented, id, "web-2048")).await,
        StatusCode::NOT_FOUND,
    );

    let nonsense = dam_core::signed_url::sign(
        &Keyring::single("connector:not-a-uuid", Secret::new("x".to_owned())),
        &DeliveryClaim {
            purpose: Purpose::Distribution,
            asset_id: id,
            transform: "web-2048".to_owned(),
            channel: "web".to_owned(),
            territory: "WORLD".to_owned(),
            identity_id: None,
            share_link_id: None,
            expires_at: now() + Duration::hours(1),
            key_id: "connector:not-a-uuid".to_owned(),
        },
    )
    .expect("sign");
    assert_eq!(status(&f, &nonsense).await, StatusCode::NOT_FOUND);

    // And the reverse: the server's own keyring must not verify a token claiming a connector id, because the
    // bounds are selected by that id. A token that verified under the server key while naming a connector
    // would skip every check in `bound_by_connector`.
    let borrowed = dam_core::signed_url::sign(
        &Keyring::single(
            format!("{CONNECTOR_KEY_PREFIX}{}", site.id),
            Secret::new("the-server-signing-key".to_owned()),
        ),
        &DeliveryClaim {
            purpose: Purpose::InternalPreview,
            asset_id: id,
            transform: "original".to_owned(),
            channel: "web".to_owned(),
            territory: "WORLD".to_owned(),
            identity_id: None,
            share_link_id: None,
            expires_at: now() + Duration::hours(1),
            key_id: format!("{CONNECTOR_KEY_PREFIX}{}", site.id),
        },
    )
    .expect("sign");
    assert_eq!(status(&f, &borrowed).await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_connector_auth_a_connector_token_is_refused_rather_than_falling_back() {
    // The fail-closed default. A deployment that cannot open connector secrets must refuse their tokens: a
    // fallback to the server keyring would verify them against a key the site never had — which fails, until
    // somebody "fixes" it by trying both.
    let f = fixture().await;
    let site = connect(
        &f,
        "marketing",
        Connect {
            all_groups: true,
            ..Default::default()
        },
    )
    .await;
    let id = asset(&f, "live", None).await;
    let token = site_signs(&site, id, "web-2048");
    assert_eq!(
        status(&f, &token).await,
        StatusCode::FOUND,
        "with auth configured"
    );

    let without = DeliveryState::new(
        f.pool.clone(),
        Arc::new(f.store.clone()) as Arc<dyn BlobStore>,
        Keyring::single("k1", Secret::new("the-server-signing-key".to_owned())),
        f.tenant_id,
    )
    .with_clock(f.clock.clone());
    let app = delivery::router(without);
    let response = app
        .oneshot(
            Request::get(format!("/d/{token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let _ = &f.state;
}
