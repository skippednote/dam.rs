//! The Messages API, over raw HTTP (M5a·3).
//!
//! There is no official Anthropic Rust SDK — ARCHITECTURE §8.3 says so and says what to do instead: speak
//! `POST /v1/messages` with `x-api-key` and `anthropic-version: 2023-06-01`. This is that, behind
//! [`crate::model::Model`].
//!
//! ## Three features this exists to use
//!
//! §8.3 picks Anthropic for enrichment on the strength of them, and skipping any one changes the cost model:
//!
//! - **Structured output** (`output_config.format` with a JSON schema) so an extraction deserialises instead of
//!   being parsed out of prose. Note the field: `output_format` is the deprecated spelling.
//! - **Prompt caching** (`cache_control: {type: "ephemeral"}` on the last block of the stable prefix). The
//!   instructions — a tenant's brand guidance and taxonomy — are byte-identical across every asset, and the
//!   per-asset content goes after the breakpoint. ~90% off that prefix, and the way to know it is working is
//!   `usage.cache_read_input_tokens`, which is why [`crate::model::Usage`] carries it.
//! - **Effort** (`output_config.effort`) so bulk classification is cheap and anything a person reads is not.
//!
//! ## What is deliberately not sent
//!
//! `budget_tokens` is **rejected with a 400** on Opus 5 — thinking is adaptive and on by default, so the
//! parameter is simply absent here rather than set to something. Assistant prefill is likewise gone from the
//! model family; structured output is the replacement and is what this uses.
//!
//! ## A refusal is an answer, not a failure
//!
//! `stop_reason: "refusal"` arrives with HTTP 200. A DAM enriching somebody's whole library *will* meet it, so
//! `stop_reason` is checked before the content is read — the alternative is a queue retrying a refusal five
//! times and dead-lettering an asset that was never going to work.

use crate::model::{Ask, Completion, Model, ModelError, Part, Transport, Usage, status_error};
use dam_core::Secret;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The version header every request carries. Pinned, because the wire format is only stable per version.
const API_VERSION: &str = "2023-06-01";

/// Where the Messages API lives, when a credential does not name a gateway.
const DEFAULT_BASE: &str = "https://api.anthropic.com";

/// A model reached through Anthropic's Messages API.
pub struct AnthropicModel {
    transport: Arc<dyn Transport>,
    key: Secret<String>,
    base_url: String,
    model: String,
}

impl std::fmt::Debug for AnthropicModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The model and the endpoint, never the key. A `Debug` that printed it is how a credential reaches a log.
        f.debug_struct("AnthropicModel")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl AnthropicModel {
    /// A client for one tenant's credential.
    ///
    /// `base_url` is an override for a gateway or a proxy; `None` is the vendor's own endpoint.
    pub fn new(
        transport: Arc<dyn Transport>,
        key: Secret<String>,
        base_url: Option<&str>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            key,
            base_url: base_url
                .unwrap_or(DEFAULT_BASE)
                .trim_end_matches('/')
                .to_owned(),
            model: model.into(),
        }
    }

    /// The request body for an ask.
    ///
    /// Separate from sending it so a test can read the JSON. That is not ceremony: every one of the three
    /// features above is a specific shape in this object, and a test that only checked the *reply* would pass
    /// while caching silently never happened.
    pub fn body(&self, ask: &Ask) -> serde_json::Value {
        message_params(&self.model, ask)
    }
}

/// The `params` of a Messages request, for one model and one ask.
///
/// A free function because the Batch API needs exactly this object per request and must not build it a second
/// way: a batch that asked a subtly different question would produce answers nobody could compare with the
/// synchronous path, and the difference would be invisible until somebody diffed two descriptions.
pub fn message_params(model: &str, ask: &Ask) -> serde_json::Value {
    let mut content = Vec::with_capacity(ask.parts.len());
    for part in &ask.parts {
        content.push(match part {
            Part::Text(text) => serde_json::json!({"type": "text", "text": text}),
            Part::Image { media_type, base64 } => serde_json::json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": base64},
            }),
        });
    }

    let mut output_config = serde_json::Map::new();
    output_config.insert("effort".to_owned(), ask.effort.as_str().into());
    if let Some(schema) = &ask.schema {
        output_config.insert(
            "format".to_owned(),
            serde_json::json!({"type": "json_schema", "schema": schema}),
        );
    }

    serde_json::json!({
        "model": model,
        "max_tokens": ask.max_tokens,
        // A block array rather than a bare string, because only the array form takes `cache_control`. The
        // breakpoint is here, at the end of the stable prefix: everything after it is per-asset and would
        // invalidate the cache on every call if it were inside.
        "system": [{
            "type": "text",
            "text": ask.instructions,
            "cache_control": {"type": "ephemeral"},
        }],
        "messages": [{"role": "user", "content": content}],
        "output_config": output_config,
    })
}

#[async_trait::async_trait]
impl Model for AnthropicModel {
    async fn ask(&self, ask: &Ask) -> Result<Completion, ModelError> {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        headers.insert("anthropic-version".to_owned(), API_VERSION.to_owned());
        // The one place the plaintext key is touched, and it goes straight into a header.
        headers.insert("x-api-key".to_owned(), self.key.expose().clone());

        let url = format!("{}/v1/messages", self.base_url);
        let answer = self
            .transport
            .post_json(&url, &headers, self.body(ask))
            .await?;
        if !answer.is_success() {
            return Err(status_error(
                answer.status,
                &answer.body,
                answer.retry_after,
            ));
        }
        let body = answer.body;

        // Checked before the content, because a refusal is a 200. Reading `content` first would produce an empty
        // completion and a caller wondering why the model said nothing.
        if body.get("stop_reason").and_then(|v| v.as_str()) == Some("refusal") {
            let why = body
                .pointer("/stop_details/explanation")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    body.pointer("/stop_details/category")
                        .and_then(|v| v.as_str())
                })
                .map(str::to_owned);
            return Err(ModelError::Declined(why));
        }

        let blocks = body
            .get("content")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| ModelError::Unreadable("the answer has no content array".to_owned()))?;
        let text = blocks
            .iter()
            .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<&str>>()
            .join("");

        // With a schema, the first text block *is* the JSON. Parsed here rather than by the caller so a model
        // that ignored the schema is one error rather than a surprise three layers up.
        let structured = match &ask.schema {
            Some(_) => Some(serde_json::from_str(text.trim()).map_err(|error| {
                ModelError::Unreadable(format!(
                    "the answer was not the JSON the schema asked for: {error}"
                ))
            })?),
            None => None,
        };

        let usage = body.get("usage");
        let count = |name: &str| {
            usage
                .and_then(|u| u.get(name))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };

        Ok(Completion {
            text,
            structured,
            // The model that answered, which is not always the one asked for — a server-side fallback can
            // substitute one, and an `enrichment_runs` row naming the wrong model is a provenance record that
            // lies.
            model: body
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or(&self.model)
                .to_owned(),
            usage: Usage {
                input_tokens: count("input_tokens"),
                output_tokens: count("output_tokens"),
                cached_input_tokens: count("cache_read_input_tokens"),
                cache_write_tokens: count("cache_creation_input_tokens"),
            },
        })
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}
