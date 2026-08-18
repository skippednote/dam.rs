//! C2PA verification, preservation and re-signing (1.9) — D13.
//!
//! The task ARCHITECTURE calls out as *wrong rather than incomplete* if it is skipped. libvips, ffmpeg
//! and pdfium all strip embedded metadata by default, so the derivative pipeline as first designed
//! silently destroyed provenance that cameras, Adobe, OpenAI and Google now attach at capture. A DAM is
//! the system of record and the worst place in the chain to break it.
//!
//! Three decisions from DECISIONS.md are load-bearing here and each has a test:
//!
//! - **C2PA 1** — damrs signs as one identity per deployment. A signature attests to who performed the
//!   transform, which is the service, not the customer.
//! - **C2PA 2** — a test certificate is refused outside development. A test-signed credential in
//!   production is *worse* than none: it looks like provenance and verifies against nothing.
//! - **C2PA 3** — an inbound manifest that fails validation is accepted, recorded, and not re-signed.
//!   Rejecting it would stop a customer ingesting their own archive.
//!
//! One mapping is worth stating because getting it backwards would be a security bug: c2pa-rs reports
//! `Valid` for a signature that verifies and `Trusted` for one that also chains to a known root. Our
//! `valid` means the *trusted* case. A cryptographically sound signature from a signer nobody
//! recognises is `untrusted`, which the schema is explicit about — and it must never be displayed as
//! though a known authority stood behind it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::config::Environment;
use dam_media::provenance::{self, ProvenanceState, SigningIdentity};
use image::{ImageFormat, RgbImage};
use std::io::Cursor;

/// A JPEG with no credentials of any kind.
fn plain_jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut img = RgbImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        px.0 = [(x % 256) as u8, (y % 256) as u8, 120];
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, ImageFormat::Jpeg)
        .expect("encode");
    out.into_inner()
}

/// A signing identity for tests, standing in for a deployment's configured certificate.
fn dev_identity() -> SigningIdentity {
    SigningIdentity::ephemeral(Environment::Development, "damrs-test.local").expect("identity")
}

/// A JPEG carrying a signed manifest, as a camera or Photoshop would hand us.
fn credentialed_jpeg() -> Vec<u8> {
    provenance::sign(
        &dev_identity(),
        &plain_jpeg(64, 48),
        "image/jpeg",
        provenance::Claim {
            claim_generator: "Test Camera 1.0".to_owned(),
            provenance: provenance::Provenance::Created(provenance::Origin::DigitalCapture),
            actions: vec![],
        },
    )
    .expect("sign")
    .bytes
}

// ─── verification ───────────────────────────────────────────────────────────

#[test]
fn a_file_with_no_credentials_verifies_as_absent_not_invalid() {
    // The distinction the schema exists to preserve. Most assets in a real library have never been
    // near a C2PA-aware tool, and reporting them as `invalid` would flood the tamper-detection queue
    // with every ordinary photograph and make the real signal unfindable.
    let outcome = provenance::verify("image/jpeg", &plain_jpeg(32, 32)).expect("verify");
    assert_eq!(outcome.state, ProvenanceState::None);
    assert!(
        outcome.manifest.is_none(),
        "there is nothing to store for an asset with no credentials"
    );
    assert!(outcome.signer_cn.is_none());
}

#[test]
fn a_signed_file_verifies_and_reports_who_signed_it() {
    let outcome = provenance::verify("image/jpeg", &credentialed_jpeg()).expect("verify");

    // `Untrusted`, not `Valid`: the signature verifies, and an ephemeral certificate chains to no
    // known root. That is the honest answer and the one the schema asks for — a valid signature from
    // an unrecognised signer must not be shown as though an authority stood behind it.
    assert_eq!(
        outcome.state,
        ProvenanceState::Untrusted,
        "an ephemeral certificate is cryptographically sound and trusted by nobody"
    );
    assert_eq!(outcome.signer_cn.as_deref(), Some("damrs-test.local"));
    assert!(
        outcome
            .claim_generator
            .as_deref()
            .is_some_and(|g| g.contains("Test Camera")),
        "got {:?}",
        outcome.claim_generator
    );
    assert!(
        outcome.manifest.is_some_and(|m| !m.is_empty()),
        "a detached manifest must come back for storage — §2 keeps it hot even when the master \
         is tiered to Deep Archive, so it cannot live only inside the original's bytes"
    );
}

#[test]
fn a_tampered_file_verifies_as_invalid() {
    // The red flag. A manifest binds a hash of the pixels, so altering them after signing must be
    // detected — this is the whole reason a DAM records provenance rather than just displaying it.
    let mut bytes = credentialed_jpeg();
    let len = bytes.len();
    // Late in the file, well past the manifest, so this corrupts image data and not the manifest
    // itself — the point is that the *binding* fails, not that the manifest became unparseable.
    for byte in bytes.iter_mut().skip(len - 200) {
        *byte ^= 0xff;
    }

    let outcome = provenance::verify("image/jpeg", &bytes).expect("verify");
    assert_eq!(
        outcome.state,
        ProvenanceState::Invalid,
        "tampering must be reported as invalid, never as absent"
    );
    assert!(
        !outcome.detail.is_null(),
        "the failure codes must be recorded — 'invalid' with no explanation is unactionable"
    );
}

#[test]
fn an_invalid_manifest_is_still_extracted_for_storage() {
    // C2PA 3: accepted, recorded, not re-signed. Discarding the broken manifest would destroy the
    // customer's evidence of *what* broke — and D13 forbids stripping regardless.
    let mut bytes = credentialed_jpeg();
    let len = bytes.len();
    for byte in bytes.iter_mut().skip(len - 200) {
        *byte ^= 0xff;
    }

    let outcome = provenance::verify("image/jpeg", &bytes).expect("verify");
    assert_eq!(outcome.state, ProvenanceState::Invalid);
    assert!(
        outcome.manifest.is_some_and(|m| !m.is_empty()),
        "a failed manifest is the customer's evidence and must be preserved"
    );
}

#[test]
fn a_file_that_is_not_an_image_at_all_is_an_error_not_a_verdict() {
    // `absent` would be a lie: we did not establish that this file has no credentials, we failed to
    // read it. Ingest needs to tell those apart, because one is normal and the other is a bug.
    let outcome = provenance::verify("image/jpeg", b"this is not a jpeg");
    assert!(
        outcome.is_err(),
        "unreadable input must not report a verdict"
    );
}

// ─── signing, and the identity rules ────────────────────────────────────────

#[test]
fn a_test_certificate_is_refused_outside_development() {
    // C2PA 2, and the one decision recorded as irreversible. A test-signed credential in production
    // looks like provenance and verifies against nothing, which is worse than no credential at all —
    // it would let a downstream consumer believe a chain had been checked.
    for environment in [
        Environment::Test,
        Environment::Staging,
        Environment::Production,
    ] {
        let refused = SigningIdentity::ephemeral(environment, "damrs.local");
        assert!(
            refused.is_err(),
            "an ephemeral certificate must be refused in {environment:?}"
        );
    }
    assert!(
        SigningIdentity::ephemeral(Environment::Development, "damrs.local").is_ok(),
        "and permitted in development, or nothing can be tested"
    );
}

#[test]
fn signing_identifies_the_service_and_not_the_tenant() {
    // C2PA 1. A signature attests to who performed the transform. Per-tenant certificates would also
    // mean provisioning a CA-issued certificate per customer, which is operationally infeasible; the
    // tenant travels as assertion metadata instead.
    let signed = provenance::sign(
        &dev_identity(),
        &plain_jpeg(32, 32),
        "image/jpeg",
        provenance::Claim {
            claim_generator: provenance::claim_generator(),
            provenance: provenance::Provenance::Created(provenance::Origin::DigitalCapture),
            actions: vec![provenance::Action::resized(32, 32)],
        },
    )
    .expect("sign");

    let outcome = provenance::verify("image/jpeg", &signed.bytes).expect("verify");
    let generator = outcome.claim_generator.expect("a claim generator");
    assert!(
        generator.starts_with("damrs"),
        "the claim generator names this service: got {generator}"
    );
    assert_eq!(outcome.signer_cn.as_deref(), Some("damrs-test.local"));
}

// ─── the chain, which is the actual requirement ─────────────────────────────

#[test]
fn a_derivative_chains_to_its_parent_rather_than_starting_fresh() {
    // D13 in one assertion. Appending an action to the credential chain and *terminating* it look
    // identical if you only check that the derivative has a manifest — both produce a signed file.
    // The difference is whether the original is recorded as an ingredient, which is what lets anyone
    // downstream walk back to the camera.
    let original = credentialed_jpeg();
    let derivative_pixels = plain_jpeg(32, 24);

    let signed = provenance::sign(
        &dev_identity(),
        &derivative_pixels,
        "image/jpeg",
        provenance::Claim {
            claim_generator: provenance::claim_generator(),
            provenance: provenance::Provenance::DerivedFrom(provenance::Parent {
                bytes: original.clone(),
                format: "image/jpeg".to_owned(),
                title: "original.jpg".to_owned(),
            }),
            actions: vec![provenance::Action::resized(32, 24)],
        },
    )
    .expect("sign derivative");

    let outcome = provenance::verify("image/jpeg", &signed.bytes).expect("verify");
    assert_eq!(
        outcome.ingredient_count, 1,
        "the derivative must record the original as an ingredient, or the chain ends here"
    );
    assert!(
        outcome.actions.iter().any(|a| a.contains("resized")),
        "and say what was done to it: got {:?}",
        outcome.actions
    );
}

#[test]
fn a_derivative_of_an_uncredentialed_original_still_records_what_we_did() {
    // The common case, and the reason signing is not conditional on an inbound manifest. Most assets
    // arrive with no credentials; a derivative that says "damrs resized this, from that original" is
    // still the truthful start of a chain, and refusing to sign would mean the DAM's own transforms
    // are the one link nobody can audit.
    let signed = provenance::sign(
        &dev_identity(),
        &plain_jpeg(16, 16),
        "image/jpeg",
        provenance::Claim {
            claim_generator: provenance::claim_generator(),
            provenance: provenance::Provenance::DerivedFrom(provenance::Parent {
                bytes: plain_jpeg(64, 64),
                format: "image/jpeg".to_owned(),
                title: "plain.jpg".to_owned(),
            }),
            actions: vec![provenance::Action::resized(16, 16)],
        },
    )
    .expect("sign");

    let outcome = provenance::verify("image/jpeg", &signed.bytes).expect("verify");
    assert_eq!(outcome.ingredient_count, 1);
    assert_eq!(outcome.state, ProvenanceState::Untrusted);
}

#[test]
fn the_detached_manifest_verifies_against_the_asset_it_came_from() {
    // Why the manifest can be stored separately at all (§2: metadata stays hot, masters tier). A
    // detached manifest that could not be re-attached would make an archived asset's provenance
    // unverifiable — the exact failure the separate-object design is meant to avoid.
    let signed = provenance::sign(
        &dev_identity(),
        &plain_jpeg(48, 48),
        "image/jpeg",
        provenance::Claim {
            claim_generator: provenance::claim_generator(),
            provenance: provenance::Provenance::Created(provenance::Origin::DigitalCapture),
            actions: vec![],
        },
    )
    .expect("sign");

    let manifest = signed.manifest.expect("a detached manifest");
    let reattached =
        provenance::verify_detached("image/jpeg", &signed.bytes, &manifest).expect("verify");
    assert_eq!(reattached.state, ProvenanceState::Untrusted);
    assert_eq!(reattached.signer_cn.as_deref(), Some("damrs-test.local"));
}

#[test]
fn every_chain_begins_with_a_creating_or_opening_action() {
    // The invariant that broke three separate ways while this was written, each time producing a
    // manifest that verified as **invalid** — indistinguishable to any consumer from a tampered file.
    // The C2PA specification requires the chain to open with `c2pa.created` or `c2pa.opened`, requires
    // a `digitalSourceType` on the former, and requires the latter to reference its ingredient by a
    // hashed URI that does not exist until the manifest is assembled.
    //
    // So the first action is not the caller's to supply, and this asserts the builder really does add
    // it for both shapes. A regression here would silently mark every derivative as suspect.
    for provenance_kind in [
        provenance::Provenance::Created(provenance::Origin::DigitalCapture),
        provenance::Provenance::DerivedFrom(provenance::Parent {
            bytes: plain_jpeg(64, 64),
            format: "image/jpeg".to_owned(),
            title: "parent.jpg".to_owned(),
        }),
    ] {
        let signed = provenance::sign(
            &dev_identity(),
            &plain_jpeg(16, 16),
            "image/jpeg",
            provenance::Claim {
                claim_generator: provenance::claim_generator(),
                provenance: provenance_kind.clone(),
                actions: vec![provenance::Action::resized(16, 16)],
            },
        )
        .expect("sign");

        let outcome = provenance::verify("image/jpeg", &signed.bytes).expect("verify");
        assert_ne!(
            outcome.state,
            ProvenanceState::Invalid,
            "{provenance_kind:?} produced an invalid manifest: {}",
            outcome.detail
        );
        let first = outcome.actions.first().map(String::as_str);
        assert!(
            first == Some("c2pa.created") || first == Some("c2pa.opened"),
            "chain for {provenance_kind:?} starts with {first:?}"
        );
    }
}

#[test]
fn an_ai_generated_original_is_marked_as_such_in_the_manifest() {
    // D15 / GAPS G2: EU AI Act Article 50 requires a **machine-readable** mark on synthetic content,
    // and `digitalSourceType` is the machine-readable mark C2PA already defines — which is why D15 says
    // it shares D13's implementation rather than needing its own. The disclosure record and its review
    // workflow are M5's; this asserts the field they will write to works now, so M5 is a database
    // concern rather than a re-litigation of the manifest format.
    let signed = provenance::sign(
        &dev_identity(),
        &plain_jpeg(32, 32),
        "image/jpeg",
        provenance::Claim {
            claim_generator: provenance::claim_generator(),
            provenance: provenance::Provenance::Created(provenance::Origin::AlgorithmicMedia),
            actions: vec![],
        },
    )
    .expect("sign");

    let outcome = provenance::verify("image/jpeg", &signed.bytes).expect("verify");
    assert_ne!(
        outcome.state,
        ProvenanceState::Invalid,
        "{}",
        outcome.detail
    );
    assert!(
        outcome
            .source_types
            .iter()
            .any(|t| t.contains("trainedAlgorithmicMedia")),
        "the synthetic-content mark must be present and machine-readable: got {:?}",
        outcome.source_types
    );
}
