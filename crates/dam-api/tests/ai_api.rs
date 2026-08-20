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

// ───────────────────────────────────────────────────────────────────────────────────────────────
// Enrichment: the settings, the queue, and the disclosure
// ───────────────────────────────────────────────────────────────────────────────────────────────

/// An asset, optionally inside a group so a scoped key can be kept away from it.
async fn asset(f: &Fixture, name: &str, group: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 100, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(&f.acme)
    .await
    .expect("asset");
    if let Some(group) = group {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(group)
            .bind(id)
            .execute(&f.acme)
            .await
            .expect("membership");
    }
    id
}

/// A term, returning its id.
async fn term(f: &Fixture, taxonomy: Uuid, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, path, slug, label) \
         VALUES ($1, $2, text2ltree($3), $3, initcap($3))",
    )
    .bind(id)
    .bind(taxonomy)
    .bind(slug)
    .execute(&f.acme)
    .await
    .expect("term");
    id
}

async fn taxonomy(f: &Fixture) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO taxonomies (id, key, label) VALUES ($1, 'subject', 'Subject')")
        .bind(id)
        .execute(&f.acme)
        .await
        .expect("taxonomy");
    id
}

/// A suggested tag on an asset, as the pipeline would have left it.
async fn suggest(f: &Fixture, asset_id: Uuid, term_id: Uuid, confidence: f32, votes: i16) {
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source, confidence, generator_votes) \
         VALUES ($1, $2, 'suggested', 'llm', $3, $4)",
    )
    .bind(asset_id)
    .bind(term_id)
    .bind(confidence)
    .bind(votes)
    .execute(&f.acme)
    .await
    .expect("suggested tag");
}

/// A machine-written value with its provenance, as `dam_db::enrichment` would have left it.
async fn machine_value(f: &Fixture, asset_id: Uuid, key: &str, value: &str) {
    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values, provenance) VALUES ($1, $2, $3) \
         ON CONFLICT (asset_id) DO UPDATE SET values = excluded.values, provenance = excluded.provenance",
    )
    .bind(asset_id)
    .bind(json!({key: value}))
    .bind(json!({key: {
        "source": "llm",
        "model": "claude-opus-5",
        "model_version": "llm_describe/1",
        "confidence": 0.7,
        "at": "2026-08-20T10:00:00Z",
        "reviewed_by": null,
    }}))
    .execute(&f.acme)
    .await
    .expect("machine value");
}

#[tokio::test]
async fn enrichment_is_off_until_somebody_turns_it_on() {
    let f = fixture().await;
    let (status, settings) = json_call(&f, "GET", "/ai/enrichment", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(
        settings["is_enabled"], false,
        "the pipeline that bills per asset"
    );
    assert_eq!(settings["language"], "English");
    assert_eq!(settings["alt_text_field"], "alt_text");
    assert_eq!(settings["suggest_tags"], true);
}

#[tokio::test]
async fn switching_it_on_without_a_credential_is_refused_where_somebody_can_fix_it() {
    let f = fixture().await;
    let (status, body) = json_call(
        &f,
        "PUT",
        "/ai/enrichment",
        &f.key,
        Some(json!({
            "is_enabled": true,
            "guidance": "",
            "language": "English",
            "model": null,
            "alt_text_field": "alt_text",
            "description_field": "description",
            "suggest_tags": true,
        })),
    )
    .await;
    // Otherwise the failure arrives later as a queue of runs that all say "no credential".
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("credential"), "{body}");
}

#[tokio::test]
async fn settings_are_saved_and_read_back_as_stored() {
    let f = fixture().await;
    json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    let (status, saved) = json_call(
        &f,
        "PUT",
        "/ai/enrichment",
        &f.key,
        Some(json!({
            "is_enabled": true,
            "guidance": "  Say 'trainers', not 'sneakers'.  ",
            "language": "  British English  ",
            "model": "claude-haiku-4-5",
            "alt_text_field": "alt_text",
            "description_field": null,
            "suggest_tags": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["is_enabled"], true);
    // Trimmed by the store, and the response is the read-back rather than the echo — so a caller learns what
    // was actually kept.
    assert_eq!(saved["language"], "British English");
    assert_eq!(saved["model"], "claude-haiku-4-5");
    assert!(
        saved["description_field"].is_null(),
        "null means write none"
    );
    assert_eq!(saved["suggest_tags"], false);
    // The guidance keeps its own whitespace: it is prose, and the prompt builder trims it where it matters.
    assert!(
        saved["guidance"]
            .as_str()
            .expect("guidance")
            .contains("trainers"),
        "{saved}"
    );
}

#[tokio::test]
async fn enrichment_can_be_asked_for_one_asset_and_only_by_somebody_who_can_see_it() {
    let f = fixture().await;
    let id = asset(&f, "one", None).await;

    // Off: refused with the reason, rather than a job that will skip.
    let (status, body) = json_call(&f, "POST", &format!("/assets/{id}/enrich"), &f.key, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    json_call(
        &f,
        "POST",
        "/ai/credentials",
        &f.key,
        Some(anthropic_credential()),
    )
    .await;
    json_call(
        &f,
        "PUT",
        "/ai/enrichment",
        &f.key,
        Some(json!({
            "is_enabled": true,
            "guidance": "",
            "language": "English",
            "model": null,
            "alt_text_field": "alt_text",
            "description_field": "description",
            "suggest_tags": true,
        })),
    )
    .await;

    let (status, queued) =
        json_call(&f, "POST", &format!("/assets/{id}/enrich"), &f.key, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["asset_id"], id.to_string());
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs WHERE kind = 'enrich' AND dedupe_key = $1",
    )
    .bind(format!("enrich:{id}"))
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(jobs, 1);

    // Asking twice is one job: this is the only queue in damrs where a duplicate costs money.
    json_call(&f, "POST", &format!("/assets/{id}/enrich"), &f.key, None).await;
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs WHERE kind = 'enrich' AND dedupe_key = $1",
    )
    .bind(format!("enrich:{id}"))
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(jobs, 1);

    // And an asset nobody may see cannot be enriched — 404, not 403: the same rule as everywhere else, because
    // the difference between the two is an existence oracle.
    let (status, _) = call(
        &f,
        "POST",
        "/assets/00000000-0000-0000-0000-000000000000/enrich",
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_review_queue_shows_what_a_model_did_and_nothing_outside_the_callers_scope() {
    let f = fixture().await;
    let taxonomy_id = taxonomy(&f).await;
    let footwear = term(&f, taxonomy_id, "footwear").await;
    let outdoor = term(&f, taxonomy_id, "outdoor").await;

    let visible = asset(&f, "visible", None).await;
    suggest(&f, visible, footwear, 0.9, 2).await;
    suggest(&f, visible, outdoor, 0.4, 1).await;
    machine_value(&f, visible, "alt_text", "A runner on a wet path").await;

    // An asset in a group the scoped key cannot reach.
    let hidden_group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'locked', 'Locked') RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("group");
    let hidden = asset(&f, "hidden", Some(hidden_group)).await;
    suggest(&f, hidden, footwear, 0.95, 3).await;

    // An asset nothing has been said about. It must not appear: a queue is a list of things to decide, and
    // padding it with assets that need no decision is how a reviewer learns to ignore it.
    let untouched = asset(&f, "untouched", None).await;

    let (status, queue) = json_call(&f, "GET", "/ai/review", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{queue}");
    let rows = queue.as_array().expect("rows");
    assert_eq!(
        rows.len(),
        2,
        "the admin sees both, and not the untouched one"
    );
    assert!(
        !rows
            .iter()
            .any(|row| row["asset_id"] == untouched.to_string()),
        "{queue}"
    );

    // Strongest evidence first: the three-vote suggestion leads.
    assert_eq!(rows[0]["asset_id"], hidden.to_string());
    let visible_row = rows
        .iter()
        .find(|row| row["asset_id"] == visible.to_string())
        .expect("the visible asset");
    let suggested = visible_row["suggested"].as_array().expect("suggested");
    assert_eq!(suggested.len(), 2);
    assert_eq!(suggested[0]["slug"], "footwear", "two votes before one");
    assert_eq!(suggested[0]["votes"], 2);
    assert_eq!(suggested[0]["source"], "llm");
    let fields = visible_row["fields"].as_array().expect("fields");
    assert_eq!(fields[0]["key"], "alt_text");
    assert_eq!(fields[0]["model"], "claude-opus-5");
    assert_eq!(fields[0]["reviewed"], false);

    // A key scoped to a group that holds neither asset sees an empty queue — the predicate is in the query, so
    // the queue cannot become a way to enumerate the library.
    let scoped_group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("group");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped_manager', 'Scoped', '{asset:read,asset:manage}', ARRAY[$1], false)",
    )
    .bind(scoped_group)
    .execute(&f.acme)
    .await
    .expect("role");
    let identity = identity(&f.global, "scoped@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{scoped_manager}', false)",
    )
    .bind(f.tenant_id)
    .bind(identity)
    .execute(&f.global)
    .await
    .expect("membership");
    let scoped_key = issue(&f.global, f.tenant_id, Some(identity), &[]).await;

    let (status, queue) = json_call(&f, "GET", "/ai/review", &scoped_key, None).await;
    assert_eq!(status, StatusCode::OK, "{queue}");
    assert_eq!(queue.as_array().expect("rows").len(), 0);
}

#[tokio::test]
async fn a_decision_about_a_tag_is_recorded_once_and_kept() {
    let f = fixture().await;
    let taxonomy_id = taxonomy(&f).await;
    let footwear = term(&f, taxonomy_id, "footwear").await;
    let outdoor = term(&f, taxonomy_id, "outdoor").await;
    let id = asset(&f, "one", None).await;
    suggest(&f, id, footwear, 0.9, 1).await;
    suggest(&f, id, outdoor, 0.3, 1).await;

    let (status, _) = call(
        &f,
        "PATCH",
        &format!("/assets/{id}/tags/{footwear}"),
        &f.key,
        Some(json!({"accept": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &f,
        "PATCH",
        &format!("/assets/{id}/tags/{outdoor}"),
        &f.key,
        Some(json!({"accept": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let states: Vec<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT state, reviewed_by FROM asset_tags WHERE asset_id = $1 ORDER BY state",
    )
    .bind(id)
    .fetch_all(&f.acme)
    .await
    .expect("states");
    assert_eq!(states[0].0, "confirmed");
    assert_eq!(states[1].0, "rejected");
    assert!(states[0].1.is_some(), "the decision names who made it");

    // Both decisions in the training set, rejection included: 0003 says losing the rejections loses the signal
    // that matters most.
    let feedback: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT verdict, proposed_by FROM tag_feedback WHERE asset_id = $1 ORDER BY verdict",
    )
    .bind(id)
    .fetch_all(&f.acme)
    .await
    .expect("feedback");
    assert_eq!(feedback.len(), 2);
    assert_eq!(feedback[0].0, "accept");
    assert_eq!(feedback[0].1.as_deref(), Some("llm"));
    assert_eq!(feedback[1].0, "reject");

    // Clicking again decides nothing — two reviewers with the same queue open is an ordinary race, and saying
    // 200 would claim the second click had done something.
    let (status, _) = call(
        &f,
        "PATCH",
        &format!("/assets/{id}/tags/{footwear}"),
        &f.key,
        Some(json!({"accept": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let state: String = sqlx::query_scalar("SELECT state FROM asset_tags WHERE term_id = $1")
        .bind(footwear)
        .fetch_one(&f.acme)
        .await
        .expect("state");
    assert_eq!(
        state, "confirmed",
        "a decision is not overwritten by a stale click"
    );
}

#[tokio::test]
async fn the_disclosure_is_visible_to_anybody_who_can_see_the_asset() {
    let f = fixture().await;
    let id = asset(&f, "disclosed", None).await;
    machine_value(&f, id, "description", "Two sentences a model wrote.").await;

    // Read, not Manage: a marking that only administrators can see is not a disclosure.
    let (status, disclosed) = json_call(
        &f,
        "GET",
        &format!("/assets/{id}/ai"),
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{disclosed}");
    let rows = disclosed.as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "description");
    assert_eq!(rows[0]["value"], "Two sentences a model wrote.");
    assert_eq!(rows[0]["model"], "claude-opus-5");
    assert_eq!(rows[0]["reviewed"], false);

    // An asset with nothing machine-written discloses nothing, rather than 404 — "no AI here" is an answer.
    let plain = asset(&f, "plain", None).await;
    let (status, disclosed) = json_call(
        &f,
        "GET",
        &format!("/assets/{plain}/ai"),
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disclosed.as_array().expect("rows").len(), 0);
}
