//! What the two hosted clients put on the wire (M5a·3).
//!
//! The reply is the boring half. Every feature §8.3 buys — structured output, prompt caching, effort — is a
//! specific shape in the *request*, and a suite that only checked the parsed answer would pass while caching
//! silently never happened and cost three times the estimate. So most of what follows reads the JSON the client
//! built.
//!
//! Fixtures come from the vendors' documented examples. See `crates/dam-ai/src/testing.rs` for the limit that
//! implies: this suite cannot notice a vendor changing a field, and no recorded transport can.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_ai::anthropic::AnthropicModel;
use dam_ai::model::{Ask, Effort, Model, ModelError, Part};
use dam_ai::openai_compatible::OpenAiCompatibleModel;
use dam_ai::testing::{Recorded, Reply, anthropic_answer, anthropic_refusal, openai_answer};
use dam_core::Secret;
use serde_json::{Value, json};
use std::sync::Arc;

fn key() -> Secret<String> {
    // Not a credential: no provider issues keys in this shape, and it is here to be asserted on.
    Secret::new("test-key-not-a-credential".to_owned())
}

fn taxonomy_ask() -> Ask {
    Ask {
        instructions: "You tag photographs against the tenant's taxonomy.".to_owned(),
        parts: vec![
            Part::Text("Tag this.".to_owned()),
            Part::Image {
                media_type: "image/jpeg".to_owned(),
                base64: "AAECAw==".to_owned(),
            },
        ],
        schema: Some(json!({
            "type": "object",
            "properties": {"tags": {"type": "array", "items": {"type": "string"}}},
            "required": ["tags"],
        })),
        max_tokens: 512,
        effort: Effort::Low,
    }
}

fn prose_ask() -> Ask {
    Ask {
        instructions: "You write alt text.".to_owned(),
        parts: vec![Part::Text("Describe it.".to_owned())],
        schema: None,
        max_tokens: 300,
        effort: Effort::High,
    }
}

// ---------------------------------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_messages_request_carries_the_three_features_the_cost_model_assumes() {
    let transport = Arc::new(Recorded::always(
        200,
        anthropic_answer(r#"{"tags":["dog"]}"#, "claude-opus-5", (10, 4, 0, 0)),
    ));
    let model = AnthropicModel::new(transport.clone(), key(), None, "claude-opus-5");
    model.ask(&taxonomy_ask()).await.expect("a completion");

    let sent = transport.only();
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(sent.header("x-api-key"), Some("test-key-not-a-credential"));
    assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(sent.header("content-type"), Some("application/json"));

    // The instructions are a block *array*, because only the array form takes a cache breakpoint. A bare string
    // here would be a request that works and a bill that is ten times what §8.3 costed.
    let system = sent.body["system"].as_array().expect("a system array");
    assert_eq!(system.len(), 1, "one breakpoint, at the end of the prefix");
    assert_eq!(
        system[0]["text"],
        "You tag photographs against the tenant's taxonomy."
    );
    assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));

    // Structured output under its current name. `output_format` is the deprecated spelling and would be
    // accepted-and-ignored by some gateways, which is the worst of both.
    assert_eq!(sent.body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(
        sent.body["output_config"]["format"]["schema"],
        taxonomy_ask().schema.expect("the ask had one")
    );
    assert_eq!(sent.body["output_config"]["effort"], "low");

    // Rejected with a 400 on this model family — thinking is adaptive and on by default. Absent, not zero.
    assert!(sent.body.get("thinking").is_none(), "{}", sent.body);
    assert!(
        sent.body.pointer("/thinking/budget_tokens").is_none(),
        "budget_tokens is a 400 here"
    );

    // The per-asset half goes after the breakpoint, and an image is `source`, not `image_url`.
    let content = sent.body["messages"][0]["content"]
        .as_array()
        .expect("user content");
    assert_eq!(content[0], json!({"type": "text", "text": "Tag this."}));
    assert_eq!(
        content[1],
        json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAECAw=="},
        })
    );
    assert_eq!(sent.body["max_tokens"], 512);
}

#[tokio::test]
async fn no_schema_means_no_format_and_no_parsing() {
    let transport = Arc::new(Recorded::always(
        200,
        anthropic_answer("A dog on grass.", "claude-opus-5", (9, 6, 0, 0)),
    ));
    let model = AnthropicModel::new(transport.clone(), key(), None, "claude-opus-5");
    let completion = model.ask(&prose_ask()).await.expect("a completion");

    assert_eq!(completion.text, "A dog on grass.");
    assert!(completion.structured.is_none(), "nothing asked for a shape");
    // `effort` still travels — it is the alt-text path, which §8.3 says gets the expensive setting.
    let sent = transport.only();
    assert_eq!(sent.body["output_config"]["effort"], "high");
    assert!(
        sent.body["output_config"].get("format").is_none(),
        "a format with no schema would constrain prose to nothing"
    );
}

#[tokio::test]
async fn every_token_count_the_bill_depends_on_is_read() {
    let transport = Arc::new(Recorded::always(
        200,
        anthropic_answer(r#"{"tags":[]}"#, "claude-opus-5", (11, 22, 33, 44)),
    ));
    let model = AnthropicModel::new(transport, key(), None, "claude-opus-5");
    let usage = model
        .ask(&taxonomy_ask())
        .await
        .expect("a completion")
        .usage;

    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 22);
    // The one that says caching worked. A client that dropped it would leave nobody able to tell a 90% discount
    // from a full-price call after the fact.
    assert_eq!(usage.cached_input_tokens, 33);
    assert_eq!(usage.cache_write_tokens, 44);
}

#[tokio::test]
async fn a_refusal_arrives_as_a_success_and_is_still_a_refusal() {
    let transport = Arc::new(Recorded::always(
        200,
        anthropic_refusal("the image appears to contain a real person"),
    ));
    let model = AnthropicModel::new(transport, key(), None, "claude-opus-5");
    let error = model
        .ask(&taxonomy_ask())
        .await
        .expect_err("a refusal is not a completion");

    match &error {
        ModelError::Declined(Some(why)) => {
            assert!(why.contains("real person"), "{why}");
        }
        other => panic!("expected a refusal carrying its reason, got {other:?}"),
    }
    // The whole point: the queue must not retry this. `content` was empty, so a client that read the body
    // before the stop reason would have returned an empty completion and looked like success.
    assert!(!error.is_transient());
}

#[tokio::test]
async fn the_model_that_answered_is_the_one_recorded() {
    // A server-side substitution is legal and an `enrichment_runs` row naming the model we *asked* for would be
    // a provenance record that lies — and provenance is the thing G2 marking depends on.
    let transport = Arc::new(Recorded::always(
        200,
        anthropic_answer(r#"{"tags":[]}"#, "claude-opus-5-20260601", (1, 1, 0, 0)),
    ));
    let model = AnthropicModel::new(transport, key(), None, "claude-opus-5");
    let completion = model.ask(&taxonomy_ask()).await.expect("a completion");
    assert_eq!(completion.model, "claude-opus-5-20260601");
    assert_eq!(model.model_name(), "claude-opus-5");
}

#[tokio::test]
async fn an_answer_that_ignored_the_schema_is_an_error_here_not_three_layers_up() {
    let transport = Arc::new(Recorded::always(
        200,
        anthropic_answer("Sure! Here are some tags:", "claude-opus-5", (1, 1, 0, 0)),
    ));
    let model = AnthropicModel::new(transport, key(), None, "claude-opus-5");
    let error = model.ask(&taxonomy_ask()).await.expect_err("not json");
    assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");
    assert!(!error.is_transient(), "retrying will not make it json");
}

#[tokio::test]
async fn a_throttle_keeps_the_wait_the_provider_asked_for() {
    let transport = Arc::new(Recorded::script(vec![Reply::Throttled(
        42,
        json!({"error": {"type": "rate_limit_error", "message": "slow down"}}),
    )]));
    let model = AnthropicModel::new(transport, key(), None, "claude-opus-5");
    let error = model.ask(&prose_ask()).await.expect_err("throttled");
    assert!(
        matches!(error, ModelError::RateLimited(Some(42))),
        "{error:?}"
    );
    assert!(error.is_transient());
}

#[tokio::test]
async fn a_rejected_key_is_permanent_and_a_dead_gateway_is_not() {
    let bad_key = Arc::new(Recorded::always(
        401,
        json!({"error": {"message": "invalid x-api-key"}}),
    ));
    let model = AnthropicModel::new(bad_key, key(), None, "claude-opus-5");
    assert!(matches!(
        model.ask(&prose_ask()).await.expect_err("401"),
        ModelError::Unauthorised
    ));

    let down = Arc::new(Recorded::script(vec![Reply::Broken(
        "connection reset by peer".to_owned(),
    )]));
    let model = AnthropicModel::new(down, key(), None, "claude-opus-5");
    let error = model.ask(&prose_ask()).await.expect_err("unreachable");
    assert!(error.is_transient(), "{error:?}");
}

#[tokio::test]
async fn only_a_2xx_is_an_answer() {
    // A 3xx is not success. A gateway answering 300 or 302 to a POST is a deployment that has been
    // misconfigured or moved, and reading its body as a completion turns that into "the model said nothing" —
    // permanent, unretried, and blamed on the model. The boundary is exclusive of 300 for that reason.
    let transport = Arc::new(Recorded::always(
        300,
        json!({"error": {"message": "moved"}}),
    ));
    let model = AnthropicModel::new(transport, key(), None, "claude-opus-5");
    let error = model
        .ask(&prose_ask())
        .await
        .expect_err("a 300 is not an answer");
    assert!(error.is_transient(), "{error:?}");
}

#[tokio::test]
async fn a_gateway_base_url_is_used_verbatim_bar_a_trailing_slash() {
    let transport = Arc::new(Recorded::always(
        200,
        anthropic_answer("ok", "claude-opus-5", (1, 1, 0, 0)),
    ));
    let model = AnthropicModel::new(
        transport.clone(),
        key(),
        Some("https://gateway.example.test/anthropic/"),
        "claude-opus-5",
    );
    model.ask(&prose_ask()).await.expect("a completion");
    assert_eq!(
        transport.only().url,
        "https://gateway.example.test/anthropic/v1/messages"
    );
}

#[tokio::test]
async fn one_ask_is_one_call() {
    // Retries and backoff belong to the queue, which already counts attempts. A client that retried too would
    // multiply the queue's budget by its own and bill a tenant for it.
    let transport = Arc::new(Recorded::always(
        500,
        json!({"error": {"message": "overloaded"}}),
    ));
    let model = AnthropicModel::new(transport.clone(), key(), None, "claude-opus-5");
    let _ = model.ask(&prose_ask()).await;
    assert_eq!(transport.sent().len(), 1);
}

// ---------------------------------------------------------------------------------------------------
// OpenAI-compatible
// ---------------------------------------------------------------------------------------------------

const KIMI: &str = "https://api.moonshot.ai/v1";

#[tokio::test]
async fn the_chat_completions_request_is_the_other_spelling_of_the_same_ask() {
    let transport = Arc::new(Recorded::always(
        200,
        openai_answer(r#"{"tags":["dog"]}"#, "kimi-k2", (30, 5, 20)),
    ));
    let model = OpenAiCompatibleModel::new(transport.clone(), key(), KIMI, "kimi-k2");
    model.ask(&taxonomy_ask()).await.expect("a completion");

    let sent = transport.only();
    assert_eq!(sent.url, "https://api.moonshot.ai/v1/chat/completions");
    assert_eq!(
        sent.header("authorization"),
        Some("Bearer test-key-not-a-credential")
    );

    // Instructions become a system *message*, and they stay first: caching in this family is automatic and
    // prefix-matched, so ordering is the whole discount.
    assert_eq!(sent.body["messages"][0]["role"], "system");
    assert_eq!(
        sent.body["messages"][0]["content"],
        "You tag photographs against the tenant's taxonomy."
    );
    assert_eq!(sent.body["messages"][1]["role"], "user");

    // An image is a data URI, not a link. A link would mean every asset needed a URL the provider could reach.
    assert_eq!(
        sent.body["messages"][1]["content"][1],
        json!({
            "type": "image_url",
            "image_url": {"url": "data:image/jpeg;base64,AAECAw=="},
        })
    );

    assert_eq!(sent.body["response_format"]["type"], "json_schema");
    assert_eq!(
        sent.body["response_format"]["json_schema"]["name"],
        "answer"
    );
    assert_eq!(
        sent.body["response_format"]["json_schema"]["schema"],
        taxonomy_ask().schema.expect("the ask had one")
    );
    // Unknown fields are a 400 on several of these servers, so effort is opt-in per credential.
    assert!(
        sent.body.get("reasoning_effort").is_none(),
        "not sent unless the credential says the endpoint knows it"
    );
}

#[tokio::test]
async fn strict_mode_is_claimed_only_for_a_schema_that_can_satisfy_it() {
    // `strict: true` is a guarantee, and a schema outside strict mode's subset is a 400 rather than a graceful
    // downgrade. One `Ask` has to work against both providers, so the schema opts in on its own terms.
    let loose = Arc::new(Recorded::always(
        200,
        openai_answer(r#"{"tags":[]}"#, "gpt-x", (1, 1, 0)),
    ));
    let model = OpenAiCompatibleModel::new(loose.clone(), key(), KIMI, "gpt-x");
    model.ask(&taxonomy_ask()).await.expect("a completion");
    assert_eq!(
        loose.only().body["response_format"]["json_schema"]["strict"],
        false
    );

    let strict_transport = Arc::new(Recorded::always(
        200,
        openai_answer(r#"{"tags":[]}"#, "gpt-x", (1, 1, 0)),
    ));
    let mut ask = taxonomy_ask();
    ask.schema = Some(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {"tags": {"type": "array", "items": {"type": "string"}}},
        "required": ["tags"],
    }));
    let model = OpenAiCompatibleModel::new(strict_transport.clone(), key(), KIMI, "gpt-x");
    model.ask(&ask).await.expect("a completion");
    assert_eq!(
        strict_transport.only().body["response_format"]["json_schema"]["strict"],
        true
    );
}

#[tokio::test]
async fn effort_travels_when_the_credential_says_the_endpoint_knows_it() {
    let transport = Arc::new(Recorded::always(
        200,
        openai_answer("A dog.", "gpt-x", (1, 1, 0)),
    ));
    let model = OpenAiCompatibleModel::new(transport.clone(), key(), KIMI, "gpt-x")
        .sending_reasoning_effort();
    model.ask(&prose_ask()).await.expect("a completion");
    assert_eq!(transport.only().body["reasoning_effort"], "high");
}

#[tokio::test]
async fn a_cached_prefix_is_not_counted_twice() {
    // This family reports `prompt_tokens` *including* the cached part; Anthropic's `input_tokens` excludes it.
    // `Usage` has to mean one thing, or a cost estimate written against it overstates one provider by the size
    // of its cache hit — which, for a tenant's shared taxonomy prefix, is most of the prompt.
    let transport = Arc::new(Recorded::always(
        200,
        openai_answer(r#"{"tags":[]}"#, "kimi-k2", (1000, 40, 900)),
    ));
    let model = OpenAiCompatibleModel::new(transport, key(), KIMI, "kimi-k2");
    let usage = model
        .ask(&taxonomy_ask())
        .await
        .expect("a completion")
        .usage;
    assert_eq!(usage.input_tokens, 100, "the uncached remainder");
    assert_eq!(usage.cached_input_tokens, 900);
    assert_eq!(usage.output_tokens, 40);
    // Nobody in this family bills a cache write, so zero is a fact rather than a missing field.
    assert_eq!(usage.cache_write_tokens, 0);
}

#[tokio::test]
async fn both_spellings_of_a_refusal_are_refusals() {
    let written = Arc::new(Recorded::always(
        200,
        json!({
            "model": "gpt-x",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": null, "refusal": "I can't help with that."},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0},
        }),
    ));
    let model = OpenAiCompatibleModel::new(written, key(), KIMI, "gpt-x");
    let error = model.ask(&taxonomy_ask()).await.expect_err("a refusal");
    assert!(
        matches!(&error, ModelError::Declined(Some(why)) if why.contains("can't help")),
        "{error:?}"
    );
    assert!(!error.is_transient());

    let filtered = Arc::new(Recorded::always(
        200,
        json!({
            "model": "gpt-x",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": ""},
                "finish_reason": "content_filter",
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0},
        }),
    ));
    let model = OpenAiCompatibleModel::new(filtered, key(), KIMI, "gpt-x");
    let error = model.ask(&taxonomy_ask()).await.expect_err("filtered");
    assert!(matches!(error, ModelError::Declined(None)), "{error:?}");
    assert!(
        !error.is_transient(),
        "the server's own verdict, not a fault"
    );
}

#[tokio::test]
async fn an_answer_with_no_choices_is_unreadable_rather_than_empty() {
    let transport = Arc::new(Recorded::always(
        200,
        json!({"model": "gpt-x", "choices": []}),
    ));
    let model = OpenAiCompatibleModel::new(transport, key(), KIMI, "gpt-x");
    let error = model.ask(&prose_ask()).await.expect_err("no choices");
    assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");
}

/// A status, and what a caller should be able to conclude from the error it became.
type Verdict = fn(&ModelError) -> bool;

#[tokio::test]
async fn the_openai_client_maps_statuses_the_same_way() {
    // Same mapping, deliberately: the queue reads `is_transient` and cannot know which vendor answered.
    let cases: Vec<(u16, Verdict)> = vec![
        (401, |error| matches!(error, ModelError::Unauthorised)),
        (403, |error| matches!(error, ModelError::Unauthorised)),
        (400, |error| !error.is_transient()),
        (503, |error| error.is_transient()),
    ];
    for (status, expected) in cases {
        let body: Value = json!({"error": {"message": "no"}});
        let transport = Arc::new(Recorded::always(status, body));
        let model = OpenAiCompatibleModel::new(transport, key(), KIMI, "gpt-x");
        let error = model.ask(&prose_ask()).await.expect_err("an error");
        assert!(expected(&error), "{status} mapped to {error:?}");
    }
}
