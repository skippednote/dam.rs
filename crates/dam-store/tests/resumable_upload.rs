//! The resumable-upload engine behind TUS (task 1.6).
//!
//! The constraint that shapes everything here: **a TUS client may PATCH chunks of any size**
//! — 64 KB is common — while **S3 refuses a multipart part below 5 MiB** except the last one.
//! So chunks cannot map one-to-one onto parts. They accumulate into a *tail*, and the tail
//! becomes a part once it is large enough.
//!
//! The tail lives in object storage rather than in the process, which is the difference
//! between a resumable upload and a sticky-session one: a client that reconnects to a
//! different node must be able to continue, and a node that dies must not take the upload
//! with it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use dam_core::StorageClass;
use dam_store::{
    BlobStore, Digest, Key,
    resumable::{self, PatchOutcome, ResumableSession, SessionStatus},
    testing::SeaweedfsHarness,
};
use uuid::Uuid;

const MIN_PART: usize = dam_store::MIN_PART_SIZE;

fn tenant() -> Uuid {
    Uuid::from_u128(0x0da3_0000_0000_0000_0000_0000_0000_0003)
}

/// Deterministic bytes, so a mis-ordered assembly is visible rather than plausible.
fn payload(len: usize) -> Bytes {
    Bytes::from((0..len).map(|i| (i % 251) as u8).collect::<Vec<u8>>())
}

fn session(id: &str, declared: Option<u64>) -> ResumableSession {
    ResumableSession::new(id.to_owned(), tenant(), declared)
}

#[tokio::test]
async fn a_chunk_below_the_part_minimum_is_held_as_a_tail_and_never_uploaded_as_a_part() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("small-chunks", Some(3000));

    let outcome = resumable::patch(&store, &mut s, 0, payload(1000), StorageClass::Standard)
        .await
        .expect("patch");
    assert_eq!(outcome, PatchOutcome::Accepted { new_offset: 1000 });
    assert_eq!(s.offset, 1000);
    assert_eq!(
        s.parts.len(),
        0,
        "a 1 KB part would be rejected by S3 at completion, after every byte had been sent"
    );
    assert!(
        s.s3_upload_id.is_none(),
        "no multipart upload is created until there is a legal part to put in it"
    );
    assert_eq!(s.tail_len, 1000);
}

#[tokio::test]
async fn the_tail_survives_in_object_storage_so_another_node_can_continue() {
    // The point of the design: nothing about the in-flight upload lives in this process. The
    // session below is reconstructed from what a database row would hold, and the upload
    // continues.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let first = payload(2048);
    let second = payload(4096).slice(0..4096);

    let mut node_a = session("hand-off", None);
    resumable::patch(
        &store,
        &mut node_a,
        0,
        first.clone(),
        StorageClass::Standard,
    )
    .await
    .expect("patch on node A");

    // Everything node B knows, it knows from the persisted session — no shared memory.
    let mut node_b = node_a.clone();
    drop(node_a);
    resumable::patch(
        &store,
        &mut node_b,
        first.len() as u64,
        second.clone(),
        StorageClass::Standard,
    )
    .await
    .expect("patch on node B");

    let key = resumable::complete(&store, &mut node_b, StorageClass::Standard)
        .await
        .expect("complete");
    let assembled = store
        .get(&key, None)
        .await
        .expect("get")
        .into_bytes(&key)
        .expect("hot");
    let mut expected = first.to_vec();
    expected.extend_from_slice(&second);
    assert_eq!(assembled, Bytes::from(expected));
}

#[tokio::test]
async fn chunks_accumulate_until_the_minimum_and_then_become_one_part() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("accumulate", None);

    // Three chunks of 2 MiB: the third takes the tail over 5 MiB.
    let chunk = payload(2 * 1024 * 1024);
    for i in 0..3u64 {
        resumable::patch(
            &store,
            &mut s,
            i * chunk.len() as u64,
            chunk.clone(),
            StorageClass::Standard,
        )
        .await
        .expect("patch");
    }
    assert_eq!(s.parts.len(), 1, "one part, not three: {:?}", s.parts);
    assert!(s.s3_upload_id.is_some());
    assert_eq!(
        s.tail_len, 0,
        "the whole buffer went into the part, so nothing is held back"
    );
    assert_eq!(s.offset, 6 * 1024 * 1024);
}

#[tokio::test]
async fn a_patch_at_the_wrong_offset_is_refused_and_reports_the_expected_one() {
    // TUS answers a mismatch with 409 and the authoritative offset, which is how a client
    // that lost its connection mid-chunk recovers.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("offset-conflict", None);
    resumable::patch(&store, &mut s, 0, payload(500), StorageClass::Standard)
        .await
        .expect("patch");

    for wrong in [0u64, 499, 501, 10_000] {
        let outcome = resumable::patch(&store, &mut s, wrong, payload(10), StorageClass::Standard)
            .await
            .expect("a conflict is an outcome, not an error");
        assert_eq!(
            outcome,
            PatchOutcome::OffsetConflict {
                expected: 500,
                got: wrong
            },
            "offset {wrong} must be refused"
        );
    }
    assert_eq!(s.offset, 500, "and nothing may be appended");
}

#[tokio::test]
async fn a_replayed_chunk_does_not_duplicate_bytes() {
    // A client that retries a chunk whose response was lost replays at the *old* offset. If
    // that were accepted the object would silently contain the chunk twice, and the digest
    // would not match anything the client can compute.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("replay", Some(300));
    let chunk = payload(300);

    resumable::patch(&store, &mut s, 0, chunk.clone(), StorageClass::Standard)
        .await
        .expect("first");
    let replay = resumable::patch(&store, &mut s, 0, chunk.clone(), StorageClass::Standard)
        .await
        .expect("replay is a conflict, not an error");
    assert!(matches!(replay, PatchOutcome::OffsetConflict { .. }));

    let key = resumable::complete(&store, &mut s, StorageClass::Standard)
        .await
        .expect("complete");
    let got = store
        .get(&key, None)
        .await
        .expect("get")
        .into_bytes(&key)
        .expect("hot");
    assert_eq!(got.len(), 300, "exactly one copy of the chunk");
    assert_eq!(Digest::of(&got), Digest::of(&chunk));
}

#[tokio::test]
async fn an_upload_of_many_odd_sized_chunks_assembles_byte_for_byte() {
    // The integration case: sizes chosen to straddle the 5 MiB boundary awkwardly, so an
    // off-by-one in the tail accounting shows up as a digest mismatch rather than as nothing.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();

    let sizes = [
        1,
        MIN_PART - 1,
        1,
        MIN_PART + 7,
        3,
        2 * MIN_PART,
        MIN_PART / 3,
    ];
    let total: usize = sizes.iter().sum();
    let whole = payload(total);

    let mut s = session("odd-sizes", Some(total as u64));
    let mut offset = 0usize;
    for size in sizes {
        let chunk = whole.slice(offset..offset + size);
        let outcome =
            resumable::patch(&store, &mut s, offset as u64, chunk, StorageClass::Standard)
                .await
                .expect("patch");
        assert_eq!(
            outcome,
            PatchOutcome::Accepted {
                new_offset: (offset + size) as u64
            }
        );
        offset += size;
    }

    let key = resumable::complete(&store, &mut s, StorageClass::Standard)
        .await
        .expect("complete");
    let assembled = store
        .get(&key, None)
        .await
        .expect("get")
        .into_bytes(&key)
        .expect("hot");
    assert_eq!(assembled.len(), total);
    assert_eq!(
        Digest::of(&assembled),
        Digest::of(&whole),
        "the assembled object must be byte-identical to what the client sent"
    );
    assert_eq!(s.status, SessionStatus::Completed);
}

#[tokio::test]
async fn exceeding_the_declared_length_is_refused_before_any_bytes_are_written() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("too-long", Some(100));

    let err = resumable::patch(&store, &mut s, 0, payload(101), StorageClass::Standard)
        .await
        .expect_err("over-long must be refused");
    assert!(
        format!("{err}").contains("100"),
        "the error must name the declared length: {err}"
    );
    assert_eq!(s.offset, 0, "and nothing may be written");
    assert_eq!(s.tail_len, 0);
}

#[tokio::test]
async fn completing_short_of_the_declared_length_is_refused() {
    // Without this, a client that stopped early would produce an object that looks complete,
    // and content addressing would give the fragment its own perfectly valid key.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("short", Some(1000));
    resumable::patch(&store, &mut s, 0, payload(400), StorageClass::Standard)
        .await
        .expect("patch");

    let err = resumable::complete(&store, &mut s, StorageClass::Standard)
        .await
        .expect_err("incomplete must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("400") && msg.contains("1000"),
        "the error must name both lengths: {msg}"
    );
    assert_eq!(
        s.status,
        SessionStatus::Active,
        "the session stays resumable — the client may still send the rest"
    );
}

#[tokio::test]
async fn a_small_upload_completes_without_ever_opening_a_multipart_upload() {
    // A 12-byte asset should cost one PUT, not a create-upload / upload-part / complete
    // round trip. The engine has to notice.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("tiny", Some(12));
    resumable::patch(
        &store,
        &mut s,
        0,
        Bytes::from_static(b"twelve bytes"),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    assert!(s.s3_upload_id.is_none());

    let key = resumable::complete(&store, &mut s, StorageClass::Standard)
        .await
        .expect("complete");
    assert_eq!(
        store
            .get(&key, None)
            .await
            .expect("get")
            .into_bytes(&key)
            .expect("hot"),
        Bytes::from_static(b"twelve bytes")
    );
}

#[tokio::test]
async fn terminating_a_session_leaves_nothing_behind() {
    // An abandoned upload that keeps its parts is billed for them indefinitely. TUS's
    // termination extension exists so a client can say it is done wasting the space.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let mut s = session("terminate", None);

    // Enough to force a real multipart upload plus a leftover tail.
    resumable::patch(
        &store,
        &mut s,
        0,
        payload(MIN_PART + 1024),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    resumable::patch(
        &store,
        &mut s,
        (MIN_PART + 1024) as u64,
        payload(64),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    assert!(s.s3_upload_id.is_some() && s.tail_len > 0, "precondition");

    resumable::terminate(&store, &mut s)
        .await
        .expect("terminate");
    assert_eq!(s.status, SessionStatus::Terminated);
    assert!(
        store
            .list(&format!("{}/staging/", tenant()), 100)
            .await
            .expect("list")
            .iter()
            .all(|k| !k.as_str().contains("terminate")),
        "no staging object, and no tail, may survive termination"
    );
}

#[tokio::test]
async fn a_completed_or_terminated_session_refuses_further_writes() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();

    let mut done = session("closed", Some(4));
    resumable::patch(
        &store,
        &mut done,
        0,
        Bytes::from_static(b"abcd"),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    resumable::complete(&store, &mut done, StorageClass::Standard)
        .await
        .expect("complete");

    assert!(
        resumable::patch(
            &store,
            &mut done,
            4,
            Bytes::from_static(b"e"),
            StorageClass::Standard
        )
        .await
        .is_err(),
        "appending to a completed upload would modify an object already handed to the \
         caller under a content-addressed key"
    );
    assert!(
        resumable::complete(&store, &mut done, StorageClass::Standard)
            .await
            .is_err(),
        "and completing twice must not re-assemble"
    );
}

#[test]
fn a_session_id_is_validated_the_way_a_staging_key_is() {
    // The id becomes part of an object key, so it is ours and constrained. A client-supplied
    // id containing a slash would let a caller write outside its own prefix.
    assert!(
        ResumableSession::new("../escape".into(), tenant(), None)
            .staging_key()
            .is_err()
    );
    assert!(Key::staging(tenant(), "ok-id_1").is_ok());
}
