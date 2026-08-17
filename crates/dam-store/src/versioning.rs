//! Versioning and object lock.
//!
//! Separate from [`crate::BlobStore`] on purpose. The trait is the narrow surface every
//! caller uses; this is the retention surface only the compliance paths touch, and only
//! against a backend that declares `versioning` / `object_lock`. Widening the trait would
//! force every driver — including `FakeS3Store`, which deliberately does not claim object
//! lock — to carry methods it cannot implement honestly.
//!
//! What this module provides is the *mechanism* ARCHITECTURE §6.6 calls for ("S3 Object
//! Lock available per pool for retention and legal hold") and §6.3 depends on ("Legal hold
//! / EULA-encumbered → pinned, S3 Object Lock, never tiers"). **Who** may apply or release
//! a hold is authorisation, and that lives in the ABAC layer — not here.

use crate::{BlobStore, Error, Key, Result, S3Store};
use aws_sdk_s3::types::{
    BucketVersioningStatus, ObjectLockLegalHold, ObjectLockLegalHoldStatus, ObjectLockRetention,
    ObjectLockRetentionMode, VersioningConfiguration,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};

/// Object-lock retention mode.
///
/// No `Default`, and no conversion from a string: the two modes differ in whether a
/// mistake is recoverable, so a caller must state which one it means. `Compliance` cannot
/// be shortened or bypassed by anyone, including the account root — choosing it by
/// accident means the object survives until its date no matter what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionMode {
    /// Overridable by a caller holding `s3:BypassGovernanceRetention`. The right default
    /// for a retention *policy*, because a wrong policy has to be correctable.
    Governance,
    /// Overridable by nobody. For a regulatory or litigation hold.
    Compliance,
}

impl RetentionMode {
    fn as_sdk(self) -> ObjectLockRetentionMode {
        match self {
            Self::Governance => ObjectLockRetentionMode::Governance,
            Self::Compliance => ObjectLockRetentionMode::Compliance,
        }
    }
}

impl std::fmt::Display for RetentionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Governance => "GOVERNANCE",
            Self::Compliance => "COMPLIANCE",
        })
    }
}

/// Whether to send `x-amz-bypass-governance-retention`.
///
/// An enum rather than a `bool` so a call site reads `Bypass::No` instead of `false` —
/// a bare boolean at a delete that may be overriding a retention policy is the kind of
/// argument that gets flipped by an autocomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bypass {
    No,
    /// Only effective against `Governance`. Against `Compliance` the server still refuses,
    /// which is the whole distinction between the modes.
    Governance,
}

/// One version of an object, or a delete marker standing where one used to be.
#[derive(Debug, Clone)]
pub struct ObjectVersion {
    pub key: String,
    pub version_id: String,
    pub is_latest: bool,
    /// A delete marker has no bytes. Distinguished because a marker on top is why a `head`
    /// reports absent while the bytes underneath are still recoverable — and why an
    /// integrity scrub must not count it as a missing object.
    pub is_delete_marker: bool,
    pub size: u64,
    pub last_modified: Option<DateTime<Utc>>,
}

impl S3Store {
    /// Turns on bucket versioning.
    ///
    /// Idempotent, and irreversible in the sense that matters: versioning can be
    /// *suspended* but never returned to never-versioned, so existing versions persist.
    pub async fn enable_versioning(&self) -> Result<()> {
        self.require(self.capabilities().versioning, "versioning")?;
        self.client()
            .put_bucket_versioning()
            .bucket(self.bucket_name())
            .versioning_configuration(
                VersioningConfiguration::builder()
                    .status(BucketVersioningStatus::Enabled)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| self.op_err("enable_versioning", &e))?;
        Ok(())
    }

    /// Creates the bucket with object lock enabled.
    ///
    /// Object lock can only be enabled **at creation**, and it forces versioning on. For
    /// the test harness; a deployed bucket is created by infrastructure.
    pub async fn create_bucket_with_object_lock(&self) -> Result<()> {
        self.require(self.capabilities().object_lock, "object lock")?;
        match self
            .client()
            .create_bucket()
            .bucket(self.bucket_name())
            .object_lock_enabled_for_bucket(true)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = format!("{e:?}");
                if msg.contains("BucketAlreadyOwnedByYou") || msg.contains("BucketAlreadyExists") {
                    Ok(())
                } else {
                    Err(self.op_err("create_bucket_with_object_lock", &e))
                }
            }
        }
    }

    /// Versions and delete markers under a prefix, newest first per key.
    pub async fn list_versions(&self, prefix: &str, limit: i32) -> Result<Vec<ObjectVersion>> {
        self.require(self.capabilities().versioning, "versioning")?;
        let out = self
            .client()
            .list_object_versions()
            .bucket(self.bucket_name())
            .prefix(prefix)
            .max_keys(limit)
            .send()
            .await
            .map_err(|e| self.op_err(&format!("list_versions({prefix})"), &e))?;

        let mut versions: Vec<ObjectVersion> = out
            .versions()
            .iter()
            .map(|v| ObjectVersion {
                key: v.key().unwrap_or_default().to_owned(),
                version_id: v.version_id().unwrap_or_default().to_owned(),
                is_latest: v.is_latest().unwrap_or(false),
                is_delete_marker: false,
                size: v.size().unwrap_or(0).max(0) as u64,
                last_modified: v.last_modified().and_then(sdk_time),
            })
            .collect();

        // Delete markers arrive in a separate list. Merging them in is not cosmetic: a
        // caller reconciling placements against the bucket has to see that the newest
        // thing at a key is a marker, or it will record the object as present.
        versions.extend(out.delete_markers().iter().map(|d| ObjectVersion {
            key: d.key().unwrap_or_default().to_owned(),
            version_id: d.version_id().unwrap_or_default().to_owned(),
            is_latest: d.is_latest().unwrap_or(false),
            is_delete_marker: true,
            size: 0,
            last_modified: d.last_modified().and_then(sdk_time),
        }));

        Ok(versions)
    }

    /// Reads a specific version, including one that a delete marker hides.
    pub async fn get_version(&self, key: &Key, version_id: &str) -> Result<Bytes> {
        self.require(self.capabilities().versioning, "versioning")?;
        let out = self
            .client()
            .get_object()
            .bucket(self.bucket_name())
            .key(key.as_str())
            .version_id(version_id)
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))?;
        let body = out
            .body
            .collect()
            .await
            .map_err(|e| Error::Backend(format!("reading {key} version {version_id}: {e}")))?;
        Ok(body.into_bytes())
    }

    /// Permanently removes one version. There is no delete marker and no recovery.
    ///
    /// This is the call object lock exists to refuse.
    pub async fn delete_version(&self, key: &Key, version_id: &str, bypass: Bypass) -> Result<()> {
        self.require(self.capabilities().versioning, "versioning")?;
        let mut req = self
            .client()
            .delete_object()
            .bucket(self.bucket_name())
            .key(key.as_str())
            .version_id(version_id);
        if matches!(bypass, Bypass::Governance) {
            req = req.bypass_governance_retention(true);
        }
        req.send()
            .await
            .map_err(|e| self.map_err(key, &e))
            .map(|_| ())
    }

    /// Applies or releases a legal hold on a specific version.
    ///
    /// Version-scoped rather than key-scoped by design: a hold on "the current version"
    /// would be silently escaped by the next overwrite.
    pub async fn set_legal_hold(&self, key: &Key, version_id: &str, on: bool) -> Result<()> {
        self.require(self.capabilities().object_lock, "object lock")?;
        let status = if on {
            ObjectLockLegalHoldStatus::On
        } else {
            ObjectLockLegalHoldStatus::Off
        };
        self.client()
            .put_object_legal_hold()
            .bucket(self.bucket_name())
            .key(key.as_str())
            .version_id(version_id)
            .legal_hold(ObjectLockLegalHold::builder().status(status).build())
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))
            .map(|_| ())
    }

    /// Whether a legal hold is currently on. Readable so a hold can be audited and shown
    /// in the UI, rather than being write-only state on the server.
    pub async fn legal_hold(&self, key: &Key, version_id: &str) -> Result<bool> {
        self.require(self.capabilities().object_lock, "object lock")?;
        let out = self
            .client()
            .get_object_legal_hold()
            .bucket(self.bucket_name())
            .key(key.as_str())
            .version_id(version_id)
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))?;
        Ok(out
            .legal_hold()
            .and_then(|h| h.status())
            .is_some_and(|s| *s == ObjectLockLegalHoldStatus::On))
    }

    /// Sets a retain-until date in the given mode.
    pub async fn set_retention(
        &self,
        key: &Key,
        version_id: &str,
        mode: RetentionMode,
        until: DateTime<Utc>,
    ) -> Result<()> {
        self.require(self.capabilities().object_lock, "object lock")?;
        if until <= Utc::now() {
            // A past date is how a caller tries to *remove* retention. S3 rejects it, but
            // with a generic 400; refusing here names what actually happened. Removing a
            // GOVERNANCE retention is a bypass-flagged delete of the retention, not a
            // backdated one, and COMPLIANCE cannot be shortened at all.
            return Err(Error::Backend(format!(
                "retain-until must be in the future, got {until} — retention can be \
                 extended but never shortened"
            )));
        }
        let sdk_until = aws_sdk_s3::primitives::DateTime::from_millis(until.timestamp_millis());
        self.client()
            .put_object_retention()
            .bucket(self.bucket_name())
            .key(key.as_str())
            .version_id(version_id)
            .retention(
                ObjectLockRetention::builder()
                    .mode(mode.as_sdk())
                    .retain_until_date(sdk_until)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))
            .map(|_| ())
    }
}

/// Converts an SDK timestamp, dropping one that cannot be represented rather than
/// panicking — a listing is not the place to abort on a malformed date from a server.
fn sdk_time(t: &aws_sdk_s3::primitives::DateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(t.to_millis().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capabilities;

    fn narrow(caps: Capabilities) -> S3Store {
        // Never dialled: every case here is refused by the capability guard first.
        S3Store::compatible(
            "http://127.0.0.1:1",
            "b",
            "us-east-1",
            "k",
            "s",
            caps,
            "narrow",
        )
    }

    #[test]
    fn the_two_modes_render_as_the_names_s3_uses() {
        assert_eq!(RetentionMode::Governance.to_string(), "GOVERNANCE");
        assert_eq!(RetentionMode::Compliance.to_string(), "COMPLIANCE");
    }

    #[tokio::test]
    async fn a_backdated_retain_until_is_refused_locally() {
        let store = narrow(Capabilities::full());
        let key = Key::new("t/o/aa").expect("key");
        let err = store
            .set_retention(
                &key,
                "v1",
                RetentionMode::Governance,
                Utc::now() - chrono::Duration::hours(1),
            )
            .await
            .expect_err("a past retain-until must be refused");
        assert!(
            format!("{err}").contains("must be in the future"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn versioning_calls_are_refused_when_only_object_lock_is_claimed() {
        // Not a real combination — object lock requires versioning — but the guards must
        // be independent, so that a driver claiming one does not get the other for free.
        let caps = Capabilities {
            object_lock: true,
            versioning: false,
            ..Capabilities::minimal()
        };
        let store = narrow(caps);
        let key = Key::new("t/o/aa").expect("key");
        assert!(matches!(
            store.enable_versioning().await,
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            store.get_version(&key, "v1").await,
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            store.delete_version(&key, "v1", Bypass::No).await,
            Err(Error::Unsupported { .. })
        ));
    }
}
