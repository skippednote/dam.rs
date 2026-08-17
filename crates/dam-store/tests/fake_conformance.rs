//! `FakeS3Store` against the shared conformance suite, plus the timing cases that
//! exist only here.
//!
//! The suite is the same one `S3Store` runs, so the two cannot diverge on anything they
//! share. Everything after it is what a real backend cannot be tested on: the cases are
//! measured in hours, so they need a clock you can move.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use bytes::Bytes;
use dam_core::{RestoreState, RestoreTier, StorageClass};
use dam_store::{BlobStore, FakeS3Store, GetOutcome, Key, conformance};
use std::time::Duration;
use uuid::Uuid;

const H: &str = "9f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

fn key() -> Key {
    Key::original(Uuid::now_v7(), H).expect("key")
}

#[tokio::test]
async fn passes_the_shared_conformance_suite() {
    let (store, _clock) = FakeS3Store::with_test_clock();
    let report = conformance::run(&store).await;
    // Printed so lost coverage is visible in CI output rather than hidden behind a
    // green tick.
    println!("{report}");
    assert!(
        report.passed.len() >= 12,
        "suite ran too few cases: {report}"
    );
    // The fake deliberately does not claim versioning or object lock — those need a
    // real server, because the point of object lock is that the server refuses.
    assert_eq!(
        report.skipped.len(),
        0,
        "the fake claims every capability the suite covers, so nothing should skip"
    );
}

#[tokio::test]
async fn a_deep_archive_restore_becomes_available_only_after_the_wait() {
    let (store, clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"master"), StorageClass::DeepArchive)
        .await
        .expect("put");

    // Cold: a normal outcome, not an error.
    assert!(matches!(
        store.get(&k, None).await.expect("cold get"),
        GetOutcome::NotAvailable(_)
    ));

    let ticket = store
        .restore(&k, RestoreTier::Standard, Duration::from_secs(7 * 86400))
        .await
        .expect("restore");
    assert_eq!(ticket.state, RestoreState::Ongoing);

    // 11 hours in: still not ready. Deep Archive Standard is ~12 h.
    clock.advance_hours(11);
    assert!(
        matches!(
            store.get(&k, None).await.expect("get at 11h"),
            GetOutcome::NotAvailable(_)
        ),
        "a Deep Archive Standard restore must not be ready at 11 hours"
    );

    // 13 hours in: ready.
    clock.advance_hours(2);
    let got = store.get(&k, None).await.expect("get at 13h");
    assert!(
        matches!(got, GetOutcome::Bytes(ref b) if b == &Bytes::from_static(b"master")),
        "the restore should be available by 13 hours"
    );
    assert_eq!(
        store.head(&k).await.expect("head").restore_state,
        RestoreState::Available
    );
}

#[tokio::test]
async fn the_temporary_copy_expires_and_the_object_goes_cold_again() {
    // The case that matters most, and the one no real backend can be tested on: a
    // download starting the day the copy lapses. Conflating restore state with storage
    // class makes an object read as available forever and 403 on this boundary.
    let (store, clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"master"), StorageClass::Glacier)
        .await
        .expect("put");
    store
        .restore(&k, RestoreTier::Bulk, Duration::from_secs(2 * 86400))
        .await
        .expect("restore");

    // Bulk on Glacier is ~12 h, so the copy arrives at t+12 and the 48-hour keep-warm
    // runs to t+60.
    clock.advance_hours(13);
    assert!(matches!(
        store.get(&k, None).await.expect("get"),
        GetOutcome::Bytes(_)
    ));

    clock.advance_hours(46); // t+59: one hour to go
    assert!(
        matches!(
            store.get(&k, None).await.expect("get"),
            GetOutcome::Bytes(_)
        ),
        "still inside the keep-warm window at t+59h"
    );

    // t+60: exactly the expiry. The boundary is EXCLUSIVE — at `expires_at` the copy is
    // already gone, matching S3's `expiry-date` semantics. Worth pinning, because an
    // inclusive boundary would hand out bytes for one more request than AWS would and
    // the difference only shows up as an intermittent 403 in production.
    clock.advance_hours(1);
    assert!(
        matches!(
            store.get(&k, None).await.expect("get"),
            GetOutcome::NotAvailable(_)
        ),
        "the expiry boundary must be exclusive: at expires_at the copy is gone"
    );
    assert_eq!(
        store.head(&k).await.expect("head").restore_state,
        RestoreState::Expired
    );
}

#[tokio::test]
async fn the_keep_warm_window_runs_from_availability_not_from_the_request() {
    // A 48-hour Bulk restore kept for 24 hours would otherwise expire before it
    // arrived, and the caller would never get the bytes it paid for.
    let (store, clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"x"), StorageClass::DeepArchive)
        .await
        .expect("put");
    store
        .restore(&k, RestoreTier::Bulk, Duration::from_secs(86400))
        .await
        .expect("restore");

    clock.advance_hours(49); // Bulk on Deep Archive is ~48 h
    assert!(
        matches!(
            store.get(&k, None).await.expect("get"),
            GetOutcome::Bytes(_)
        ),
        "the keep-warm window must start when the copy arrives, not when it was asked for"
    );
}

#[tokio::test]
async fn deep_archive_refuses_expedited_retrieval() {
    let (store, _clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"x"), StorageClass::DeepArchive)
        .await
        .expect("put");
    let err = store
        .restore(&k, RestoreTier::Expedited, Duration::from_secs(86400))
        .await
        .expect_err("Deep Archive has no Expedited tier");
    assert!(err.to_string().contains("expedited"), "{err}");
}

#[tokio::test]
async fn restoring_a_hot_object_is_refused_rather_than_silently_succeeding() {
    // A caller restoring a Standard object has a bug. Succeeding would hide it and
    // bill a retrieval that was never needed.
    let (store, _clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"x"), StorageClass::Standard)
        .await
        .expect("put");
    assert!(
        store
            .restore(&k, RestoreTier::Standard, Duration::from_secs(86400))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_minimum_duration_blocks_an_early_re_tier() {
    // Tier to Deep Archive, change your mind three days later, and all 180 days are
    // still billed. The lifecycle engine has to know before it moves anything.
    let (store, clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"x"), StorageClass::DeepArchive)
        .await
        .expect("put");

    clock.advance_hours(72);
    assert_eq!(
        store.min_duration_elapsed(&k),
        Some(false),
        "3 days into a 180-day minimum"
    );

    clock.advance_hours(178 * 24);
    assert_eq!(store.min_duration_elapsed(&k), Some(true), "past 180 days");
}

#[tokio::test]
async fn a_transition_invalidates_a_live_restore() {
    // The temporary copy belonged to the previous class. Keeping it would report an
    // object as readable when the class it now sits in cannot serve it.
    let (store, clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"x"), StorageClass::Glacier)
        .await
        .expect("put");
    store
        .restore(&k, RestoreTier::Bulk, Duration::from_secs(7 * 86400))
        .await
        .expect("restore");
    clock.advance_hours(13);
    assert!(matches!(
        store.get(&k, None).await.expect("get"),
        GetOutcome::Bytes(_)
    ));

    store
        .transition(&k, StorageClass::DeepArchive)
        .await
        .expect("transition");
    assert_eq!(
        store.head(&k).await.expect("head").restore_state,
        RestoreState::None,
        "a transition must clear the restore"
    );
    assert!(matches!(
        store.get(&k, None).await.expect("get"),
        GetOutcome::NotAvailable(_)
    ));
}

#[tokio::test]
async fn a_restore_estimate_is_available_before_the_user_confirms() {
    // The one price shown anywhere in the product (§6.5), so it must be computable
    // without issuing the restore.
    let (store, _clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(
            &k,
            Bytes::from(vec![0u8; 1024 * 1024]),
            StorageClass::Glacier,
        )
        .await
        .expect("put");
    assert!(
        store.estimated_restore_cost_cents(&k).unwrap_or(0) > 0,
        "an estimate must be available before confirming"
    );
}

#[tokio::test]
async fn an_available_restore_always_carries_an_expiry() {
    // The database refuses an available placement with no expiry (§6.5). The store must
    // never produce one, or the two layers disagree.
    let (store, clock) = FakeS3Store::with_test_clock();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"x"), StorageClass::Glacier)
        .await
        .expect("put");
    store
        .restore(&k, RestoreTier::Standard, Duration::from_secs(86400))
        .await
        .expect("restore");
    clock.advance_hours(6);

    let head = store.head(&k).await.expect("head");
    assert_eq!(head.restore_state, RestoreState::Available);
    assert!(
        head.restore_expires_at.is_some(),
        "an Available restore with no expiry is unreclaimable state"
    );
}
