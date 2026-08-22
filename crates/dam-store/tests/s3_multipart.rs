//! Multipart upload against a real server (§18.3).
//!
//! A 200 GB ProRes master cannot be buffered, so the upload path is multipart. The cases
//! here are the ones that only show up on the wire: part ordering, the 5 MiB minimum S3
//! enforces at *complete* time rather than at upload time, and the `-N` ETag suffix that
//! makes a multipart ETag useless as a checksum — which is why §6.4 stores BLAKE3
//! separately instead of trusting the ETag.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::{Bytes, BytesMut};
use dam_core::StorageClass;
use dam_store::{
    BlobStore, ByteRange, MIN_PART_SIZE,
    testing::{SeaweedfsHarness, unique_key},
};

/// A part of `len` bytes whose contents identify it, so an out-of-order assembly is
/// visible in the reassembled body rather than hidden behind equal-looking zeroes.
fn part(marker: u8, len: usize) -> Bytes {
    let mut buf = BytesMut::with_capacity(len);
    buf.resize(len, marker);
    buf.freeze()
}

#[tokio::test]
async fn a_multipart_upload_assembles_its_parts_in_order() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let key = unique_key("prores-master");

    let a = part(b'a', MIN_PART_SIZE);
    let b = part(b'b', MIN_PART_SIZE);
    let tail = part(b'c', 4096); // only the final part may be under the minimum

    let mut upload = store
        .begin_multipart(&key, StorageClass::Standard)
        .await
        .expect("begin");
    upload.upload_part(a.clone()).await.expect("part 1");
    upload.upload_part(b.clone()).await.expect("part 2");
    upload.upload_part(tail.clone()).await.expect("part 3");
    let placement = upload.finish().await.expect("complete");

    let expected_len = a.len() + b.len() + tail.len();
    assert_eq!(placement.size, expected_len as u64);

    let body = store
        .get(&key, None)
        .await
        .expect("get")
        .into_bytes(&key)
        .expect("hot");
    assert_eq!(body.len(), expected_len, "reassembled length");
    assert_eq!(&body[..MIN_PART_SIZE], &a[..], "part 1 first");
    assert_eq!(
        &body[MIN_PART_SIZE..MIN_PART_SIZE * 2],
        &b[..],
        "part 2 second"
    );
    assert_eq!(&body[MIN_PART_SIZE * 2..], &tail[..], "tail last");

    // The seam is where an off-by-one in part accounting shows up, and a ranged read is
    // how the media probe touches a large file — so read across the boundary explicitly.
    let seam = store
        .get(
            &key,
            Some(ByteRange::new(
                (MIN_PART_SIZE - 2) as u64,
                Some((MIN_PART_SIZE + 1) as u64),
            )),
        )
        .await
        .expect("ranged get")
        .into_bytes(&key)
        .expect("hot");
    assert_eq!(
        &seam[..],
        b"aabb",
        "the part boundary must be seamless to a ranged read"
    );
}

#[tokio::test]
async fn a_multipart_etag_is_marked_as_composite_so_it_is_never_used_as_a_checksum() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let key = unique_key("composite-etag");

    let mut upload = store
        .begin_multipart(&key, StorageClass::Standard)
        .await
        .expect("begin");
    upload
        .upload_part(part(b'x', MIN_PART_SIZE))
        .await
        .expect("part 1");
    upload.upload_part(part(b'y', 512)).await.expect("part 2");
    let placement = upload.finish().await.expect("complete");

    let etag = placement.etag.expect("etag");
    assert!(
        etag.trim_matches('"').contains('-'),
        "a multipart ETag carries a `-<partcount>` suffix and is NOT the MD5 of the \
         object; treating it as a checksum silently corrupts the scrub. Got {etag}"
    );
    assert!(
        placement.checksum.is_none(),
        "the driver must not invent a whole-object checksum it did not compute — \
         the streaming BLAKE3 in the upload path is the checksum of record (§6.4)"
    );
}

#[tokio::test]
async fn a_non_final_part_below_the_minimum_is_refused_before_it_is_uploaded() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let key = unique_key("undersized");

    let mut upload = store
        .begin_multipart(&key, StorageClass::Standard)
        .await
        .expect("begin");
    // Accepted: it might be the last part.
    upload.upload_part(part(b'a', 1024)).await.expect("part 1");
    // Now it cannot be. S3 only reports this at CompleteMultipartUpload — by which point
    // every byte has already been paid for and uploaded — so refuse it here instead.
    let err = upload
        .upload_part(part(b'b', 1024))
        .await
        .expect_err("a second undersized part must be refused");
    assert!(
        format!("{err}").contains("5 MiB") || format!("{err}").contains(&MIN_PART_SIZE.to_string()),
        "the error must name the minimum so the caller can fix its part sizing: {err}"
    );

    upload.abort().await.expect("abort");
}

#[tokio::test]
async fn an_aborted_upload_leaves_no_object_behind() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let key = unique_key("aborted");

    let mut upload = store
        .begin_multipart(&key, StorageClass::Standard)
        .await
        .expect("begin");
    upload
        .upload_part(part(b'z', MIN_PART_SIZE))
        .await
        .expect("part 1");
    upload.abort().await.expect("abort");

    assert!(
        store.head(&key).await.is_err(),
        "an aborted upload must not materialise a partial object — a half-written \
         original that reads as present is worse than no original at all"
    );
    assert!(
        store.list(key.as_str(), 10).await.expect("list").is_empty(),
        "nor appear in a listing"
    );
}

#[tokio::test]
async fn an_upload_with_no_parts_is_refused_rather_than_creating_an_empty_object() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let key = unique_key("no-parts");

    let upload = store
        .begin_multipart(&key, StorageClass::Standard)
        .await
        .expect("begin");
    let err = upload.finish().await.expect_err("completing with no parts");
    assert!(
        format!("{err}").contains("no parts"),
        "expected a driver-side refusal naming the cause, got {err}"
    );
    assert!(
        store.head(&key).await.is_err(),
        "and nothing must be created"
    );
}
