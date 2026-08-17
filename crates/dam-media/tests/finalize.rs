//! Finalising a staged upload: sniff, hash, then promote (task 1.6).
//!
//! This is the only validation point that works for **every** upload path, and the presigned
//! one is why it has to exist. A presigned `PUT` hands the client a URL and gets out of the
//! way: it cannot cap the size, cannot constrain the type, and cannot see the bytes. So
//! nothing about an upload may be trusted at mint time — the checks happen here, after the
//! bytes have landed at a staging key and before they are promoted to a content-addressed
//! one.
//!
//! Reading the object back to hash it is not waste: §18.3 budgets the original being read
//! exactly twice, once to hash and once to derive. The read is ranged, so a 200 GB master
//! never materialises.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use dam_core::StorageClass;
use dam_media::ingest::{self, Policy};
use dam_store::{BlobStore, Digest, Ingested, Key, testing::SeaweedfsHarness};
use uuid::Uuid;

fn tenant() -> Uuid {
    Uuid::from_u128(0x0da3_0000_0000_0000_0000_0000_0000_0004)
}

/// A JPEG header followed by filler, so the object is big enough to exercise ranged reads.
fn jpeg(len: usize) -> Bytes {
    let mut v = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1,
    ];
    v.extend((0..len.saturating_sub(v.len())).map(|i| (i % 251) as u8));
    Bytes::from(v)
}

async fn stage(store: &dam_store::S3Store, id: &str, body: Bytes) -> Key {
    let key = Key::staging(tenant(), id).expect("key");
    store
        .put(&key, body, StorageClass::Standard)
        .await
        .expect("stage");
    key
}

#[tokio::test]
async fn a_staged_upload_is_sniffed_hashed_and_promoted() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let body = jpeg(3 * 1024 * 1024);
    let staging = stage(&store, "finalize-happy", body.clone()).await;

    let out = ingest::finalize(
        &store,
        tenant(),
        &staging,
        // What the client claimed. Both wrong, and neither is used.
        Some("image/png"),
        Some(body.len() as u64),
        StorageClass::Standard,
        Policy::default(),
    )
    .await
    .expect("finalize");

    assert_eq!(out.sniffed.mime, "image/jpeg");
    assert_eq!(
        out.sniffed.declared_mismatch.as_deref(),
        Some("image/png"),
        "the client's wrong declaration is recorded, not adopted"
    );
    assert_eq!(out.digest, Digest::of(&body), "hashed over every byte");
    assert_eq!(out.size, body.len() as u64);
    assert_eq!(
        out.ingested.key(),
        &out.digest.original_key(tenant()).expect("key"),
        "promoted to its content-addressed key"
    );
    assert!(
        store.head(&staging).await.is_err(),
        "and the staging object is gone"
    );
}

#[tokio::test]
async fn an_executable_is_refused_and_its_staged_bytes_are_destroyed() {
    // A presigned PUT cannot stop this being uploaded, so the refusal has to be here. Leaving
    // the bytes for the reaper would keep malware retrievable at a known key for as long as
    // the reaper's window.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let elf = Bytes::from(vec![0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0]);
    let staging = stage(&store, "finalize-elf", elf).await;

    let err = ingest::finalize(
        &store,
        tenant(),
        &staging,
        Some("image/jpeg"),
        None,
        StorageClass::Standard,
        Policy::default(),
    )
    .await
    .expect_err("an executable must be refused");
    assert!(
        format!("{err}").contains("executable"),
        "the error must name why: {err}"
    );
    assert!(
        store.head(&staging).await.is_err(),
        "the staged bytes must not survive a refusal"
    );
}

#[tokio::test]
async fn a_tenant_that_needs_executables_can_allow_them() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let elf = Bytes::from(vec![0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0]);
    let staging = stage(&store, "finalize-elf-allowed", elf).await;

    let out = ingest::finalize(
        &store,
        tenant(),
        &staging,
        None,
        None,
        StorageClass::Standard,
        Policy {
            refuse_executables: false,
            ..Policy::default()
        },
    )
    .await
    .expect("allowed by policy");
    assert!(
        out.sniffed.is_dangerous(),
        "still flagged, just not refused"
    );
}

#[tokio::test]
async fn an_svg_is_accepted_and_flagged_rather_than_refused() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let svg = Bytes::from_static(
        br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#,
    );
    let staging = stage(&store, "finalize-svg", svg).await;

    let out = ingest::finalize(
        &store,
        tenant(),
        &staging,
        Some("image/svg+xml"),
        None,
        StorageClass::Standard,
        Policy::default(),
    )
    .await
    .expect("an SVG is a real asset");
    assert_eq!(out.sniffed.mime, "image/svg+xml");
    assert!(
        out.sniffed.carries_active_content(),
        "the delivery path must be told never to serve this inline unsanitised"
    );
}

#[tokio::test]
async fn a_size_that_disagrees_with_the_declaration_is_refused() {
    // On the presigned path the declared length is the only cross-check available: the client
    // asked for a URL for N bytes and the object at the key is a different size.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let body = jpeg(4096);
    let staging = stage(&store, "finalize-size", body.clone()).await;

    let err = ingest::finalize(
        &store,
        tenant(),
        &staging,
        None,
        Some(99_999),
        StorageClass::Standard,
        Policy::default(),
    )
    .await
    .expect_err("a size mismatch must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("4096") && msg.contains("99999"),
        "the error must name both sizes: {msg}"
    );
}

#[tokio::test]
async fn an_upload_over_the_size_limit_is_refused_without_being_read() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let body = jpeg(200_000);
    let staging = stage(&store, "finalize-too-big", body).await;

    let err = ingest::finalize(
        &store,
        tenant(),
        &staging,
        None,
        None,
        StorageClass::Standard,
        Policy {
            max_bytes: Some(100_000),
            ..Policy::default()
        },
    )
    .await
    .expect_err("over the limit");
    assert!(format!("{err}").contains("100000"), "got {err}");
}

#[tokio::test]
async fn finalising_duplicate_content_reports_it_and_transfers_nothing() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let body = jpeg(64 * 1024);

    let first = stage(&store, "finalize-dup-1", body.clone()).await;
    ingest::finalize(
        &store,
        tenant(),
        &first,
        None,
        None,
        StorageClass::Standard,
        Policy::default(),
    )
    .await
    .expect("first");

    let second = stage(&store, "finalize-dup-2", body.clone()).await;
    let out = ingest::finalize(
        &store,
        tenant(),
        &second,
        None,
        None,
        StorageClass::Standard,
        Policy::default(),
    )
    .await
    .expect("second");
    assert!(
        matches!(out.ingested, Ingested::AlreadyPresent { .. }),
        "the second upload of identical bytes must skip the copy: {:?}",
        out.ingested
    );
}

#[tokio::test]
async fn a_small_read_window_still_produces_the_digest_of_the_whole_object() {
    // A 12 MiB object read through a 1 MiB window: this proves the ranged reads stitch in
    // order and cover every byte. It does **not** prove memory is bounded — that is structural
    // (the loop holds one window at a time) and not observable from out here, so the test name
    // says what it actually checks.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let body = jpeg(12 * 1024 * 1024);
    let staging = stage(&store, "finalize-chunked", body.clone()).await;

    let out = ingest::finalize(
        &store,
        tenant(),
        &staging,
        None,
        Some(body.len() as u64),
        StorageClass::Standard,
        Policy {
            read_chunk_bytes: 1024 * 1024,
            ..Policy::default()
        },
    )
    .await
    .expect("finalize");
    assert_eq!(out.digest, Digest::of(&body));
    assert_eq!(out.size, body.len() as u64);
}

#[tokio::test]
async fn finalising_a_missing_staging_object_is_a_clear_error() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let staging = Key::staging(tenant(), "never-arrived").expect("key");

    let err = ingest::finalize(
        &store,
        tenant(),
        &staging,
        None,
        None,
        StorageClass::Standard,
        Policy::default(),
    )
    .await
    .expect_err("nothing to finalise");
    assert!(format!("{err}").contains("not found"), "got {err}");
}
