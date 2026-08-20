//! A [`Transport`] that answers from a script and remembers what it was asked (M5a·3).
//!
//! ## Why not hit the API
//!
//! Because there is no key in this repository, and there should not be. Two things follow. The first is
//! obvious: a suite that reached the network would be skipped everywhere, which is a suite that does not exist.
//! The second is the reason this module is worth the lines — **the request is the part that can be wrong**. A
//! live call would prove Anthropic answers; it would not prove the `cache_control` breakpoint sits at the end
//! of the stable prefix, or that `budget_tokens` is absent, or that the image block is the shape Anthropic
//! reads rather than the shape OpenAI reads. Those are the bugs available here, and every one of them is
//! visible in the JSON before it is sent.
//!
//! So [`Recorded`] keeps every request, and the suites assert on them. Read
//! `crates/dam-ai/tests/hosted_models.rs` for what that looks like.
//!
//! ## The honest limit
//!
//! Fixtures are transcribed from the vendors' own documented examples, not captured from a live call. If a
//! vendor changes a field, this suite keeps passing and production breaks — no recorded transport can protect
//! against that. The mitigation is a smoke test against a real key, and there is a task open for it (M5a·4).
//! Until that has been run, nothing here should be read as "the integration works".

use crate::model::{Answer, ModelError, Transport};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One request, as the client sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

impl Sent {
    /// A header, by lowercase name. Sugar, because every assertion wants one.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// What the transport should answer with.
#[derive(Debug, Clone)]
pub enum Reply {
    /// A status and a body — including the unhappy statuses, which is how the error mapping gets tested.
    Http(u16, Value),
    /// A throttle, with the wait the provider asked for. Its own variant because `Retry-After` is a header and
    /// a fixture body cannot express it.
    Throttled(u64, Value),
    /// The request never arrived. Becomes [`ModelError::Transient`], because a connection that failed is a
    /// connection worth trying again.
    Broken(String),
}

/// A scripted transport that records what it was asked.
///
/// Replies are consumed in order; when the script runs out, the last reply repeats. Repeating rather than
/// panicking is deliberate — a retry test wants "and then it keeps succeeding" without counting calls.
#[derive(Debug)]
pub struct Recorded {
    replies: Mutex<std::collections::VecDeque<Reply>>,
    last: Mutex<Option<Reply>>,
    sent: Mutex<Vec<Sent>>,
}

impl Recorded {
    /// A transport that answers the same thing every time.
    pub fn always(status: u16, body: Value) -> Self {
        Self::script(vec![Reply::Http(status, body)])
    }

    /// A transport that works through a script.
    pub fn script(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            last: Mutex::new(None),
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Every request so far, oldest first.
    pub fn sent(&self) -> Vec<Sent> {
        self.sent
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The one request, when a test made exactly one.
    ///
    /// Panics if there was not exactly one, which is the assertion most of these tests want to make anyway: a
    /// client that retried silently, or sent two calls for one ask, is a bug and this is where it surfaces.
    pub fn only(&self) -> Sent {
        let sent = self.sent();
        assert_eq!(
            sent.len(),
            1,
            "expected exactly one request, got {}",
            sent.len()
        );
        sent.into_iter().next().unwrap_or_else(|| unreachable!())
    }
}

#[async_trait::async_trait]
impl Transport for Recorded {
    async fn post_json(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: Value,
    ) -> Result<Answer, ModelError> {
        self.sent
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Sent {
                url: url.to_owned(),
                headers: headers.clone(),
                body,
            });

        let next = self
            .replies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front();
        let mut last = self.last.lock().unwrap_or_else(|error| error.into_inner());
        let reply = match next {
            Some(reply) => {
                *last = Some(reply.clone());
                reply
            }
            None => last
                .clone()
                .unwrap_or_else(|| Reply::Broken("the script was empty".to_owned())),
        };
        match reply {
            Reply::Http(status, body) => Ok(Answer::new(status, body)),
            Reply::Throttled(seconds, body) => Ok(Answer {
                status: 429,
                body,
                retry_after: Some(seconds),
            }),
            Reply::Broken(why) => Err(ModelError::Transient(why)),
        }
    }
}

/// A plausible Messages API answer.
///
/// The token counts are arguments rather than constants so a caching test can assert that a read shows up where
/// it should. `input`/`output`/`cache_read`/`cache_write`, in that order.
pub fn anthropic_answer(text: &str, model: &str, tokens: (u64, u64, u64, u64)) -> Value {
    json!({
        "id": "msg_01XFDUDYJgAACzvnptvVoYEL",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": tokens.0,
            "output_tokens": tokens.1,
            "cache_read_input_tokens": tokens.2,
            "cache_creation_input_tokens": tokens.3,
        },
    })
}

/// A Messages API refusal — which arrives as a 200, and is the case a client most often gets wrong.
pub fn anthropic_refusal(explanation: &str) -> Value {
    json!({
        "id": "msg_01XFDUDYJgAACzvnptvVoYEM",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5",
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"type": "refusal", "category": "policy", "explanation": explanation},
        "usage": {"input_tokens": 42, "output_tokens": 0},
    })
}

/// A plausible `/chat/completions` answer. `prompt`/`completion`/`cached`, where `prompt` *includes* `cached` —
/// as the format defines it, and as the client has to undo.
pub fn openai_answer(text: &str, model: &str, tokens: (u64, u64, u64)) -> Value {
    json!({
        "id": "chatcmpl-123",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": tokens.0,
            "completion_tokens": tokens.1,
            "total_tokens": tokens.0 + tokens.1,
            "prompt_tokens_details": {"cached_tokens": tokens.2},
        },
    })
}
