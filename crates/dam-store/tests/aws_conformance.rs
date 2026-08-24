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
//! ## What the shared suite deliberately does not wait for, and what this file now does
//!
//! The shared suite does not wait for a restore to complete. Standard against Glacier is three to five hours
//! and Bulk up to twelve; a test that waited would be a test nobody runs. It asserts everything observable in
//! the first seconds — the ticket, its ETA, that an in-flight restore yields no bytes, that a duplicate
//! request does not double-charge, and that the storage class does *not* change while one is in flight.
//!
//! That left **completion** covered only by `FakeS3Store`'s controllable clock, which is a fake asserting
//! that our own state machine agrees with itself. Whether *AWS* reports what we expect at the moment a
//! restored copy appears — the state, the bytes, the expiry, and the class staying put — was observed once by
//! hand and never asserted.
//!
//! `a_glacier_restore_completes_and_serves_the_original_bytes` closes that, and the tier is why it is
//! possible: **Expedited against Glacier is one to five minutes**, where Standard is hours and Deep Archive
//! has no Expedited tier at all. So the one restore this project can actually watch finish is a Glacier
//! Expedited one, and that is what it watches. Deep Archive completion stays unprovable in a test by
//! construction — twelve hours minimum — and is called out as such rather than implied by proximity.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bytes::Bytes;
use chrono::Utc;
use dam_core::{RestoreState, RestoreTier, StorageClass};
use dam_store::{BlobStore, GetOutcome, Key, S3Store, conformance};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use uuid::Uuid;

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

/// How long to wait for an Expedited Glacier restore before calling it a failure.
///
/// AWS documents Expedited as one to five minutes. Fifteen is generous enough that a slow day is not a red
/// test and short enough that somebody will actually run this. Deliberately *not* hours: a test that could
/// take an afternoon is a test that gets `--skip`ped, which is how this gap opened in the first place.
const EXPEDITED_BUDGET: Duration = Duration::from_secs(15 * 60);

/// How long the restored copy is asked to live. One day, not seven: an unnecessary six days of a Glacier
/// restore's storage is real money for no extra coverage.
const KEEP_FOR: Duration = Duration::from_secs(86_400);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "waits minutes for a real Glacier restore; run with --ignored and DAMRS_TEST_BUCKET set"]
async fn a_glacier_restore_completes_and_serves_the_original_bytes() {
    // The case the fake cannot make: a fake with a controllable clock proves our state machine agrees with
    // itself. This proves AWS agrees with it — at the one moment that matters, when the temporary copy
    // appears.
    let store = S3Store::aws(&bucket(), &region()).await;
    let key = Key::original(Uuid::now_v7(), &"e".repeat(64)).expect("key");
    let body = Bytes::from_static(b"the bytes that have to come back unchanged");

    store
        .put(&key, body.clone(), StorageClass::Glacier)
        .await
        .expect("put GLACIER");

    // Cleaned up even if an assertion below fails, because a leaked Glacier object bills a 90-day minimum.
    let cleanup = Cleanup {
        store: &store,
        key: key.clone(),
    };

    let ticket = store
        .restore(&key, RestoreTier::Expedited, KEEP_FOR)
        .await
        .expect("expedited restore");
    assert!(
        matches!(
            ticket.state,
            RestoreState::Requested | RestoreState::Ongoing
        ),
        "a fresh restore should be Requested or Ongoing, got {:?}",
        ticket.state
    );
    println!("restore requested; eta {:?}", ticket.eta);

    let started = Instant::now();
    let available = loop {
        let head = store.head(&key).await.expect("head while restoring");
        // Asserted on every poll rather than once at the end. A restore makes a *temporary copy*; if the
        // class ever changed we would have a permanent move reported as a restore, and the object would read
        // as available forever and then 403 the day the copy expired.
        assert_eq!(
            head.storage_class,
            StorageClass::Glacier,
            "the class must not change at any point during a restore"
        );
        match head.restore_state {
            RestoreState::Available => break head,
            RestoreState::Requested | RestoreState::Ongoing => {
                assert!(
                    started.elapsed() < EXPEDITED_BUDGET,
                    "an Expedited Glacier restore did not complete within {}s. AWS documents one to five \
                     minutes; either the tier was silently downgraded or our state reading is wrong",
                    EXPEDITED_BUDGET.as_secs()
                );
                sleep(Duration::from_secs(15)).await;
            }
            other => panic!("unexpected restore state while waiting: {other:?}"),
        }
    };
    println!("restore completed in {:?}", started.elapsed());

    // **An available restore must carry an expiry.** §6.5 makes this a database constraint, and the reason is
    // that an available placement with no expiry is unreclaimable state — nothing knows when to stop serving.
    let expiry = available
        .restore_expires_at
        .expect("an Available restore must report when the copy goes away");
    let asked_for = Utc::now() + chrono::Duration::seconds(KEEP_FOR.as_secs() as i64);
    // AWS rounds `expiry-date` up to a day boundary, so its value is *later* than the moment we asked for.
    // Ours being the earlier of the two is the direction to be wrong in: delivery stops before the bytes do.
    assert!(
        expiry >= asked_for - chrono::Duration::minutes(5),
        "reported expiry {expiry} is before what we asked to keep ({asked_for}); delivery would keep \
         serving after the copy had gone"
    );

    // And the whole point: the bytes.
    match store.get(&key, None).await.expect("get after restore") {
        GetOutcome::Bytes(returned) => assert_eq!(
            returned, body,
            "the restored copy must be the original bytes, unchanged"
        ),
        other => panic!("a completed restore must yield bytes, got {other:?}"),
    }

    drop(cleanup);
}

/// Deletes the object however the test ends.
///
/// Not tidiness: a leaked Glacier object bills a 90-day minimum on whatever it touched, so an `assert!` firing
/// three lines from the end should not cost three months of storage.
///
/// `block_in_place` plus `block_on` rather than a spawned task, and that is the point — a task spawned from
/// `Drop` is not awaited by anyone, so the runtime shuts down with the test and the delete may never be
/// issued. Cleanup that runs "usually" is cleanup that bills.
struct Cleanup<'a> {
    store: &'a S3Store,
    key: Key,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let store = self.store;
        let key = self.key.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                if let Err(error) = store.delete(&key).await {
                    // Loud, because the cost of a missed delete is a bill rather than a failure.
                    eprintln!(
                        "FAILED TO DELETE {key:?}: {error}. This object bills a 90-day minimum."
                    );
                }
            });
        });
    }
}
