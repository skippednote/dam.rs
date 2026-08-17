//! An in-process `BlobStore` with a clock you can move.
//!
//! Not a mock. It is a real implementation of the tiering state machine, and it exists
//! because **no S3-compatible server practical as a test dependency implements
//! `RestoreObject`** — and even real AWS cannot be tested against usefully, since the
//! cases that matter are measured in hours:
//!
//! - the temporary copy expires while a download is in flight
//! - a minimum-duration charge blocks a re-tier
//! - a Deep Archive restore takes 48 h on Bulk and 12 h on Standard
//!
//! With [`dam_core::TestClock`] each of those is a deterministic assertion instead of a
//! production incident. `FakeS3Store` shares the [`crate::conformance`] suite with
//! `S3Store`, so it cannot drift from real S3 on anything they both do.

use crate::{
    BlobStore, ByteRange, Capabilities, Error, GetOutcome, Key, ObjectState, Placement,
    RestoreTicket, Result,
};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dam_core::{Clock, LatencyClass, RestoreState, RestoreTier, StorageClass, TestClock};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone)]
struct Object {
    body: Bytes,
    storage_class: StorageClass,
    last_modified: DateTime<Utc>,
    /// When the object entered its current class. Drives the minimum-duration check —
    /// tier an object and delete it three days later and the full minimum is charged.
    class_since: DateTime<Utc>,
    restore: Option<Restore>,
}

#[derive(Debug, Clone)]
struct Restore {
    tier: RestoreTier,
    requested_at: DateTime<Utc>,
    available_at: DateTime<Utc>,
    /// Set when the copy actually becomes available, not when it is requested.
    expires_at: DateTime<Utc>,
}

/// An in-memory store with controllable time.
///
/// `BTreeMap` rather than `HashMap` so `list` returns keys in lexicographic order for
/// free — the same ordering S3 guarantees, which the import reconciler depends on.
#[derive(Debug, Clone)]
pub struct FakeS3Store {
    objects: Arc<Mutex<BTreeMap<String, Object>>>,
    clock: Arc<dyn Clock>,
    /// Simulated retrieval cost, so the restore-budget path has something to assert on.
    retrieval_cost_per_gb_cents: u64,
}

impl FakeS3Store {
    /// A store on a [`TestClock`], returning the clock so a test can move it.
    pub fn with_test_clock() -> (Self, TestClock) {
        let clock = TestClock::new();
        (Self::with_clock(Arc::new(clock.clone())), clock)
    }

    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            objects: Arc::new(Mutex::new(BTreeMap::new())),
            clock,
            retrieval_cost_per_gb_cents: 300,
        }
    }

    /// How much a restore of this object would cost, in cents. Feeds the estimate the
    /// UI shows before a user confirms (§6.5).
    pub fn estimated_restore_cost_cents(&self, key: &Key) -> Option<u64> {
        let objects = self.lock();
        let obj = objects.get(key.as_str())?;
        let gb = (obj.body.len() as f64 / 1_073_741_824.0).max(0.000_001);
        Some((gb * self.retrieval_cost_per_gb_cents as f64).ceil() as u64)
    }

    /// Whether the minimum billable duration for the current class has elapsed.
    ///
    /// The lifecycle engine must check this before any transition: moving an object out
    /// of Deep Archive after three days still bills all 180.
    pub fn min_duration_elapsed(&self, key: &Key) -> Option<bool> {
        let objects = self.lock();
        let obj = objects.get(key.as_str())?;
        let required = ChronoDuration::days(i64::from(obj.storage_class.min_duration_days()));
        Some(self.clock.now() >= obj.class_since + required)
    }

    /// Number of objects held. For tests asserting cleanup.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned lock means another thread panicked mid-write; recovering beats a
    /// second panic that buries the first.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Object>> {
        match self.objects.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Resolves an object's restore state against the current clock.
    ///
    /// The transition from `Ongoing` to `Available` to `Expired` is derived from time
    /// rather than stored, which is what makes it impossible for the fake to report an
    /// available restore whose copy has actually lapsed — the exact bug that conflating
    /// restore state with storage class produces.
    fn restore_state_now(&self, obj: &Object) -> (RestoreState, Option<DateTime<Utc>>) {
        let now = self.clock.now();
        match &obj.restore {
            None => (RestoreState::None, None),
            Some(r) if now < r.available_at => (RestoreState::Ongoing, None),
            Some(r) if now < r.expires_at => (RestoreState::Available, Some(r.expires_at)),
            Some(_) => (RestoreState::Expired, None),
        }
    }

    fn readable(&self, obj: &Object) -> bool {
        if !obj.storage_class.requires_restore() {
            return true;
        }
        matches!(self.restore_state_now(obj).0, RestoreState::Available)
    }

    fn ticket(&self, obj: &Object) -> RestoreTicket {
        let (state, expires_at) = self.restore_state_now(obj);
        RestoreTicket {
            class: obj.storage_class,
            state,
            tier: obj.restore.as_ref().map(|r| r.tier),
            eta: obj.restore.as_ref().map(|r| r.available_at),
            expires_at,
        }
    }
}

#[async_trait]
impl BlobStore for FakeS3Store {
    fn driver(&self) -> &'static str {
        "fake"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            storage_classes: true,
            restore: true,
            // Versioning and object lock are deliberately NOT claimed. Object lock's
            // whole point is that the *server* refuses the delete, so a fake that
            // refuses proves nothing about a real server — that runs against SeaweedFS
            // and the AWS nightly instead (§20.3).
            versioning: false,
            object_lock: false,
            presigned_urls: true,
            ranged_get: true,
            server_checksums: true,
        }
    }

    fn latency_class(&self) -> LatencyClass {
        LatencyClass::Instant
    }

    async fn put(&self, key: &Key, body: Bytes, class: StorageClass) -> Result<Placement> {
        let now = self.clock.now();
        let size = body.len() as u64;
        let checksum = blake3::hash(&body).to_hex().to_string();

        self.lock().insert(
            key.as_str().to_owned(),
            Object {
                body,
                storage_class: class,
                last_modified: now,
                class_since: now,
                restore: None,
            },
        );

        Ok(Placement {
            key: key.clone(),
            size,
            storage_class: class,
            etag: Some(format!("\"{}\"", &checksum[..32])),
            checksum: Some(checksum),
        })
    }

    async fn get(&self, key: &Key, range: Option<ByteRange>) -> Result<GetOutcome> {
        let objects = self.lock();
        let obj = objects
            .get(key.as_str())
            .ok_or_else(|| Error::NotFound(key.as_str().to_owned()))?;

        if !self.readable(obj) {
            return Ok(GetOutcome::NotAvailable(self.ticket(obj)));
        }

        let body = match range {
            None => obj.body.clone(),
            Some(r) => {
                let size = obj.body.len() as u64;
                // An over-long range clamps rather than erroring, matching S3 — the
                // media probe relies on it when reading a trailing box of unknown size.
                match r.length_within(size) {
                    None => {
                        return Err(Error::InvalidRange {
                            range: r.as_header(),
                            size,
                        });
                    }
                    Some(len) => {
                        let start = r.start as usize;
                        obj.body.slice(start..start + len as usize)
                    }
                }
            }
        };
        Ok(GetOutcome::Bytes(body))
    }

    async fn head(&self, key: &Key) -> Result<ObjectState> {
        let objects = self.lock();
        let obj = objects
            .get(key.as_str())
            .ok_or_else(|| Error::NotFound(key.as_str().to_owned()))?;
        let (restore_state, restore_expires_at) = self.restore_state_now(obj);
        let checksum = blake3::hash(&obj.body).to_hex().to_string();

        Ok(ObjectState {
            size: obj.body.len() as u64,
            storage_class: obj.storage_class,
            restore_state,
            restore_expires_at,
            etag: Some(format!("\"{}\"", &checksum[..32])),
            checksum: Some(checksum),
            last_modified: Some(obj.last_modified),
        })
    }

    async fn delete(&self, key: &Key) -> Result<()> {
        // Idempotent, matching S3. The purge worker relies on it: a retried purge must
        // not fail because the first attempt succeeded.
        self.lock().remove(key.as_str());
        Ok(())
    }

    async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<Key>> {
        let objects = self.lock();
        objects
            .range(prefix.to_owned()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .take(limit)
            .map(|(k, _)| Key::new(k.clone()))
            .collect()
    }

    async fn transition(&self, key: &Key, to: StorageClass) -> Result<()> {
        let now = self.clock.now();
        let mut objects = self.lock();
        let obj = objects
            .get_mut(key.as_str())
            .ok_or_else(|| Error::NotFound(key.as_str().to_owned()))?;
        obj.storage_class = to;
        obj.class_since = now;
        // A transition invalidates any restore: the temporary copy belonged to the
        // previous class.
        obj.restore = None;
        Ok(())
    }

    async fn restore(
        &self,
        key: &Key,
        tier: RestoreTier,
        keep_for: Duration,
    ) -> Result<RestoreTicket> {
        let now = self.clock.now();
        let mut objects = self.lock();
        let obj = objects
            .get_mut(key.as_str())
            .ok_or_else(|| Error::NotFound(key.as_str().to_owned()))?;

        if !obj.storage_class.requires_restore() {
            return Err(Error::Unsupported {
                driver: "fake",
                capability: "restore of a non-archive object",
            });
        }
        if !tier.is_available_for(obj.storage_class) {
            // Deep Archive has no Expedited tier. Refusing here rather than at the
            // provider means the error names the real reason.
            return Err(Error::Backend(format!(
                "{tier} retrieval is not available for {}",
                obj.storage_class
            )));
        }

        // A restore already in flight or live is a no-op, not a second charge.
        let existing = self.restore_state_now(obj).0;
        if matches!(existing, RestoreState::Ongoing | RestoreState::Available) {
            return Ok(self.ticket(obj));
        }

        let wait = ChronoDuration::from_std(tier.expected_wait(obj.storage_class))
            .unwrap_or_else(|_| ChronoDuration::hours(12));
        let keep = ChronoDuration::from_std(keep_for).unwrap_or_else(|_| ChronoDuration::days(7));
        let available_at = now + wait;

        obj.restore = Some(Restore {
            tier,
            requested_at: now,
            available_at,
            // Measured from availability, not from the request — otherwise a 48-hour
            // Bulk restore kept for 24 hours would expire before it arrived.
            expires_at: available_at + keep,
        });
        Ok(self.ticket(obj))
    }

    async fn presign_get(&self, key: &Key, ttl: Duration) -> Result<String> {
        // Shaped like a real presigned URL so a caller parsing one does not behave
        // differently against the fake.
        Ok(format!(
            "https://fake.invalid/{}?X-Amz-Expires={}&X-Amz-Signature=fake",
            key.as_str(),
            ttl.as_secs()
        ))
    }

    async fn presign_put(&self, key: &Key, ttl: Duration) -> Result<String> {
        Ok(format!(
            "https://fake.invalid/{}?X-Amz-Expires={}&X-Amz-Signature=fake&method=PUT",
            key.as_str(),
            ttl.as_secs()
        ))
    }
}

impl Object {
    /// When the restore was asked for. Not used by the trait, but the restore-batching
    /// logic in M3 needs it, and dropping the field now would mean re-adding it later.
    #[allow(dead_code)]
    fn requested_at(&self) -> Option<DateTime<Utc>> {
        self.restore.as_ref().map(|r| r.requested_at)
    }
}
