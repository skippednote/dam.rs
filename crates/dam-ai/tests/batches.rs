//! What the Batch API client puts on the wire, and how it reads what comes back (M5c).
//!
//! The reply half matters more here than it does for the synchronous client, because the results format is
//! genuinely awkward: JSONL, unordered, with four terminal states per request, three of which are not
//! successes. A backfill over a million assets that mis-read one of them would leave a silent hole.
//!
//! What is asserted:
//!
//! - **The submitted params are the synchronous params.** A batch that asked a subtly different question would
//!   produce descriptions nobody could compare with the live path, and nothing would say so.
//! - **`custom_id` travels verbatim**, because it is the only thing that lets an unordered result find its asset.
//! - **Nothing is read before `ended`.** Not even the finished requests — the provider does not offer them.
//! - **A refusal in a batch is a refusal**, not an empty description, exactly as on the synchronous path.
//! - **Errored, expired and cancelled stay apart.** They mean different things: retry, resubmit, somebody's
//!   decision.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_ai::batch::{AnthropicBatch, BatchItem, BatchOutcome, BatchState};
use dam_ai::model::{Ask, Effort, ModelError, Part};
use dam_ai::testing::{Recorded, Reply};
use dam_core::Secret;
use serde_json::json;
use std::sync::Arc;

const KEY: &str = "test-key-not-a-credential";

fn ask(text: &str) -> Ask {
    Ask {
        instructions: "You tag photographs against the tenant's taxonomy.".to_owned(),
        parts: vec![
            Part::Text(text.to_owned()),
            Part::Image {
                media_type: "image/jpeg".to_owned(),
                base64: "AAECAw==".to_owned(),
            },
        ],
        schema: Some(json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"tags": {"type": "array", "items": {"type": "string"}}},
            "required": ["tags"],
        })),
        max_tokens: 512,
        effort: Effort::Low,
    }
}

fn client(transport: Arc<Recorded>) -> AnthropicBatch {
    AnthropicBatch::new(
        transport,
        Secret::new(KEY.to_owned()),
        None,
        "claude-opus-5",
    )
}

fn items() -> Vec<BatchItem> {
    vec![
        BatchItem {
            custom_id: "run-0001".to_owned(),
            ask: ask("the first asset"),
        },
        BatchItem {
            custom_id: "run-0002".to_owned(),
            ask: ask("the second asset"),
        },
    ]
}

#[tokio::test]
async fn a_submission_carries_the_synchronous_params_under_each_custom_id() {
    let transport = Arc::new(Recorded::always(
        200,
        json!({"id": "msgbatch_01", "processing_status": "in_progress"}),
    ));
    let batch = client(Arc::clone(&transport));
    let id = batch.submit(&items()).await.expect("a batch id");
    assert_eq!(id, "msgbatch_01");

    let sent = transport.only();
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages/batches");
    assert_eq!(sent.header("x-api-key"), Some(KEY));
    assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));

    let requests = sent.body["requests"].as_array().expect("requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["custom_id"], "run-0001");
    assert_eq!(requests[1]["custom_id"], "run-0002");

    // The params are the synchronous ones, feature for feature. This is the assertion that stops a batch from
    // quietly costing full price: no `cache_control` here and the shared prefix is billed fresh a million times.
    let params = &requests[0]["params"];
    assert_eq!(params["model"], "claude-opus-5");
    assert_eq!(params["max_tokens"], 512);
    assert_eq!(params["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(params["output_config"]["format"]["type"], "json_schema");
    assert_eq!(params["output_config"]["effort"], "low");
    assert_eq!(params["messages"][0]["content"][1]["type"], "image");
    // And no per-request key: the credential is a header on the submission, not something repeated a hundred
    // thousand times inside the body.
    assert!(params.get("api_key").is_none());
}

#[tokio::test]
async fn an_empty_or_oversized_batch_is_refused_before_it_is_sent() {
    let transport = Arc::new(Recorded::always(200, json!({"id": "msgbatch_01"})));
    let batch = client(Arc::clone(&transport));

    let error = batch.submit(&[]).await.expect_err("nothing to submit");
    assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");

    let too_many: Vec<BatchItem> = (0..=dam_ai::batch::MAX_REQUESTS)
        .map(|n| BatchItem {
            custom_id: format!("run-{n}"),
            ask: ask("x"),
        })
        .collect();
    let error = batch.submit(&too_many).await.expect_err("too many");
    assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");

    // Neither reached the provider: a request that will be refused is cheaper not to make, and a 400 for a
    // hundred thousand requests is a large upload wasted.
    assert!(transport.sent().is_empty());
}

#[tokio::test]
async fn a_poll_reports_progress_and_only_ended_permits_a_read() {
    let transport = Arc::new(Recorded::script(vec![
        Reply::Http(
            200,
            json!({
                "id": "msgbatch_01",
                "processing_status": "in_progress",
                "request_counts": {"processing": 2, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0},
            }),
        ),
        Reply::Http(
            200,
            json!({
                "id": "msgbatch_01",
                "processing_status": "ended",
                "request_counts": {"processing": 0, "succeeded": 1, "errored": 1, "canceled": 0, "expired": 0},
            }),
        ),
    ]));
    let batch = client(Arc::clone(&transport));

    let status = batch.poll("msgbatch_01").await.expect("a status");
    assert_eq!(status.state, BatchState::InProgress);
    assert!(!status.is_ended(), "nothing is readable yet");
    assert_eq!(status.processing, 2);
    assert_eq!(status.finished(), 0);

    let status = batch.poll("msgbatch_01").await.expect("a status");
    assert!(status.is_ended());
    assert_eq!(status.succeeded, 1);
    assert_eq!(status.errored, 1);
    assert_eq!(status.finished(), 2, "however each one got there");

    assert_eq!(
        transport.sent()[0].url,
        "https://api.anthropic.com/v1/messages/batches/msgbatch_01"
    );
}

#[tokio::test]
async fn a_status_this_build_does_not_know_is_treated_as_still_working() {
    // Failing closed: reading results that are not there would turn a provider adding a status into a backfill
    // that reported every asset as missing.
    let transport = Arc::new(Recorded::always(
        200,
        json!({"id": "msgbatch_01", "processing_status": "reticulating_splines"}),
    ));
    let status = client(transport)
        .poll("msgbatch_01")
        .await
        .expect("a status");
    assert_eq!(status.state, BatchState::InProgress);
    assert!(!status.is_ended());
}

#[tokio::test]
async fn results_are_jsonl_and_every_terminal_state_stays_itself() {
    let lines = [
        json!({
            "custom_id": "run-0001",
            "result": {
                "type": "succeeded",
                "message": {
                    "id": "msg_1",
                    "model": "claude-opus-5-20260601",
                    "content": [{"type": "text", "text": "{\"tags\":[\"dog\"]}"}],
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 300,
                        "output_tokens": 20,
                        "cache_read_input_tokens": 2700,
                        "cache_creation_input_tokens": 0,
                    },
                },
            },
        }),
        json!({
            "custom_id": "run-0002",
            "result": {
                "type": "succeeded",
                "message": {
                    "id": "msg_2",
                    "model": "claude-opus-5",
                    "content": [],
                    "stop_reason": "refusal",
                    "stop_details": {"type": "refusal", "explanation": "policy"},
                    "usage": {"input_tokens": 40, "output_tokens": 0},
                },
            },
        }),
        json!({
            "custom_id": "run-0003",
            "result": {"type": "errored", "error": {"error": {"message": "overloaded"}}},
        }),
        json!({"custom_id": "run-0004", "result": {"type": "expired"}}),
        json!({"custom_id": "run-0005", "result": {"type": "canceled"}}),
    ];
    // A blank line in the middle, because a JSONL file legitimately has them and a parser that choked would
    // lose the whole batch.
    let body = format!(
        "{}\n\n{}\n{}\n{}\n{}\n",
        lines[0], lines[1], lines[2], lines[3], lines[4]
    );

    let transport = Arc::new(Recorded::script(vec![Reply::Text(200, body)]));
    let batch = client(Arc::clone(&transport));
    let results = batch.results("msgbatch_01").await.expect("results");
    assert_eq!(results.len(), 5);
    assert_eq!(
        transport.only().url,
        "https://api.anthropic.com/v1/messages/batches/msgbatch_01/results"
    );

    // The answer, with its usage — including the cache read, which is what says the discount happened.
    let BatchOutcome::Answered(completion) = &results[0].outcome else {
        panic!("expected an answer, got {:?}", results[0].outcome);
    };
    assert_eq!(results[0].custom_id, "run-0001");
    assert_eq!(completion.model, "claude-opus-5-20260601");
    assert_eq!(completion.usage.cached_input_tokens, 2700);
    assert_eq!(
        completion.structured.as_ref().expect("structured")["tags"][0],
        "dog"
    );

    // A refusal is a *successful* request whose answer is no — the same rule as the synchronous path, and the
    // one most likely to be got wrong here because the message sits two levels down.
    assert!(
        matches!(&results[1].outcome, BatchOutcome::Declined(Some(why)) if why == "policy"),
        "{:?}",
        results[1].outcome
    );

    // Three ways to fail, kept apart because they call for three different things.
    assert!(
        matches!(&results[2].outcome, BatchOutcome::Errored(why) if why.contains("overloaded")),
        "{:?}",
        results[2].outcome
    );
    assert_eq!(results[3].outcome, BatchOutcome::Expired);
    assert_eq!(results[4].outcome, BatchOutcome::Canceled);
}

#[tokio::test]
async fn a_result_line_that_cannot_be_read_is_an_error_rather_than_a_missing_asset() {
    // A dropped line is an asset left undescribed with nothing to say why, which is the failure mode a backfill
    // over a large library must not have.
    let transport = Arc::new(Recorded::script(vec![Reply::Text(
        200,
        "{\"custom_id\":\"run-0001\",\"result\":{\"type\":\"succeeded\",\"message\":{}}}\nnot json at all\n"
            .to_owned(),
    )]));
    let error = client(transport)
        .results("msgbatch_01")
        .await
        .expect_err("a broken line");
    assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");
    assert!(error.to_string().contains("line 2"), "{error}");

    // And a line with no custom_id: nothing could claim it, so it cannot be silently skipped either.
    let transport = Arc::new(Recorded::script(vec![Reply::Text(
        200,
        "{\"result\":{\"type\":\"expired\"}}\n".to_owned(),
    )]));
    let error = client(transport)
        .results("msgbatch_01")
        .await
        .expect_err("no custom_id");
    assert!(error.to_string().contains("custom_id"), "{error}");
}

#[tokio::test]
async fn a_rejected_key_is_permanent_on_the_batch_endpoints_too() {
    let transport = Arc::new(Recorded::always(
        401,
        json!({"error": {"message": "invalid x-api-key"}}),
    ));
    let batch = client(Arc::clone(&transport));
    assert!(matches!(
        batch.submit(&items()).await.expect_err("401"),
        ModelError::Unauthorised
    ));
    assert!(matches!(
        batch.poll("msgbatch_01").await.expect_err("401"),
        ModelError::Unauthorised
    ));
    assert!(matches!(
        batch.results("msgbatch_01").await.expect_err("401"),
        ModelError::Unauthorised
    ));
}
