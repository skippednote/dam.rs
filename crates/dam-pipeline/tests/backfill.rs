//! A library backfill, end to end against a real database and a scripted provider (M5c).
//!
//! §8.3 routes all backfill through the Batch API, and the properties worth defending are the ones that only
//! show up at library scale:
//!
//! - **A run row exists before the batch is posted**, or an unordered result has nothing to claim it.
//! - **A submission that fails closes its runs.** An open run makes an asset invisible to the work list, so a
//!   failed submission that left them open would silently skip part of the library forever.
//! - **Nothing is applied before the batch ends**, and polling is a re-queue rather than a wait.
//! - **Every terminal state is handled, including a result that never arrives.** Errored is a failure, expired
//!   and cancelled are not — those requests were never billed and their assets go back on the list.
//! - **A batched call is charged at half price**, which is the entire reason this path exists.
//! - **The work list does not re-describe what is already described**, and does not skip what merely failed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_ai::testing::{Recorded, Reply};
use dam_core::sealed::SealingKeyring;
use dam_core::{Secret, TenantSlug};
use dam_db::enrichment::{self, Settings};
use dam_db::{migrate, quotas, testing::PostgresHarness};
use dam_pipeline::backfill::{Collected, Submitted};
use dam_pipeline::enrich::AiContext;
use dam_store::{BlobStore, FakeS3Store, Key};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const FAKE_PROVIDER_KEY: &str = "sk-test-not-a-credential-4242";

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    tenant: PgPool,
    /// The concrete fake, so the same object store can be handed to a stage as a `BlobStore` and to a worker
    /// context as a `ResumableStore` — a backfill driven through the queue has to read the proxies the fixture
    /// wrote.
    store: Arc<FakeS3Store>,
    slug: TenantSlug,
    tenant_id: Uuid,
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

    for key in ["alt_text", "description"] {
        sqlx::query(
            "INSERT INTO field_defs (id, key, label, kind, ai_writable) VALUES ($1, $2, $2, 'text', true)",
        )
        .bind(Uuid::now_v7())
        .bind(key)
        .execute(&tenant)
        .await
        .expect("field def");
    }

    let store = FakeS3Store::with_test_clock().0;
    Fixture {
        _pg: pg,
        global,
        tenant,
        store: Arc::new(store),
        slug: TenantSlug::new("acme").expect("slug"),
        tenant_id,
    }
}

/// An asset with a proxy object and a proxy row.
async fn asset(f: &Fixture, name: &str, mime: &str) -> Uuid {
    let id = Uuid::now_v7();
    let content_hash = blake3::hash(name.as_bytes()).to_hex().to_string();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(&content_hash)
    .bind(format!("{name}.jpg"))
    .execute(&f.tenant)
    .await
    .expect("asset");

    let object_key = format!("proxy/{name}.jpg");
    let key = Key::new(object_key.clone()).expect("key");
    f.store
        .put(
            &key,
            bytes::Bytes::from_static(b"not really a jpeg"),
            dam_core::StorageClass::Standard,
        )
        .await
        .expect("proxy object");
    sqlx::query(
        "INSERT INTO derivatives \
            (id, asset_id, role, profile, op_hash, object_key, mime, bytes, width, height) \
         VALUES ($1, $2, 'proxy', 'proxy_2048', $3, $4, $5, 17, 2048, 1365)",
    )
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(&object_key)
    .bind(mime)
    .execute(&f.tenant)
    .await
    .expect("proxy row");
    id
}

async fn enable(f: &Fixture) {
    let mut conn = f.tenant.acquire().await.expect("connection");
    enrichment::save_settings(
        &mut conn,
        &Settings {
            is_enabled: true,
            ..Settings::default()
        },
    )
    .await
    .expect("settings");
}

async fn credential(f: &Fixture, provider: dam_db::ai_credentials::Provider) {
    let id = Uuid::now_v7();
    let aad = dam_db::ai_credentials::associated_data("acme", provider.as_str(), id);
    let sealed = keyring()
        .seal(&Secret::new(FAKE_PROVIDER_KEY.to_owned()), &aad)
        .expect("seal");
    let mut conn = f.tenant.acquire().await.expect("connection");
    dam_db::ai_credentials::add(
        &mut conn,
        &dam_db::ai_credentials::NewCredential {
            id,
            provider,
            label: "Test key".to_owned(),
            base_url: match provider {
                dam_db::ai_credentials::Provider::Anthropic => None,
                dam_db::ai_credentials::Provider::OpenAiCompatible => {
                    Some("https://api.moonshot.ai/v1".to_owned())
                }
            },
            sealed_key: sealed,
            sealing_key_id: "k1".to_owned(),
            hint: "…4242".to_owned(),
            default_model: "claude-opus-5".to_owned(),
            make_default: true,
        },
    )
    .await
    .expect("credential");
}

/// One succeeded result line for a run.
fn succeeded(custom_id: &str, tokens: (u64, u64, u64)) -> String {
    json!({
        "custom_id": custom_id,
        "result": {
            "type": "succeeded",
            "message": {
                "id": "msg_1",
                "model": "claude-opus-5-20260601",
                "content": [{"type": "text", "text": json!({
                    "alt_text": "A harbour at dawn",
                    "description": "Boats at rest before sunrise.",
                    "tags": [],
                    "confidence": 0.7,
                }).to_string()}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": tokens.0,
                    "output_tokens": tokens.1,
                    "cache_read_input_tokens": tokens.2,
                    "cache_creation_input_tokens": 0,
                },
            },
        },
    })
    .to_string()
}

async fn open_runs(f: &Fixture) -> Vec<(Uuid, Option<String>, Option<String>, String)> {
    sqlx::query_as(
        "SELECT id, llm_batch_id, llm_custom_id, state FROM enrichment_runs ORDER BY started_at",
    )
    .fetch_all(&f.tenant)
    .await
    .expect("runs")
}

#[tokio::test]
async fn a_submission_opens_a_run_per_asset_before_it_posts_the_batch() {
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    let first = asset(&f, "harbour", "image/jpeg").await;
    let second = asset(&f, "dawn", "image/jpeg").await;
    // Not describable: no image block takes a PDF, so it must not go in the batch.
    asset(&f, "brochure", "application/pdf").await;

    let transport = Arc::new(Recorded::always(
        200,
        json!({"id": "msgbatch_01", "processing_status": "in_progress"}),
    ));
    let submitted = dam_pipeline::backfill::submit(
        &f.global,
        f.store.as_ref(),
        &ai(Arc::clone(&transport)),
        &f.slug,
        f.tenant_id,
        50,
    )
    .await
    .expect("a submission");

    assert_eq!(
        submitted,
        Submitted::Batch {
            batch_id: "msgbatch_01".to_owned(),
            count: 2
        }
    );

    // Every request carries the run id it will be claimed by — the only thing that lets an unordered result
    // find its asset after a restart.
    let sent = transport.only();
    let requests = sent.body["requests"].as_array().expect("requests");
    assert_eq!(requests.len(), 2);

    let runs = open_runs(&f).await;
    assert_eq!(runs.len(), 2, "one per describable asset, and no more");
    for (id, batch_id, custom_id, state) in &runs {
        assert_eq!(state, "running");
        assert_eq!(batch_id.as_deref(), Some("msgbatch_01"));
        assert_eq!(custom_id.as_deref(), Some(id.to_string().as_str()));
        assert!(
            requests
                .iter()
                .any(|request| request["custom_id"] == id.to_string()),
            "run {id} is not in the batch"
        );
    }

    // And the asset with no describable proxy has no run at all.
    let described: Vec<Uuid> = sqlx::query_scalar("SELECT asset_id FROM enrichment_runs")
        .fetch_all(&f.tenant)
        .await
        .expect("assets");
    assert!(described.contains(&first) && described.contains(&second));
}

#[tokio::test]
async fn a_failed_submission_closes_the_runs_it_opened() {
    // The failure that matters: an open run makes its asset invisible to the work list, so a submission that
    // died leaving them open would skip part of the library for good.
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    asset(&f, "harbour", "image/jpeg").await;

    let transport = Arc::new(Recorded::always(
        500,
        json!({"error": {"message": "overloaded"}}),
    ));
    let error = dam_pipeline::backfill::submit(
        &f.global,
        f.store.as_ref(),
        &ai(transport),
        &f.slug,
        f.tenant_id,
        50,
    )
    .await
    .expect_err("a failed submission");
    assert!(error.is_transient(), "{error}");

    let runs = open_runs(&f).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].3, "failed", "the run was closed, not abandoned");

    // Which means the asset is a candidate again.
    let mut conn = f.tenant.acquire().await.expect("connection");
    let candidates = enrichment::needing_description(
        &mut conn,
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
        50,
    )
    .await
    .expect("candidates");
    assert_eq!(candidates.len(), 1);
}

#[tokio::test]
async fn nothing_is_applied_until_the_batch_has_ended() {
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    asset(&f, "harbour", "image/jpeg").await;

    let transport = Arc::new(Recorded::script(vec![
        Reply::Http(
            200,
            json!({"id": "msgbatch_01", "processing_status": "in_progress"}),
        ),
        Reply::Http(
            200,
            json!({
                "id": "msgbatch_01",
                "processing_status": "in_progress",
                "request_counts": {"processing": 1, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0},
            }),
        ),
    ]));
    let context = ai(Arc::clone(&transport));
    dam_pipeline::backfill::submit(
        &f.global,
        f.store.as_ref(),
        &context,
        &f.slug,
        f.tenant_id,
        50,
    )
    .await
    .expect("submitted");

    let collected =
        dam_pipeline::backfill::collect(&f.global, &context, &f.slug, f.tenant_id, "msgbatch_01")
            .await
            .expect("a poll");
    assert_eq!(
        collected,
        Collected::Waiting {
            finished: 0,
            total: 1
        }
    );

    // Nothing written, and the run still open — the provider offers no partial results, so neither does this.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_metadata")
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(rows, 0);
    assert_eq!(open_runs(&f).await[0].3, "running");
    // Two calls: the submit and one poll. No results fetch before it has ended.
    assert_eq!(transport.sent().len(), 2);
}

#[tokio::test]
async fn an_ended_batch_is_applied_at_half_price() {
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    let asset_id = asset(&f, "harbour", "image/jpeg").await;

    let submit = Arc::new(Recorded::always(
        200,
        json!({"id": "msgbatch_01", "processing_status": "in_progress"}),
    ));
    let context = ai(Arc::clone(&submit));
    dam_pipeline::backfill::submit(
        &f.global,
        f.store.as_ref(),
        &context,
        &f.slug,
        f.tenant_id,
        50,
    )
    .await
    .expect("submitted");
    let run_id = open_runs(&f).await[0].0;

    let transport = Arc::new(Recorded::script(vec![
        Reply::Http(
            200,
            json!({
                "id": "msgbatch_01",
                "processing_status": "ended",
                "request_counts": {"processing": 0, "succeeded": 1, "errored": 0, "canceled": 0, "expired": 0},
            }),
        ),
        Reply::Text(200, succeeded(&run_id.to_string(), (300, 20, 2700))),
    ]));
    let collected = dam_pipeline::backfill::collect(
        &f.global,
        &ai(Arc::clone(&transport)),
        &f.slug,
        f.tenant_id,
        "msgbatch_01",
    )
    .await
    .expect("applied");

    // Half the synchronous price, which is the whole reason §8.3 sends backfill this way.
    let usage = dam_ai::model::Usage {
        input_tokens: 300,
        output_tokens: 20,
        cached_input_tokens: 2700,
        cache_write_tokens: 0,
    };
    let full = dam_ai::pricing::Prices::default().estimate("claude-opus-5-20260601", &usage);
    assert_eq!(
        collected,
        Collected::Applied {
            wrote: 1,
            declined: 0,
            errored: 0,
            expired: 0,
            micro_cents: full / 2,
        }
    );

    // The values went through the same write path as a synchronous run: provenance, disclosure, the lot.
    let (values, provenance): (serde_json::Value, serde_json::Value) =
        sqlx::query_as("SELECT values, provenance FROM asset_metadata WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("metadata");
    assert_eq!(values["alt_text"], "A harbour at dawn");
    assert_eq!(provenance["alt_text"]["model"], "claude-opus-5-20260601");
    let disclosures: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_disclosures")
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(disclosures, 2, "one per written field");

    let (state, cost, cached): (String, String, i64) = sqlx::query_as(
        "SELECT state, est_cost_cents::text, cached_tokens FROM enrichment_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_one(&f.tenant)
    .await
    .expect("run");
    assert_eq!(state, "succeeded");
    assert_eq!(
        cached, 2700,
        "the cached prefix, which is most of the saving"
    );
    assert_ne!(cost, "0.0000");

    // And the spend is charged once, at the batched rate.
    let mut conn = f.global.acquire().await.expect("connection");
    let period = quotas::month_start(chrono::Utc::now());
    let remainder: i64 = sqlx::query_scalar(
        "SELECT spend_remainder_micro FROM dam_global.tenant_spend WHERE tenant_id = $1",
    )
    .bind(f.tenant_id)
    .fetch_one(&f.global)
    .await
    .expect("remainder");
    let used = quotas::used(&mut conn, f.tenant_id, quotas::AI_SPEND, period)
        .await
        .expect("used");
    assert_eq!(used * quotas::MICRO + remainder, full / 2);
}

#[tokio::test]
async fn every_terminal_state_is_closed_and_only_failures_look_like_failures() {
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    for name in ["one", "two", "three", "four", "five"] {
        asset(&f, name, "image/jpeg").await;
    }

    let submit = Arc::new(Recorded::always(
        200,
        json!({"id": "msgbatch_01", "processing_status": "in_progress"}),
    ));
    dam_pipeline::backfill::submit(
        &f.global,
        f.store.as_ref(),
        &ai(submit),
        &f.slug,
        f.tenant_id,
        50,
    )
    .await
    .expect("submitted");
    let runs: Vec<Uuid> = open_runs(&f).await.into_iter().map(|row| row.0).collect();
    assert_eq!(runs.len(), 5);

    // One of each, and the fifth request simply absent from the results.
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        succeeded(&runs[0].to_string(), (10, 5, 0)),
        json!({"custom_id": runs[1].to_string(), "result": {"type": "errored", "error": {"error": {"message": "overloaded"}}}}),
        json!({"custom_id": runs[2].to_string(), "result": {"type": "expired"}}),
        json!({
            "custom_id": runs[3].to_string(),
            "result": {"type": "succeeded", "message": {
                "model": "claude-opus-5",
                "content": [],
                "stop_reason": "refusal",
                "stop_details": {"type": "refusal", "explanation": "policy"},
                "usage": {"input_tokens": 10, "output_tokens": 0},
            }},
        }),
    );
    let transport = Arc::new(Recorded::script(vec![
        Reply::Http(
            200,
            json!({
                "id": "msgbatch_01",
                "processing_status": "ended",
                "request_counts": {"processing": 0, "succeeded": 2, "errored": 1, "canceled": 0, "expired": 1},
            }),
        ),
        Reply::Text(200, body),
    ]));
    let collected = dam_pipeline::backfill::collect(
        &f.global,
        &ai(transport),
        &f.slug,
        f.tenant_id,
        "msgbatch_01",
    )
    .await
    .expect("applied");

    let Collected::Applied {
        wrote,
        declined,
        errored,
        expired,
        ..
    } = collected
    else {
        panic!("expected an application, got {collected:?}");
    };
    assert_eq!(wrote, 1);
    assert_eq!(declined, 1, "a refusal is not a failure");
    assert_eq!(
        errored, 2,
        "the errored one, and the one that never arrived"
    );
    assert_eq!(expired, 1);

    // Nothing is left running: an open run hides its asset from the work list for good.
    let still_open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM enrichment_runs WHERE state = 'running'")
            .fetch_one(&f.tenant)
            .await
            .expect("count");
    assert_eq!(still_open, 0);

    // And each one is closed as what it *was*. This is not bookkeeping pedantry: a run marked `failed` reads as
    // "something went wrong here" to whoever looks at the failure list, and an expired request means the
    // provider never ran it — nothing went wrong and nothing was billed.
    let states: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT id, state, stages -> 'describe' ->> 'state' FROM enrichment_runs ORDER BY started_at",
    )
    .fetch_all(&f.tenant)
    .await
    .expect("states");
    let state_of = |run: Uuid| {
        states
            .iter()
            .find(|(id, _, _)| *id == run)
            .map(|(_, state, describe)| (state.clone(), describe.clone()))
            .expect("a run")
    };
    assert_eq!(state_of(runs[0]).0, "succeeded");
    assert_eq!(
        state_of(runs[1]),
        ("failed".to_owned(), Some("errored".to_owned()))
    );
    assert_eq!(
        state_of(runs[2]),
        ("skipped".to_owned(), Some("expired".to_owned())),
        "an expired request never ran and was never billed"
    );
    assert_eq!(
        state_of(runs[3]),
        ("skipped".to_owned(), Some("declined".to_owned()))
    );
    assert_eq!(
        state_of(runs[4]),
        ("failed".to_owned(), Some("missing_from_results".to_owned())),
        "a result that never arrived is a failure, and a named one"
    );

    // And the expired one is a candidate again — it never ran and was never billed.
    let mut conn = f.tenant.acquire().await.expect("connection");
    let candidates = enrichment::needing_description(
        &mut conn,
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
        50,
    )
    .await
    .expect("candidates");
    // The expired, the refused, the errored and the missing — four of the five, all still to do.
    assert_eq!(candidates.len(), 4);
}

#[tokio::test]
async fn an_openai_compatible_credential_is_told_why_it_cannot_batch() {
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::OpenAiCompatible).await;
    asset(&f, "harbour", "image/jpeg").await;

    let transport = Arc::new(Recorded::always(200, json!({"id": "msgbatch_01"})));
    let submitted = dam_pipeline::backfill::submit(
        &f.global,
        f.store.as_ref(),
        &ai(Arc::clone(&transport)),
        &f.slug,
        f.tenant_id,
        50,
    )
    .await
    .expect("a refusal, not an error");

    match submitted {
        Submitted::Nothing(why) => {
            assert!(why.contains("Anthropic"), "{why}");
            assert!(why.contains("full price"), "{why}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Nothing was posted, and no run was opened for a batch that could not happen.
    assert!(transport.sent().is_empty());
    assert!(open_runs(&f).await.is_empty());
}

#[tokio::test]
async fn a_hard_cap_stops_a_backfill_before_it_starts() {
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    asset(&f, "harbour", "image/jpeg").await;

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

    let transport = Arc::new(Recorded::always(200, json!({"id": "msgbatch_01"})));
    let submitted = dam_pipeline::backfill::submit(
        &f.global,
        f.store.as_ref(),
        &ai(Arc::clone(&transport)),
        &f.slug,
        f.tenant_id,
        50,
    )
    .await
    .expect("a refusal");
    assert!(matches!(submitted, Submitted::Nothing(ref why) if why.contains("spend cap")));
    // A whole batch is one commitment: a cap reached halfway through is not something the provider offers.
    assert!(transport.sent().is_empty());
}

#[tokio::test]
async fn the_work_list_skips_what_is_described_and_keeps_what_merely_failed() {
    let f = fixture().await;
    let described = asset(&f, "described", "image/jpeg").await;
    let failed = asset(&f, "failed", "image/jpeg").await;
    let untouched = asset(&f, "untouched", "image/jpeg").await;

    let mut conn = f.tenant.acquire().await.expect("connection");
    for (asset_id, outcome) in [
        (described, dam_db::enrichment::Outcome::Succeeded),
        (failed, dam_db::enrichment::Outcome::Failed),
    ] {
        let run = enrichment::start_run(
            &mut conn,
            asset_id,
            dam_ai::enrich::PIPELINE,
            dam_ai::enrich::PIPELINE_VERSION,
        )
        .await
        .expect("run");
        enrichment::finish_run(
            &mut conn,
            run,
            outcome,
            dam_db::enrichment::Cost::default(),
            &json!({}),
            None,
            false,
        )
        .await
        .expect("finish");
    }

    let candidates = enrichment::needing_description(
        &mut conn,
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
        50,
    )
    .await
    .expect("candidates");
    let ids: Vec<Uuid> = candidates.iter().map(|c| c.asset_id).collect();
    assert!(!ids.contains(&described), "already described");
    assert!(
        ids.contains(&failed),
        "a failure was a bad day, not a verdict"
    );
    assert!(ids.contains(&untouched));

    // The progress counts a screen shows.
    let progress = enrichment::backfill_progress(
        &mut conn,
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
    )
    .await
    .expect("progress");
    assert_eq!(progress.outstanding, 2);
    assert_eq!(progress.described, 1);
    assert_eq!(progress.in_flight, 0);
}

#[tokio::test]
async fn a_new_prompt_version_makes_the_whole_library_a_candidate_again() {
    // §8.3's "re-run everything the old prompt touched", and the reason the version is on the row.
    let f = fixture().await;
    let asset_id = asset(&f, "described", "image/jpeg").await;
    let mut conn = f.tenant.acquire().await.expect("connection");
    let run = enrichment::start_run(&mut conn, asset_id, dam_ai::enrich::PIPELINE, 1)
        .await
        .expect("run");
    enrichment::finish_run(
        &mut conn,
        run,
        dam_db::enrichment::Outcome::Succeeded,
        dam_db::enrichment::Cost::default(),
        &json!({}),
        None,
        false,
    )
    .await
    .expect("finish");

    let same = enrichment::needing_description(&mut conn, dam_ai::enrich::PIPELINE, 1, 50)
        .await
        .expect("candidates");
    assert!(same.is_empty());

    let newer = enrichment::needing_description(&mut conn, dam_ai::enrich::PIPELINE, 2, 50)
        .await
        .expect("candidates");
    assert_eq!(newer.len(), 1);
}

/// A worker context over the fixture, so the *chain* can be driven rather than the stages.
fn context(f: &Fixture, transport: Arc<Recorded>) -> dam_pipeline::worker::Context {
    dam_pipeline::worker::Context {
        global: f.global.clone(),
        store: Arc::new(dam_store::FakeS3Store::with_test_clock().0),
        scanner: None,
        signing_identity: None,
        indexes: Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
            std::path::Path::new("/tmp/damrs-backfill-chain"),
        ))),
        ai: Some(ai(transport)),
        worker: "backfill-chain-test".to_owned(),
        // No webhook subscriptions in these fixtures, so nothing is ever dispatched. A default client
        // rather than a builder, because what these suites exercise is unrelated to how it is configured.
        http: reqwest::Client::new(),
    }
}

/// Claims and handles one job of a kind, returning its id.
/// Claims and runs the next job of `kind`, making it due first.
///
/// The "making it due" is the point. A collector enqueues its own next poll with a `run_after` in the future —
/// that delay is production behaviour, and another case in this file asserts it exists. A test that claimed
/// immediately was therefore racing it: the job is real, queued, and not yet claimable, so `claim` returned
/// nothing and the case failed about one run in ten with "collect ran". Two gate suites died on that before it
/// was worth chasing.
///
/// Bringing the timestamp forward is what a test wants and says so, where a sleep would trade a flake for a
/// slower flake.
async fn run_one(f: &Fixture, context: &dam_pipeline::worker::Context, kind: &str) -> Option<Uuid> {
    sqlx::query(
        "UPDATE dam_global.jobs SET run_after = now() \
          WHERE kind = $1 AND state = 'queued' AND run_after > now()",
    )
    .bind(kind)
    .execute(&f.global)
    .await
    .expect("make the delayed poll due");

    let claimed = dam_db::jobs::claim(
        &f.global,
        &context.worker,
        dam_db::jobs::ClaimOptions::default(),
    )
    .await
    .expect("claim");
    let job = claimed.iter().find(|job| job.kind == kind)?;
    dam_pipeline::worker::handle(context, job)
        .await
        .expect("handle");
    dam_db::jobs::complete(&f.global, job.id)
        .await
        .expect("complete");
    Some(job.id)
}

#[tokio::test]
async fn a_batch_that_is_still_working_leaves_a_poll_behind_it() {
    // The bug this exists for: the collector used to re-queue itself under the same dedupe key, which conflicts
    // with the *running* job doing the enqueueing — so `enqueue` returned that job's own id, the handler
    // completed, and the batch was never polled again. Stage-level tests could not see it; only driving the
    // chain through the queue can.
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    asset(&f, "harbour", "image/jpeg").await;

    // The store the *context* holds is a fresh fake, so the submit stage must read the proxy through it: put
    // the object there too.
    let transport = Arc::new(Recorded::script(vec![
        Reply::Http(
            200,
            json!({"id": "msgbatch_01", "processing_status": "in_progress"}),
        ),
        Reply::Http(
            200,
            json!({
                "id": "msgbatch_01",
                "processing_status": "in_progress",
                "request_counts": {"processing": 1, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0},
            }),
        ),
    ]));
    let mut context = context(&f, Arc::clone(&transport));
    context.store = f.store.clone() as Arc<dyn dam_store::ResumableStore>;

    dam_pipeline::worker::enqueue_backfill(&f.global, f.tenant_id, 50)
        .await
        .expect("enqueue");
    run_one(&f, &context, dam_pipeline::worker::kind::BACKFILL_SUBMIT)
        .await
        .expect("the submit job ran");

    // Submitting queued a collector.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs WHERE kind = 'backfill_collect' AND state = 'queued'",
    )
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(queued, 1);

    run_one(&f, &context, dam_pipeline::worker::kind::BACKFILL_COLLECT)
        .await
        .expect("the collect job ran");

    // And the poll that said "still working" left another behind it, waiting.
    let (queued, waiting): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE run_after > now()) FROM dam_global.jobs \
          WHERE kind = 'backfill_collect' AND state = 'queued'",
    )
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(queued, 1, "the chain must not end while a batch is open");
    assert_eq!(waiting, 1, "and it waits rather than polling in a loop");
}

#[tokio::test]
async fn an_applied_batch_queues_the_next_slice_and_reindexes_what_it_wrote() {
    let f = fixture().await;
    enable(&f).await;
    credential(&f, dam_db::ai_credentials::Provider::Anthropic).await;
    let asset_id = asset(&f, "harbour", "image/jpeg").await;
    // A second asset, so there is a next slice to queue.
    asset(&f, "dawn", "image/jpeg").await;

    let submit = Arc::new(Recorded::always(
        200,
        json!({"id": "msgbatch_01", "processing_status": "in_progress"}),
    ));
    let mut context = context(&f, Arc::clone(&submit));
    context.store = f.store.clone() as Arc<dyn dam_store::ResumableStore>;
    dam_pipeline::worker::enqueue_backfill(&f.global, f.tenant_id, 1)
        .await
        .expect("enqueue");
    run_one(&f, &context, dam_pipeline::worker::kind::BACKFILL_SUBMIT)
        .await
        .expect("submit ran");

    let run_id = open_runs(&f).await[0].0;
    let transport = Arc::new(Recorded::script(vec![
        Reply::Http(
            200,
            json!({
                "id": "msgbatch_01",
                "processing_status": "ended",
                "request_counts": {"processing": 0, "succeeded": 1, "errored": 0, "canceled": 0, "expired": 0},
            }),
        ),
        Reply::Text(200, succeeded(&run_id.to_string(), (250, 40, 1000))),
    ]));
    let mut applied = context;
    applied.ai = Some(ai(transport));
    run_one(&f, &applied, dam_pipeline::worker::kind::BACKFILL_COLLECT)
        .await
        .expect("collect ran");

    // The described asset is queued for reindexing — a description nothing indexed is a description nobody can
    // search for.
    let index_jobs: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM dam_global.jobs WHERE kind = 'index' AND state = 'queued'",
    )
    .fetch_all(&f.global)
    .await
    .expect("index jobs");
    assert!(
        index_jobs
            .iter()
            .any(|payload| payload["asset_id"] == asset_id.to_string()),
        "{index_jobs:?}"
    );

    // And the next slice is queued, because a backfill is a chain of batches rather than one big one.
    let next: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs WHERE kind = 'backfill_submit' AND state = 'queued'",
    )
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(next, 1);
}
