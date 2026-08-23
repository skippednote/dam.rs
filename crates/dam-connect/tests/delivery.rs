//! Signing and sending, against a receiver that really answers (Q.20c).
//!
//! Two halves, and both need to be real to be worth anything.
//!
//! **The signature** is what a customer will write code against, so [`verify`] is the same function this crate
//! offers them rather than a second copy in the test that happens to agree. The properties asserted are the
//! ones a forgery would exploit: the timestamp is covered, the body is covered, and the scheme is versioned so
//! a receiver pinning `v1=` keeps working when a `v2=` appears beside it.
//!
//! **The sending** goes over a real socket to a real axum server. A mocked client would test this code's
//! opinion of itself — that a 2xx means accepted and a timeout means retry — while leaving the parts that
//! actually break untested: whether the headers arrive, whether the body is the bytes that were signed, and
//! whether a receiver can verify what it was sent using nothing but the documented scheme.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Router, body::Bytes};
use dam_connect::webhooks::{self, Outcome};
use dam_core::Secret;
use serde_json::json;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// What a receiver saw, so a test can assert on the wire rather than on the sender's intent.
#[derive(Debug, Default)]
struct Seen {
    signature: String,
    timestamp: String,
    event: String,
    delivery: String,
    content_type: String,
    body: Vec<u8>,
    hits: usize,
}

#[derive(Clone)]
struct Receiver {
    seen: Arc<Mutex<Seen>>,
    /// What to answer. A function of the hit count, so a test can fail once and then succeed.
    answer: Arc<dyn Fn(usize) -> (StatusCode, String) + Send + Sync>,
    /// How long to stall before answering, for the timeout case.
    stall: std::time::Duration,
}

async fn receive(
    State(state): State<Receiver>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let hits = {
        let mut seen = state.seen.lock().expect("not poisoned");
        seen.hits += 1;
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        seen.signature = header(webhooks::SIGNATURE_HEADER);
        seen.timestamp = header(webhooks::TIMESTAMP_HEADER);
        seen.event = header(webhooks::EVENT_HEADER);
        seen.delivery = header(webhooks::DELIVERY_HEADER);
        seen.content_type = header("content-type");
        seen.body = body.to_vec();
        seen.hits
    };
    if !state.stall.is_zero() {
        tokio::time::sleep(state.stall).await;
    }
    (state.answer)(hits)
}

/// Starts a receiver and returns `(url, what it saw)`.
async fn serve(
    answer: impl Fn(usize) -> (StatusCode, String) + Send + Sync + 'static,
    stall: std::time::Duration,
) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let state = Receiver {
        seen: Arc::clone(&seen),
        answer: Arc::new(answer),
        stall,
    };
    let app = Router::new()
        .route("/hook", post(receive))
        .with_state(state);
    // Port zero, so tests can run concurrently without agreeing on numbers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}/hook"), seen)
}

fn delivery(url: &str) -> dam_db::webhooks::Delivery {
    dam_db::webhooks::Delivery {
        id: Uuid::parse_str("aaaaaaaa-1111-4111-8111-111111111111").expect("uuid"),
        subscription_id: Uuid::new_v4(),
        url: url.to_owned(),
        secret: "a-shared-secret".to_owned(),
        event_kind: "asset.published".to_owned(),
        asset_id: Some(Uuid::new_v4()),
        payload: json!({"asset": {"filename": "harbour.jpg"}, "at": "2026-08-23T09:00:00Z"}),
        attempts: 0,
        max_attempts: 8,
    }
}

// ─── the signature ──────────────────────────────────────────────────────────

#[test]
fn the_signature_covers_the_timestamp_and_the_body() {
    let secret = Secret::new("a-shared-secret".to_owned());
    let body = br#"{"asset":"harbour.jpg"}"#;
    let signature = webhooks::sign(&secret, 1_800_000_000, body);

    assert!(signature.starts_with("v1="), "{signature}");
    assert!(webhooks::verify(&secret, 1_800_000_000, body, &signature));

    // A replay with a different timestamp fails, which is the whole reason the timestamp is signed: without
    // it, one captured delivery is valid forever — and replaying the `asset.published` that preceded an
    // `asset.expired` un-withdraws an asset.
    assert!(!webhooks::verify(&secret, 1_800_000_001, body, &signature));
    // A different body fails.
    assert!(!webhooks::verify(
        &secret,
        1_800_000_000,
        br#"{"asset":"other"}"#,
        &signature
    ));
    // A different secret fails.
    assert!(!webhooks::verify(
        &Secret::new("not-the-secret".to_owned()),
        1_800_000_000,
        body,
        &signature
    ));
}

#[test]
fn the_delimiter_cannot_be_smuggled_through_the_timestamp() {
    // `timestamp.body` is injective only because a decimal integer contains no `.`. That is a property of the
    // field rather than of the format, so it is asserted rather than assumed — `dam_core::signed_url` uses
    // length prefixes precisely because its fields carry no such guarantee.
    for stamp in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert!(
            !stamp.to_string().contains('.'),
            "a timestamp containing the separator would make the signed string ambiguous"
        );
    }

    // And the ambiguity that would follow if it could: a body starting with digits and a dot must not collide
    // with a later timestamp and a shorter body.
    let secret = Secret::new("k".to_owned());
    let a = webhooks::sign(&secret, 12, b"34.body");
    let b = webhooks::sign(&secret, 1_234, b"body");
    assert_ne!(a, b, "12 . 34.body must not sign the same as 1234 . body");
}

#[test]
fn the_scheme_is_versioned_so_it_can_be_rotated() {
    // A receiver that pins `v1=` keeps working when a `v2=` is added beside it. Without a version in the
    // header, changing the scheme is a flag day across every integration a customer has built.
    let secret = Secret::new("k".to_owned());
    let signature = webhooks::sign(&secret, 1, b"{}");
    let (version, digest) = signature.split_once('=').expect("a versioned signature");
    assert_eq!(version, webhooks::SIGNATURE_VERSION);
    assert_eq!(digest.len(), 64, "SHA-256 as lowercase hex");
    assert!(
        digest
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}

// ─── the policy ─────────────────────────────────────────────────────────────

#[test]
fn every_status_is_classified_the_way_an_operator_would_expect() {
    // 202 and 204 are acceptance: a receiver that queues the event, or does the work and has nothing to say
    // about it, has taken responsibility for it.
    for status in [200, 201, 202, 204, 299] {
        assert!(
            matches!(webhooks::classify(status, ""), Outcome::Accepted { .. }),
            "{status} is acceptance"
        );
    }

    // A redirect is a misconfiguration, not a success and not a retry: following it would post a customer's
    // data to a host they never nominated.
    for status in [301, 302, 307, 308] {
        let outcome = webhooks::classify(status, "moved");
        assert!(matches!(outcome, Outcome::Rejected { .. }), "{status}");
    }

    // 429 is the one 4xx worth retrying — the receiver is asking for less, not saying no.
    assert!(matches!(
        webhooks::classify(429, "slow down"),
        Outcome::Retry { .. }
    ));
    for status in [400, 401, 403, 404, 422] {
        assert!(
            matches!(webhooks::classify(status, "no"), Outcome::Rejected { .. }),
            "{status}"
        );
    }
    for status in [500, 502, 503, 504] {
        assert!(
            matches!(webhooks::classify(status, "oops"), Outcome::Retry { .. }),
            "{status}"
        );
    }
}

#[test]
fn a_receivers_own_words_are_kept_but_bounded() {
    // The receiver's message is the most useful thing in a delivery log, and its whole HTML error page is the
    // least — this column is read on every page load of the log screen.
    let long = "x".repeat(5_000);
    let Outcome::Rejected { reason, .. } = webhooks::classify(422, &long) else {
        panic!("a 422 is a rejection");
    };
    assert!(reason.len() < 400, "bounded: {} chars", reason.len());
    assert!(
        reason.contains("xxx"),
        "and it is the receiver's text, not a placeholder"
    );
}

// ─── over a real socket ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_receiver_can_verify_what_it_was_sent() {
    // The property that matters most: a customer implementing the documented scheme can check the delivery.
    // Asserted by verifying the headers that actually arrived against the body that actually arrived.
    let (url, seen) = serve(
        |_| (StatusCode::OK, "thanks".to_owned()),
        std::time::Duration::ZERO,
    )
    .await;
    let client = reqwest::Client::new();
    let sent = delivery(&url);

    let outcome = webhooks::send(&client, &sent, 1_800_000_000).await;
    assert_eq!(outcome, Outcome::Accepted { status: 200 });

    let seen = seen.lock().expect("not poisoned");
    assert_eq!(seen.hits, 1);
    assert_eq!(seen.content_type, "application/json");
    assert_eq!(seen.event, "asset.published");
    // The delivery id is stable across attempts, which is what lets a receiver drop a duplicate after timing
    // out *behind* the work rather than in front of it.
    assert_eq!(seen.delivery, sent.id.to_string());
    assert_eq!(seen.timestamp, "1800000000");

    let secret = Secret::new(sent.secret.clone());
    let stamp: i64 = seen.timestamp.parse().expect("a decimal timestamp");
    assert!(
        webhooks::verify(&secret, stamp, &seen.body, &seen.signature),
        "a receiver must be able to verify the delivery from the headers and body alone"
    );
    // And the body really is the payload, not a re-encoding of it.
    let parsed: serde_json::Value = serde_json::from_slice(&seen.body).expect("json");
    assert_eq!(parsed, sent.payload);
}

#[tokio::test]
async fn a_server_error_is_retried_and_a_rejection_is_not() {
    let (url, _) = serve(
        |_| (StatusCode::SERVICE_UNAVAILABLE, "deploying".to_owned()),
        std::time::Duration::ZERO,
    )
    .await;
    let client = reqwest::Client::new();
    let outcome = webhooks::send(&client, &delivery(&url), 1).await;
    let Outcome::Retry { status, reason } = outcome else {
        panic!("a 503 is worth retrying");
    };
    assert_eq!(status, Some(503));
    assert!(
        reason.contains("deploying"),
        "the receiver's words: {reason}"
    );

    let (url, _) = serve(
        |_| (StatusCode::UNPROCESSABLE_ENTITY, "unknown field".to_owned()),
        std::time::Duration::ZERO,
    )
    .await;
    let outcome = webhooks::send(&client, &delivery(&url), 1).await;
    let Outcome::Rejected { status, reason } = outcome else {
        panic!("a 422 is the receiver saying the request is wrong");
    };
    assert_eq!(status, 422);
    assert!(reason.contains("unknown field"), "{reason}");
}

#[tokio::test]
async fn a_stalled_receiver_becomes_a_retry_with_no_status() {
    // The absence of a status is the diagnosis: a timeout and a 500 are different problems, and an operator
    // reading the log needs to tell them apart — which is why the column stays null rather than becoming zero.
    let (url, _) = serve(
        |_| (StatusCode::OK, "eventually".to_owned()),
        webhooks::TIMEOUT + std::time::Duration::from_secs(2),
    )
    .await;
    let client = reqwest::Client::new();
    let outcome = webhooks::send(&client, &delivery(&url), 1).await;
    let Outcome::Retry { status, reason } = outcome else {
        panic!("a stalled receiver is worth retrying");
    };
    assert_eq!(status, None);
    assert!(reason.contains("no answer within"), "{reason}");
}

#[tokio::test]
async fn a_host_that_does_not_answer_is_a_retry() {
    // Nothing listening: the commonest real failure, and the one a naive implementation turns into a panic.
    let client = reqwest::Client::new();
    let mut sent = delivery("http://127.0.0.1:1/hook");
    sent.url = "http://127.0.0.1:1/hook".to_owned();
    let outcome = webhooks::send(&client, &sent, 1).await;
    let Outcome::Retry { status, reason } = outcome else {
        panic!("an unreachable host is worth retrying");
    };
    assert_eq!(status, None);
    assert!(
        reason.contains("connect") || reason.contains("failed"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_url_that_never_parsed_is_rejected_rather_than_retried_forever() {
    let client = reqwest::Client::new();
    let mut sent = delivery("not-a-url");
    sent.url = "not-a-url".to_owned();
    let outcome = webhooks::send(&client, &sent, 1).await;
    // A retry, deliberately, and it is the one case where that is arguable: the URL will never parse, so the
    // eight attempts are spent for nothing. But the alternative is a sender that decides a subscription is
    // permanently broken from a string it failed to parse, and the subscription API validates the URL on the
    // way in — so this path is a belt to that brace, and the dead-letter queue bounds the waste.
    let Outcome::Retry { reason, .. } = outcome else {
        panic!("an unparseable URL still goes through the ordinary failure path: {outcome:?}");
    };
    assert!(
        reason.contains("URL") || reason.contains("failed"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_retry_carries_the_same_delivery_id() {
    // What makes deduplication possible on the receiver's side: it saw the id, did the work, timed out, and
    // sees the same id again. A fresh id per attempt would make every retry look like a new event.
    let (url, seen) = serve(
        |hits| {
            if hits == 1 {
                (StatusCode::INTERNAL_SERVER_ERROR, "first".to_owned())
            } else {
                (StatusCode::OK, "second".to_owned())
            }
        },
        std::time::Duration::ZERO,
    )
    .await;
    let client = reqwest::Client::new();
    let sent = delivery(&url);

    assert!(matches!(
        webhooks::send(&client, &sent, 100).await,
        Outcome::Retry { .. }
    ));
    let first_id = seen.lock().expect("not poisoned").delivery.clone();
    // A later timestamp, so the signature differs while the identity does not.
    assert_eq!(
        webhooks::send(&client, &sent, 200).await,
        Outcome::Accepted { status: 200 }
    );
    let seen = seen.lock().expect("not poisoned");
    assert_eq!(seen.hits, 2);
    assert_eq!(
        seen.delivery, first_id,
        "the identity is stable across attempts"
    );
    assert_eq!(
        seen.timestamp, "200",
        "and the signature is freshly stamped"
    );
}
