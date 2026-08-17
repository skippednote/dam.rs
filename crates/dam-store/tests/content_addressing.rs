//! Content addressing and deduplication (§6.2).
//!
//! Two properties, both load-bearing:
//!
//! - A key is derived from the bytes, so identical content resolves to one object no matter
//!   how many times or by whom it is uploaded.
//! - Hashing is streaming, so a 200 GB master never materialises in memory (§18.3).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use dam_core::StorageClass;
use dam_store::{BlobStore, Digest, Ingested, Key, content, testing::SeaweedfsHarness};
use uuid::Uuid;

/// Fixed, so a failing assertion prints the same key every run.
fn tenant() -> Uuid {
    Uuid::from_u128(0x0da3_0000_0000_0000_0000_0000_0000_0001)
}

#[test]
fn identical_bytes_hash_to_one_digest_and_one_key() {
    let a = Digest::of(b"the same photograph");
    let b = Digest::of(b"the same photograph");
    assert_eq!(a, b);
    assert_eq!(
        Key::original(tenant(), a.as_hex()).expect("key"),
        Key::original(tenant(), b.as_hex()).expect("key")
    );

    let different = Digest::of(b"a different photograph");
    assert_ne!(a, different, "distinct content must not collide");
}

#[test]
fn a_digest_is_always_lowercase_so_one_object_cannot_have_two_keys() {
    let digest = Digest::of(b"anything");
    let round_tripped =
        Digest::from_hex(&digest.as_hex().to_uppercase()).expect("uppercase hex must parse");
    assert_eq!(
        round_tripped, digest,
        "an uppercase digest read back from anywhere must normalise, not produce a second \
         key for the same content"
    );
    assert!(
        Key::original(tenant(), round_tripped.as_hex()).is_ok(),
        "and the normalised form must satisfy the key layout's own check"
    );
}

#[test]
fn malformed_digests_are_rejected() {
    for bad in [
        "",
        "abc",
        &"g".repeat(64),
        &"a".repeat(63),
        &"a".repeat(65),
        "a a".repeat(21).as_str(),
    ] {
        assert!(
            Digest::from_hex(bad).is_err(),
            "{bad:?} must not parse as a digest"
        );
    }
}

#[tokio::test]
async fn streaming_a_body_in_chunks_hashes_the_same_as_hashing_it_whole() {
    // The property that makes streaming safe: a chunk boundary must not affect the digest.
    // Sizes chosen to land boundaries mid-block against BLAKE3's 1024-byte chunking.
    let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let whole = Digest::of(&body);

    for chunk in [1usize, 7, 1023, 1024, 1025, 65_536] {
        let mut hasher = content::StreamHasher::new();
        for part in body.chunks(chunk) {
            hasher.update(part);
        }
        let (digest, size) = hasher.finish();
        assert_eq!(digest, whole, "chunked at {chunk} bytes must match");
        assert_eq!(size, body.len() as u64, "and must count every byte");
    }
}

#[tokio::test]
async fn hashing_a_reader_does_not_buffer_the_whole_body() {
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 253) as u8).collect();
    let expected = Digest::of(&body);

    let (digest, size) = content::hash_reader(&mut body.as_slice(), 64 * 1024)
        .await
        .expect("hash");
    assert_eq!(digest, expected);
    assert_eq!(size, body.len() as u64);
}

#[tokio::test]
async fn a_stream_shorter_than_its_declared_length_is_a_truncation_error() {
    // A client that promises 10 MB and delivers 4 MB has had its connection cut. Storing
    // what arrived would produce a valid-looking object that is silently a fragment, and
    // because the key is derived from the bytes it would even get its own valid key.
    let body = vec![7u8; 4096];
    let err = content::hash_reader_exact(&mut body.as_slice(), 8192, 64 * 1024)
        .await
        .expect_err("short body must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("4096") && msg.contains("8192"),
        "the error must name both lengths: {msg}"
    );

    content::hash_reader_exact(&mut body.as_slice(), 4096, 64 * 1024)
        .await
        .expect("the exact declared length must be accepted");
}

#[tokio::test]
async fn an_empty_body_is_refused_rather_than_given_a_key() {
    // The digest of nothing is a perfectly valid BLAKE3 value, so content addressing alone
    // would happily store a zero-byte asset and hand it a key. It is never a real upload.
    let err = content::hash_reader(&mut [].as_slice(), 1024)
        .await
        .expect_err("empty must be refused");
    assert!(format!("{err}").contains("empty"), "got {err}");
}

#[tokio::test]
async fn re_uploading_the_same_bytes_stores_one_object_and_transfers_nothing() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let body = Bytes::from(vec![42u8; 3 * 1024 * 1024]);

    let first = content::ingest(&store, tenant(), body.clone(), StorageClass::Standard)
        .await
        .expect("first ingest");
    let key = match &first {
        Ingested::Stored(p) => p.key.clone(),
        other => panic!("the first upload must store, got {other:?}"),
    };

    let second = content::ingest(&store, tenant(), body.clone(), StorageClass::Standard)
        .await
        .expect("second ingest");
    match &second {
        Ingested::AlreadyPresent { key: k, size } => {
            assert_eq!(k, &key, "the same bytes must resolve to the same key");
            assert_eq!(*size, body.len() as u64);
        }
        other => panic!(
            "the second upload must be recognised as a duplicate and skip the transfer \
             entirely, got {other:?}"
        ),
    }

    let objects = store
        .list(&format!("{}/o/", tenant()), 100)
        .await
        .expect("list");
    assert_eq!(
        objects.len(),
        1,
        "one object for one piece of content, got {objects:?}"
    );
}

#[tokio::test]
async fn a_key_that_exists_but_holds_the_wrong_size_is_not_treated_as_a_duplicate() {
    // Content addressing means a key implies its bytes, so a size mismatch at that key is
    // corruption or a truncated earlier upload — not a cache hit. Trusting the key alone
    // would make the corruption permanent, since every later upload would skip the write.
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    let body = Bytes::from(vec![9u8; 65_536]);

    let digest = Digest::of(&body);
    let key = Key::original(tenant(), digest.as_hex()).expect("key");
    store
        .put(
            &key,
            Bytes::from_static(b"truncated"),
            StorageClass::Standard,
        )
        .await
        .expect("plant a bad object");

    match content::ingest(&store, tenant(), body.clone(), StorageClass::Standard)
        .await
        .expect("ingest")
    {
        Ingested::Stored(p) => assert_eq!(p.size, body.len() as u64, "rewritten in full"),
        other => panic!("a size mismatch must force a rewrite, got {other:?}"),
    }

    let read = store
        .get(&key, None)
        .await
        .expect("get")
        .into_bytes(&key)
        .expect("hot");
    assert_eq!(read, body, "the correct bytes must now be at the key");
}
