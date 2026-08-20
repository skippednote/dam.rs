//! The Batch API: half price, and the only affordable way to describe a library that already exists (M5c).
//!
//! §8.3 is unambiguous about this — "all library backfill runs here, never synchronously" — and its cost table
//! is why: the same million assets are ~$23k synchronously and ~$6–8k batched with a cached prefix. A backfill
//! is not a faster version of the per-asset path; it is the only version anybody can pay for.
//!
//! ## Three calls, and the order matters more than the shapes
//!
//! 1. `POST /v1/messages/batches` with up to 100,000 requests, each carrying a `custom_id` of the caller's
//!    choosing and the same `params` a synchronous call would take.
//! 2. `GET /v1/messages/batches/{id}` until `processing_status` is `ended`. Nothing is available before then —
//!    not even the finished ones.
//! 3. `GET /v1/messages/batches/{id}/results`, which is **JSONL**: one object per line, keyed by `custom_id`,
//!    in no particular order.
//!
//! The `custom_id` is the whole design. Results come back unordered and possibly incomplete, so the id has to be
//! something the caller can resolve on its own afterwards — which is why `enrichment_runs.llm_custom_id` is
//! persisted *before* the batch is submitted. A caller that kept the mapping in memory would lose it to any
//! restart and have a paid batch it could not read.
//!
//! ## A batch has four ways to finish, and only one is a success
//!
//! Per request: `succeeded`, `errored`, `canceled`, `expired` (a batch not finished in 24 hours). They are kept
//! apart here because they mean different things to a caller: an error may be worth retrying, an expiry means
//! the request never ran and was never billed, and a cancellation was somebody's decision.
//!
//! ## Anthropic only, deliberately
//!
//! The OpenAI-compatible family has a batch API too, and it works differently enough — upload a file, reference
//! it, poll a job, download an output file — that supporting it here would be a second implementation wearing
//! the same name. §8.3's economics are Anthropic's, so this is the one that pays for itself; an
//! OpenAI-compatible credential falls back to the synchronous path, which is correct and dearer, and says so.

use crate::model::{Ask, Completion, ModelError, Transport, Usage, status_error};
use dam_core::Secret;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The version header, pinned like the Messages API's.
const API_VERSION: &str = "2023-06-01";

const DEFAULT_BASE: &str = "https://api.anthropic.com";

/// The most requests one batch may carry. The provider's own limit.
pub const MAX_REQUESTS: usize = 100_000;

/// One asset's question, with the id its answer will come back under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchItem {
    /// Persisted before submission — see the module note. Anything the caller can resolve later.
    pub custom_id: String,
    pub ask: Ask,
}

/// Where a submitted batch has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    /// Still working. Nothing is readable yet, not even the finished requests.
    InProgress,
    /// Being cancelled.
    Cancelling,
    /// Done. Results are readable, whatever each individual request did.
    Ended,
}

/// What a poll found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchStatus {
    pub id: String,
    pub state: BatchState,
    pub succeeded: u64,
    pub errored: u64,
    pub canceled: u64,
    pub expired: u64,
    pub processing: u64,
}

impl BatchStatus {
    /// Whether the results can be fetched. Nothing is readable before this is true — not even the requests that
    /// have already finished.
    pub fn is_ended(&self) -> bool {
        matches!(self.state, BatchState::Ended)
    }

    /// How many requests reached a terminal state, however they got there.
    pub fn finished(&self) -> u64 {
        self.succeeded + self.errored + self.canceled + self.expired
    }
}

/// One request's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum BatchOutcome {
    /// The model answered.
    Answered(Completion),
    /// The model refused. Not an error: see [`ModelError::Declined`].
    Declined(Option<String>),
    /// The request failed. Carries the provider's own words.
    Errored(String),
    /// The batch did not finish inside the provider's window. Never billed, and worth resubmitting.
    Expired,
    /// Somebody cancelled the batch.
    Canceled,
}

/// One line of the results file.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchResult {
    pub custom_id: String,
    pub outcome: BatchOutcome,
}

/// A client for one tenant's credential, speaking the Batch API.
pub struct AnthropicBatch {
    transport: Arc<dyn Transport>,
    key: Secret<String>,
    base_url: String,
    model: String,
}

impl std::fmt::Debug for AnthropicBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicBatch")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

/// The batch endpoints, as this build spells them. Named once so a test cannot disagree with the client.
fn batches_url(base: &str) -> String {
    format!("{base}/v1/messages/batches")
}

impl AnthropicBatch {
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

    fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        headers.insert("anthropic-version".to_owned(), API_VERSION.to_owned());
        headers.insert("x-api-key".to_owned(), self.key.expose().clone());
        headers
    }

    /// The submission body.
    ///
    /// Public for the same reason the synchronous client's is: every feature the cost model assumes is a shape in
    /// here, and a test that only read the reply would pass while the batch cost full price.
    pub fn body(&self, items: &[BatchItem]) -> serde_json::Value {
        let requests: Vec<serde_json::Value> = items
            .iter()
            .map(|item| {
                // The same params a synchronous call takes, from the same builder — see `message_params`.
                let params = crate::anthropic::message_params(&self.model, &item.ask);
                serde_json::json!({"custom_id": item.custom_id, "params": params})
            })
            .collect();
        serde_json::json!({"requests": requests})
    }

    /// Submits a batch, returning the provider's id for it.
    pub async fn submit(&self, items: &[BatchItem]) -> Result<String, ModelError> {
        if items.is_empty() {
            return Err(ModelError::Unreadable(
                "a batch with no requests is not a batch".to_owned(),
            ));
        }
        if items.len() > MAX_REQUESTS {
            return Err(ModelError::Unreadable(format!(
                "a batch takes at most {MAX_REQUESTS} requests; this one has {}",
                items.len()
            )));
        }
        let answer = self
            .transport
            .post_json(
                &batches_url(&self.base_url),
                &self.headers(),
                self.body(items),
            )
            .await?;
        if !answer.is_success() {
            return Err(status_error(
                answer.status,
                &answer.body,
                answer.retry_after,
            ));
        }
        answer
            .body
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .ok_or_else(|| ModelError::Unreadable("the batch was created without an id".to_owned()))
    }

    /// Asks how a batch is getting on.
    pub async fn poll(&self, batch_id: &str) -> Result<BatchStatus, ModelError> {
        let url = format!("{}/{batch_id}", batches_url(&self.base_url));
        let (status, text) = self.transport.get_text(&url, &self.headers()).await?;
        let body: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
            ModelError::Transient(format!("the batch status was not json ({status}): {error}"))
        })?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, &body, None));
        }

        let counts = |name: &str| {
            body.pointer(&format!("/request_counts/{name}"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        let state = match body.get("processing_status").and_then(|v| v.as_str()) {
            Some("ended") => BatchState::Ended,
            Some("canceling" | "cancelling") => BatchState::Cancelling,
            // Anything unrecognised is treated as still working, because the alternative — reading results that
            // are not there — turns a provider adding a status into a batch reported as empty.
            _ => BatchState::InProgress,
        };
        Ok(BatchStatus {
            id: body
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or(batch_id)
                .to_owned(),
            state,
            succeeded: counts("succeeded"),
            errored: counts("errored"),
            canceled: counts("canceled"),
            expired: counts("expired"),
            processing: counts("processing"),
        })
    }

    /// Fetches and parses the results.
    ///
    /// JSONL: a blank line is skipped and an unparseable one is an error rather than a silently missing asset —
    /// a backfill that quietly dropped a line would leave assets undescribed with nothing to say why.
    pub async fn results(&self, batch_id: &str) -> Result<Vec<BatchResult>, ModelError> {
        let url = format!("{}/{batch_id}/results", batches_url(&self.base_url));
        let (status, text) = self.transport.get_text(&url, &self.headers()).await?;
        if !(200..300).contains(&status) {
            let body = serde_json::from_str(&text)
                .unwrap_or_else(|_| serde_json::json!({"error": {"message": text}}));
            return Err(status_error(status, &body, None));
        }

        let mut results = Vec::new();
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line).map_err(|error| {
                ModelError::Unreadable(format!(
                    "line {} of the results is not json: {error}",
                    line_number + 1
                ))
            })?;
            results.push(read_line(&value)?);
        }
        Ok(results)
    }
}

/// One result line, as a caller can act on it.
fn read_line(value: &serde_json::Value) -> Result<BatchResult, ModelError> {
    let custom_id = value
        .get("custom_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            ModelError::Unreadable(
                "a result line carries no custom_id, so nothing can claim it".to_owned(),
            )
        })?
        .to_owned();

    let result = value.get("result");
    let kind = result
        .and_then(|result| result.get("type"))
        .and_then(|value| value.as_str())
        .unwrap_or("errored");

    let outcome = match kind {
        "succeeded" => {
            let message = result
                .and_then(|result| result.get("message"))
                .ok_or_else(|| {
                    ModelError::Unreadable(format!("{custom_id} succeeded with no message"))
                })?;
            // The same refusal rule as the synchronous path: `stop_reason` before content, because a refusal is
            // a *successful* request whose answer is no.
            if message.get("stop_reason").and_then(|v| v.as_str()) == Some("refusal") {
                BatchOutcome::Declined(
                    message
                        .pointer("/stop_details/explanation")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                )
            } else {
                BatchOutcome::Answered(completion(message))
            }
        }
        "errored" => BatchOutcome::Errored(
            result
                .and_then(|result| result.pointer("/error/error/message"))
                .or_else(|| result.and_then(|result| result.pointer("/error/message")))
                .and_then(|value| value.as_str())
                .unwrap_or("no reason given")
                .to_owned(),
        ),
        "expired" => BatchOutcome::Expired,
        "canceled" | "cancelled" => BatchOutcome::Canceled,
        other => {
            return Err(ModelError::Unreadable(format!(
                "{custom_id} came back with a result type this build does not know: {other}"
            )));
        }
    };
    Ok(BatchResult { custom_id, outcome })
}

/// A completion from a batch's message object. The same shape the synchronous path returns, so one write path
/// serves both.
fn completion(message: &serde_json::Value) -> Completion {
    let text = message
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<&str>>()
                .join("")
        })
        .unwrap_or_default();
    let structured = serde_json::from_str(text.trim()).ok();
    let count = |name: &str| {
        message
            .pointer(&format!("/usage/{name}"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    Completion {
        text,
        structured,
        model: message
            .get("model")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_owned(),
        usage: Usage {
            input_tokens: count("input_tokens"),
            output_tokens: count("output_tokens"),
            cached_input_tokens: count("cache_read_input_tokens"),
            cache_write_tokens: count("cache_creation_input_tokens"),
        },
    }
}
