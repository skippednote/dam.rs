//! One suite, every driver.
//!
//! `S3Store` and `FakeS3Store` must not diverge on anything they share, and the only
//! way to guarantee that is to run identical assertions against both. Living in the
//! library rather than a test file is deliberate: a `#[cfg(test)]` suite cannot be run
//! against a driver from another crate, and the AWS nightly needs to run it too.
//!
//! ## Skips are reported, never swallowed
//!
//! Drivers differ — SeaweedFS has no `RestoreObject`, the fake has no real network. The
//! suite consults [`Capabilities`] and returns a [`Report`] listing what it skipped and
//! why. A silent skip is how a capability gap becomes a production surprise: the suite
//! stays green while covering less and less.
//!
//! The inverse is also enforced. A driver that *claims* a capability must implement it —
//! over-claiming fails, under-claiming shows up in the report as lost coverage.

// This module is test-support code that ships in the library on purpose — a
// `#[cfg(test)]` suite cannot be run against a driver from another crate, and the AWS
// nightly needs it too. So the workspace's panic lints are relaxed here for the same
// reason they are relaxed in `tests/`: a panic carrying the failed assertion IS the
// useful outcome. Nothing outside this module gets the same allowance.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use crate::{BlobStore, ByteRange, Capabilities, Error, GetOutcome, Key};
use bytes::Bytes;
use dam_core::{RestoreState, RestoreTier, StorageClass};
use std::{fmt, time::Duration};
use uuid::Uuid;

/// What ran, what did not, and why.
#[derive(Debug, Default)]
pub struct Report {
    pub driver: String,
    pub passed: Vec<String>,
    /// `(case, reason)`. Printed by the caller so lost coverage is visible in CI logs
    /// rather than invisible in a green tick.
    pub skipped: Vec<(String, String)>,
}

impl Report {
    fn pass(&mut self, case: &str) {
        self.passed.push(case.to_owned());
    }

    fn skip(&mut self, case: &str, reason: &str) {
        self.skipped.push((case.to_owned(), reason.to_owned()));
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{}: {} passed, {} skipped",
            self.driver,
            self.passed.len(),
            self.skipped.len()
        )?;
        for (case, reason) in &self.skipped {
            writeln!(f, "  skipped {case}: {reason}")?;
        }
        Ok(())
    }
}

/// A distinct key namespace per run, so two suites against the same bucket cannot
/// collide.
fn ns() -> Uuid {
    Uuid::now_v7()
}

fn digest(seed: u8) -> String {
    // A valid-shaped BLAKE3 digest that varies per case. Not a real hash — the suite is
    // testing the store, not the hasher.
    format!("{:02x}", seed).repeat(32)
}

/// Runs everything the driver claims to support.
///
/// # Panics
///
/// On any conformance failure. This is a test helper; a panic with the failing
/// assertion is the useful outcome.
pub async fn run<S: BlobStore>(store: &S) -> Report {
    let mut r = Report {
        driver: store.driver().to_owned(),
        ..Default::default()
    };
    let caps = store.capabilities();

    data_plane(store, &mut r).await;
    server_side_copy(store, &mut r).await;
    ranged_get(store, caps, &mut r).await;
    presigning(store, caps, &mut r).await;
    listing(store, &mut r).await;
    storage_classes(store, caps, &mut r).await;
    restore_lifecycle(store, caps, &mut r).await;
    r
}

// ─── server-side copy: every driver must pass this ─────────────────────────

/// A copy is how a staged upload is promoted to its content-addressed key, so both drivers
/// have to agree on it — including that it leaves the source alone.
async fn server_side_copy<S: BlobStore>(store: &S, r: &mut Report) {
    let t = ns();
    let from = Key::staging(t, "conformance").expect("key");
    let to = Key::original(t, &digest(7)).expect("key");
    let body = Bytes::from_static(b"promote me");

    store
        .put(&from, body.clone(), StorageClass::Standard)
        .await
        .expect("stage");
    store
        .copy(&from, &to, body.len() as u64, StorageClass::Standard)
        .await
        .expect("copy must succeed");

    let copied = store
        .get(&to, None)
        .await
        .expect("get copy")
        .into_bytes(&to)
        .expect("hot");
    assert_eq!(copied, body, "copy corrupted the body");
    assert!(
        store.head(&from).await.is_ok(),
        "a copy must not consume its source — promotion deletes staging only after the \
         copy has succeeded, and a driver that moved instead of copying would destroy the \
         only remaining bytes on a partial failure"
    );
    r.pass("server-side copy leaves the source intact");

    let missing = Key::staging(t, "absent").expect("key");
    assert!(
        matches!(
            store.copy(&missing, &to, 1, StorageClass::Standard).await,
            Err(crate::Error::NotFound(_))
        ),
        "copying a missing source must be NotFound, not a generic backend error"
    );
    r.pass("copying a missing source is NotFound");
}

// ─── data plane: every driver must pass this ────────────────────────────────

async fn data_plane<S: BlobStore>(store: &S, r: &mut Report) {
    let t = ns();
    let key = Key::original(t, &digest(1)).expect("key");
    let body = Bytes::from_static(b"the quick brown fox");

    let placement = store
        .put(&key, body.clone(), StorageClass::Standard)
        .await
        .expect("put must succeed");
    assert_eq!(placement.size, body.len() as u64, "put reported wrong size");
    assert_eq!(placement.key, key);

    let got = store.get(&key, None).await.expect("get must succeed");
    match got {
        GetOutcome::Bytes(b) => assert_eq!(b, body, "round-trip corrupted the body"),
        GetOutcome::NotAvailable(_) => panic!("a Standard-class object must be readable"),
    }
    r.pass("put/get round-trip");

    let head = store.head(&key).await.expect("head must succeed");
    assert_eq!(head.size, body.len() as u64);
    assert_eq!(head.storage_class, StorageClass::Standard);
    assert!(head.is_readable());
    r.pass("head reports size and class");

    // Overwriting must replace, not append or fail.
    let replacement = Bytes::from_static(b"replaced");
    store
        .put(&key, replacement.clone(), StorageClass::Standard)
        .await
        .expect("overwrite must succeed");
    let after = store.get(&key, None).await.expect("get after overwrite");
    assert!(
        matches!(after, GetOutcome::Bytes(ref b) if *b == replacement),
        "overwrite did not replace the body"
    );
    r.pass("overwrite replaces");

    store.delete(&key).await.expect("delete must succeed");
    let missing = store.get(&key, None).await;
    assert!(
        matches!(missing, Err(Error::NotFound(_))),
        "get after delete should be NotFound, got {missing:?}"
    );
    r.pass("delete removes");

    // Deleting a key that is not there is a no-op, not an error. S3 behaves this way and
    // the purge worker relies on it: a retried purge must not fail because the first
    // attempt succeeded.
    store
        .delete(&key)
        .await
        .expect("deleting a missing key must be a no-op");
    r.pass("delete is idempotent");

    let head_missing = store.head(&key).await;
    assert!(
        matches!(head_missing, Err(Error::NotFound(_))),
        "head on a missing key should be NotFound"
    );
    r.pass("head on missing key is NotFound");

    // Empty objects are legitimate — a zero-byte sidecar, an empty transcript.
    let empty_key = Key::original(t, &digest(2)).expect("key");
    store
        .put(&empty_key, Bytes::new(), StorageClass::Standard)
        .await
        .expect("empty put");
    let empty = store.head(&empty_key).await.expect("head empty");
    assert_eq!(empty.size, 0);
    r.pass("zero-byte objects");
    let _ = store.delete(&empty_key).await;

    // A body big enough to cross a typical buffer boundary, so a driver that chunks is
    // exercised rather than only its small-body path.
    let big_key = Key::original(t, &digest(3)).expect("key");
    let big = Bytes::from(vec![0xABu8; 5 * 1024 * 1024]);
    store
        .put(&big_key, big.clone(), StorageClass::Standard)
        .await
        .expect("5MiB put");
    let back = store.get(&big_key, None).await.expect("5MiB get");
    match back {
        GetOutcome::Bytes(b) => {
            assert_eq!(b.len(), big.len(), "5MiB round-trip changed length");
            assert_eq!(b, big, "5MiB round-trip corrupted the body");
        }
        GetOutcome::NotAvailable(_) => panic!("unexpected cold object"),
    }
    r.pass("5MiB round-trip");
    let _ = store.delete(&big_key).await;
}

// ─── ranged get ─────────────────────────────────────────────────────────────

async fn ranged_get<S: BlobStore>(store: &S, caps: Capabilities, r: &mut Report) {
    if !caps.ranged_get {
        r.skip("ranged get", "driver does not claim ranged_get");
        return;
    }
    let key = Key::original(ns(), &digest(4)).expect("key");
    let body = Bytes::from_static(b"0123456789");
    store
        .put(&key, body, StorageClass::Standard)
        .await
        .expect("put");

    let head = |o: GetOutcome| match o {
        GetOutcome::Bytes(b) => b,
        GetOutcome::NotAvailable(_) => panic!("unexpected cold object"),
    };

    let first3 = head(
        store
            .get(&key, Some(ByteRange::new(0, Some(2))))
            .await
            .expect("range 0-2"),
    );
    assert_eq!(&first3[..], b"012", "inclusive range returned wrong bytes");

    let tail = head(
        store
            .get(&key, Some(ByteRange::from_start(7)))
            .await
            .expect("range 7-"),
    );
    assert_eq!(&tail[..], b"789", "open-ended range returned wrong bytes");

    let one = head(
        store
            .get(&key, Some(ByteRange::new(5, Some(5))))
            .await
            .expect("single byte"),
    );
    assert_eq!(&one[..], b"5", "single-byte range");

    // A range extending past the end is clamped, not an error — S3 does this and the
    // media probe relies on it when reading a trailing box of unknown size.
    let over = head(
        store
            .get(&key, Some(ByteRange::new(8, Some(999))))
            .await
            .expect("over-long range"),
    );
    assert_eq!(&over[..], b"89", "over-long range should clamp to the end");

    r.pass("ranged get (inclusive, open-ended, clamped)");
    let _ = store.delete(&key).await;
}

// ─── presigning ─────────────────────────────────────────────────────────────

async fn presigning<S: BlobStore>(store: &S, caps: Capabilities, r: &mut Report) {
    if !caps.presigned_urls {
        r.skip("presigned urls", "driver does not claim presigned_urls");
        return;
    }
    let key = Key::original(ns(), &digest(5)).expect("key");
    store
        .put(&key, Bytes::from_static(b"x"), StorageClass::Standard)
        .await
        .expect("put");

    let url = store
        .presign_get(&key, Duration::from_secs(300))
        .await
        .expect("presign get");
    assert!(url.starts_with("http"), "presigned URL is not a URL: {url}");
    assert!(
        url.contains(key.as_str()) || url.contains(&urlencode(key.as_str())),
        "presigned URL does not reference the key: {url}"
    );
    // A presigned URL that carries no signature is just a URL, and would 403.
    assert!(
        url.contains("X-Amz-Signature") || url.contains("Signature"),
        "presigned URL carries no signature: {url}"
    );

    let put_url = store
        .presign_put(&key, Duration::from_secs(300))
        .await
        .expect("presign put");
    assert!(put_url.starts_with("http"));

    r.pass("presigned get and put");
    let _ = store.delete(&key).await;
}

fn urlencode(s: &str) -> String {
    s.replace('/', "%2F")
}

// ─── listing ────────────────────────────────────────────────────────────────

async fn listing<S: BlobStore>(store: &S, r: &mut Report) {
    let t = ns();
    let prefix = format!("{t}/o");
    let mut keys = Vec::new();
    for i in 10..15u8 {
        let k = Key::original(t, &digest(i)).expect("key");
        store
            .put(&k, Bytes::from_static(b"x"), StorageClass::Standard)
            .await
            .expect("put");
        keys.push(k);
    }

    let listed = store.list(&prefix, 100).await.expect("list");
    assert_eq!(
        listed.len(),
        5,
        "expected 5 keys under {prefix}, got {listed:?}"
    );

    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "list must return keys lexicographically");

    let limited = store.list(&prefix, 2).await.expect("list limited");
    assert_eq!(limited.len(), 2, "limit must be honoured");

    // A prefix with nothing under it is an empty list, not an error.
    let empty = store
        .list(&format!("{}/nothing-here", ns()), 10)
        .await
        .expect("list empty prefix");
    assert!(empty.is_empty());

    r.pass("list: prefix, order, limit, empty");
    for k in keys {
        let _ = store.delete(&k).await;
    }
}

// ─── storage classes ────────────────────────────────────────────────────────

async fn storage_classes<S: BlobStore>(store: &S, caps: Capabilities, r: &mut Report) {
    if !caps.storage_classes {
        r.skip(
            "storage classes",
            "driver does not honour StorageClass — note that echoing the header back \
             without changing behaviour does NOT count",
        );
        return;
    }
    let key = Key::original(ns(), &digest(6)).expect("key");
    store
        .put(&key, Bytes::from_static(b"cold"), StorageClass::GlacierIr)
        .await
        .expect("put GLACIER_IR");

    let head = store.head(&key).await.expect("head");
    assert_eq!(
        head.storage_class,
        StorageClass::GlacierIr,
        "the class must survive a round-trip"
    );
    // Glacier IR needs no restore, so it must still be directly readable — the property
    // that makes it the right default archive tier.
    assert!(
        head.is_readable(),
        "GLACIER_IR must be readable without a restore"
    );
    assert!(matches!(
        store.get(&key, None).await.expect("get"),
        GetOutcome::Bytes(_)
    ));
    r.pass("GLACIER_IR is readable without a restore");

    store
        .transition(&key, StorageClass::DeepArchive)
        .await
        .expect("transition");
    let after = store.head(&key).await.expect("head after transition");
    assert_eq!(after.storage_class, StorageClass::DeepArchive);
    assert!(
        !after.is_readable(),
        "DEEP_ARCHIVE must not be readable without a restore"
    );
    r.pass("transition to DEEP_ARCHIVE");

    // The whole point of GetOutcome: a cold read is a normal outcome, not an error.
    match store
        .get(&key, None)
        .await
        .expect("cold get must not error")
    {
        GetOutcome::NotAvailable(ticket) => {
            assert_eq!(ticket.class, StorageClass::DeepArchive);
            assert!(
                !matches!(ticket.state, RestoreState::Available),
                "an unrestored object must not report Available"
            );
        }
        GetOutcome::Bytes(_) => panic!("DEEP_ARCHIVE returned bytes without a restore"),
    }
    r.pass("cold get returns NotAvailable, not an error");
    let _ = store.delete(&key).await;
}

// ─── restore lifecycle ──────────────────────────────────────────────────────

async fn restore_lifecycle<S: BlobStore>(store: &S, caps: Capabilities, r: &mut Report) {
    if !caps.restore {
        r.skip(
            "restore lifecycle",
            "driver has no RestoreObject — covered by FakeS3Store and the AWS nightly",
        );
        return;
    }
    let key = Key::original(ns(), &digest(7)).expect("key");
    store
        .put(&key, Bytes::from_static(b"archived"), StorageClass::Glacier)
        .await
        .expect("put GLACIER");

    let ticket = store
        .restore(&key, RestoreTier::Standard, Duration::from_secs(7 * 86400))
        .await
        .expect("restore");
    assert!(
        matches!(
            ticket.state,
            RestoreState::Requested | RestoreState::Ongoing
        ),
        "a fresh restore should be Requested or Ongoing, got {:?}",
        ticket.state
    );
    assert!(
        ticket.eta.is_some(),
        "a restore must carry an ETA for the UI"
    );
    r.pass("restore returns a ticket with an ETA");

    // Not available yet: the ticket is a promise, not a result.
    assert!(
        matches!(
            store.get(&key, None).await.expect("get during restore"),
            GetOutcome::NotAvailable(_)
        ),
        "an in-progress restore must not yield bytes"
    );
    r.pass("in-progress restore does not yield bytes");

    // Requesting again must not double-charge.
    let again = store
        .restore(&key, RestoreTier::Standard, Duration::from_secs(7 * 86400))
        .await
        .expect("duplicate restore must be a no-op");
    assert!(
        matches!(again.state, RestoreState::Requested | RestoreState::Ongoing),
        "a duplicate restore should report the in-flight state"
    );
    r.pass("duplicate restore is a no-op");

    // The storage class must NOT have changed — a restore makes a temporary copy, it
    // does not move the object. Conflating the two is the bug that makes an object read
    // as available forever and 403 the day the copy expires.
    let mid = store.head(&key).await.expect("head mid-restore");
    assert_eq!(
        mid.storage_class,
        StorageClass::Glacier,
        "a restore must not change the storage class"
    );
    r.pass("restore leaves the storage class alone");

    let _ = store.delete(&key).await;
}
