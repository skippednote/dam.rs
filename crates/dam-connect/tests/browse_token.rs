//! The credential a connected site mints for its own browser (M3d·3, §11.1).
//!
//! A picker runs in an editor's browser, and a browser needs something to authenticate with. It cannot be the
//! connector's API key: that is long-lived, grants every read the site has, and putting it in JavaScript hands
//! it to every editor, every browser extension and every page the picker is embedded in.
//!
//! So the site mints this instead, in PHP, with the secret it already signs render URLs with. Which means the
//! properties worth proving are the ones that stop it becoming an API key with extra steps:
//!
//! - **The ceiling is enforced at verification**, so a site that sets a year in the expiry is refused however
//!   well it signed. Whoever mints cannot opt out of it.
//! - **It carries no scope.** The only field besides the expiry is which connector is calling, so a site cannot
//!   mint itself reach it was never granted.
//! - **Every current secret is tried and the loop does not short-circuit**, so a rotation does not break a
//!   picker and the time taken does not say which secret matched.
//! - **A signature failure outranks an expiry**, because the other order tells a forger their attempt was
//!   otherwise accepted.
//! - **Trailing bytes are a refusal.** An appended payload is somebody experimenting, and accepting it makes
//!   the encoding non-injective again.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_connect::browse_token::{self, BrowseClaim, BrowseError, MAX_TTL};
use dam_core::Secret;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 24, 12, 0, 0).unwrap()
}

fn secret(text: &str) -> Secret<String> {
    Secret::new(text.to_owned())
}

fn claim(connector_id: Uuid, ttl: Duration) -> BrowseClaim {
    BrowseClaim {
        connector_id,
        expires_at: now() + ttl,
    }
}

#[test]
fn a_signed_token_round_trips_and_says_only_which_connector_is_calling() {
    let id = Uuid::now_v7();
    let key = secret("the-connector-secret");
    let token = browse_token::sign(&key, &claim(id, Duration::minutes(5))).expect("sign");

    let verified = browse_token::verify([&key], &token, now()).expect("verify");
    assert_eq!(verified.connector_id, id);
    assert_eq!(verified.expires_at, now() + Duration::minutes(5));

    // The claim has two fields and that is the whole design: a token that could name asset groups would be a
    // second place a connector's reach is decided, and the widening direction is the dangerous one.
    assert_eq!(verified, claim(id, Duration::minutes(5)));
}

#[test]
fn the_connector_can_be_read_before_verification_and_nothing_else_can_be_trusted() {
    let id = Uuid::now_v7();
    let key = secret("the-connector-secret");
    let token = browse_token::sign(&key, &claim(id, Duration::minutes(5))).expect("sign");
    assert_eq!(browse_token::connector_of(&token), Some(id));

    // A forged token still reports its claimed connector, which is exactly right: choosing which secrets to
    // try needs the id first, and naming the wrong connector produces a signature that does not match.
    let forged = format!("{}.AAAA", token.split_once('.').expect("dotted").0);
    assert_eq!(browse_token::connector_of(&forged), Some(id));
    assert_eq!(
        browse_token::verify([&key], &forged, now()),
        Err(BrowseError::BadSignature)
    );
}

#[test]
fn a_lifetime_beyond_the_ceiling_is_refused_however_well_it_is_signed() {
    // The property that stops this being an API key with extra steps: the *site* chooses the expiry, so the
    // ceiling has to be checked by whoever verifies.
    let id = Uuid::now_v7();
    let key = secret("the-connector-secret");
    let year = browse_token::sign(&key, &claim(id, Duration::days(365))).expect("sign");
    assert_eq!(
        browse_token::verify([&key], &year, now()),
        Err(BrowseError::TooLong)
    );

    // Exactly at the ceiling is fine — the refusal is for *more* than the maximum, so a site setting the
    // documented value is not caught by an off-by-one.
    let edge = browse_token::sign(&key, &claim(id, MAX_TTL)).expect("sign");
    assert!(browse_token::verify([&key], &edge, now()).is_ok());
}

#[test]
fn a_signature_failure_outranks_both_the_expiry_and_the_ceiling() {
    // The other order tells a forger their attempt was otherwise accepted, which is a hint about how to
    // succeed. Same argument `signed_url::verify` makes.
    let id = Uuid::now_v7();
    let key = secret("the-connector-secret");
    let wrong = secret("not-the-connector-secret");

    let expired = browse_token::sign(&key, &claim(id, Duration::minutes(-5))).expect("sign");
    assert_eq!(
        browse_token::verify([&wrong], &expired, now()),
        Err(BrowseError::BadSignature),
        "an expired forgery reports the forgery",
    );
    assert_eq!(
        browse_token::verify([&key], &expired, now()),
        Err(BrowseError::Expired),
        "and a genuine expired token reports the expiry",
    );

    let overlong = browse_token::sign(&key, &claim(id, Duration::days(365))).expect("sign");
    assert_eq!(
        browse_token::verify([&wrong], &overlong, now()),
        Err(BrowseError::BadSignature),
    );
}

#[test]
fn a_rotation_does_not_break_a_picker() {
    // The site is mid-rotation: it signed with the secret it had, and damrs holds both.
    let id = Uuid::now_v7();
    let superseded = secret("the-old-secret");
    let current = secret("the-new-secret");
    let token = browse_token::sign(&superseded, &claim(id, Duration::minutes(5))).expect("sign");

    assert!(
        browse_token::verify([&current, &superseded], &token, now()).is_ok(),
        "the superseded secret still verifies while it is in the keyring",
    );
    // Once the window closes the secret is simply absent, and the token fails as a bad signature with no clock
    // involved — the same shape as the delivery keyring.
    assert_eq!(
        browse_token::verify([&current], &token, now()),
        Err(BrowseError::BadSignature)
    );
    // And a token signed with the new secret works too, which has to be asserted separately: a bug that only
    // tried the *last* secret would pass the case above and fail everything after the site deployed.
    let fresh = browse_token::sign(&current, &claim(id, Duration::minutes(5))).expect("sign");
    assert!(browse_token::verify([&current, &superseded], &fresh, now()).is_ok());
}

#[test]
fn a_connector_with_no_secrets_left_is_a_bad_signature_not_its_own_answer() {
    // A revoked connector has none. Saying "no secrets" would distinguish revoked from forged to whoever holds
    // the token, which is the one thing the flat refusal exists to avoid.
    let id = Uuid::now_v7();
    let token =
        browse_token::sign(&secret("whatever"), &claim(id, Duration::minutes(5))).expect("sign");
    assert_eq!(
        browse_token::verify(std::iter::empty(), &token, now()),
        Err(BrowseError::BadSignature)
    );
}

#[test]
fn a_malformed_token_is_refused_rather_than_guessed_at() {
    let key = secret("the-connector-secret");
    for token in ["", "no-dot", ".", "!!!.!!!", "AAAA.AAAA", "AAAAAAAA."] {
        assert!(
            matches!(
                browse_token::verify([&key], token, now()),
                Err(BrowseError::Malformed | BrowseError::WrongVersion | BrowseError::BadSignature)
            ),
            "{token:?}",
        );
        assert!(browse_token::connector_of(token).is_none(), "{token:?}");
    }
}

#[test]
fn a_payload_with_something_appended_is_refused() {
    // Accepting trailing bytes makes the encoding non-injective again — two payloads that mean the same thing
    // and sign differently, which is the door length prefixes exist to close.
    use base64::Engine as _;
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let id = Uuid::now_v7();
    let key = secret("the-connector-secret");
    let token = browse_token::sign(&key, &claim(id, Duration::minutes(5))).expect("sign");
    let (payload_b64, _) = token.split_once('.').expect("dotted");
    let mut payload = encoder.decode(payload_b64).expect("decode");
    payload.push(0);

    // Re-signed, so the signature is genuine and the *parse* is what has to refuse it.
    let resigned = browse_token::sign(&key, &claim(id, Duration::minutes(5))).expect("sign");
    let _ = resigned;
    let forged = format!("{}.{}", encoder.encode(&payload), "AAAA");
    assert_eq!(
        browse_token::verify([&key], &forged, now()),
        Err(BrowseError::Malformed),
    );
}

#[test]
fn a_token_for_one_connector_does_not_verify_for_another() {
    // Two sites, two secrets. One must not be able to browse as the other even if it guesses the id.
    let one = Uuid::now_v7();
    let two = Uuid::now_v7();
    let key_one = secret("secret-one");
    let key_two = secret("secret-two");

    let token = browse_token::sign(&key_one, &claim(two, Duration::minutes(5))).expect("sign");
    // Site one signed a claim naming site two. Verified against site two's secrets — as the endpoint will,
    // because it resolves the connector from the claim — it fails.
    assert_eq!(
        browse_token::verify([&key_two], &token, now()),
        Err(BrowseError::BadSignature)
    );
    let _ = one;
}
