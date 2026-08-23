//! The enrichment stage end to end (M5b).
//!
//! A real Postgres, a fake object store, and a recorded transport — because the one thing that must *not* be
//! real here is the provider: a suite that called it would cost money per run and be skipped everywhere for want
//! of a key. What is real is everything that decides whether the call happens at all, and everything that
//! happens to the answer afterwards.
//!
//! The properties, in the order they can go wrong:
//!
//! - **Off by default.** A tenant who has not switched enrichment on is never charged, and the run row says so
//!   rather than the queue swallowing it.
//! - **The budget is checked before the call**, so a hard cap is a stop rather than a discovery.
//! - **No credential is a skip, not a dead letter.** Half-finished setup is not a broken asset.
//! - **The proxy is read, never the original** — `used_original` is the column that catches a restore storm.
//! - **A refusal costs one call and no retries.** A 401 is permanent; a 429 is not.
//! - **Every written value carries provenance and a disclosure row** (G2), and tags land as suggestions.
//! - **The spend is charged after the call**, in micro-cents, so a fraction of a cent is not lost.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_ai::testing::{Recorded, Reply, anthropic_answer, anthropic_refusal};
use dam_core::sealed::SealingKeyring;
use dam_core::{Secret, TenantSlug};
use dam_db::enrichment::{self, Settings};
use dam_db::{migrate, quotas, testing::PostgresHarness};
use dam_pipeline::enrich::{AiContext, EnrichOutcome};
use dam_store::{BlobStore, FakeS3Store, Key};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const PROXY_KEY: &str = "proxy/object/key.jpg";
const FAKE_PROVIDER_KEY: &str = "sk-test-not-a-credential-4242";

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    tenant: PgPool,
    store: Arc<dyn BlobStore>,
    slug: TenantSlug,
    tenant_id: Uuid,
    asset_id: Uuid,
}

fn keyring() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a test sealing passphrase".to_owned()))
}

fn ai(transport: Arc<Recorded>) -> AiContext {
    AiContext {
        keyring: keyring(),
        prices: dam_ai::pricing::Prices::default(),
        transport: transport as Arc<dyn dam_ai::model::Transport>,
    }
}

/// A tenant with one asset, one proxy, two describable fields and a two-term taxonomy.
async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant row");

    for (key, ai_writable) in [
        ("alt_text", true),
        ("description", true),
        ("copyright", false),
    ] {
        sqlx::query("INSERT INTO field_defs (id, key, label, kind, ai_writable) VALUES ($1, $2, $2, 'text', $3)")
            .bind(Uuid::now_v7())
            .bind(key)
            .bind(ai_writable)
            .execute(&tenant)
            .await
            .expect("field def");
    }

    let taxonomy_id = Uuid::now_v7();
    // Opened to machine tagging, and stated rather than defaulted: since 0034 the vocabulary offered to a model
    // is the governed one, so a taxonomy nobody opened contributes no terms and the enrichment pass would have
    // nothing to suggest from. Which is the point of the gate — but it makes this fixture's intent explicit.
    sqlx::query(
        "INSERT INTO taxonomies (id, key, label, kind, ai_taggable) \
         VALUES ($1, 'subject', 'Subject', 'vocabulary', true)",
    )
    .bind(taxonomy_id)
    .execute(&tenant)
    .await
    .expect("taxonomy");
    for slug in ["footwear", "outdoor"] {
        sqlx::query(
            "INSERT INTO taxonomy_terms (id, taxonomy_id, path, slug, label) \
             VALUES ($1, $2, text2ltree($3), $3, initcap($3))",
        )
        .bind(Uuid::now_v7())
        .bind(taxonomy_id)
        .bind(slug)
        .execute(&tenant)
        .await
        .expect("term");
    }

    let asset_id = Uuid::now_v7();
    let content_hash = blake3::hash(b"an asset").to_hex().to_string();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, 'SS26_lookbook_cover.jpg', 'image/jpeg', 4096, $1)",
    )
    .bind(asset_id)
    .bind(&content_hash)
    .execute(&tenant)
    .await
    .expect("asset");

    let store = FakeS3Store::with_test_clock().0;
    let key = Key::new(PROXY_KEY).expect("key");
    store
        .put(
            &key,
            bytes::Bytes::from_static(b"not really a jpeg"),
            dam_core::StorageClass::Standard,
        )
        .await
        .expect("proxy object");

    Fixture {
        _pg: pg,
        global,
        tenant,
        store: Arc::new(store),
        slug: TenantSlug::new("acme").expect("slug"),
        tenant_id,
        asset_id,
    }
}

/// Records a proxy derivative pointing at the object the fixture wrote.
async fn proxy(f: &Fixture, mime: &str) {
    sqlx::query(
        "INSERT INTO derivatives \
            (id, asset_id, role, profile, op_hash, object_key, mime, bytes, width, height) \
         VALUES ($1, $2, 'proxy', 'proxy_2048', $3, $4, $5, 17, 2048, 1365)",
    )
    .bind(Uuid::now_v7())
    .bind(f.asset_id)
    .bind(blake3::hash(b"op").to_hex().to_string())
    .bind(PROXY_KEY)
    .bind(mime)
    .execute(&f.tenant)
    .await
    .expect("proxy row");
}

async fn enable(f: &Fixture, settings: Settings) {
    let mut conn = f.tenant.acquire().await.expect("connection");
    enrichment::save_settings(&mut conn, &settings)
        .await
        .expect("settings");
}

/// Stores a credential the way the API does, so the stage has one to open.
async fn credential(f: &Fixture, model: &str) {
    let id = Uuid::now_v7();
    let aad = dam_db::ai_credentials::associated_data("acme", "anthropic", id);
    let sealed = keyring()
        .seal(&Secret::new(FAKE_PROVIDER_KEY.to_owned()), &aad)
        .expect("seal");
    let mut conn = f.tenant.acquire().await.expect("connection");
    dam_db::ai_credentials::add(
        &mut conn,
        &dam_db::ai_credentials::NewCredential {
            id,
            provider: dam_db::ai_credentials::Provider::Anthropic,
            label: "Test key".to_owned(),
            base_url: None,
            sealed_key: sealed,
            sealing_key_id: "k1".to_owned(),
            hint: "…4242".to_owned(),
            default_model: model.to_owned(),
            make_default: true,
        },
    )
    .await
    .expect("credential");
}

fn suggestion_json() -> String {
    json!({
        "alt_text": "A runner on a wet path at dusk",
        "description": "A person running along a rain-soaked path. Low light, long shadows.",
        "tags": ["footwear", "Outdoor", "streetwear"],
        "confidence": 0.71,
    })
    .to_string()
}

fn answering(json_text: String) -> Arc<Recorded> {
    Arc::new(Recorded::always(
        200,
        anthropic_answer(&json_text, "claude-opus-5-20260601", (900, 120, 800, 40)),
    ))
}

async fn run(
    f: &Fixture,
    transport: Arc<Recorded>,
) -> dam_pipeline::Result<dam_pipeline::enrich::Enriched> {
    dam_pipeline::enrich::asset(
        &f.global,
        f.store.as_ref(),
        &ai(transport),
        &f.slug,
        f.tenant_id,
        f.asset_id,
    )
    .await
}

async fn run_state(f: &Fixture) -> (String, i64, i64, String, bool, serde_json::Value) {
    sqlx::query_as(
        "SELECT state, input_tokens, cached_tokens, est_cost_cents::text, used_original, stages \
           FROM enrichment_runs WHERE asset_id = $1 ORDER BY started_at DESC LIMIT 1",
    )
    .bind(f.asset_id)
    .fetch_one(&f.tenant)
    .await
    .expect("run row")
}

#[tokio::test]
async fn a_tenant_who_has_not_switched_it_on_is_never_charged() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    let transport = answering(suggestion_json());

    let enriched = run(&f, Arc::clone(&transport)).await.expect("a run");
    assert!(
        matches!(enriched.outcome, EnrichOutcome::Skipped(ref why) if why.contains("switched off"))
    );
    // The important assertion: no call was made at all.
    assert!(
        transport.sent().is_empty(),
        "a disabled tenant made a paid call"
    );
    let (state, _, _, cost, _, stages) = run_state(&f).await;
    assert_eq!(state, "skipped");
    assert_eq!(cost, "0.0000");
    assert!(
        stages["describe"]["reason"]
            .as_str()
            .expect("reason")
            .contains("switched off"),
        "{stages}"
    );
}

#[tokio::test]
async fn a_tenant_with_no_credential_is_skipped_rather_than_dead_lettered() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await;

    let enriched = run(&f, answering(suggestion_json())).await.expect("a run");
    assert!(
        matches!(enriched.outcome, EnrichOutcome::Skipped(ref why) if why.contains("no model credential")),
        "{:?}",
        enriched.outcome
    );
    assert_eq!(run_state(&f).await.0, "skipped");
}

#[tokio::test]
async fn a_hard_cap_stops_the_call_before_it_is_made() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await;

    let mut conn = f.global.acquire().await.expect("connection");
    quotas::set(
        &mut conn,
        f.tenant_id,
        quotas::AI_SPEND,
        &quotas::Quota {
            limit_value: 100,
            warn_at_fraction: 0.8,
            enforcement: quotas::Enforcement::Hard,
        },
    )
    .await
    .expect("cap");
    let period = quotas::month_start(chrono::Utc::now());
    quotas::charge(
        &mut conn,
        f.tenant_id,
        quotas::AI_SPEND,
        period,
        200 * quotas::MICRO,
    )
    .await
    .expect("spend");

    let transport = answering(suggestion_json());
    let enriched = run(&f, Arc::clone(&transport)).await.expect("a run");
    assert!(
        matches!(enriched.outcome, EnrichOutcome::Skipped(ref why) if why.contains("spend cap")),
        "{:?}",
        enriched.outcome
    );
    // Checked before, not after: a cap discovered by exceeding it is not a cap.
    assert!(transport.sent().is_empty());
}

#[tokio::test]
async fn a_described_asset_gets_values_provenance_disclosure_tags_and_a_bill() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            guidance: "Say 'trainers', not 'sneakers'.".to_owned(),
            ..Settings::default()
        },
    )
    .await;

    let transport = answering(suggestion_json());
    let enriched = run(&f, Arc::clone(&transport)).await.expect("a run");
    let EnrichOutcome::Wrote {
        fields,
        tags,
        unknown_tags,
        micro_cents,
    } = enriched.outcome
    else {
        panic!("expected a write, got {:?}", enriched.outcome);
    };
    assert_eq!(fields, vec!["alt_text", "description"]);
    assert_eq!(
        tags, 2,
        "footwear and outdoor; streetwear is not a term here"
    );
    assert_eq!(
        unknown_tags,
        vec!["streetwear"],
        "the vocabulary gap is worth keeping"
    );

    // The request: the tenant's guidance and its vocabulary in the cached prefix, the proxy after it.
    let sent = transport.only();
    let instructions = sent.body["system"][0]["text"]
        .as_str()
        .expect("instructions");
    assert!(instructions.contains("Say 'trainers'"), "{instructions}");
    assert!(
        instructions.contains("footwear — Footwear"),
        "{instructions}"
    );
    assert_eq!(sent.body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sent.body["messages"][0]["content"][0]["type"], "image");
    assert_eq!(sent.header("x-api-key"), Some(FAKE_PROVIDER_KEY));

    // The values, and the provenance that makes them undoable.
    let (values, provenance): (serde_json::Value, serde_json::Value) =
        sqlx::query_as("SELECT values, provenance FROM asset_metadata WHERE asset_id = $1")
            .bind(f.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("metadata");
    assert_eq!(values["alt_text"], "A runner on a wet path at dusk");
    // The model that answered, not the one that was asked for.
    assert_eq!(provenance["alt_text"]["model"], "claude-opus-5-20260601");
    assert_eq!(provenance["alt_text"]["model_version"], "llm_describe/1");
    assert!(provenance["alt_text"]["reviewed_by"].is_null());

    // The disclosure rows (G2): per field, `metadata_only`, and a digest rather than the prompt.
    let disclosures: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT field_key, disclosure_kind, human_oversight, prompt_digest FROM ai_disclosures \
          WHERE asset_id = $1 ORDER BY field_key",
    )
    .bind(f.asset_id)
    .fetch_all(&f.tenant)
    .await
    .expect("disclosures");
    assert_eq!(disclosures.len(), 2);
    assert_eq!(disclosures[0].0, "alt_text");
    assert_eq!(
        disclosures[0].1, "metadata_only",
        "the picture itself is untouched"
    );
    assert_eq!(disclosures[0].2, "none", "nobody has reviewed it yet");
    let digest = disclosures[0].3.clone().expect("a digest");
    assert_eq!(digest.len(), 64, "a hash, not the prompt");
    assert!(!digest.contains("trainers"));

    // The tags, as suggestions.
    let states: Vec<(String, String, Option<f32>)> = sqlx::query_as(
        "SELECT t.slug, a.state, a.confidence FROM asset_tags a \
         JOIN taxonomy_terms t ON t.id = a.term_id WHERE a.asset_id = $1 ORDER BY t.slug",
    )
    .bind(f.asset_id)
    .fetch_all(&f.tenant)
    .await
    .expect("tags");
    assert_eq!(states.len(), 2);
    assert!(states.iter().all(|(_, state, _)| state == "suggested"));
    assert!((states[0].2.expect("confidence") - 0.71).abs() < 0.001);

    // The run, with the counts as reported and the proxy flag false.
    let (state, input, cached, cost, used_original, stages) = run_state(&f).await;
    assert_eq!(state, "succeeded");
    assert_eq!(input, 900);
    assert_eq!(cached, 800, "the field that says caching is working");
    assert!(!used_original, "a stage reading masters is a restore storm");
    assert_eq!(stages["unknown_tags"][0], "streetwear");
    assert_eq!(stages["vocabulary_truncated"], false);

    // And the money: 900 fresh + 120 out + 800 cached + 40 written on Opus 5, charged in micro-cents.
    assert!(micro_cents > 0);
    assert_eq!(cost, dam_test_cost(micro_cents));
    let mut conn = f.global.acquire().await.expect("connection");
    let period = quotas::month_start(chrono::Utc::now());
    let used = quotas::used(&mut conn, f.tenant_id, quotas::AI_SPEND, period)
        .await
        .expect("used");
    // Under a cent, which is exactly why the spend column carries a remainder — whole cents would record zero.
    assert_eq!(used, micro_cents / quotas::MICRO);
    let remainder: i64 = sqlx::query_scalar(
        "SELECT spend_remainder_micro FROM dam_global.tenant_spend WHERE tenant_id = $1",
    )
    .bind(f.tenant_id)
    .fetch_one(&f.global)
    .await
    .expect("remainder");
    assert_eq!(
        remainder,
        micro_cents % quotas::MICRO,
        "nothing is lost to rounding"
    );
}

/// The cost as the column stores it, so the assertion above does not restate the formatting rule.
fn dam_test_cost(micro: i64) -> String {
    let ten_thousandths = micro / 100 + i64::from(micro % 100 >= 50);
    format!(
        "{}.{:04}",
        ten_thousandths / 10_000,
        ten_thousandths % 10_000
    )
}

#[tokio::test]
async fn a_refusal_is_recorded_and_not_retried() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await;

    let transport = Arc::new(Recorded::always(200, anthropic_refusal("policy")));
    let enriched = run(&f, Arc::clone(&transport))
        .await
        .expect("a refusal is not an error");
    assert!(matches!(enriched.outcome, EnrichOutcome::Declined(Some(ref why)) if why == "policy"));
    // One call, and the job returns Ok so the queue does not spend four more attempts on it.
    assert_eq!(transport.sent().len(), 1);
    let (state, _, _, _, _, stages) = run_state(&f).await;
    assert_eq!(state, "skipped");
    assert_eq!(stages["describe"]["state"], "declined");
    // Nothing was written.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_metadata")
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn a_rejected_key_fails_permanently_and_a_throttle_does_not() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await;

    let rejected = Arc::new(Recorded::always(
        401,
        json!({"error": {"message": "invalid x-api-key"}}),
    ));
    let error = run(&f, rejected).await.expect_err("a rejected key");
    assert!(!error.is_transient(), "{error}");
    assert_eq!(run_state(&f).await.0, "failed");

    let throttled = Arc::new(Recorded::script(vec![Reply::Throttled(
        30,
        json!({"error": {"message": "slow"}}),
    )]));
    let error = run(&f, throttled).await.expect_err("a throttle");
    assert!(error.is_transient(), "{error}");
}

#[tokio::test]
async fn an_asset_with_no_proxy_is_retried_rather_than_failed() {
    let f = fixture().await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await;

    // No proxy row: the derive job has not finished. Retrying is right, and the attempt budget bounds it.
    let error = run(&f, answering(suggestion_json()))
        .await
        .expect_err("no proxy");
    assert!(error.is_transient(), "{error}");
    assert!(error.to_string().contains("no proxy"), "{error}");
}

#[tokio::test]
async fn a_proxy_no_image_block_accepts_is_skipped() {
    let f = fixture().await;
    proxy(&f, "application/pdf").await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await;

    let transport = answering(suggestion_json());
    let enriched = run(&f, Arc::clone(&transport)).await.expect("a run");
    assert!(
        matches!(enriched.outcome, EnrichOutcome::Skipped(ref why) if why.contains("application/pdf")),
        "{:?}",
        enriched.outcome
    );
    assert!(
        transport.sent().is_empty(),
        "a 400 nobody can act on, avoided"
    );
}

#[tokio::test]
async fn a_field_the_tenant_withholds_makes_the_run_partial() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    // Aimed at a field the tenant marked not-ai-writable. The write is refused and the run says so, rather
    // than reporting success over a value nobody stored.
    enable(
        &f,
        Settings {
            is_enabled: true,
            description_field: Some("copyright".to_owned()),
            ..Settings::default()
        },
    )
    .await;

    run(&f, answering(suggestion_json())).await.expect("a run");
    let (state, _, _, _, _, stages) = run_state(&f).await;
    assert_eq!(state, "partial");
    assert_eq!(stages["refused"][0], "copyright");
    assert_eq!(stages["written"][0], "alt_text");
}

#[tokio::test]
async fn the_settings_model_overrides_the_credentials_default() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            // §8.3: model routing per pipeline stage is configuration, not code.
            model: Some("claude-haiku-4-5".to_owned()),
            ..Settings::default()
        },
    )
    .await;

    let transport = answering(suggestion_json());
    run(&f, Arc::clone(&transport)).await.expect("a run");
    assert_eq!(transport.only().body["model"], "claude-haiku-4-5");
}

#[tokio::test]
async fn a_second_run_does_not_undo_a_persons_edit() {
    let f = fixture().await;
    proxy(&f, "image/jpeg").await;
    credential(&f, "claude-opus-5").await;
    enable(
        &f,
        Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await;

    run(&f, answering(suggestion_json()))
        .await
        .expect("first run");
    // A person rewrites the alt text: the value changes and its provenance goes, which is what the API's
    // metadata route does.
    sqlx::query("UPDATE asset_metadata SET values = jsonb_set(values, '{alt_text}', '\"What a person wrote\"'), provenance = provenance - 'alt_text' WHERE asset_id = $1")
        .bind(f.asset_id)
        .execute(&f.tenant)
        .await
        .expect("human edit");

    run(&f, answering(suggestion_json()))
        .await
        .expect("second run");
    let values: serde_json::Value =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(f.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("values");
    assert_eq!(values["alt_text"], "What a person wrote");
    let (state, _, _, _, _, stages) = run_state(&f).await;
    assert_eq!(state, "partial", "something was deliberately not written");
    assert_eq!(stages["kept_human"][0], "alt_text");
}
