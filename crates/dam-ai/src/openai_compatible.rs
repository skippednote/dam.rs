//! `/chat/completions`, which is not one vendor but a de-facto wire format (M5a·3).
//!
//! OpenAI defined it; Kimi/Moonshot, DeepSeek, Together, Groq, Fireworks, OpenRouter, vLLM, llama.cpp and
//! Ollama all answer it. So this is one client for the whole field, and a vendor here is two strings — a base
//! URL and a model name — held in a row of `ai_credentials`. That is the point: the user asked for "claude /
//! chatgpt / kimi / etc", and the honest way to say "etc" in code is this format rather than a client per logo.
//!
//! ## Where it differs from [`crate::anthropic`]
//!
//! Same three ideas, three different spellings, and the differences are exactly why [`crate::model::Ask`] exists:
//!
//! | idea | Anthropic | here |
//! |---|---|---|
//! | instructions | `system` block array | a `system` message in `messages` |
//! | image | `source: {type: "base64", media_type, data}` | `image_url: {url: "data:…;base64,…"}` |
//! | schema | `output_config.format` | `response_format.json_schema` |
//! | cached prefix | `cache_control` breakpoint | automatic, prefix-matched, no request field |
//! | cache accounting | `cache_read_input_tokens` | `prompt_tokens_details.cached_tokens` |
//!
//! Caching needs no request field here, which sounds simpler and is worse: it is automatic and prefix-matched,
//! so the *ordering* discipline in [`crate::model::Ask`] — stable instructions first, per-asset content after —
//! is what earns the discount, and nothing in the request will tell you when you have lost it. Only
//! `cached_tokens` will.
//!
//! ## Two things this refuses to assume
//!
//! **The base URL carries the version segment.** `https://api.openai.com/v1`, `https://api.moonshot.ai/v1`,
//! `https://api.groq.com/openai/v1`, `http://localhost:11434/v1` — the prefix before `/chat/completions` is not
//! predictable from the vendor, so it is stored per credential rather than guessed here.
//!
//! **Unknown fields are not free.** OpenAI ignores extras; several of these servers answer 400. So
//! `reasoning_effort` is opt-in per credential ([`OpenAiCompatibleModel::sending_reasoning_effort`]) instead of
//! always sent, and `strict` is only set for a schema that meets strict mode's own rules. The alternative is a
//! client that works against the one vendor it was tested on.

use crate::model::{Ask, Completion, Model, ModelError, Part, Transport, Usage, status_error};
use dam_core::Secret;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The name a schema is registered under in `response_format`. Required by the format, unused by the caller.
const SCHEMA_NAME: &str = "answer";

/// A model reached through an OpenAI-compatible `/chat/completions` endpoint.
pub struct OpenAiCompatibleModel {
    transport: Arc<dyn Transport>,
    key: Secret<String>,
    base_url: String,
    model: String,
    reasoning_effort: bool,
}

impl std::fmt::Debug for OpenAiCompatibleModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleModel")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatibleModel {
    /// A client for one tenant's credential. `base_url` includes the version segment — see the module note.
    pub fn new(
        transport: Arc<dyn Transport>,
        key: Secret<String>,
        base_url: &str,
        model: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model: model.into(),
            reasoning_effort: false,
        }
    }

    /// Send `reasoning_effort` to this endpoint.
    ///
    /// Off by default because a server that does not know the field may reject the whole request, and a tenant
    /// discovering that through failed enrichment runs is a worse trade than a slightly dearer default.
    #[must_use]
    pub fn sending_reasoning_effort(mut self) -> Self {
        self.reasoning_effort = true;
        self
    }

    /// The request body for an ask. Public for the same reason as [`crate::anthropic::AnthropicModel::body`]:
    /// the shape *is* the behaviour, and only a test that reads it can pin it.
    pub fn body(&self, ask: &Ask) -> serde_json::Value {
        let mut content = Vec::with_capacity(ask.parts.len());
        for part in &ask.parts {
            content.push(match part {
                Part::Text(text) => serde_json::json!({"type": "text", "text": text}),
                Part::Image { media_type, base64 } => serde_json::json!({
                    "type": "image_url",
                    // A data URI rather than an http one, deliberately: an `image_url` pointing at object
                    // storage would need the provider to be able to reach the tenant's bytes, which means a
                    // public or presigned URL for every asset enriched. Inline base64 keeps the bytes on the
                    // path they were already travelling.
                    "image_url": {"url": format!("data:{media_type};base64,{base64}")},
                }),
            });
        }

        let mut object = serde_json::Map::new();
        object.insert("model".to_owned(), self.model.clone().into());
        object.insert("max_tokens".to_owned(), ask.max_tokens.into());
        object.insert(
            "messages".to_owned(),
            serde_json::json!([
                {"role": "system", "content": ask.instructions},
                {"role": "user", "content": content},
            ]),
        );

        if let Some(schema) = &ask.schema {
            // `strict` makes the server *guarantee* the shape rather than merely ask for it — but it accepts
            // only a subset of JSON Schema, and a schema outside that subset is a 400 rather than a downgrade.
            // Opting in on the schema's own say-so keeps one `Ask` usable against both providers.
            let strict =
                schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false));
            object.insert(
                "response_format".to_owned(),
                serde_json::json!({
                    "type": "json_schema",
                    "json_schema": {"name": SCHEMA_NAME, "schema": schema, "strict": strict},
                }),
            );
        }
        if self.reasoning_effort {
            object.insert("reasoning_effort".to_owned(), ask.effort.as_str().into());
        }
        serde_json::Value::Object(object)
    }
}

#[async_trait::async_trait]
impl Model for OpenAiCompatibleModel {
    async fn ask(&self, ask: &Ask) -> Result<Completion, ModelError> {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        headers.insert(
            "authorization".to_owned(),
            format!("Bearer {}", self.key.expose()),
        );

        let url = format!("{}/chat/completions", self.base_url);
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

        let choice = body
            .pointer("/choices/0")
            .ok_or_else(|| ModelError::Unreadable("the answer has no choices".to_owned()))?;

        // Two spellings of the same event. `refusal` is a string the model wrote; `content_filter` is the
        // server's own verdict. Either way the request was well formed and the answer is no, so it must not
        // reach the queue as a failure — see `ModelError::Declined`.
        if let Some(refusal) = choice
            .pointer("/message/refusal")
            .and_then(|value| value.as_str())
        {
            return Err(ModelError::Declined(Some(refusal.to_owned())));
        }
        if choice.get("finish_reason").and_then(|v| v.as_str()) == Some("content_filter") {
            return Err(ModelError::Declined(None));
        }

        let text = choice
            .pointer("/message/content")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ModelError::Unreadable("the answer has no message content".to_owned()))?
            .to_owned();

        let structured = match &ask.schema {
            Some(_) => Some(serde_json::from_str(text.trim()).map_err(|error| {
                ModelError::Unreadable(format!(
                    "the answer was not the JSON the schema asked for: {error}"
                ))
            })?),
            None => None,
        };

        let count = |pointer: &str| {
            body.pointer(pointer)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        let cached = count("/usage/prompt_tokens_details/cached_tokens");

        Ok(Completion {
            text,
            structured,
            model: body
                .get("model")
                .and_then(|value| value.as_str())
                .unwrap_or(&self.model)
                .to_owned(),
            usage: Usage {
                // `prompt_tokens` here is inclusive of the cached part, where Anthropic's `input_tokens` is
                // exclusive of it. Subtracting keeps `Usage` meaning one thing, so a cost estimate written
                // against it does not silently double-count on one provider.
                input_tokens: count("/usage/prompt_tokens").saturating_sub(cached),
                output_tokens: count("/usage/completion_tokens"),
                cached_input_tokens: cached,
                // No provider in this family reports a cache *write*, because writing is not a billable event
                // for them. Zero is the truth, not a gap.
                cache_write_tokens: 0,
            },
        })
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}
