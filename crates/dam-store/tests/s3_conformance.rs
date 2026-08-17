//! `S3Store` against a real SeaweedFS container.
//!
//! Runs the **same** conformance suite as `FakeS3Store`, which is the only thing that
//! stops the two diverging on shared behaviour. Then the object-lock and versioning
//! cases the fake deliberately does not claim — because object lock's whole point is
//! that the *server* refuses the delete, and a fake that refuses proves nothing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use bytes::Bytes;
use dam_core::{RestoreTier, StorageClass};
use dam_store::{BlobStore, Error, GetOutcome, Key, conformance, testing::SeaweedfsHarness};
use std::time::Duration;
use uuid::Uuid;

const H: &str = "9f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

fn key() -> Key {
    Key::original(Uuid::now_v7(), H).expect("key")
}

#[tokio::test]
async fn passes_the_shared_conformance_suite() {
    let sw = SeaweedfsHarness::start().await.expect("start seaweedfs");
    let store = sw.store();
    let report = conformance::run(&store).await;
    // Printed so the skipped cases are visible in CI output. Lost coverage behind a
    // green tick is how a capability gap becomes a production surprise.
    println!("{report}");

    assert!(
        report.passed.len() >= 8,
        "the data plane should have run: {report}"
    );
    // SeaweedFS has no storage classes and no RestoreObject, so those two must skip —
    // and must skip *explicitly*, not silently.
    let skipped: Vec<&str> = report.skipped.iter().map(|(c, _)| c.as_str()).collect();
    assert!(
        skipped.contains(&"storage classes"),
        "SeaweedFS echoes the storage-class header without honouring it, so this must \
         skip rather than appear to pass: {report}"
    );
    assert!(
        skipped.contains(&"restore lifecycle"),
        "SeaweedFS has no RestoreObject: {report}"
    );
}

#[tokio::test]
async fn the_two_drivers_agree_on_the_data_plane() {
    // The differential check. §20.2 claims the fake cannot drift from real S3 on shared
    // behaviour; this is what makes that true rather than aspirational.
    let sw = SeaweedfsHarness::start().await.expect("start");
    let real = sw.store();
    let (fake, _clock) = dam_store::FakeS3Store::with_test_clock();

    let k = key();
    let body = Bytes::from_static(b"identical behaviour on both");

    let a = real
        .put(&k, body.clone(), StorageClass::Standard)
        .await
        .expect("real put");
    let b = fake
        .put(&k, body.clone(), StorageClass::Standard)
        .await
        .expect("fake put");
    assert_eq!(a.size, b.size, "put reported different sizes");
    assert_eq!(a.checksum, b.checksum, "BLAKE3 must match across drivers");

    for (name, store) in [
        ("real", &real as &dyn BlobStore),
        ("fake", &fake as &dyn BlobStore),
    ] {
        let got = store.get(&k, None).await.expect("get");
        assert!(
            matches!(got, GetOutcome::Bytes(ref g) if *g == body),
            "{name} returned the wrong body"
        );

        let head = store.head(&k).await.expect("head");
        assert_eq!(head.size, body.len() as u64, "{name} size");
        assert!(head.is_readable(), "{name} readable");

        // Both must report a missing key the same way, or a caller cannot handle
        // absence uniformly.
        let missing = Key::original(Uuid::now_v7(), H).expect("key");
        assert!(
            matches!(store.get(&missing, None).await, Err(Error::NotFound(_))),
            "{name} should report NotFound for a missing key"
        );

        // Both must treat deleting a missing key as a no-op.
        store
            .delete(&missing)
            .await
            .unwrap_or_else(|e| panic!("{name} delete of a missing key should be a no-op: {e}"));
    }
}

#[tokio::test]
async fn an_unsupported_capability_is_refused_explicitly_not_silently_ignored() {
    // A transition against SeaweedFS must error rather than appear to succeed. Silently
    // ignoring it would let the lifecycle engine record a placement in a class the
    // object is not actually in — and then serve a URL for an object it thinks is warm.
    let sw = SeaweedfsHarness::start().await.expect("start");
    let store = sw.store();
    let k = key();
    store
        .put(&k, Bytes::from_static(b"x"), StorageClass::Standard)
        .await
        .expect("put");

    let err = store
        .transition(&k, StorageClass::GlacierIr)
        .await
        .expect_err("SeaweedFS must refuse a transition rather than pretend");
    assert!(
        matches!(err, Error::Unsupported { .. }),
        "expected Unsupported, got {err:?}"
    );

    let err = store
        .restore(&k, RestoreTier::Bulk, Duration::from_secs(86400))
        .await
        .expect_err("SeaweedFS must refuse a restore");
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
}

#[tokio::test]
async fn the_storage_class_header_is_not_sent_to_a_backend_that_only_echoes_it() {
    // SeaweedFS reports back whatever class you send. If the driver sent it anyway,
    // `head` would claim GLACIER_IR for an object sitting in ordinary storage — the
    // most dangerous kind of wrong, because everything downstream would trust it.
    let sw = SeaweedfsHarness::start().await.expect("start");
    let store = sw.store();
    let k = key();

    let placement = store
        .put(&k, Bytes::from_static(b"x"), StorageClass::GlacierIr)
        .await
        .expect("put");
    assert_eq!(
        placement.storage_class,
        StorageClass::Standard,
        "a backend that does not honour storage classes must report Standard, \
         not the class it was asked for"
    );
    assert_eq!(
        store.head(&k).await.expect("head").storage_class,
        StorageClass::Standard
    );
}

#[tokio::test]
async fn a_presigned_get_url_actually_works() {
    // The suite checks the URL's shape. This checks it functions — a presigned URL that
    // parses but 403s is worse than none, and the Drupal connector's whole
    // render-without-an-API-call design rests on these working.
    let sw = SeaweedfsHarness::start().await.expect("start");
    let store = sw.store();
    let k = key();
    let body = Bytes::from_static(b"fetched through a presigned url");
    store
        .put(&k, body.clone(), StorageClass::Standard)
        .await
        .expect("put");

    let url = store
        .presign_get(&k, Duration::from_secs(300))
        .await
        .expect("presign");

    let fetched = reqwest::get(&url).await.expect("fetch presigned url");
    assert!(
        fetched.status().is_success(),
        "presigned GET returned {}: {url}",
        fetched.status()
    );
    assert_eq!(fetched.bytes().await.expect("body"), body);
}

#[tokio::test]
async fn a_presigned_put_url_actually_uploads() {
    let sw = SeaweedfsHarness::start().await.expect("start");
    let store = sw.store();
    let k = key();

    let url = store
        .presign_put(&k, Duration::from_secs(300))
        .await
        .expect("presign put");

    let body = Bytes::from_static(b"uploaded straight to storage");
    let put = reqwest::Client::new()
        .put(&url)
        .body(body.clone())
        .send()
        .await
        .expect("presigned put");
    assert!(
        put.status().is_success(),
        "presigned PUT returned {}",
        put.status()
    );

    // The direct-to-storage upload path (§G21) depends on this: the browser uploads
    // without the bytes passing through damd at all.
    let got = store.get(&k, None).await.expect("get after presigned put");
    assert!(matches!(got, GetOutcome::Bytes(ref b) if *b == body));
}

#[tokio::test]
async fn a_5mib_body_survives_a_round_trip_through_a_real_server() {
    // The fake holds bytes in memory, so only a real server exercises chunked transfer
    // and the SDK's body handling.
    let sw = SeaweedfsHarness::start().await.expect("start");
    let store = sw.store();
    let k = key();
    let body = Bytes::from(vec![0x5Au8; 5 * 1024 * 1024]);

    store
        .put(&k, body.clone(), StorageClass::Standard)
        .await
        .expect("put 5MiB");
    let got = store.get(&k, None).await.expect("get 5MiB");
    match got {
        GetOutcome::Bytes(b) => {
            assert_eq!(b.len(), body.len());
            assert_eq!(
                blake3::hash(&b),
                blake3::hash(&body),
                "5MiB round-trip corrupted the body"
            );
        }
        GetOutcome::NotAvailable(_) => panic!("unexpected cold object"),
    }
}
