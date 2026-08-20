//! The [`Transport`] that actually reaches a provider (M5a·3).
//!
//! Small on purpose. Everything interesting about talking to a model lives in [`crate::anthropic`] and
//! [`crate::openai_compatible`]; this is the part that cannot be tested without a network, so it is kept down
//! to the three decisions that have to be made somewhere.
//!
//! **A non-2xx is a normal return, not an error.** Both providers put the reason in the body of a 400, and both
//! clients read it. A transport that turned status into `Err` would throw that away and leave a caller with
//! "400" and no idea which field was wrong.
//!
//! **`Retry-After` is lifted out of the headers.** It is the one response header either client reads, and a 429
//! whose wait was dropped becomes either a hammered provider or a made-up backoff.
//!
//! **A timeout, always.** An enrichment worker holds a job lease while it waits; a request with no deadline is
//! a job that never finishes and a lease that never expires. Long by HTTP standards because a large image and a
//! thinking model is genuinely slow, but finite.

use crate::model::{Answer, ModelError, Transport};
use std::collections::BTreeMap;
use std::time::Duration;

/// How long to wait for a whole exchange.
///
/// Generous: a multi-megapixel image plus adaptive thinking can take minutes, and a timeout that fires on a
/// working call costs the tokens *and* the answer. The queue's own attempt budget is the outer bound.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// A transport over `reqwest`.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    /// Builds a client with the timeout above.
    ///
    /// Fails only if the TLS backend cannot be initialised, which is a deployment fault rather than a runtime
    /// one — hence a `Result` here and none on the calls.
    pub fn new() -> Result<Self, ModelError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ModelError::Transient(format!("no http client: {error}")))?;
        Ok(Self { client })
    }

    /// Wraps a client the caller already has, so a deployment can share a connection pool or install a proxy.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn post_json(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: serde_json::Value,
    ) -> Result<Answer, ModelError> {
        let mut request = self.client.post(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.json(&body).send().await.map_err(|error| {
            // `error` here can name the URL but never a header, so the key cannot travel into a log through it.
            ModelError::Transient(format!("the provider was unreachable: {error}"))
        })?;

        let status = response.status().as_u16();
        let retry_after = retry_after_seconds(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
        );

        // Read as text first: an overloaded gateway answers 502 with HTML, and `json()` on that would report a
        // parse failure where the real fact is "the provider is down".
        let text = response.text().await.map_err(|error| {
            ModelError::Transient(format!("the answer could not be read: {error}"))
        })?;
        let body = parse_body(status, &text)?;
        Ok(Answer {
            status,
            body,
            retry_after,
        })
    }
}

/// Reads `Retry-After`, seconds only.
///
/// The header also allows an HTTP date, and turning one into a duration needs a clock and a timezone to get
/// wrong. `None` means the queue falls back on its own backoff — a worse wait than the provider's, and never an
/// incorrect one.
fn retry_after_seconds(header: Option<&str>) -> Option<u64> {
    header?.trim().parse::<u64>().ok()
}

/// Turns a response body into JSON, or into an error that says what actually happened.
///
/// The interesting case is a non-2xx that is not JSON at all: an overloaded gateway answers 502 with an HTML
/// error page, and reporting that as a parse failure would bury the one useful fact. Folding the text into the
/// shape both clients already read means the gateway's own words reach the log instead.
fn parse_body(status: u16, text: &str) -> Result<serde_json::Value, ModelError> {
    match serde_json::from_str(text) {
        Ok(body) => Ok(body),
        Err(_) if !(200..300).contains(&status) => {
            Ok(serde_json::json!({"error": {"message": text}}))
        }
        Err(error) => Err(ModelError::Unreadable(format!(
            "the provider answered {status} with something that is not json: {error}"
        ))),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_hint_is_read_when_it_is_a_number_and_ignored_when_it_is_a_date() {
        assert_eq!(retry_after_seconds(Some("30")), Some(30));
        assert_eq!(retry_after_seconds(Some(" 30 ")), Some(30));
        assert_eq!(retry_after_seconds(None), None);
        // A date, which this deliberately does not try to interpret.
        assert_eq!(
            retry_after_seconds(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        // Negative and fractional values are not seconds either; a wait parsed wrong is worse than no wait.
        assert_eq!(retry_after_seconds(Some("-5")), None);
        assert_eq!(retry_after_seconds(Some("1.5")), None);
    }

    #[test]
    fn an_html_error_page_reaches_the_caller_as_words_rather_than_a_parse_failure() {
        let body = parse_body(502, "<html>Bad Gateway</html>").expect("folded, not failed");
        assert_eq!(body["error"]["message"], "<html>Bad Gateway</html>");
        // A 200 that is not JSON is a different fact: the provider claimed success and sent something this
        // build cannot read, which is `Unreadable` rather than a retry.
        let error = parse_body(200, "not json").expect_err("a 200 has to be json");
        assert!(matches!(error, ModelError::Unreadable(_)), "{error:?}");
        // And a real body survives untouched.
        assert_eq!(
            parse_body(200, r#"{"ok":true}"#).expect("json"),
            serde_json::json!({"ok": true})
        );
    }
}
