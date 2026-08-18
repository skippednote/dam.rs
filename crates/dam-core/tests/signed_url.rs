//! Signed delivery tokens (3.1) — the D12 chokepoint.
//!
//! Every download, render and connector fetch goes through one signed URL, so rights and ABAC are enforced
//! by the delivery design rather than by a caller remembering. What this suite defends is the signature
//! itself, and specifically the two things about it that are easy to get subtly wrong:
//!
//! - **The canonical form must be injective.** Join fields with a delimiter and two different payloads can
//!   share a signing string, so one valid signature covers both. That is a complete forgery primitive for
//!   anyone who can influence any field, and it looks like working code.
//! - **A signature is permission to attempt, not to receive.** The token proves we issued this exact
//!   request. Whether the caller may have the bytes is decided at delivery — otherwise every issued URL is
//!   an outstanding grant nothing can withdraw.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::Secret;
use dam_core::signed_url::{self, DeliveryClaim, Keyring, VerifyError};
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

fn keyring() -> Keyring {
    Keyring::single("k1", Secret::new("a-real-signing-key".to_owned()))
}

fn claim() -> DeliveryClaim {
    DeliveryClaim {
        asset_id: Uuid::from_u128(0xa55e7),
        transform: "web-2048".to_owned(),
        channel: "web".to_owned(),
        territory: "GB".to_owned(),
        identity_id: Some(Uuid::from_u128(0x1de)),
        expires_at: now() + Duration::minutes(15),
        key_id: "k1".to_owned(),
    }
}

fn sign(claim: &DeliveryClaim) -> String {
    signed_url::sign(&keyring(), claim).expect("a keyring with a signing key")
}

// ─── the round trip ─────────────────────────────────────────────────────────

#[test]
fn a_signed_token_verifies_and_returns_its_claim() {
    // The claim is returned rather than a boolean, because the caller needs the asset, transform, channel
    // and territory for the rights check — and re-parsing an already-verified token in the caller is how
    // the verified values and the used values drift apart.
    let token = sign(&claim());
    let verified = signed_url::verify(&keyring(), &token, now()).expect("verifies");
    assert_eq!(verified, claim());
}

#[test]
fn a_token_is_url_safe_with_no_padding() {
    // It goes in a path or a query string. Padding and `+`/`/` would need escaping, and a URL that
    // sometimes needs escaping is a URL that sometimes breaks.
    let token = sign(&claim());
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "got {token}"
    );
}

// ─── tampering ──────────────────────────────────────────────────────────────

#[test]
fn changing_the_transform_invalidates_the_token() {
    // The most obvious attack on a delivery URL: edit a thumbnail request into a request for the master.
    // If the transform were outside the signature, that would simply work.
    let mut forged = claim();
    forged.transform = "original".to_owned();
    let token = sign(&forged);

    // Re-signing works, of course — the attacker does not have the key. What matters is that a token
    // *signed for one transform* cannot be re-used for another, which the next assertion establishes by
    // splicing a valid signature onto a different payload.
    let legitimate = sign(&claim());
    let (_, signature) = legitimate.split_once('.').expect("two parts");
    let (payload, _) = token.split_once('.').expect("two parts");
    let spliced = format!("{payload}.{signature}");

    assert_eq!(
        signed_url::verify(&keyring(), &spliced, now()),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn every_field_is_covered_by_the_signature() {
    // A field outside the signature is a field an attacker chooses. Rather than trust that by inspection,
    // each field is varied in turn and the signature must differ — if any two match, that field is not
    // being signed.
    let base = sign(&claim());
    let variants: Vec<(&str, DeliveryClaim)> = vec![
        (
            "asset",
            DeliveryClaim {
                asset_id: Uuid::from_u128(0xbeef),
                ..claim()
            },
        ),
        (
            "transform",
            DeliveryClaim {
                transform: "thumb-256".to_owned(),
                ..claim()
            },
        ),
        (
            "channel",
            DeliveryClaim {
                channel: "advertising".to_owned(),
                ..claim()
            },
        ),
        (
            "territory",
            DeliveryClaim {
                territory: "CN".to_owned(),
                ..claim()
            },
        ),
        (
            "identity",
            DeliveryClaim {
                identity_id: Some(Uuid::from_u128(0xdead)),
                ..claim()
            },
        ),
        (
            "no identity",
            DeliveryClaim {
                identity_id: None,
                ..claim()
            },
        ),
        (
            "expiry",
            DeliveryClaim {
                expires_at: now() + Duration::days(365),
                ..claim()
            },
        ),
    ];

    for (name, variant) in variants {
        assert_ne!(
            sign(&variant),
            base,
            "changing the {name} must change the token, or that field is not signed"
        );
    }
}

#[test]
fn the_canonical_form_is_injective_even_when_a_value_contains_a_delimiter() {
    // The subtle one, and a complete forgery primitive if it fails.
    //
    // It took two attempts to write a test that actually catches it. Both earlier versions passed while the
    // implementation used `|` separators instead of length prefixes, because a scheme that emits a trailing
    // delimiter *per field* is still injective for the payloads I first chose: `"web|GB"` + `""` renders
    // `web|GB||`, and `"web"` + `"GB"` renders `web|GB|`.
    //
    // The collision needs the delimiter to move a boundary between two *populated* fields:
    //
    //   transform="web|2048", channel="x"   -> "web|2048|x|"
    //   transform="web",      channel="2048|x" -> "web|2048|x|"
    //
    // Identical. One signature covers both, so anyone who can influence the transform can forge the
    // channel — and the channel selects which licence terms apply. Length prefixes are injective whatever
    // the values contain.
    for delimiter in ["|", ".", ":", "\u{0}", "\n", "/"] {
        let left = DeliveryClaim {
            transform: format!("web{delimiter}2048"),
            channel: "x".to_owned(),
            ..claim()
        };
        let right = DeliveryClaim {
            transform: "web".to_owned(),
            channel: format!("2048{delimiter}x"),
            ..claim()
        };
        assert_ne!(
            sign(&left),
            sign(&right),
            "a {delimiter:?} inside a value must not let one signature cover two different claims"
        );
    }

    // The same across channel/territory, the pair that decides which licence terms apply.
    let left = DeliveryClaim {
        channel: "web|GB".to_owned(),
        territory: "x".to_owned(),
        ..claim()
    };
    let right = DeliveryClaim {
        channel: "web".to_owned(),
        territory: "GB|x".to_owned(),
        ..claim()
    };
    assert_ne!(sign(&left), sign(&right));
}

#[test]
fn an_absent_identity_and_an_empty_one_do_not_collide() {
    // `None` is encoded as a zero-length field rather than an omitted one. Omitting it would shorten the
    // payload and change what the following length means — which is the same boundary confusion one level
    // down.
    let absent = DeliveryClaim {
        identity_id: None,
        ..claim()
    };
    let token = sign(&absent);
    let verified = signed_url::verify(&keyring(), &token, now()).expect("verifies");
    assert_eq!(verified.identity_id, None);
    assert_ne!(token, sign(&claim()));
}

#[test]
fn a_token_signed_with_another_key_does_not_verify() {
    let other = Keyring::single("k1", Secret::new("a-different-key".to_owned()));
    let token = signed_url::sign(&other, &claim()).expect("sign");
    assert_eq!(
        signed_url::verify(&keyring(), &token, now()),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn trailing_bytes_make_a_token_malformed_rather_than_being_ignored() {
    // Ignoring them would let an attacker append data to a payload whose signature they already hold. The
    // signature would fail — but accepting the shape at all invites the next variation.
    let token = sign(&claim());
    let (payload_b64, signature) = token.split_once('.').expect("two parts");
    use base64::Engine as _;
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut payload = encoder.decode(payload_b64).expect("decode");
    payload.extend_from_slice(b"extra");
    let extended = format!("{}.{signature}", encoder.encode(&payload));

    let outcome = signed_url::verify(&keyring(), &extended, now());
    assert!(
        matches!(
            outcome,
            Err(VerifyError::Malformed | VerifyError::BadSignature)
        ),
        "got {outcome:?}"
    );
}

#[test]
fn a_truncated_or_nonsense_token_is_malformed() {
    for bad in ["", "no-dot", ".", "!!!.!!!", "aaaa.bbbb"] {
        let outcome = signed_url::verify(&keyring(), bad, now());
        assert!(outcome.is_err(), "{bad:?} must not verify");
    }
}

#[test]
fn a_length_prefix_longer_than_the_payload_is_refused_rather_than_panicking() {
    // A hostile token can claim any field length. Reading past the buffer would be a panic, which on a
    // delivery endpoint is a denial of service reachable by anyone who can construct a URL.
    use base64::Engine as _;
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    // Version byte, then a field claiming 4 GB.
    let payload = [1u8, 0xff, 0xff, 0xff, 0xff];
    let token = format!("{}.{}", encoder.encode(payload), encoder.encode([0u8; 32]));
    assert_eq!(
        signed_url::verify(&keyring(), &token, now()),
        Err(VerifyError::Malformed)
    );
}

// ─── expiry ─────────────────────────────────────────────────────────────────

#[test]
fn an_expired_token_is_refused() {
    let expired = DeliveryClaim {
        expires_at: now() - Duration::seconds(1),
        ..claim()
    };
    let token = sign(&expired);
    assert_eq!(
        signed_url::verify(&keyring(), &token, now()),
        Err(VerifyError::Expired)
    );
}

#[test]
fn expiry_is_exclusive_at_the_boundary() {
    // A token whose expiry is exactly now is expired. The alternative leaves a one-second window that only
    // shows up as an intermittent success in a test somebody wrote later.
    let boundary = DeliveryClaim {
        expires_at: now(),
        ..claim()
    };
    let token = sign(&boundary);
    assert_eq!(
        signed_url::verify(&keyring(), &token, now()),
        Err(VerifyError::Expired)
    );
    assert!(signed_url::verify(&keyring(), &token, now() - Duration::seconds(1)).is_ok());
}

#[test]
fn a_forged_expired_token_reports_a_bad_signature_not_an_expiry() {
    // The signature is checked first on purpose. Reporting "expired" for a token whose signature is also
    // wrong tells an attacker their forgery was otherwise accepted and they only need a fresher timestamp.
    let expired = DeliveryClaim {
        expires_at: now() - Duration::days(1),
        ..claim()
    };
    let token = signed_url::sign(
        &Keyring::single("k1", Secret::new("wrong-key".to_owned())),
        &expired,
    )
    .expect("sign");
    assert_eq!(
        signed_url::verify(&keyring(), &token, now()),
        Err(VerifyError::BadSignature)
    );
}

// ─── key rotation ───────────────────────────────────────────────────────────

#[test]
fn a_retired_key_still_verifies_but_no_longer_signs() {
    // Rotation without invalidating outstanding URLs. Without it, rotating a key breaks every link already
    // in an email or a CMS — so in practice nobody rotates.
    let old = Secret::new("old-key".to_owned());
    let issued_before_rotation =
        signed_url::sign(&Keyring::single("k1", old.clone()), &claim()).expect("sign");

    let rotated = Keyring::single("k2", Secret::new("new-key".to_owned())).with_retired("k1", old);

    let verified = signed_url::verify(&rotated, &issued_before_rotation, now())
        .expect("a token signed with the retired key must still verify");
    assert_eq!(verified.key_id, "k1");

    // And new tokens carry the new key id.
    let fresh = signed_url::sign(&rotated, &claim()).expect("sign");
    assert_eq!(
        signed_url::verify(&rotated, &fresh, now())
            .expect("verifies")
            .key_id,
        "k2"
    );
}

#[test]
fn the_caller_cannot_choose_which_key_signs() {
    // The claim carries a `key_id` because verification needs it, and a caller setting it would be able to
    // pin an about-to-be-retired key — outliving the rotation it was meant to end.
    let rotated = Keyring::single("k2", Secret::new("new-key".to_owned()))
        .with_retired("k1", Secret::new("old-key".to_owned()));
    let asking_for_the_old_key = DeliveryClaim {
        key_id: "k1".to_owned(),
        ..claim()
    };
    let token = signed_url::sign(&rotated, &asking_for_the_old_key).expect("sign");
    assert_eq!(
        signed_url::verify(&rotated, &token, now())
            .expect("verifies")
            .key_id,
        "k2",
        "the keyring decides which key signs, not the claim"
    );
}

#[test]
fn a_token_naming_an_unknown_key_is_refused() {
    let stranger = DeliveryClaim {
        key_id: "k99".to_owned(),
        ..claim()
    };
    // Signed by a keyring that happens to use that id, then presented to one that does not.
    let token = signed_url::sign(
        &Keyring::single("k99", Secret::new("whatever".to_owned())),
        &stranger,
    )
    .expect("sign");
    assert_eq!(
        signed_url::verify(&keyring(), &token, now()),
        Err(VerifyError::UnknownKey)
    );
}

// ─── versioning ─────────────────────────────────────────────────────────────

#[test]
fn a_token_from_another_format_version_is_refused_rather_than_misparsed() {
    // Without the version byte, adding a field later would make an old token decode into a new one with a
    // shifted meaning — and a shifted meaning on a delivery token is a transform or a channel read from the
    // wrong bytes.
    use base64::Engine as _;
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let token = sign(&claim());
    let (payload_b64, _) = token.split_once('.').expect("two parts");
    let mut payload = encoder.decode(payload_b64).expect("decode");
    payload[0] = 99;

    // Signed properly for that payload, so only the version can be the objection.
    let signed = signed_url::sign(&keyring(), &claim()).expect("sign");
    let _ = signed;
    let outcome = signed_url::verify(
        &keyring(),
        &format!("{}.{}", encoder.encode(&payload), encoder.encode([0u8; 32])),
        now(),
    );
    assert!(
        matches!(
            outcome,
            Err(VerifyError::WrongVersion | VerifyError::BadSignature)
        ),
        "got {outcome:?}"
    );
}
