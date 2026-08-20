//! The AI credential and budget endpoints (M5a·4).
//!
//! `dam_db`'s suites prove the storage and the arithmetic. What only exists here is the HTTP contract, and four
//! decisions that are about the *interface* rather than the model:
//!
//! - **A key goes in and never comes out.** No route returns one, sealed or plaintext, and this suite reads the
//!   raw response body looking for the key it just sent. A hint of four characters is the whole disclosure.
//! - **An unusable credential is refused at the door**, not at enrichment time hours later with nobody watching:
//!   an unknown provider and an OpenAI-compatible credential with no endpoint are both 422 on the way in.
//! - **Verification is a real call**, and its result distinguishes "the key is wrong" from "the model said no".
//!   Driven here through a recorded transport, which is also how the request shape gets asserted.
//! - **A budget with no cap is not a cap of zero.** An unconfigured tenant enriches unmetered, and the view says
//!   so with a null limit rather than a zero somebody would read as "blocked".

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_ai::testing::{Recorded, Reply, anthropic_answer, anthropic_refusal};
use dam_api::ai::{AiState, router};
use dam_core::Secret;
use dam_core::sealed::SealingKeyring;
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

/// The plaintext this suite pretends is a provider key. Not one: no provider issues keys in this shape.
const FAKE_KEY: &str = "sk-test-not-a-credential-9911";

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    transport: Arc<Recorded>,
    app: axum::Router,
    /// A tenant admin: Manage.
    key: String,
    /// `asset:read` only, to prove configuration is not readable by everybody.
    read_only_key: String,
    tenant_id: Uuid,
}

fn keyring() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a test sealing passphrase".to_owned()))
}

async fn fixture_with(transport: Arc<Recorded>) -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");
    let identity = identity(&global, "ada@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(identity)
    .execute(&global)
    .await
    .expect("membership");
    let key = issue(&global, tenant_id, Some(identity), &[]).await;
    let read_only_key = issue(&global, tenant_id, Some(identity), &["asset:read"]).await;

    let app = router(AiState {
        global: global.clone(),
        keyring: keyring(),
        prices: dam_ai::pricing::Prices::default(),
        transport: Arc::clone(&transport) as Arc<dyn dam_ai::model::Transport>,
    });

    Fixture {
        _pg: pg,
        global,
        acme,
        transport,
        app,
        key,
        read_only_key,
        tenant_id,
    }
}

async fn fixture() -> Fixture {
    fixture_with(Arc::new(Recorded::always(
        200,
        anthropic_answer("ready", "claude-opus-5", (8, 2, 0, 0)),
    )))
    .await
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
            .map(|s| (*s).to_owned())
            .collect::<Vec<String>>(),
    )
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

/// One request, returning the status and the raw body — raw, because some of these assertions are about text
/// that must *not* be present.
async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {key}"));
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(match &body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        })
        .expect("request");
    let response = f.app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn json_call(
    f: &Fixture,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, text) = call(f, method, path, key, body).await;
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value)
}

fn anthropic_credential() -> Value {
    json!({
        "provider": "anthropic",
        "label": "Ada's key",
        "default_model": "claude-opus-5",
        "api_key": FAKE_KEY,
        "make_default": true,
    })
}

#[tokio::test]
async fn a_stored_key_is_never_readable_again() {
    let f = fixture().await;
    let (status, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["hint"], "…9911",
        "four characters and an ellipsis, to tell two keys apart"
    );
    assert_eq!(created["is_default"], true);
    assert_eq!(created["needs_resealing"], false);

    // The response body, in full, must not contain the key or any part of it beyond the hint.
    let (_, raw) = call(&f, "GET", "/ai/credentials", &f.key, None).await;
    assert!(
        !raw.contains(FAKE_KEY),
        "the plaintext key came back: {raw}"
    );
    assert!(
        !raw.contains("sealed"),
        "even the ciphertext stays behind the API: {raw}"
    );

    // And it is genuinely encrypted at rest: the stored value is not the key.
    let sealed: String = sqlx::query_scalar("SELECT sealed_key FROM ai_credentials")
        .fetch_one(&f.acme)
        .await
        .expect("sealed key");
    assert!(!sealed.contains(FAKE_KEY), "stored in the clear: {sealed}");
    assert!(sealed.starts_with("v1."), "versioned envelope: {sealed}");
}

#[tokio::test]
async fn the_sealed_key_is_bound_to_its_row() {
    // The associated data is `tenant:provider:id`, so a ciphertext copied to another row must not open. Asserted
    // through the API's own keyring rather than by reimplementing the derivation.
    let f = fixture().await;
    let (_, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");
    let sealed: String = sqlx::query_scalar("SELECT sealed_key FROM ai_credentials WHERE id = $1")
        .bind(id)
        .fetch_one(&f.acme)
        .await
        .expect("sealed key");

    let ring = keyring();
    let honest = dam_db::ai_credentials::associated_data("acme", "anthropic", id);
    assert_eq!(
        ring.open(&sealed, &honest)
            .expect("opens for its own row")
            .expose(),
        FAKE_KEY
    );
    let elsewhere = dam_db::ai_credentials::associated_data("acme", "anthropic", Uuid::now_v7());
    assert!(
        ring.open(&sealed, &elsewhere).is_err(),
        "a ciphertext moved to another row must refuse"
    );
}

#[tokio::test]
async fn configuration_is_not_readable_by_everybody() {
    let f = fixture().await;
    let (status, _) = call(&f, "GET", "/ai/credentials", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&f, "GET", "/ai/budget", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_unusable_credential_is_refused_on_the_way_in() {
    let f = fixture().await;

    // A provider this build has no client for. Refused rather than stored, because the alternative is a row that
    // fails at enrichment time.
    let (status, body) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(json!({
            "provider": "some-new-vendor",
            "label": "x",
            "default_model": "m",
            "api_key": FAKE_KEY,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // An OpenAI-compatible credential with no endpoint. There is no default to guess: the prefix before
    // /chat/completions differs per vendor, and guessing would send the key wherever the guess pointed.
    let (status, body) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(json!({
            "provider": "openai_compatible",
            "label": "kimi",
            "default_model": "kimi-k2",
            "api_key": FAKE_KEY,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.to_string().contains("base url"),
        "the refusal says what is missing: {body}"
    );

    // An empty key stores nothing rather than sealing the empty string.
    let (status, _) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(json!({
            "provider": "anthropic",
            "label": "empty",
            "default_model": "claude-opus-5",
            "api_key": "   ",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_credentials")
        .fetch_one(&f.acme)
        .await
        .expect("count");
    assert_eq!(count, 0, "nothing was stored");
}

#[tokio::test]
async fn a_replaced_key_changes_the_hint_and_nothing_else() {
    let f = fixture().await;
    let (_, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, updated) = json_call(
        &f,
        "PUT",
        &format!("/ai/credentials/{id}/key"),
        &f.key,
        Some(json!({"api_key": "sk-test-rotated-0002"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["hint"], "…0002");
    assert_eq!(updated["label"], created["label"], "the label is untouched");
    assert_eq!(updated["is_default"], true, "and so is the default flag");

    // The old ciphertext is gone, not kept alongside.
    let sealed: String = sqlx::query_scalar("SELECT sealed_key FROM ai_credentials")
        .fetch_one(&f.acme)
        .await
        .expect("sealed");
    let opened = keyring()
        .open(
            &sealed,
            &dam_db::ai_credentials::associated_data(
                "acme",
                "anthropic",
                id.parse().expect("uuid"),
            ),
        )
        .expect("opens");
    assert_eq!(opened.expose(), "sk-test-rotated-0002");

    let (status, _) = json_call(
        &f,
        "PUT",
        "/ai/credentials/00000000-0000-0000-0000-000000000000/key",
        &f.key,
        Some(json!({"api_key": "sk-test-nowhere-0003"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn withdrawing_the_default_leaves_no_default_behind() {
    let f = fixture().await;
    let (_, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, withdrawn) = json_call(
        &f,
        "PATCH",
        &format!("/ai/credentials/{id}/active"),
        &f.key,
        Some(json!({"is_active": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{withdrawn}");
    assert_eq!(withdrawn["is_active"], false);
    assert_eq!(
        withdrawn["is_default"], false,
        "a default nobody may use would be picked by enrichment anyway"
    );

    // And it cannot be made the default again while withdrawn — a 409 that says why, rather than a constraint
    // error that says which index.
    let (status, body) = json_call(
        &f,
        "PATCH",
        &format!("/ai/credentials/{id}/default"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_verification_makes_one_real_call_and_says_what_happened() {
    let f = fixture().await;
    let (_, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, result) = json_call(
        &f,
        "POST",
        &format!("/ai/credentials/{id}/verify"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["ok"], true);
    assert_eq!(result["model"], "claude-opus-5");
    assert_eq!(result["detail"], "ready");

    // The key that reached the provider is the one that was stored — which is the end-to-end property this whole
    // slice exists for: sealed on the way in, opened on the way out, and never in between.
    let sent = f.transport.only();
    assert_eq!(sent.header("x-api-key"), Some(FAKE_KEY));
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages");
    // Cheap on purpose: this is asking whether the key works.
    assert_eq!(sent.body["max_tokens"], 16);
    assert!(
        sent.body["output_config"].get("format").is_none(),
        "a schema would add a way for the check to fail that is nothing to do with the key"
    );
}

#[tokio::test]
async fn a_rejected_key_and_a_refusal_are_different_answers() {
    let rejected = Arc::new(Recorded::always(
        401,
        json!({"error": {"message": "invalid x-api-key"}}),
    ));
    let f = fixture_with(rejected).await;
    let (_, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let (status, result) = json_call(
        &f,
        "POST",
        &format!("/ai/credentials/{id}/verify"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the attempt was made; it failed");
    assert_eq!(result["ok"], false);
    assert_eq!(
        result["worth_retrying"], false,
        "a rejected credential is not a transient failure"
    );

    // A refusal is the other case, and the distinction matters to whoever pasted the key: the credential works.
    let refused = Arc::new(Recorded::always(200, anthropic_refusal("policy")));
    let f = fixture_with(refused).await;
    let (_, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let (_, result) = json_call(
        &f,
        "POST",
        &format!("/ai/credentials/{id}/verify"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(result["ok"], false);
    assert!(
        result["detail"]
            .as_str()
            .expect("detail")
            .contains("the credential works"),
        "{result}"
    );

    // And a throttle is worth another go.
    let throttled = Arc::new(Recorded::script(vec![Reply::Throttled(
        30,
        json!({"error": {"message": "slow down"}}),
    )]));
    let f = fixture_with(throttled).await;
    let (_, created) = json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let (_, result) = json_call(
        &f,
        "POST",
        &format!("/ai/credentials/{id}/verify"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(result["worth_retrying"], true);
}

#[tokio::test]
async fn verifying_something_that_is_not_there_is_a_404_and_costs_nothing() {
    let f = fixture().await;
    let (status, _) = call(
        &f,
        "POST",
        "/ai/credentials/00000000-0000-0000-0000-000000000000/verify",
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        f.transport.sent().is_empty(),
        "no provider call for a credential that does not exist"
    );
}

#[tokio::test]
async fn an_unmetered_tenant_has_no_cap_rather_than_a_cap_of_zero() {
    let f = fixture().await;
    let (status, budget) = json_call(&f, "GET", "/ai/budget", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{budget}");
    assert!(budget["limit_cents"].is_null(), "{budget}");
    assert_eq!(budget["used_cents"], 0);
    assert_eq!(
        budget["state"], "allowed",
        "nothing is metering it, which is not the same as blocked"
    );
}

#[tokio::test]
async fn a_cap_is_set_read_back_and_reflects_what_has_been_spent() {
    let f = fixture().await;
    let (status, budget) = json_call(
        &f,
        "PUT",
        "/ai/budget",
        &f.key,
        Some(json!({"limit_cents": 10_000, "hard": true, "warn_at_fraction": 0.5})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{budget}");
    assert_eq!(budget["limit_cents"], 10_000);
    assert_eq!(budget["enforcement"], "hard");
    assert_eq!(budget["state"], "allowed");

    // Charge past the warning line, in micro-cents: 6000 cents of a 10000 limit, warning at half.
    let mut conn = f.global.acquire().await.expect("connection");
    let period = dam_db::quotas::month_start(chrono::Utc::now());
    dam_db::quotas::charge(
        &mut conn,
        f.tenant_id,
        dam_db::quotas::AI_SPEND,
        period,
        6_000 * dam_db::quotas::MICRO,
    )
    .await
    .expect("charge");

    let (_, budget) = json_call(&f, "GET", "/ai/budget", &f.key, None).await;
    assert_eq!(budget["used_cents"], 6_000);
    assert_eq!(budget["state"], "warned");

    // And over the limit, a hard cap says refused — which is what an enrichment job reads before it starts.
    dam_db::quotas::charge(
        &mut conn,
        f.tenant_id,
        dam_db::quotas::AI_SPEND,
        period,
        5_000 * dam_db::quotas::MICRO,
    )
    .await
    .expect("charge");
    let (_, budget) = json_call(&f, "GET", "/ai/budget", &f.key, None).await;
    assert_eq!(budget["used_cents"], 11_000);
    assert_eq!(budget["state"], "refused");
}

#[tokio::test]
async fn a_nonsense_cap_is_refused() {
    let f = fixture().await;
    for body in [
        json!({"limit_cents": -1}),
        json!({"limit_cents": 100, "warn_at_fraction": 1.5}),
    ] {
        let (status, response) =
            json_call(&f, "PUT", "/ai/budget", &f.key, Some(body.clone())).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body} accepted: {response}"
        );
    }
}
