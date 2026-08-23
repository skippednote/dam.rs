//! Signing and sending an outbox delivery (Q.20c, §11).
//!
//! `dam_db::webhooks` decides *what* to send and in what order. This module signs it, sends it, and classifies
//! the answer. The split is what lets the ordering rules be tested against a real database with no server, and
//! the signature be tested with neither.
//!
//! ## The signature covers the timestamp, or it is a replay token
//!
//! A signature over the body alone is valid forever, so anybody who ever saw one delivery can send it again —
//! and for a CMS consuming `asset.expired` that means replaying a withdrawal, or worse, replaying the
//! `asset.published` that preceded it. So the signed string is `timestamp.body`, the timestamp travels in its
//! own header, and a receiver is expected to reject one that is too old. Stripe's scheme, and for the same
//! reason.
//!
//! ## Delimiters, and why this one is safe
//!
//! `signed = "{timestamp}.{body}"` is only injective because the timestamp is decimal digits and therefore
//! cannot contain the `.` separator. That is a property of the field, not of the format, so it is asserted in
//! the tests rather than assumed — `dam_core::signed_url` uses length prefixes precisely because its fields
//! have no such guarantee.
//!
//! ## What counts as delivered
//!
//! Any 2xx. Not "200 only": a receiver that queues the event and answers 202 has accepted it, and one that
//! answers 204 has done the same with no body to show for it. A 3xx is *not* success — a redirect from a
//! webhook endpoint is a misconfiguration, and following it would post a customer's data somewhere they did
//! not nominate.
//!
//! ## Which failures are worth retrying
//!
//! Anything that might be temporary: a timeout, a connection refused, a 5xx, a 429. A 4xx other than 429 is
//! the receiver saying the request is wrong, and eight retries will not make it right — but it still costs the
//! attempts, because the alternative is a delivery abandoned on the first deploy that returns a stray 404
//! from a load balancer. Retrying a permanent failure wastes a little; abandoning a temporary one loses an
//! event, and the schema's dead-letter queue exists so the waste is bounded.

use dam_core::Secret;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::Duration;

/// The header carrying the signature.
pub const SIGNATURE_HEADER: &str = "X-Damrs-Signature";
/// The header carrying the signed timestamp, in seconds since the epoch.
pub const TIMESTAMP_HEADER: &str = "X-Damrs-Timestamp";
/// The header naming the event, so a receiver can route without parsing the body.
pub const EVENT_HEADER: &str = "X-Damrs-Event";
/// The header carrying the delivery id, so a receiver can deduplicate a retry.
///
/// The id is stable across attempts, which is what makes it useful: a receiver that timed out *after* doing
/// the work sees the same id on the retry and can drop it. Without this every retry looks like a new event.
pub const DELIVERY_HEADER: &str = "X-Damrs-Delivery";

/// The signature scheme, in the signature itself.
///
/// A receiver that pins `v1=` keeps working when a `v2=` is added beside it, which is what makes rotating the
/// scheme possible at all. Without a version, changing it is a flag day across every integration.
pub const SIGNATURE_VERSION: &str = "v1";

/// How long to wait for a receiver.
///
/// Ten seconds, and it is a deliberate compromise. A CMS that writes to its own database on receipt needs more
/// than a second or two; a dispatcher blocked for a minute per delivery cannot keep up with a bulk operation,
/// and because ordering is per asset, one slow endpoint would stall that asset's whole stream.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// What happened to one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The receiver accepted it. Any 2xx.
    Accepted { status: i32 },
    /// It failed, and it may work later.
    Retry { status: Option<i32>, reason: String },
    /// The receiver rejected it in a way that will not change, but the attempt is still spent — see the module
    /// docs on why a permanent-looking failure is not abandoned immediately.
    Rejected { status: i32, reason: String },
}

/// Signs `body` for `at`, returning the header value.
///
/// The `v1=` prefix and lowercase hex, so the whole header is one token a receiver can compare in constant
/// time after splitting on `=`.
#[must_use]
pub fn sign(secret: &Secret<String>, timestamp: i64, body: &[u8]) -> String {
    let mut mac = match <Hmac<Sha256>>::new_from_slice(secret.expose().as_bytes()) {
        Ok(mac) => mac,
        // HMAC accepts a key of any length, so this is unreachable. Handled rather than unwrapped because a
        // panic reachable from a queue worker takes the worker down, and "this cannot happen" is the reasoning
        // that puts one there.
        Err(_) => return String::new(),
    };
    // `timestamp.body`, injective because a decimal integer contains no `.` — asserted in the tests, since it
    // is a property of the field rather than of the format.
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut out = String::with_capacity(SIGNATURE_VERSION.len() + 1 + digest.len() * 2);
    out.push_str(SIGNATURE_VERSION);
    out.push('=');
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Verifies a signature the way a receiver would.
///
/// Here so the scheme has one implementation and the tests can check the thing a customer will write, rather
/// than a second copy of it that happens to agree. Constant-time, because a byte-by-byte comparison that
/// returns early leaks the prefix of a valid signature — and with an oracle that is a forgery.
#[must_use]
pub fn verify(secret: &Secret<String>, timestamp: i64, body: &[u8], presented: &str) -> bool {
    use subtle::ConstantTimeEq;
    let expected = sign(secret, timestamp, body);
    if expected.is_empty() {
        return false;
    }
    expected.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// Classifies a status code.
///
/// Separate from the sending so the policy is testable without a server, and readable without following an
/// HTTP client's control flow.
#[must_use]
pub fn classify(status: u16, body: &str) -> Outcome {
    let code = i32::from(status);
    // A truncated snippet of the body, because a receiver's error message is the most useful thing in the
    // delivery log and its whole HTML error page is the least.
    let snippet: String = body.chars().take(200).collect();
    match status {
        200..=299 => Outcome::Accepted { status: code },
        // Not success. A redirect from a webhook endpoint is a misconfiguration, and following it would post a
        // customer's data to a host they did not nominate.
        300..=399 => Outcome::Rejected {
            status: code,
            reason: format!("redirected, which a webhook endpoint must not do: {snippet}"),
        },
        429 => Outcome::Retry {
            status: Some(code),
            reason: format!("rate limited: {snippet}"),
        },
        400..=499 => Outcome::Rejected {
            status: code,
            reason: format!("rejected: {snippet}"),
        },
        _ => Outcome::Retry {
            status: Some(code),
            reason: format!("server error: {snippet}"),
        },
    }
}

/// Sends one delivery.
///
/// The client is passed in rather than built here, so connections are pooled across deliveries: a dispatcher
/// building one per event pays a TLS handshake per event, which for a busy tenant is most of the cost.
pub async fn send(
    client: &reqwest::Client,
    delivery: &dam_db::webhooks::Delivery,
    now_epoch_seconds: i64,
) -> Outcome {
    let body = match serde_json::to_vec(&delivery.payload) {
        Ok(body) => body,
        // The payload came out of a jsonb column, so it is valid JSON by construction. Rejected rather than
        // retried: re-serialising it will fail identically.
        Err(error) => {
            return Outcome::Rejected {
                status: 0,
                reason: format!("the stored payload could not be serialised: {error}"),
            };
        }
    };
    let secret = Secret::new(delivery.secret.clone());
    let signature = sign(&secret, now_epoch_seconds, &body);

    let response = client
        .post(&delivery.url)
        .header(SIGNATURE_HEADER, signature)
        .header(TIMESTAMP_HEADER, now_epoch_seconds.to_string())
        .header(EVENT_HEADER, &delivery.event_kind)
        .header(DELIVERY_HEADER, delivery.id.to_string())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(TIMEOUT)
        .body(body)
        .send()
        .await;

    match response {
        Ok(response) => {
            let status = response.status().as_u16();
            // Read before classifying, because the body is the receiver's own explanation and it is the most
            // useful thing in the log. Bounded by the client's own limits.
            let text = response.text().await.unwrap_or_default();
            classify(status, &text)
        }
        // Everything reqwest reports here is transport: a timeout, a refused connection, a DNS failure, a TLS
        // error. All of them are things that were true a moment ago and may not be true in ten minutes.
        Err(error) => Outcome::Retry {
            status: None,
            reason: transport_reason(&error),
        },
    }
}

/// A short description of a transport failure, naming the kind rather than dumping the chain.
///
/// The kind is what an operator acts on — a timeout means a slow endpoint, a connect error means the wrong
/// host or a firewall, and a builder error means the URL never parsed. The full error chain includes the URL
/// on every line, which makes a log column unreadable for no extra information.
fn transport_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        format!("no answer within {}s", TIMEOUT.as_secs())
    } else if error.is_connect() {
        "could not connect".to_owned()
    } else if error.is_request() {
        "the request could not be made; check the URL".to_owned()
    } else if error.is_body() || error.is_decode() {
        "the response could not be read".to_owned()
    } else {
        "the request failed".to_owned()
    }
}
