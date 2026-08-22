//! Promoting a streamed upload to its content-addressed key (§6.2, §18.3).
//!
//! A streaming upload cannot know its key before it has read the bytes — the key *is* the
//! digest — so it lands at a staging key and is promoted by a server-side copy once the
//! digest is known. The alternative, buffering to hash first, is exactly what a 200 GB
//! master rules out.
//!
//! The copy is server-side, so promotion never moves bytes over the client's connection
//! twice. Above S3's 5 GiB `CopyObject` limit it has to become a multipart copy, which is
//! the constraint that makes this more than a rename.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use dam_core::StorageClass;
use dam_store::{BlobStore, Digest, Ingested, Key, content, testing::SeaweedfsHarness};
use uuid::Uuid;

fn tenant() -> Uuid {
    Uuid::from_u128(0x0da3_0000_0000_0000_0000_0000_0000_0002)
}

#[test]
fn a_staging_key_is_namespaced_and_never_tiers() {
    let key = Key::staging(tenant(), "01J8Z9QX4E7N2VYB3K6M5T8W1R").expect("key");
    assert!(
        key.as_str().contains("/staging/"),
        "a reaper finds abandoned uploads by prefix, so the namespace must be distinct: {key}"
    );
    assert!(
        key.is_tier_exempt(),
        "a staging object lives for minutes; tiering it would start a minimum-duration \
         charge on something about to be deleted"
    );
}

#[tokio::test]
async fn a_streamed_upload_is_promoted_to_its_content_key_and_the_staging_object_is_removed() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();

    let body = Bytes::from(vec![3u8; 512 * 1024]);
    let staging = Key::staging(tenant(), "upload-one").expect("key");
    store
        .put(&staging, body.clone(), StorageClass::Standard)
        .await
        .expect("stage");

    // The digest is what the ingest path computed while streaming the bytes through.
    let digest = Digest::of(&body);
    let promoted = content::promote(
        &store,
        tenant(),
        &staging,
        &digest,
        body.len() as u64,
        StorageClass::Standard,
    )
    .await
    .expect("promote");

    let key = match &promoted {
        Ingested::Stored(p) => p.key.clone(),
        other => panic!("expected Stored, got {other:?}"),
    };
    assert_eq!(key, digest.original_key(tenant()).expect("content key"));
    assert_eq!(
        store
            .get(&key, None)
            .await
            .expect("get")
            .into_bytes(&key)
            .expect("hot"),
        body,
        "every byte must survive the server-side copy"
    );
    assert!(
        store.head(&staging).await.is_err(),
        "the staging object must be removed once the content object exists"
    );
}

#[tokio::test]
async fn promoting_content_that_already_exists_skips_the_copy_entirely() {
    // The saving that matters: a duplicate 200 GB upload should not pay for a 200 GB
    // server-side copy just to discover the object was already there.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();

    let body = Bytes::from(vec![11u8; 256 * 1024]);
    let digest = Digest::of(&body);
    let content_key = digest.original_key(tenant()).expect("key");
    store
        .put(&content_key, body.clone(), StorageClass::Standard)
        .await
        .expect("pre-existing content");

    let staging = Key::staging(tenant(), "upload-two").expect("key");
    store
        .put(&staging, body.clone(), StorageClass::Standard)
        .await
        .expect("stage");

    match content::promote(
        &store,
        tenant(),
        &staging,
        &digest,
        body.len() as u64,
        StorageClass::Standard,
    )
    .await
    .expect("promote")
    {
        Ingested::AlreadyPresent { key, size } => {
            assert_eq!(key, content_key);
            assert_eq!(size, body.len() as u64);
        }
        other => panic!("expected AlreadyPresent, got {other:?}"),
    }
    assert!(
        store.head(&staging).await.is_err(),
        "the staging object is still cleaned up — it is redundant either way"
    );
}

#[tokio::test]
async fn a_staging_object_of_the_wrong_size_is_refused_and_left_in_place() {
    // Staging is the only copy of the bytes at this point. If promotion fails, deleting it
    // would destroy an upload that could otherwise be retried or inspected — so the reaper
    // cleans up on a timer instead, and a failed promotion leaves evidence.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();

    let body = Bytes::from(vec![5u8; 4096]);
    let staging = Key::staging(tenant(), "upload-three").expect("key");
    store
        .put(&staging, body.clone(), StorageClass::Standard)
        .await
        .expect("stage");

    let err = content::promote(
        &store,
        tenant(),
        &staging,
        &Digest::of(&body),
        8192, // the length the client declared
        StorageClass::Standard,
    )
    .await
    .expect_err("a size mismatch must refuse");
    let msg = format!("{err}");
    assert!(
        msg.contains("4096") && msg.contains("8192"),
        "the error must name both lengths: {msg}"
    );
    assert!(
        store.head(&staging).await.is_ok(),
        "the staged bytes must survive a failed promotion"
    );
}

#[tokio::test]
async fn a_missing_staging_object_is_a_clear_error_not_a_silent_success() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let staging = Key::staging(tenant(), "never-uploaded").expect("key");

    let err = content::promote(
        &store,
        tenant(),
        &staging,
        &Digest::of(b"whatever"),
        8,
        StorageClass::Standard,
    )
    .await
    .expect_err("nothing to promote");
    assert!(
        matches!(err, dam_store::Error::NotFound(_)),
        "expected NotFound, got {err:?}"
    );
}

#[test]
fn a_copy_above_five_gibibytes_is_planned_as_ranged_parts() {
    // S3 rejects CopyObject above 5 GiB, so a large promotion must become a multipart copy.
    // The plan is unit-tested because exercising it needs a >5 GiB object: that path runs
    // against AWS in the nightly, never in a container.
    const GIB: u64 = 1024 * 1024 * 1024;

    assert!(
        content::copy_part_ranges(5 * GIB, content::MAX_COPY_PART).is_empty(),
        "at or below the limit a single CopyObject is used, so there are no part ranges"
    );

    let parts = content::copy_part_ranges(5 * GIB + 1, content::MAX_COPY_PART);
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0],
        (0, 5 * GIB - 1),
        "ranges are inclusive, as S3's are"
    );
    assert_eq!(parts[1], (5 * GIB, 5 * GIB), "the tail is a single byte");

    // 200 GB — the §18.3 file. Contiguous, no gaps, no overlaps, and every byte covered.
    let size = 200 * GIB;
    let parts = content::copy_part_ranges(size, content::MAX_COPY_PART);
    assert_eq!(parts.len(), 40);
    assert_eq!(parts[0].0, 0);
    assert_eq!(parts[parts.len() - 1].1, size - 1);
    for pair in parts.windows(2) {
        assert_eq!(
            pair[1].0,
            pair[0].1 + 1,
            "a gap or overlap here silently corrupts the copy: {pair:?}"
        );
    }
    assert!(
        parts.len() <= 10_000,
        "S3 caps a multipart upload at 10,000 parts"
    );
}

#[test]
fn a_part_size_that_would_exceed_the_ten_thousand_part_cap_is_grown_to_fit() {
    // A caller passing a small part size for a huge object would otherwise produce a plan
    // S3 rejects at completion — after every part copy has been paid for.
    const TIB: u64 = 1024 * 1024 * 1024 * 1024;
    let parts = content::copy_part_ranges(50 * TIB, 5 * 1024 * 1024);
    assert!(
        parts.len() <= 10_000,
        "expected the part size to grow, got {} parts",
        parts.len()
    );
    assert_eq!(parts[parts.len() - 1].1, 50 * TIB - 1, "still complete");
}
