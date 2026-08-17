//! Blob storage: the `BlobStore` trait, its drivers, pools, placements, lifecycle.
//!
//! ## Two drivers, one trait
//!
//! No S3-compatible server that is practical as a test dependency implements
//! storage-class semantics or `RestoreObject`. SeaweedFS accepts the storage-class
//! header and *reports it back* while behaviour is unchanged — for testing that is
//! worse than rejecting it, because a test would pass while proving nothing.
//!
//! So `S3Store` proves the wire protocol against a real server, and `FakeS3Store`
//! proves the tiering state machine against a clock you can move. Both run the same
//! [`conformance`] suite for everything they share, so the fake cannot quietly diverge
//! (ARCHITECTURE §20.2).
//!
//! ## Capabilities are declared, and the declaration is checked
//!
//! A driver states what it supports via [`BlobStore::capabilities`]. The conformance
//! suite skips what a driver does not claim — and **asserts that anything it does
//! claim actually works**. A silent skip is how a capability gap becomes a production
//! surprise, so skips are returned in the report rather than swallowed.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)
)]

pub mod conformance;
pub mod content;
pub mod fake;
pub mod key;
pub mod multipart;
pub mod pool;
pub mod s3;
pub mod versioning;

#[cfg(feature = "testing")]
pub mod testing;

pub use content::{Digest, Ingested, StreamHasher};
pub use fake::FakeS3Store;
pub use key::Key;
pub use multipart::{MIN_PART_SIZE, MultipartUpload};
pub use pool::{PlacementRef, PoolRegistry, PoolSpec, Rate, ReadPlan};
pub use s3::S3Store;
pub use versioning::{Bypass, ObjectVersion, RetentionMode};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use dam_core::{LatencyClass, RestoreState, RestoreTier, StorageClass};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("object not found: {0}")]
    NotFound(String),

    /// The object is in an archive class and has no live restore. Distinct from
    /// `NotFound` because the caller's next move is completely different: request a
    /// restore and wait, rather than give up.
    #[error("object {key} is in {class} and not currently restored")]
    NotRestored { key: String, class: StorageClass },

    #[error("checksum mismatch for {key}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        key: String,
        expected: String,
        actual: String,
    },

    /// The driver does not implement this. Its own variant so a caller can degrade
    /// gracefully — the Drupal connector resolving a cold original to the proxy, for
    /// instance — rather than treating it as an outage.
    #[error("{driver} does not support {capability}")]
    Unsupported {
        driver: &'static str,
        capability: &'static str,
    },

    #[error("invalid range {range} for object of {size} bytes")]
    InvalidRange { range: String, size: u64 },

    /// A placement referencing a pool the registry does not know. Its own variant because
    /// this is configuration drift, not a storage failure, and the fix is a config change
    /// rather than a retry.
    #[error("unknown storage pool {0} — a placement references a pool that is not configured")]
    UnknownPool(uuid::Uuid),

    /// No copy of the object can be read or restored. Carries why each copy was unusable,
    /// because "no usable placement" alone leaves an operator querying the table by hand.
    #[error("no usable placement: {reason}")]
    NoUsablePlacement { reason: String },

    #[error("storage backend: {0}")]
    Backend(String),

    #[error(transparent)]
    Core(#[from] dam_core::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A byte range request, half-open at neither end: `bytes=start-end` inclusive, as S3
/// defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    /// Inclusive. `None` means "to the end", matching `bytes=N-`.
    pub end: Option<u64>,
}

impl ByteRange {
    pub fn new(start: u64, end: Option<u64>) -> Self {
        Self { start, end }
    }

    pub fn from_start(start: u64) -> Self {
        Self { start, end: None }
    }

    /// The `Range` header value.
    pub fn as_header(&self) -> String {
        match self.end {
            Some(e) => format!("bytes={}-{}", self.start, e),
            None => format!("bytes={}-", self.start),
        }
    }

    /// How many bytes this range covers of an object of `size`, or `None` if the range
    /// starts past the end.
    pub fn length_within(&self, size: u64) -> Option<u64> {
        if self.start >= size {
            return None;
        }
        let last = self.end.unwrap_or(size - 1).min(size - 1);
        Some(last.saturating_sub(self.start) + 1)
    }
}

/// What a `GET` produced.
///
/// An enum rather than an error for the cold case, because "in Deep Archive" is a
/// normal, expected outcome the caller must handle — not an exceptional one. Making it
/// a variant means the compiler asks about it at every call site.
#[derive(Debug)]
pub enum GetOutcome {
    Bytes(Bytes),
    /// Archive class, no live restore. Carries what the caller needs to tell a user
    /// how long and how much.
    NotAvailable(RestoreTicket),
}

impl GetOutcome {
    /// The bytes, or an error. For call sites that genuinely cannot handle the cold
    /// case — the internal enrichment pipeline reading a proxy, which is always hot.
    pub fn into_bytes(self, key: &Key) -> Result<Bytes> {
        match self {
            Self::Bytes(b) => Ok(b),
            Self::NotAvailable(t) => Err(Error::NotRestored {
                key: key.as_str().to_owned(),
                class: t.class,
            }),
        }
    }
}

/// What a caller needs to know about an in-progress or possible restore.
#[derive(Debug, Clone)]
pub struct RestoreTicket {
    pub class: StorageClass,
    pub state: RestoreState,
    pub tier: Option<RestoreTier>,
    /// When the temporary copy is expected. An estimate, deliberately pessimistic.
    pub eta: Option<DateTime<Utc>>,
    /// When the temporary copy goes away again. `Some` only while `state` is
    /// `Available` — an available restore without an expiry is unreclaimable state, and
    /// the database refuses it (§6.5).
    pub expires_at: Option<DateTime<Utc>>,
}

/// What a `HEAD` produced.
#[derive(Debug, Clone)]
pub struct ObjectState {
    pub size: u64,
    pub storage_class: StorageClass,
    pub restore_state: RestoreState,
    pub restore_expires_at: Option<DateTime<Utc>>,
    pub etag: Option<String>,
    /// Server-side checksum where the backend stores one. Its presence is what lets
    /// the integrity scrub verify without paying egress to re-download (§6.4).
    pub checksum: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
}

impl ObjectState {
    /// Whether a `GET` would succeed right now.
    pub fn is_readable(&self) -> bool {
        !self.storage_class.requires_restore()
            || matches!(self.restore_state, RestoreState::Available)
    }
}

/// The result of a successful `PUT`.
#[derive(Debug, Clone)]
pub struct Placement {
    pub key: Key,
    pub size: u64,
    pub storage_class: StorageClass,
    pub etag: Option<String>,
    /// Whole-object checksum the driver actually computed. `None` for a multipart upload,
    /// where the ETag is a composite of part digests and is not the digest of the object —
    /// the streaming BLAKE3 in the upload path is the checksum of record (§6.4).
    pub checksum: Option<String>,
    /// The version this write created, on a versioned bucket. `None` when versioning is
    /// off. Carried because a legal hold attaches to a *version*, not to a key — holding
    /// "the current version" is a race, since the next overwrite moves it.
    pub version_id: Option<String>,
}

/// What a driver supports.
///
/// Declared rather than probed, and the conformance suite verifies every claim. A
/// driver that under-claims loses coverage; one that over-claims fails the suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Honours `StorageClass` on put and reports it back on head. SeaweedFS *echoes*
    /// the header without changing behaviour, which does not count.
    pub storage_classes: bool,
    /// Implements `RestoreObject` and the `x-amz-restore` lifecycle.
    pub restore: bool,
    pub versioning: bool,
    /// Object lock: legal hold and retention modes.
    pub object_lock: bool,
    pub presigned_urls: bool,
    pub ranged_get: bool,
    /// Returns a server-side checksum on head, so the scrub need not download.
    pub server_checksums: bool,
}

impl Capabilities {
    /// Nothing but the data plane. A sensible base for a new driver to widen from.
    pub const fn minimal() -> Self {
        Self {
            storage_classes: false,
            restore: false,
            versioning: false,
            object_lock: false,
            presigned_urls: false,
            ranged_get: false,
            server_checksums: false,
        }
    }

    /// What real S3 does.
    pub const fn full() -> Self {
        Self {
            storage_classes: true,
            restore: true,
            versioning: true,
            object_lock: true,
            presigned_urls: true,
            ranged_get: true,
            server_checksums: true,
        }
    }
}

/// The one storage abstraction.
///
/// Deliberately narrow. Multipart upload, lifecycle transitions across pools, and
/// placement resolution all sit *above* this — they are policy, and policy that leaks
/// into a driver has to be reimplemented per backend.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// A short, stable name for error messages and metrics.
    fn driver(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// The latency a caller should expect. Drives whether the download path hands over
    /// bytes or a restore ticket.
    fn latency_class(&self) -> LatencyClass;

    async fn put(&self, key: &Key, body: Bytes, class: StorageClass) -> Result<Placement>;

    async fn get(&self, key: &Key, range: Option<ByteRange>) -> Result<GetOutcome>;

    async fn head(&self, key: &Key) -> Result<ObjectState>;

    async fn delete(&self, key: &Key) -> Result<()>;

    /// Keys under a prefix, lexicographically. Used by the integrity scrub and by the
    /// import reconciler.
    async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<Key>>;

    /// Moves an object between storage classes.
    async fn transition(&self, key: &Key, to: StorageClass) -> Result<()>;

    /// Requests a temporary copy of an archived object.
    ///
    /// Returns immediately with a ticket; the copy is not available yet. Calling it
    /// twice for the same object is a no-op rather than a second charge.
    async fn restore(
        &self,
        key: &Key,
        tier: RestoreTier,
        keep_for: Duration,
    ) -> Result<RestoreTicket>;

    async fn presign_get(&self, key: &Key, ttl: Duration) -> Result<String>;

    async fn presign_put(&self, key: &Key, ttl: Duration) -> Result<String>;
}
