//! `S3Store` against **real AWS S3** — the only place Glacier semantics are actually exercised.
//!
//! ## Why this file has to exist
//!
//! ARCHITECTURE §20.2 says SeaweedFS gives the wire protocol, `FakeS3Store` gives the tiering state machine
//! with a controllable clock, and "neither can prove an actual `RestoreObject` against Deep Archive". The
//! shared conformance suite has the cases; both other drivers *skip* them, and the skip messages point at
//! "the AWS nightly" as the thing that covers them.
//!
//! The AWS nightly did not cover them. It invoked `--features aws-conformance`, which did not exist, and
//! exited early every night because the credential secret was unset — so it had never failed, never run, and
//! never proved anything. Both skip messages were writing cheques against this file before it was written.
//!
//! So what is asserted here is the mirror image of `s3_conformance.rs`: that the two cases which must skip on
//! SeaweedFS must **pass** here. A run where they skipped would mean the store reported itself as incapable,
//! which against real AWS is a bug in the capability detection rather than a fact about the backend.
//!
//! ## Ignored, and gated on a bucket
//!
//! `#[ignore]` because it costs real money and takes real time: Deep Archive bills a 180-day minimum on
//! whatever it touches, and a Standard restore against Glacier takes hours. Run deliberately:
//!
//! ```text
//! DAMRS_TEST_BUCKET=my-bucket AWS_REGION=ap-south-1 AWS_PROFILE=… \
//!     cargo test -p dam-store --features aws-conformance -- --ignored
//! ```
//!
//! Missing configuration is a **failure**, not a skip. A silent skip is what the nightly was already doing,
//! and the whole point of this file is that nobody should be able to believe it ran when it did not.
//!
//! ## What it deliberately does not wait for
//!
//! A restore completing. Standard against Glacier is three to five hours and Bulk is up to twelve; a test
//! that waited would be a test nobody runs. The suite asserts everything observable in the first seconds — the
//! ticket, its ETA, that an in-flight restore yields no bytes, that a duplicate request does not double-charge,
//! and that the storage class does *not* change while a restore is in flight. Completion is what
//! `dam-pipeline`'s poll covers against the fake's clock, and what a manual end-to-end pass covers against
//! real AWS.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use dam_store::{S3Store, conformance};

/// The bucket to run against. No default: a hard-coded one would either not exist or belong to somebody else.
fn bucket() -> String {
    std::env::var("DAMRS_TEST_BUCKET").expect(
        "DAMRS_TEST_BUCKET must name a real S3 bucket. This test costs money and cannot invent one — \
         and it fails rather than skipping, because a skip is exactly how the nightly managed to pass for \
         months without running",
    )
}

fn region() -> String {
    std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .expect("AWS_REGION must be set so the bucket's region is explicit rather than guessed")
}

#[tokio::test]
#[ignore = "costs money against real AWS; run with --ignored and DAMRS_TEST_BUCKET set"]
async fn real_s3_passes_the_cases_every_other_driver_skips() {
    let store = S3Store::aws(&bucket(), &region()).await;
    let report = conformance::run(&store).await;
    // Printed unconditionally: the report is the artefact. A green tick that hides which cases ran is how a
    // capability gap becomes a production surprise, which is the same argument `s3_conformance.rs` makes.
    println!("{report}");

    let skipped: Vec<&str> = report
        .skipped
        .iter()
        .map(|(case, _)| case.as_str())
        .collect();
    assert!(
        !skipped.contains(&"storage classes"),
        "real S3 has storage classes; skipping means the store reported itself incapable, which is a bug \
         in capability detection rather than a fact about AWS: {report}"
    );
    assert!(
        !skipped.contains(&"restore lifecycle"),
        "real S3 has RestoreObject, and this file exists precisely to run this case: {report}"
    );

    // The named cases, so a rename in the suite cannot quietly drop coverage here.
    for case in [
        "GLACIER_IR is readable without a restore",
        "transition to DEEP_ARCHIVE",
        "restore returns a ticket with an ETA",
        "in-progress restore does not yield bytes",
        "duplicate restore is a no-op",
    ] {
        assert!(
            report.passed.iter().any(|passed| passed == case),
            "{case:?} did not run against real AWS: {report}"
        );
    }
}
