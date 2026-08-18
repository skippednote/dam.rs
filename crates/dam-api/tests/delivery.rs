//! Signed delivery (3.1) — the one chokepoint every download passes through.
//!
//! D12 says rights are enforced at the point of distribution rather than recorded and hoped for. The
//! property that makes that true is not that the handler checks rights — it is **when** it checks them.
//!
//! A signed URL proves we issued this exact request unaltered. It does not prove entitlement. Rights are
//! evaluated at delivery, so a URL issued on Monday under a valid licence stops working on Tuesday when the
//! licence lapses. If the signature authorised, every URL ever issued would be an outstanding grant that
//! nothing could withdraw — and the same mechanism is what 3.3's revocation-on-an-issued-URL depends on.
//!
//! One container; the cases are functions over a borrowed fixture.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_api::delivery::{self, DeliveryState};
use dam_core::Secret;
use dam_core::rights_eval::Usage;
use dam_core::signed_url::Keyring;
use dam_db::{migrate, testing::PostgresHarness};
use dam_store::{BlobStore, FakeS3Store, Key};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

struct Fixture {
    _pg: PostgresHarness,
    pool: PgPool,
    state: DeliveryState,
    app: axum::Router,
    tenant_id: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");

    let store: Arc<dyn BlobStore> = Arc::new(FakeS3Store::with_test_clock().0);
    let keyring = Keyring::single("k1", Secret::new("a-signing-key".to_owned()));
    let tenant_id = Uuid::from_u128(0xacc0);
    // A fixed clock, so "expired" and "lapsed" mean the same thing to the test and the handler. With the
    // handler reading the wall clock, a token minted one second before the fixture's `now()` was still in
    // the *future* in real time, and the expiry case passed a 302 while claiming to test a 404.
    let clock = Arc::new(dam_core::TestClock::new());
    clock.set(now());
    let state =
        DeliveryState::new(pool.clone(), store, keyring, tenant_id).with_clock(clock.clone());
    let app = delivery::router(state.clone());

    Fixture {
        _pg: pg,
        pool,
        state,
        app,
        tenant_id,
    }
}

/// An asset with an original object and a `web-2048` derivative, both present in the store.
async fn asset_with_bytes(f: &Fixture, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    // A real BLAKE3 hex digest, because `Key::original` validates it — the key is *derived* from the hash
    // rather than stored, which is what makes a delivery URL unable to name an object the hash does not
    // account for.
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

    // The `op_hash` must be the profile's real one. Delivery resolves name -> profile -> op_hash -> row, so
    // a fixture inventing a hash is a fixture whose derivative is correctly never served — which is how
    // this test caught the change.
    let profile = dam_media::profiles::by_name("web-2048").expect("a built-in profile");
    let derivative = format!("acme/p/{label}-2048");
    sqlx::query(
        "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, 'image/jpeg', 5)",
    )
    .bind(id)
    .bind(profile.role)
    .bind(profile.name)
    .bind(profile.op_hash())
    .bind(&derivative)
    .execute(&f.pool)
    .await
    .expect("derivative");
    id
}

/// Attaches a perpetual worldwide licence, optionally ending at `ends_at`.
async fn licence(f: &Fixture, asset_id: Uuid, ends_at: Option<DateTime<Utc>>) {
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual, ends_at) \
         VALUES ($1, 'worldwide', 'royalty_free', $2, $3)",
    )
    .bind(license_id)
    .bind(ends_at.is_none())
    .bind(ends_at)
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
        .bind(asset_id)
        .bind(license_id)
        .execute(&f.pool)
        .await
        .expect("attach");
}

fn web() -> Usage {
    Usage {
        channel: "web".to_owned(),
        territory: "WORLD".to_owned(),
    }
}

async fn get(app: &axum::Router, token: &str) -> axum::http::Response<Body> {
    app.clone()
        .oneshot(
            Request::get(format!("/d/{token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router")
}

// ─── the happy path ─────────────────────────────────────────────────────────

async fn a_signed_url_for_a_licensed_asset_redirects_to_the_object(f: &Fixture) {
    let id = asset_with_bytes(f, "allowed").await;
    licence(f, id, None).await;

    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::minutes(10),
        now(),
    )
    .await
    .expect("issue");

    let response = get(&f.app, &token).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("a Location header");
    assert!(
        location.contains("acme/p/allowed-2048"),
        "the redirect must point at the derivative, got {location}"
    );

    // Never cached by a shared cache: the URL embeds a credential and the verdict behind it can change at
    // any moment, so a proxy holding this redirect would serve access after it was withdrawn.
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .expect("Cache-Control");
    assert!(
        cache.contains("no-store") && cache.contains("private"),
        "got {cache}"
    );
}

async fn the_transform_selects_which_object_is_served(f: &Fixture) {
    // The transform is part of the signature and is resolved against `derivatives` rather than trusted as
    // a path. A transform that reached the key builder directly would be a path-traversal parameter that
    // *we had signed* — the signature would make it harder to notice, not safer.
    let id = asset_with_bytes(f, "transforms").await;
    licence(f, id, None).await;

    let original_key = Key::original(f.tenant_id, &blake3::hash(b"transforms").to_hex())
        .expect("a derived original key");
    for (transform, expected) in [
        ("original", original_key.as_str().to_owned()),
        ("web-2048", "acme/p/transforms-2048".to_owned()),
    ] {
        let token = delivery::issue(
            &f.state,
            id,
            transform,
            &web(),
            None,
            Duration::minutes(10),
            now(),
        )
        .await
        .expect("issue");
        let response = get(&f.app, &token).await;
        assert_eq!(response.status(), StatusCode::FOUND, "{transform}");
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .expect("location");
        assert!(
            location.contains(&expected),
            "{transform} gave {location}, expected it to name {expected}"
        );
    }
}

async fn a_redefined_profile_misses_the_cache_instead_of_serving_stale_bytes(f: &Fixture) {
    // The property 3.2 exists for, and the bug 3.1 shipped with. `op_hash` covers size, format, quality,
    // fit, background, colour profile and rendering intent (§18.1). A lookup by profile *name* would keep
    // serving bytes rendered under the old definition forever — no error, nothing in a log, and a customer
    // seeing yesterday's quality setting indefinitely.
    //
    // Simulated by storing a derivative under a *different* recipe's hash, which is what an existing row
    // becomes the moment a profile is redefined.
    let id = asset_with_bytes(f, "redefined").await;
    licence(f, id, None).await;

    // Serving works while the recipe matches.
    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::minutes(10),
        now(),
    )
    .await
    .expect("issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);

    // Now the profile changes. The stored row keeps its old hash, so the current recipe has no derivative.
    let profile = dam_media::profiles::by_name("web-2048").expect("profile");
    let mut redefined = *profile;
    redefined.revision += 1;
    assert_ne!(
        redefined.op_hash(),
        profile.op_hash(),
        "a revision bump must move the hash, or this test proves nothing"
    );

    sqlx::query("UPDATE derivatives SET op_hash = $2 WHERE asset_id = $1")
        .bind(id)
        .bind(redefined.op_hash())
        .execute(&f.pool)
        .await
        .expect("simulate a redefinition");

    assert_eq!(
        get(&f.app, &token).await.status(),
        StatusCode::NOT_FOUND,
        "a derivative rendered under a superseded recipe must not be served"
    );
}

async fn a_serve_is_recorded_at_most_once_an_hour(f: &Fixture) {
    // The lifecycle engine reads `last_served_at` to decide what is cold enough to evict. Writing it on
    // every delivery turns the hottest read path in the system into a write, costing a row of WAL per
    // download — the same argument `auth::LAST_USED_RESOLUTION` makes about API keys.
    let id = asset_with_bytes(f, "served").await;
    licence(f, id, None).await;
    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::minutes(10),
        now(),
    )
    .await
    .expect("issue");

    for _ in 0..3 {
        assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);
    }

    let served: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_served_at FROM derivatives WHERE asset_id = $1")
            .bind(id)
            .fetch_one(&f.pool)
            .await
            .expect("last_served_at");
    assert_eq!(
        served,
        Some(now()),
        "the first delivery records the serve at the clock's instant"
    );

    // A second write inside the window is refused, which is what makes the throttle observable.
    let derivative_id: Uuid = sqlx::query_scalar("SELECT id FROM derivatives WHERE asset_id = $1")
        .bind(id)
        .fetch_one(&f.pool)
        .await
        .expect("id");
    assert!(
        !dam_db::derivatives::mark_served(&f.pool, derivative_id, now())
            .await
            .expect("mark"),
        "a second serve inside the resolution window must not write"
    );
    assert!(
        dam_db::derivatives::mark_served(
            &f.pool,
            derivative_id,
            now() + dam_db::derivatives::SERVED_RESOLUTION + Duration::seconds(1)
        )
        .await
        .expect("mark"),
        "and a serve past the window must"
    );
}

async fn an_unknown_transform_is_not_deliverable(f: &Fixture) {
    // Signed by us, so the signature verifies — and `print-a3` is not a built-in profile at all. It answers
    // 404, the same as an unsigned token, so a caller cannot enumerate which profiles exist by watching the
    // status change.
    let id = asset_with_bytes(f, "no-such-profile").await;
    licence(f, id, None).await;

    let token = delivery::issue(
        &f.state,
        id,
        "print-a3",
        &web(),
        None,
        Duration::minutes(10),
        now(),
    )
    .await
    .expect("issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::NOT_FOUND);
}

// ─── the D12 property ───────────────────────────────────────────────────────

async fn a_valid_signature_over_an_unlicensed_asset_is_still_refused(f: &Fixture) {
    // The heart of it. The signature is ours and unaltered; the asset has no licence. If the signature
    // authorised, this would serve bytes.
    let id = asset_with_bytes(f, "unlicensed").await;
    // No licence attached.

    // Issuing already refuses, which is the first half — a link that looks valid in an email and fails when
    // clicked is worse than an error in front of the person who can fix it.
    let refused = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::minutes(10),
        now(),
    )
    .await;
    assert!(refused.is_err(), "issuing must refuse an unlicensed asset");

    // And a token minted directly — as one issued before the licence was removed would be — is refused at
    // delivery. This is the half that matters.
    let token = mint_directly(f, id, "web-2048", &web(), now() + Duration::minutes(10));
    let response = get(&f.app, &token).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let body = body_json(response).await;
    assert_eq!(body["error"], "rights_denied");
    assert_eq!(body["rights_state"], "unknown");
    assert_eq!(
        body["reasons"][0], "no_license",
        "a customer who cannot download their own asset needs to know why: {body}"
    );
}

async fn a_licence_that_lapses_after_issue_stops_an_already_issued_url(f: &Fixture) {
    // The property revocation depends on. The URL was valid when minted and the licence ends before it is
    // used — nothing about the token changes, and it must stop working.
    let id = asset_with_bytes(f, "lapsing").await;
    licence(f, id, Some(now() + Duration::days(2))).await;

    // Valid at issue.
    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        // Deliberately outliving the licence, which is exactly the case that must not work later.
        Duration::hours(20),
        now(),
    )
    .await
    .expect("issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);

    // Now expire the licence by moving it into the past, which is what a lapse looks like to the evaluator.
    sqlx::query("UPDATE licenses SET ends_at = $1, perpetual = false")
        .bind(now() - Duration::days(1))
        .execute(&f.pool)
        .await
        .expect("lapse");
    dam_db::rights::invalidate(&f.pool, id)
        .await
        .expect("invalidate");

    let response = get(&f.app, &token).await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "the same token, unchanged, must stop working once the licence has lapsed"
    );
    let body = body_json(response).await;
    assert_eq!(body["rights_state"], "denied");
}

async fn a_legal_hold_stops_delivery_of_an_already_issued_url(f: &Fixture) {
    // A legal hold arrives as an instruction to stop distributing *now*, so it has to bite on URLs already
    // in circulation. This is the same mechanism as a lapse, reached a different way.
    let id = asset_with_bytes(f, "held").await;
    licence(f, id, None).await;
    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::hours(20),
        now(),
    )
    .await
    .expect("issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);

    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .expect("hold");
    dam_db::rights::invalidate(&f.pool, id)
        .await
        .expect("invalidate");

    let response = get(&f.app, &token).await;
    assert_eq_forbidden(&response);
    let body = body_json(response).await;
    assert_eq!(body["reasons"][0], "legal_hold");
}

async fn the_channel_in_the_token_selects_which_licence_terms_apply(f: &Fixture) {
    // Signed, so it cannot be edited — and it decides the answer. A licence scoped to editorial must not
    // deliver an advertising request, and the only reason it cannot is that the channel is inside the
    // signature.
    let id = asset_with_bytes(f, "editorial-only").await;
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual) \
         VALUES ($1, 'editorial', 'editorial_only', true)",
    )
    .bind(license_id)
    .execute(&f.pool)
    .await
    .expect("licence");
    sqlx::query(
        "INSERT INTO license_scopes (id, license_id, territories, channels) \
         VALUES (gen_random_uuid(), $1, '{WORLD}', '{editorial}')",
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

    let editorial = Usage {
        channel: "editorial".to_owned(),
        territory: "WORLD".to_owned(),
    };
    let allowed = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &editorial,
        None,
        Duration::minutes(10),
        now(),
    )
    .await
    .expect("editorial is licensed");
    assert_eq!(get(&f.app, &allowed).await.status(), StatusCode::FOUND);

    // The advertising request is refused, and a token minted for it directly is refused at delivery too.
    let advertising = Usage {
        channel: "advertising".to_owned(),
        territory: "WORLD".to_owned(),
    };
    assert!(
        delivery::issue(
            &f.state,
            id,
            "web-2048",
            &advertising,
            None,
            Duration::minutes(10),
            now()
        )
        .await
        .is_err()
    );
    let forged_channel = mint_directly(
        f,
        id,
        "web-2048",
        &advertising,
        now() + Duration::minutes(10),
    );
    assert_eq_forbidden(&get(&f.app, &forged_channel).await);
}

// ─── share links ────────────────────────────────────────────────────────────

async fn revoking_a_share_stops_a_url_it_already_issued(f: &Fixture) {
    // The requirement TASKS.md names for 3.3, and the reason the share id is inside the signature.
    //
    // Resolving the share token per request makes revoking the *share page* immediate. But a share mints
    // delivery URLs, and one of those is valid for its own TTL — so without re-checking, revoking a share
    // would leave every outstanding download URL working for up to a day. "Revoke" would mean "revoke,
    // eventually", which is not what anybody means when they revoke a link they sent to the wrong client.
    let id = asset_with_bytes(f, "shared").await;
    licence(f, id, None).await;

    let share = dam_db::shares::create(
        &f.pool,
        &dam_db::shares::ShareSpec {
            kind: "asset",
            target_id: Some(id),
            search_query: None,
            passcode: None,
            expires_at: None,
            max_downloads: None,
            allow_original: false,
            requires_eula: false,
            created_by: None,
        },
    )
    .await
    .expect("create a share");

    let token = delivery::issue_for_share(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Some(share.id),
        // Deliberately long-lived, which is exactly the case revocation has to reach.
        Duration::hours(20),
        now(),
    )
    .await
    .expect("issue");

    assert_eq!(
        get(&f.app, &token).await.status(),
        StatusCode::FOUND,
        "the URL works while the share is live"
    );

    dam_db::shares::revoke(&f.pool, share.id, now())
        .await
        .expect("revoke");

    assert_eq!(
        get(&f.app, &token).await.status(),
        StatusCode::NOT_FOUND,
        "the same URL, unchanged, must stop working the moment the share is revoked"
    );
}

async fn an_exhausted_share_stops_its_issued_urls_too(f: &Fixture) {
    // Same mechanism, different reason. A share with a download limit that has been spent is no longer live,
    // so URLs it issued stop — otherwise the limit bounds how many times the *share page* is opened rather
    // than how many times the asset leaves.
    let id = asset_with_bytes(f, "share-limited").await;
    licence(f, id, None).await;

    let share = dam_db::shares::create(
        &f.pool,
        &dam_db::shares::ShareSpec {
            kind: "asset",
            target_id: Some(id),
            search_query: None,
            passcode: None,
            expires_at: None,
            max_downloads: Some(1),
            allow_original: false,
            requires_eula: false,
            created_by: None,
        },
    )
    .await
    .expect("create a share");

    let token = delivery::issue_for_share(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Some(share.id),
        Duration::hours(20),
        now(),
    )
    .await
    .expect("issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);

    dam_db::shares::consume_download(&f.pool, share.id, now())
        .await
        .expect("spend the one download");

    assert_eq!(
        get(&f.app, &token).await.status(),
        StatusCode::NOT_FOUND,
        "a spent limit must stop the URL, not just the share page"
    );
}

async fn a_url_with_no_share_is_unaffected_by_share_state(f: &Fixture) {
    // The share check must be scoped to tokens that carry one. A URL issued to a logged-in user has no share
    // link, and must not be refused because some unrelated share was revoked.
    let id = asset_with_bytes(f, "no-share").await;
    licence(f, id, None).await;
    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::hours(1),
        now(),
    )
    .await
    .expect("issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);
}

// ─── tokens that should get nowhere ─────────────────────────────────────────

async fn an_expiring_licence_still_delivers(f: &Fixture) {
    // `Expiring` is a warning with a deadline, not a refusal. A warning that blocks is a denial with extra
    // steps, and people route around it — which is how a licence reaches its end date with nobody having
    // renewed it. Made explicit rather than left incidental to another case.
    let id = asset_with_bytes(f, "expiring").await;
    licence(f, id, Some(now() + Duration::days(3))).await;

    let verdict = dam_db::rights::effective(&f.pool, id, &web(), now())
        .await
        .expect("evaluate");
    assert_eq!(
        verdict,
        dam_core::rights::RightsState::Expiring,
        "three days out is inside the notice window, so the fixture must actually be expiring"
    );

    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::minutes(10),
        now(),
    )
    .await
    .expect("an expiring licence must still issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);
}

async fn a_tampered_or_unsigned_token_is_a_flat_404(f: &Fixture) {
    // Every signature failure answers identically. Distinguishing "bad signature" from "expired" tells a
    // forger their attempt was otherwise accepted, and 404 rather than 403 avoids confirming the asset
    // exists at all.
    let id = asset_with_bytes(f, "tamper").await;
    licence(f, id, None).await;
    let good = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::minutes(10),
        now(),
    )
    .await
    .expect("issue");

    let (payload, signature) = good.split_once('.').expect("two parts");
    let candidates = vec![
        ("empty", String::new()),
        ("nonsense", "not-a-token".to_owned()),
        ("payload only", payload.to_owned()),
        ("signature only", signature.to_owned()),
        ("swapped halves", format!("{signature}.{payload}")),
        (
            "truncated signature",
            format!("{payload}.{}", &signature[..signature.len() - 4]),
        ),
    ];
    for (name, token) in candidates {
        let response = get(&f.app, &token).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{name} must be a flat 404, got {}",
            response.status()
        );
    }
}

async fn an_expired_token_is_refused_even_though_the_rights_are_fine(f: &Fixture) {
    let id = asset_with_bytes(f, "expired-token").await;
    licence(f, id, None).await;
    let token = mint_directly(f, id, "web-2048", &web(), now() - Duration::seconds(1));
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::NOT_FOUND);
}

async fn a_deleted_asset_is_not_deliverable(f: &Fixture) {
    // Soft-deleted, licence intact, token valid. The delete is the answer, and it is a 404 — a 403 would
    // confirm the asset had existed.
    let id = asset_with_bytes(f, "deleted").await;
    licence(f, id, None).await;
    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::hours(1),
        now(),
    )
    .await
    .expect("issue");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::FOUND);

    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&f.pool)
        .await
        .expect("delete");
    assert_eq!(get(&f.app, &token).await.status(), StatusCode::NOT_FOUND);
}

async fn a_token_ttl_is_clamped_rather_than_refused(f: &Fixture) {
    // A caller asking for a year is asking for a share link. Answering with a 24-hour URL is more useful
    // than an error about a constant they cannot see — and 3.3 is the supported way to publish a URL.
    let id = asset_with_bytes(f, "clamped").await;
    licence(f, id, None).await;
    let token = delivery::issue(
        &f.state,
        id,
        "web-2048",
        &web(),
        None,
        Duration::days(365),
        now(),
    )
    .await
    .expect("issue");

    let claim = dam_core::signed_url::verify(
        &Keyring::single("k1", Secret::new("a-signing-key".to_owned())),
        &token,
        now(),
    )
    .expect("verifies");
    assert_eq!(
        claim.expires_at,
        now() + delivery::MAX_TOKEN_TTL,
        "a year must be clamped to the maximum, not honoured"
    );
}

// ─── helpers ────────────────────────────────────────────────────────────────

/// Signs a claim without going through `issue`, so a token can exist for a state `issue` would refuse.
///
/// That is the whole point of the D12 tests: the interesting case is a token that *was* legitimately issued
/// and whose asset has since changed, and this is how that state is reached in a test.
fn mint_directly(
    f: &Fixture,
    asset_id: Uuid,
    transform: &str,
    usage: &Usage,
    expires_at: DateTime<Utc>,
) -> String {
    let _ = f;
    dam_core::signed_url::sign(
        &Keyring::single("k1", Secret::new("a-signing-key".to_owned())),
        &dam_core::signed_url::DeliveryClaim {
            purpose: dam_core::signed_url::Purpose::Distribution,
            asset_id,
            transform: transform.to_owned(),
            channel: usage.channel.clone(),
            territory: usage.territory.clone(),
            identity_id: None,
            share_link_id: None,
            expires_at,
            key_id: String::new(),
        },
    )
    .expect("sign")
}

// ─── the internal-preview purpose (A.7) ─────────────────────────────────────

/// A live share link over `asset_id`.
///
/// Live matters: the delivery handler re-checks the share before anything else, so a share that does not exist
/// refuses the token for a reason that has nothing to do with what a test is asserting.
async fn live_share(f: &Fixture, asset_id: Uuid) -> Uuid {
    dam_db::shares::create(
        &f.pool,
        &dam_db::shares::ShareSpec {
            kind: "asset",
            target_id: Some(asset_id),
            search_query: None,
            passcode: None,
            expires_at: None,
            max_downloads: None,
            allow_original: false,
            requires_eula: false,
            created_by: None,
        },
    )
    .await
    .expect("create a share")
    .id
}

/// A preview token minted the way the asset endpoints mint one.
fn mint_preview(asset_id: Uuid, transform: &str, identity: Option<Uuid>) -> String {
    dam_core::signed_url::sign(
        &Keyring::single("k1", Secret::new("a-signing-key".to_owned())),
        &dam_core::signed_url::DeliveryClaim {
            purpose: dam_core::signed_url::Purpose::InternalPreview,
            asset_id,
            transform: transform.to_owned(),
            channel: "internal".to_owned(),
            territory: "WORLD".to_owned(),
            identity_id: identity,
            share_link_id: None,
            expires_at: now() + Duration::hours(1),
            key_id: String::new(),
        },
    )
    .expect("sign")
}

async fn a_preview_of_an_unlicensed_asset_is_delivered(f: &Fixture) {
    // The whole point of A.7. An asset with no licence is `RightsState::Unknown` and unknown denies, so a
    // *download* of this asset is refused — the case below asserts that has not changed. A thumbnail in the
    // DAM's own grid is internal cataloguing rather than distribution, and this is what makes a fresh library
    // show anything at all.
    let asset_id = asset_with_bytes(f, "preview-unlicensed").await;

    // The distribution path still says no, on the same asset, in the same test.
    let download = mint_directly(
        f,
        asset_id,
        "web-2048",
        &web(),
        now() + Duration::minutes(5),
    );
    assert_eq_forbidden(&get(&f.app, &download).await);

    let preview = mint_preview(asset_id, "web-2048", Some(Uuid::from_u128(0x1de)));
    let response = get(&f.app, &preview).await;
    assert_eq!(
        response.status(),
        StatusCode::FOUND,
        "an internal preview of an unlicensed asset is delivered; a download of it is not"
    );
}

async fn a_preview_of_the_original_is_refused(f: &Fixture) {
    // Restriction one. The original is the thing a licence is *about*, so an internal preview may never name
    // it — and this is checked at delivery as well as at the mint, because a token minted before a restriction
    // tightened must stop working.
    let asset_id = asset_with_bytes(f, "preview-original").await;
    let token = mint_preview(asset_id, "original", Some(Uuid::from_u128(0x1de)));

    let response = get(&f.app, &token).await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a preview token naming the original must be refused, not served"
    );
}

async fn a_preview_with_no_identity_is_refused(f: &Fixture) {
    // Restriction three. "Internal" means a member of the tenant; an anonymous internal preview is a
    // contradiction, and the audit trail needs to say which person a preview was issued to.
    let asset_id = asset_with_bytes(f, "preview-anonymous").await;
    let token = mint_preview(asset_id, "web-2048", None);

    assert_eq!(
        get(&f.app, &token).await.status(),
        StatusCode::NOT_FOUND,
        "a preview with no identity must be refused"
    );
}

async fn a_preview_token_cannot_carry_a_share_link(f: &Fixture) {
    // Restriction two, and the sharpest of the three: a share is distribution by definition, so an external
    // recipient looking at a thumbnail of an unlicensed asset is exactly the exposure the rights model exists
    // to prevent. Minted by hand, because nothing in the codebase will produce this combination.
    //
    // The share is **live**, and that matters: an earlier version of this case invented a share id that did not
    // exist, so `shares::is_live` refused the token before the preview restriction was ever consulted. The case
    // passed and proved nothing — a mutation allowing a shared preview survived it.
    let asset_id = asset_with_bytes(f, "preview-shared").await;
    let share_id = live_share(f, asset_id).await;

    let token = dam_core::signed_url::sign(
        &Keyring::single("k1", Secret::new("a-signing-key".to_owned())),
        &dam_core::signed_url::DeliveryClaim {
            purpose: dam_core::signed_url::Purpose::InternalPreview,
            asset_id,
            transform: "web-2048".to_owned(),
            channel: "internal".to_owned(),
            territory: "WORLD".to_owned(),
            identity_id: Some(Uuid::from_u128(0x1de)),
            share_link_id: Some(share_id),
            expires_at: now() + Duration::hours(1),
            key_id: String::new(),
        },
    )
    .expect("sign");

    assert_eq!(
        get(&f.app, &token).await.status(),
        StatusCode::NOT_FOUND,
        "a preview issued through a live share link must still be refused"
    );
}

async fn a_preview_of_a_name_that_is_not_a_profile_is_refused(f: &Fixture) {
    // The other half of restriction one. `original` is not a profile, and neither is a typo or a future
    // tenant-defined render — all three land here, and all three are refused rather than approximated.
    let asset_id = asset_with_bytes(f, "preview-unknown-profile").await;
    for transform in ["original", "thumb-512", "tenant-hero-banner"] {
        let token = mint_preview(asset_id, transform, Some(Uuid::from_u128(0x1de)));
        assert_eq!(
            get(&f.app, &token).await.status(),
            StatusCode::NOT_FOUND,
            "{transform} is not a built-in profile and must not be previewable"
        );
    }
}

async fn a_distribution_token_cannot_be_edited_into_a_preview(f: &Fixture) {
    // The forgery this design has to survive. If the purpose were outside the signature, anyone holding a
    // refused download URL could turn it into a preview and skip the rights check — strictly worse than not
    // having the feature at all.
    let asset_id = asset_with_bytes(f, "preview-forged").await;
    let download = mint_directly(
        f,
        asset_id,
        "web-2048",
        &web(),
        now() + Duration::minutes(5),
    );
    assert_eq_forbidden(&get(&f.app, &download).await);

    // The payload's purpose byte flipped from `Distribution` (1) to `InternalPreview` (2), signature untouched.
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (payload_b64, signature_b64) = download.split_once('.').expect("two parts");
    let mut payload = base64::Engine::decode(&encoder, payload_b64).expect("decodes");
    // Version byte, a 4-byte length, then the purpose.
    assert_eq!(payload[5], 1, "the premise: this is a distribution token");
    payload[5] = 2;
    let forged = format!(
        "{}.{}",
        base64::Engine::encode(&encoder, &payload),
        signature_b64
    );

    assert_eq!(
        get(&f.app, &forged).await.status(),
        StatusCode::NOT_FOUND,
        "flipping the purpose must break the signature, not the rights check"
    );
}

fn assert_eq_forbidden(response: &axum::http::Response<Body>) {
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn the_delivery_chokepoint_holds() {
    let f = fixture().await;

    a_signed_url_for_a_licensed_asset_redirects_to_the_object(&f).await;
    the_transform_selects_which_object_is_served(&f).await;
    an_unknown_transform_is_not_deliverable(&f).await;
    a_redefined_profile_misses_the_cache_instead_of_serving_stale_bytes(&f).await;
    a_serve_is_recorded_at_most_once_an_hour(&f).await;

    a_valid_signature_over_an_unlicensed_asset_is_still_refused(&f).await;
    the_channel_in_the_token_selects_which_licence_terms_apply(&f).await;
    a_legal_hold_stops_delivery_of_an_already_issued_url(&f).await;

    an_expiring_licence_still_delivers(&f).await;
    revoking_a_share_stops_a_url_it_already_issued(&f).await;
    an_exhausted_share_stops_its_issued_urls_too(&f).await;
    a_url_with_no_share_is_unaffected_by_share_state(&f).await;
    a_tampered_or_unsigned_token_is_a_flat_404(&f).await;
    an_expired_token_is_refused_even_though_the_rights_are_fine(&f).await;
    a_deleted_asset_is_not_deliverable(&f).await;
    a_token_ttl_is_clamped_rather_than_refused(&f).await;

    a_preview_of_an_unlicensed_asset_is_delivered(&f).await;
    a_preview_of_the_original_is_refused(&f).await;
    a_preview_with_no_identity_is_refused(&f).await;
    a_preview_token_cannot_carry_a_share_link(&f).await;
    a_preview_of_a_name_that_is_not_a_profile_is_refused(&f).await;
    a_distribution_token_cannot_be_edited_into_a_preview(&f).await;

    // Last: it edits every licence row, so it must not run before the cases above.
    a_licence_that_lapses_after_issue_stops_an_already_issued_url(&f).await;
}
