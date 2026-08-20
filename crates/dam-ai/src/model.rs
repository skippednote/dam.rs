//! One shape for "ask a model", and the transport seam that makes it testable (M5a·3).
//!
//! ## Why a trait at all
//!
//! §8.3 specifies Anthropic and builds the enrichment cost model on two Anthropic-specific features — the Batch
//! API at half price and prompt caching at roughly 90% off a shared prefix. So Anthropic is not one provider
//! among equals here; it is the one the economics assume. The trait exists because a tenant may bring its own
//! key for something else, and because the *caller* — "describe this image against this taxonomy" — should not
//! know which vendor answered.
//!
//! Two implementations cover the field: [`crate::anthropic`] for the Messages API, and
//! [`crate::openai_compatible`] for `/chat/completions`, which OpenAI, Kimi/Moonshot, DeepSeek, Together, Groq
//! and every local server imitating them all speak. A vendor is a base URL and a model name.
//!
//! ## The transport is injected, and that is not only for tests
//!
//! Enrichment costs money per call. A test suite that could reach the network would either be useless (no key)
//! or expensive and flaky (a key), so [`Transport`] is a trait and the suites drive a recorded one. The same
//! seam is what lets a deployment put a proxy, a gateway or a rate limiter in front without the clients
//! knowing — which is the version of this that matters in production.
//!
//! ## What the caller gets back
//!
//! [`Completion`] carries the text, the parsed JSON when a schema was asked for, and the token counts *as the
//! provider reported them*. Not a cost in cents: prices change, differ per model and per vendor, and a number
//! computed here would be a guess baked into a database. The counts are facts; `enrichment_runs.est_cost_cents`
//! is where an estimate belongs, computed by whoever knows the current price list.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Why a model call failed.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The provider refused on policy grounds.
    ///
    /// Its own variant because it is **not** an error in the usual sense: the request was well formed, the
    /// provider answered, and the answer is "no". A DAM enriching a customer's whole library will meet this —
    /// somebody's asset will trip a classifier — and treating it as a transport failure would retry it five
    /// times and then dead-letter the asset. Carries whatever the provider said about why.
    #[error("the model declined this request{}", detail_suffix(.0))]
    Declined(Option<String>),

    /// The credential was rejected.
    ///
    /// Separate from a transient failure because retrying will not fix it, and separate from a refusal because
    /// the fix is a person changing a setting rather than the content of a request.
    #[error("the provider rejected the credential")]
    Unauthorised,

    /// Rate limited. Carries the provider's own retry hint in seconds, when it gave one.
    #[error("rate limited by the provider")]
    RateLimited(Option<u64>),

    /// The provider was reachable and unhappy in some other way, or unreachable.
    #[error("{0}")]
    Transient(String),

    /// The provider answered with something this build cannot read.
    ///
    /// A wire-format change, a truncated body, a schema the model ignored. Not transient: retrying an
    /// unparseable answer produces another unparseable answer.
    #[error("the provider's answer could not be read: {0}")]
    Unreadable(String),
}

fn detail_suffix(detail: &Option<String>) -> String {
    detail
        .as_ref()
        .map(|why| format!(": {why}"))
        .unwrap_or_default()
}

impl ModelError {
    /// Whether trying again could plausibly succeed.
    ///
    /// The queue reads this. A refusal and a bad key are permanent by nature; a 429 and a 503 are not.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Transient(_))
    }
}

/// A piece of what the model is being asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    /// An image, already downsampled by the caller.
    ///
    /// §8.3 budgets enrichment at 1568px on the long edge, which is also the size above which providers
    /// downsample server-side and bill for the original. Resizing is the pipeline's job — this type carries
    /// bytes and says what they are.
    Image {
        media_type: String,
        /// Base64, no data-URI prefix. The prefix is a wire detail and the two providers spell it differently.
        base64: String,
    },
}

/// What to ask, and how to constrain the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    /// The stable prefix: instructions, the tenant's taxonomy, few-shot examples.
    ///
    /// Kept separate from [`Self::parts`] because it is the half that repeats across every asset in a tenant,
    /// and providers that support prompt caching cache exactly this. Anything per-asset in here is a cache miss
    /// on every call — see `shared/prompt-caching.md`'s silent-invalidator list, of which a timestamp is the
    /// commonest.
    pub instructions: String,
    /// The per-asset half.
    pub parts: Vec<Part>,
    /// A JSON schema the answer must satisfy, when the caller wants data rather than prose.
    ///
    /// §8.3: "on every extraction path, so results deserialize into typed structs instead of being parsed out
    /// of prose". Both providers support it; the request shapes differ and the clients hide that.
    pub schema: Option<serde_json::Value>,
    pub max_tokens: u32,
    /// How hard to think, where the provider understands the idea.
    ///
    /// `Low` for bulk classification, which is most of a library. Anthropic's Opus 5 thinks by default and this
    /// maps onto `output_config.effort`; a provider with no equivalent ignores it, which is why it is a hint
    /// rather than a parameter.
    pub effort: Effort,
}

/// How much reasoning to spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Effort {
    /// Bulk work: classification, tag candidates over a whole library.
    Low,
    #[default]
    Medium,
    /// Anything a person reads — §8.3's rule for alt text, which is an accessibility artefact rather than a
    /// nice-to-have.
    High,
}

impl Effort {
    /// The spelling Anthropic's `output_config.effort` uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// What a provider reported about the tokens a call used.
///
/// Reported, not derived. `cached_input` is the field that tells you prompt caching is actually working — §8.3
/// says to verify with it rather than assume, because a shared prefix that is one byte different every call
/// caches nothing and looks identical from the outside.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens served from the provider's cache.
    pub cached_input_tokens: u64,
    /// Input tokens written *into* the cache by this call, which are billed at a premium.
    pub cache_write_tokens: u64,
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Every text block, joined. Empty when the model only produced structured output.
    pub text: String,
    /// The parsed JSON, when [`Ask::schema`] was set and the answer parsed.
    pub structured: Option<serde_json::Value>,
    /// The model that actually answered, as the provider named it — which is not always the model asked for.
    pub model: String,
    pub usage: Usage,
}

/// A model that can be asked something.
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Asks, once. Retries and backoff belong to the queue, which already has an attempt counter.
    async fn ask(&self, ask: &Ask) -> Result<Completion, ModelError>;

    /// What to record in `enrichment_runs` and `ai_models`.
    fn model_name(&self) -> &str;
}

/// What came back.
///
/// `retry_after` is here rather than in a headers map because it is the *only* response header either client
/// reads, and it is the one that cannot be recovered from the body: a 429 carries its wait in `Retry-After`,
/// and a client that ignored it either hammers a throttled provider or invents a backoff the provider already
/// told it. Everything else worth knowing is in the JSON, so a full header map would be surface with no reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub body: serde_json::Value,
    /// Seconds the provider asked us to wait, from `Retry-After`, when it sent one and it was a delay in
    /// seconds rather than an HTTP date.
    pub retry_after: Option<u64>,
}

impl Answer {
    /// A plain status and body — the common case, and what a fixture writes.
    pub fn new(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body,
            retry_after: None,
        }
    }

    /// Whether the status is a success. Spelled once so the two clients cannot disagree about it.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// One HTTP exchange, with everything the clients need and nothing they do not.
///
/// Deliberately not `reqwest::Client`: the clients build a body and read a body, and a trait with those two
/// facts in it can be satisfied by a recording of a real exchange. Request headers are a map rather than typed,
/// because what the two providers need differs and neither needs anything exotic.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// POSTs JSON and returns the [`Answer`].
    ///
    /// A non-2xx status is *not* an error here: the providers put a readable reason in the body of a 400, and a
    /// transport that swallowed it would leave the clients guessing. Only a failure to reach the endpoint or to
    /// read the body at all is an `Err`.
    async fn post_json(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: serde_json::Value,
    ) -> Result<Answer, ModelError>;
}

/// Maps an HTTP status onto the error a caller can act on.
///
/// Shared by both clients so a 429 means the same thing whichever vendor sent it — the queue reads
/// `is_transient`, and two spellings of "rate limited" would make one vendor's throttling look permanent.
pub(crate) fn status_error(
    status: u16,
    body: &serde_json::Value,
    retry_after: Option<u64>,
) -> ModelError {
    // Both providers nest a human-readable reason; the paths differ.
    let message = body
        .pointer("/error/message")
        .and_then(|value| value.as_str())
        .unwrap_or("no reason given")
        .to_owned();
    match status {
        401 | 403 => ModelError::Unauthorised,
        429 => ModelError::RateLimited(retry_after),
        400..=499 => ModelError::Unreadable(format!(
            "the provider refused the request ({status}): {message}"
        )),
        _ => ModelError::Transient(format!("the provider failed ({status}): {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_and_a_bad_key_are_permanent_and_throttling_is_not() {
        // The queue reads this to decide between a retry and a dead letter. A refusal retried five times is
        // five refusals and a dead-lettered asset that was never going to work; a 429 dead-lettered on the
        // first try is an asset dropped because the provider was busy.
        assert!(!ModelError::Declined(None).is_transient());
        assert!(!ModelError::Unauthorised.is_transient());
        assert!(!ModelError::Unreadable("truncated".into()).is_transient());
        assert!(ModelError::RateLimited(Some(30)).is_transient());
        assert!(ModelError::Transient("connection reset".into()).is_transient());
    }

    #[test]
    fn a_status_maps_to_something_a_caller_can_act_on() {
        let body = serde_json::json!({"error": {"message": "invalid x-api-key"}});
        assert!(matches!(
            status_error(401, &body, None),
            ModelError::Unauthorised
        ));
        assert!(matches!(
            status_error(429, &body, Some(12)),
            ModelError::RateLimited(Some(12))
        ));
        // A 400 is the request being wrong, which retrying does not fix — and the provider's own words travel,
        // because "the provider refused" with no reason is a support ticket.
        let bad = status_error(400, &body, None);
        assert!(!bad.is_transient());
        assert!(format!("{bad}").contains("invalid x-api-key"), "{bad}");
        // A 5xx is worth another go.
        assert!(status_error(503, &body, None).is_transient());
    }

    #[test]
    fn a_declined_call_says_why_when_the_provider_said() {
        assert_eq!(
            format!("{}", ModelError::Declined(Some("policy".into()))),
            "the model declined this request: policy"
        );
        assert_eq!(
            format!("{}", ModelError::Declined(None)),
            "the model declined this request"
        );
    }
}
