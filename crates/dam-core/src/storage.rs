//! Storage domain types.
//!
//! In `dam-core` rather than `dam-store` because both the storage layer and the
//! database layer need them: `object_placements.storage_class` and
//! `storage_pools.latency_class` are the same vocabulary the `BlobStore` trait speaks,
//! and two enums that must agree are better as one enum.
//!
//! Every variant list here matches a CHECK constraint in the migrations. When they
//! disagree, one layer accepts what the other refuses — the same failure the
//! `TenantSlug` cross-check exists to prevent.

use crate::Error;
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr, time::Duration};

/// S3 storage classes, matching `object_placements.storage_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StorageClass {
    Standard,
    StandardIa,
    OnezoneIa,
    IntelligentTiering,
    /// Millisecond `GET`, no restore step, roughly six times cheaper than Standard.
    /// The correct default archive tier for a DAM (§6.3) — cold originals stay
    /// directly downloadable.
    GlacierIr,
    /// Requires `RestoreObject`. Expedited 1-5 min, Standard 3-5 h, Bulk 5-12 h.
    Glacier,
    /// Requires `RestoreObject`. **No Expedited tier**: Standard ~12 h, Bulk ~48 h.
    DeepArchive,
}

impl StorageClass {
    /// Whether a `GET` needs a completed restore first.
    ///
    /// The property the whole tiering design turns on, so it lives on the type rather
    /// than as a check scattered through the delivery path.
    pub fn requires_restore(self) -> bool {
        matches!(self, Self::Glacier | Self::DeepArchive)
    }

    /// Minimum billable duration in days. Tier an object and delete it three days
    /// later and the full minimum is still charged, which is why the lifecycle engine
    /// checks this before any transition (§6.4).
    pub fn min_duration_days(self) -> u32 {
        match self {
            Self::Standard | Self::IntelligentTiering => 0,
            Self::StandardIa | Self::OnezoneIa => 30,
            Self::GlacierIr | Self::Glacier => 90,
            Self::DeepArchive => 180,
        }
    }

    /// Minimum billable object size in bytes.
    ///
    /// 128 KiB on IA and Glacier IR is why thumbnails never tier: a 20 KB thumbnail
    /// billed as 128 KB costs *more* there than in Standard.
    pub fn min_billable_bytes(self) -> u64 {
        match self {
            Self::StandardIa | Self::OnezoneIa | Self::GlacierIr => 128 * 1024,
            _ => 0,
        }
    }

    /// The latency a pool in this class must declare. Enforced by a CHECK on
    /// `storage_pools`; mirrored here so code cannot build a pool the database would
    /// reject.
    pub fn latency_class(self) -> LatencyClass {
        if self.requires_restore() {
            LatencyClass::Hours
        } else {
            LatencyClass::Instant
        }
    }

    /// The wire value S3 expects.
    pub fn as_s3(self) -> &'static str {
        match self {
            Self::Standard => "STANDARD",
            Self::StandardIa => "STANDARD_IA",
            Self::OnezoneIa => "ONEZONE_IA",
            Self::IntelligentTiering => "INTELLIGENT_TIERING",
            Self::GlacierIr => "GLACIER_IR",
            Self::Glacier => "GLACIER",
            Self::DeepArchive => "DEEP_ARCHIVE",
        }
    }
}

impl fmt::Display for StorageClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_s3())
    }
}

impl FromStr for StorageClass {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "STANDARD" => Ok(Self::Standard),
            "STANDARD_IA" => Ok(Self::StandardIa),
            "ONEZONE_IA" => Ok(Self::OnezoneIa),
            "INTELLIGENT_TIERING" => Ok(Self::IntelligentTiering),
            "GLACIER_IR" => Ok(Self::GlacierIr),
            "GLACIER" => Ok(Self::Glacier),
            "DEEP_ARCHIVE" => Ok(Self::DeepArchive),
            other => Err(Error::validation(
                "storage_class",
                format!("unknown: {other}"),
            )),
        }
    }
}

/// How long a `GET` takes to become possible.
///
/// The download path and the UI branch on **this**, not on the provider name. That is
/// what lets Azure Archive or LTO tape slot in later without special-casing (§6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatencyClass {
    Instant,
    Seconds,
    Minutes,
    Hours,
    Days,
}

impl LatencyClass {
    /// Whether a caller must be told to wait rather than handed bytes.
    pub fn is_async(self) -> bool {
        self > Self::Instant
    }
}

impl fmt::Display for LatencyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Instant => "instant",
            Self::Seconds => "seconds",
            Self::Minutes => "minutes",
            Self::Hours => "hours",
            Self::Days => "days",
        };
        f.write_str(s)
    }
}

/// Retrieval speed for a restore, matching `restore_requests.tier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestoreTier {
    /// 1-5 minutes on Glacier. **Not available on Deep Archive** — validated against
    /// the resolved pool before `RestoreObject` is issued, because that depends on the
    /// class and a CHECK constraint cannot see it.
    Expedited,
    /// 3-5 h on Glacier, ~12 h on Deep Archive.
    Standard,
    /// 5-12 h on Glacier, ~48 h on Deep Archive. Roughly a tenth the cost of
    /// Expedited, which is why it is the tier the UI preselects.
    Bulk,
}

impl RestoreTier {
    /// Whether this tier is offered for the class. Deep Archive has no Expedited.
    pub fn is_available_for(self, class: StorageClass) -> bool {
        !(matches!(self, Self::Expedited) && matches!(class, StorageClass::DeepArchive))
    }

    /// Rough expected wait, for the ETA shown to a user. Deliberately the pessimistic
    /// end of each published range: an estimate that runs early is a pleasant
    /// surprise, one that runs late is a support ticket.
    pub fn expected_wait(self, class: StorageClass) -> Duration {
        let secs = match (self, class) {
            (Self::Expedited, _) => 5 * 60,
            (Self::Standard, StorageClass::DeepArchive) => 12 * 3600,
            (Self::Standard, _) => 5 * 3600,
            (Self::Bulk, StorageClass::DeepArchive) => 48 * 3600,
            (Self::Bulk, _) => 12 * 3600,
        };
        Duration::from_secs(secs)
    }
}

impl fmt::Display for RestoreTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Expedited => "expedited",
            Self::Standard => "standard",
            Self::Bulk => "bulk",
        };
        f.write_str(s)
    }
}

/// Where a restore has got to, matching `object_placements.restore_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestoreState {
    /// Not restorable, or not in an archive class.
    None,
    Requested,
    Ongoing,
    /// A temporary copy exists. **The storage class is unchanged** — which is why
    /// restore state is a separate field rather than another `StorageClass` variant,
    /// and why an `available` placement must carry an expiry (§6.5).
    Available,
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_glacier_and_deep_archive_need_a_restore() {
        assert!(!StorageClass::GlacierIr.requires_restore());
        assert!(StorageClass::Glacier.requires_restore());
        assert!(StorageClass::DeepArchive.requires_restore());
        assert!(!StorageClass::Standard.requires_restore());
    }

    #[test]
    fn latency_class_follows_from_the_storage_class() {
        // Mirrors the CHECK on storage_pools, so code cannot construct a pool the
        // database would reject.
        assert_eq!(
            StorageClass::GlacierIr.latency_class(),
            LatencyClass::Instant
        );
        assert_eq!(StorageClass::Glacier.latency_class(), LatencyClass::Hours);
    }

    #[test]
    fn deep_archive_has_no_expedited_tier() {
        assert!(!RestoreTier::Expedited.is_available_for(StorageClass::DeepArchive));
        assert!(RestoreTier::Expedited.is_available_for(StorageClass::Glacier));
    }

    #[test]
    fn minimum_durations_match_the_published_values() {
        assert_eq!(StorageClass::Standard.min_duration_days(), 0);
        assert_eq!(StorageClass::StandardIa.min_duration_days(), 30);
        assert_eq!(StorageClass::GlacierIr.min_duration_days(), 90);
        assert_eq!(StorageClass::DeepArchive.min_duration_days(), 180);
    }

    #[test]
    fn the_128k_minimum_is_why_thumbnails_never_tier() {
        assert_eq!(StorageClass::GlacierIr.min_billable_bytes(), 128 * 1024);
        assert_eq!(StorageClass::Standard.min_billable_bytes(), 0);
    }

    #[test]
    fn s3_wire_values_round_trip() {
        for c in [
            StorageClass::Standard,
            StorageClass::StandardIa,
            StorageClass::OnezoneIa,
            StorageClass::IntelligentTiering,
            StorageClass::GlacierIr,
            StorageClass::Glacier,
            StorageClass::DeepArchive,
        ] {
            assert_eq!(c.as_s3().parse::<StorageClass>().expect("parse"), c);
        }
    }

    #[test]
    fn bulk_is_cheaper_and_slower_than_expedited() {
        let g = StorageClass::Glacier;
        assert!(RestoreTier::Bulk.expected_wait(g) > RestoreTier::Expedited.expected_wait(g));
    }
}

/// Lifecycle state of one `object_placements` row.
///
/// Mirrors the `object_placements.state` CHECK. In `dam-core` rather than `dam-store`
/// because both the database layer that loads it and the resolution layer that acts on it
/// need the same vocabulary — and a second definition is how the two drift.
///
/// Only `Present` is readable. The others each mean something different to an operator,
/// which is why this is not a boolean: `Uploading` will resolve itself, `Missing` needs a
/// re-replication, `Corrupt` needs a scrub, and `Deleting` is expected to disappear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlacementState {
    Uploading,
    Present,
    Transitioning,
    Missing,
    Corrupt,
    Deleting,
}

impl PlacementState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uploading => "uploading",
            Self::Present => "present",
            Self::Transitioning => "transitioning",
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::Deleting => "deleting",
        }
    }

    /// Whether bytes can be served from a placement in this state, before any
    /// storage-class or restore consideration.
    pub fn is_serveable(self) -> bool {
        matches!(self, Self::Present)
    }
}

impl std::fmt::Display for PlacementState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PlacementState {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "uploading" => Ok(Self::Uploading),
            "present" => Ok(Self::Present),
            "transitioning" => Ok(Self::Transitioning),
            "missing" => Ok(Self::Missing),
            "corrupt" => Ok(Self::Corrupt),
            "deleting" => Ok(Self::Deleting),
            // The value is echoed deliberately: a placement state comes from our own
            // schema, never from a user, so naming it is diagnostic rather than a leak.
            other => Err(crate::Error::Validation {
                field: "object_placements.state".into(),
                reason: format!("unknown placement state {other:?}"),
            }),
        }
    }
}
