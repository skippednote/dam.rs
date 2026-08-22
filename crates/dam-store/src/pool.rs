//! Pools, placements, and resolution (§6.3).
//!
//! Storage is modelled as logical **pools** — a (driver, endpoint, bucket, prefix, storage
//! class) tuple plus its cost and latency characteristics — and an object may sit in
//! several at once. Two questions follow, and both are policy rather than plumbing:
//!
//! - **Read**: given several copies, which one do we serve from?
//! - **Write**: given a new object, where does it land?
//!
//! ## Readability outranks price
//!
//! "Cheapest wins" is the wrong rule stated on its own. Deep Archive is the cheapest place
//! in the estate to keep bytes, and its per-GB retrieval charge can be *lower* than Glacier
//! IR's — but reading it takes hours. A resolver that ranked on price alone would turn
//! ordinary downloads into restore tickets while looking like it was saving money. So
//! resolution is lexicographic: readable-now first, then price, then a stable name
//! tiebreak so two identical requests resolve identically.
//!
//! ## An unknown pool is an error
//!
//! A placement referencing a pool the registry does not know is configuration drift. The
//! tempting behaviour — skip it, serve from another copy — hides the drift while it is
//! still cheap to fix, and the same missing pool will also be silently absent from write
//! selection and from the lifecycle engine's cost model.

use crate::{Error, Key, Result};
use chrono::{DateTime, Utc};
use dam_core::{LatencyClass, PlacementState, RestoreState, StorageClass};
use std::collections::BTreeMap;
use uuid::Uuid;

/// A price, as an integer count of 1e-12 of the billing currency.
///
/// Integer rather than float so comparisons are exact: ties in a cost comparison must break
/// the same way every time, or an audit log and a cache key disagree between two identical
/// requests.
///
/// The scale is deliberately **finer than the database's**. `storage_pools` stores
/// `numeric(12,8)`, and at that scale a single S3 GET — $0.0004 per 1,000 requests, so
/// 4e-7 each — divides to zero and vanishes from every estimate. Four extra digits keep the
/// request term alive while conversion from the database stays exact (a multiply by
/// [`Rate::DB_SCALE_FACTOR`]). `u64` at this scale tops out around 18 million currency
/// units, far above any single object's storage or retrieval cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Rate(u64);

impl Rate {
    /// Units per whole currency unit.
    pub const SCALE: u64 = 1_000_000_000_000;

    /// `numeric(12,8)` has eight decimal places; this scale has twelve.
    pub const DB_SCALE_FACTOR: u64 = 10_000;

    /// From a `numeric(12,8)` column, already multiplied out to an integer by the loader.
    pub const fn from_db_units(units: u64) -> Self {
        Self(units.saturating_mul(Self::DB_SCALE_FACTOR))
    }

    pub const fn from_units(units: u64) -> Self {
        Self(units)
    }

    pub const fn units(self) -> u64 {
        self.0
    }

    /// Saturating throughout: a cost estimate must not panic on absurd input, and a
    /// saturated estimate is still ordered correctly against a smaller one.
    fn plus(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// `self` per GB, applied to `bytes`.
    fn per_gb(self, bytes: u64) -> Self {
        const GB: u128 = 1_073_741_824;
        let scaled = u128::from(self.0).saturating_mul(u128::from(bytes)) / GB;
        Self(u64::try_from(scaled).unwrap_or(u64::MAX))
    }

    /// `self` per 1,000 requests, applied to `count` requests.
    fn per_requests(self, count: u64) -> Self {
        let scaled = u128::from(self.0).saturating_mul(u128::from(count)) / 1000;
        Self(u64::try_from(scaled).unwrap_or(u64::MAX))
    }
}

impl std::fmt::Display for Rate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Rendered as currency because that is how it reaches a user in a restore
        // confirmation; the integer is an implementation detail.
        let whole = self.0 / Rate::SCALE;
        let frac = self.0 % Rate::SCALE;
        write!(f, "{whole}.{frac:012}")
    }
}

/// Storage backend behind a pool. Mirrors the `storage_pools.driver` CHECK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    S3,
    Azure,
    Fs,
    Tape,
}

impl Driver {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Azure => "azure",
            Self::Fs => "fs",
            Self::Tape => "tape",
        }
    }
}

impl std::str::FromStr for Driver {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "s3" => Ok(Self::S3),
            "azure" => Ok(Self::Azure),
            "fs" => Ok(Self::Fs),
            "tape" => Ok(Self::Tape),
            other => Err(Error::Backend(format!("unknown storage driver {other:?}"))),
        }
    }
}

/// A pool's configuration, as loaded from `dam_global.storage_pools`.
///
/// Credentials are referenced by pointer (`credentials_ref`) and resolved from the
/// environment or a secret manager at connect time — never carried in this struct, so a
/// `Debug` of a pool cannot leak one (§12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSpec {
    pub id: Uuid,
    pub name: String,
    pub driver: Driver,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: String,
    pub prefix: String,
    pub force_path_style: bool,
    pub credentials_ref: String,
    pub storage_class: StorageClass,
    pub latency_class: LatencyClass,
    /// S3 Object Lock enabled on the bucket.
    pub immutable: bool,
    pub min_duration_days: u32,
    /// Minimum billable object size. 131072 on IA and Glacier IR — the reason a thumbnail
    /// must never be placed there.
    pub min_billable_bytes: u64,
    pub cost_per_gb_month: Rate,
    pub cost_per_gb_retrieval: Rate,
    pub cost_per_1k_requests: Rate,
    /// Retired from *new* placements. Existing objects remain readable — see
    /// [`PoolRegistry::resolve_read`].
    pub enabled: bool,
}

impl PoolSpec {
    /// Whether a read from this pool returns bytes without a restore step.
    pub fn is_instant(&self) -> bool {
        matches!(self.latency_class, LatencyClass::Instant)
    }

    /// Whether this pool is a safe home for an object that must stay hot and may be small.
    ///
    /// Derived from the minimums rather than from the class name: the reason a thumbnail
    /// cannot live in Glacier IR is the 128 KiB minimum billable size and the 90-day
    /// minimum duration, not the label. A future pool with a cheap class and no minimums
    /// would be perfectly fine, and a check on the name would wrongly exclude it.
    pub fn suits_small_permanent_objects(&self) -> bool {
        self.is_instant() && self.min_billable_bytes == 0 && self.min_duration_days == 0
    }

    /// The full object prefix for a key in this pool.
    pub fn object_path(&self, key: &Key) -> String {
        if self.prefix.is_empty() {
            key.as_str().to_owned()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), key.as_str())
        }
    }
}

/// One row of `object_placements`, reduced to what resolution reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRef {
    pub pool_id: Uuid,
    pub size_bytes: u64,
    pub state: PlacementState,
    /// The class the **object** is in, which can differ from its pool's default: a pool is
    /// where an object lives, and a transition changes the object's class in place.
    pub storage_class: StorageClass,
    pub restore_state: RestoreState,
    pub restore_expires_at: Option<DateTime<Utc>>,
}

impl PlacementRef {
    /// Whether a `GET` against this placement returns bytes right now.
    fn is_readable_at(&self, now: DateTime<Utc>) -> bool {
        if !matches!(self.state, PlacementState::Present) {
            return false;
        }
        if !self.storage_class.requires_restore() {
            return true;
        }
        // A live restore, with the expiry boundary exclusive — the same rule as S3's
        // `expiry-date` and `FakeS3Store`. Serving one request past the expiry is how this
        // becomes an intermittent production 403.
        matches!(self.restore_state, RestoreState::Available)
            && self.restore_expires_at.is_some_and(|e| now < e)
    }
}

/// What to do about a read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadPlan {
    /// Serve from this pool now.
    Ready { pool_id: Uuid, cost: Rate },
    /// Every copy is archived. Restore from this pool — the cheapest one to retrieve from.
    Restore {
        pool_id: Uuid,
        estimated_cost: Rate,
        latency_class: LatencyClass,
    },
}

/// What is being written, which decides how strict the pool requirements are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteIntent {
    /// An uploaded original. Lands hot; the lifecycle engine tiers it later.
    Original,
    /// A proxy, thumbnail, or derivative. Small, hot, and for the tier-exempt kinds
    /// permanent — so pool minimums matter more than storage price.
    Derived,
}

/// The pools damrs knows about.
#[derive(Debug, Clone)]
pub struct PoolRegistry {
    /// Keyed and iterated in id order so every scan is deterministic.
    pools: BTreeMap<Uuid, PoolSpec>,
}

impl PoolRegistry {
    /// Builds a registry, refusing a duplicate id or name.
    ///
    /// Duplicates are refused rather than deduplicated: two pools sharing an id makes
    /// resolution non-deterministic, and two sharing a name makes an operator's policy
    /// ("pin this collection to `cool`") ambiguous. Both are configuration errors that are
    /// cheap to fix at load and expensive to diagnose later.
    pub fn new(pools: Vec<PoolSpec>) -> Result<Self> {
        let mut by_id: BTreeMap<Uuid, PoolSpec> = BTreeMap::new();
        let mut names: BTreeMap<String, Uuid> = BTreeMap::new();
        for pool in pools {
            if let Some(existing) = by_id.get(&pool.id) {
                return Err(Error::Backend(format!(
                    "duplicate storage pool id {}: {:?} and {:?}",
                    pool.id, existing.name, pool.name
                )));
            }
            if let Some(other) = names.get(&pool.name) {
                return Err(Error::Backend(format!(
                    "duplicate storage pool name {:?}: {other} and {}",
                    pool.name, pool.id
                )));
            }
            names.insert(pool.name.clone(), pool.id);
            by_id.insert(pool.id, pool);
        }
        Ok(Self { pools: by_id })
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &PoolSpec> {
        self.pools.values()
    }

    /// A pool by id. An unknown id is an error, never a fallback.
    pub fn get(&self, id: Uuid) -> Result<&PoolSpec> {
        self.pools.get(&id).ok_or(Error::UnknownPool(id))
    }

    pub fn by_name(&self, name: &str) -> Result<&PoolSpec> {
        self.pools
            .values()
            .find(|p| p.name == name)
            .ok_or_else(|| Error::Backend(format!("no storage pool named {name:?}")))
    }

    /// Chooses where to read from.
    ///
    /// Disabled pools are included: `enabled = false` retires a pool from *new* placements,
    /// and refusing to read from it would turn a configuration change into a data outage
    /// for every object already there.
    pub fn resolve_read(
        &self,
        candidates: &[PlacementRef],
        now: DateTime<Utc>,
    ) -> Result<ReadPlan> {
        if candidates.is_empty() {
            return Err(Error::NoUsablePlacement {
                reason: "the object has no placements at all".into(),
            });
        }

        // Resolve every pool first, so an unknown one is an error even when another copy
        // would have served. Skipping it here is what hides configuration drift.
        let resolved: Vec<(&PoolSpec, &PlacementRef)> = candidates
            .iter()
            .map(|p| self.get(p.pool_id).map(|spec| (spec, p)))
            .collect::<Result<_>>()?;

        // Readable now, cheapest first, name as a stable tiebreak.
        let ready = resolved
            .iter()
            .filter(|(_, p)| p.is_readable_at(now))
            .map(|(spec, p)| (Self::read_cost(spec, p), &spec.name, spec.id))
            .min();
        if let Some((cost, _, pool_id)) = ready {
            return Ok(ReadPlan::Ready { pool_id, cost });
        }

        // Nothing readable. A restore is possible only from a copy that is actually there.
        let restorable = resolved
            .iter()
            .filter(|(_, p)| {
                matches!(p.state, PlacementState::Present) && p.storage_class.requires_restore()
            })
            .map(|(spec, p)| {
                (
                    Self::read_cost(spec, p),
                    &spec.name,
                    spec.id,
                    spec.latency_class,
                )
            });
        if let Some((estimated_cost, _, pool_id, latency_class)) = restorable.min() {
            return Ok(ReadPlan::Restore {
                pool_id,
                estimated_cost,
                latency_class,
            });
        }

        // Neither readable nor restorable: say why each copy failed. An operator with
        // "no usable placement" and nothing else has to go and query the table by hand.
        let reasons: Vec<String> = resolved
            .iter()
            .map(|(spec, p)| format!("{} is {}", spec.name, Self::unusable_reason(p, now)))
            .collect();
        Err(Error::NoUsablePlacement {
            reason: reasons.join("; "),
        })
    }

    /// Chooses where to write a new object.
    pub fn resolve_write(&self, intent: WriteIntent, key: &Key) -> Result<&PoolSpec> {
        // A tier-exempt object (proxy, thumbnail, detached manifest) is permanently hot and
        // often small, so pool minimums decide. Everything else needs an instant pool: an
        // original ingested straight into Deep Archive starts a 180-day minimum charge
        // before anyone has previewed it, and could not be probed or derived from without
        // a restore.
        let strict = key.is_tier_exempt() || matches!(intent, WriteIntent::Derived);
        let requirement = if strict {
            "instant, with no minimum billable size or duration"
        } else {
            "instant"
        };

        self.pools
            .values()
            .filter(|p| p.enabled)
            .filter(|p| {
                if strict {
                    p.suits_small_permanent_objects()
                } else {
                    p.is_instant()
                }
            })
            // Storage price, then name — writes are ranked on what holding the object
            // costs, not on retrieval, because most objects are read rarely.
            .min_by(|a, b| {
                a.cost_per_gb_month
                    .cmp(&b.cost_per_gb_month)
                    .then_with(|| a.name.cmp(&b.name))
            })
            .ok_or_else(|| Error::NoUsablePlacement {
                reason: format!(
                    "no enabled pool is {requirement}, which {} requires",
                    key.as_str()
                ),
            })
    }

    /// What one read of this placement is estimated to cost.
    fn read_cost(spec: &PoolSpec, placement: &PlacementRef) -> Rate {
        spec.cost_per_gb_retrieval
            .per_gb(placement.size_bytes)
            .plus(spec.cost_per_1k_requests.per_requests(1))
    }

    fn unusable_reason(placement: &PlacementRef, now: DateTime<Utc>) -> String {
        match placement.state {
            PlacementState::Present => match placement.restore_state {
                RestoreState::Available => format!(
                    "restored but the copy expired at {}",
                    placement
                        .restore_expires_at
                        .map_or_else(|| "an unrecorded time".to_owned(), |e| e.to_rfc3339())
                ),
                RestoreState::Ongoing | RestoreState::Requested => {
                    "waiting on a restore".to_owned()
                }
                _ => format!("in {} with no restore", placement.storage_class),
            },
            other => {
                let _ = now;
                other.as_str().to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_renders_as_currency_and_converts_exactly_from_the_database_scale() {
        assert_eq!(Rate::from_db_units(2_300_000).to_string(), "0.023000000000");
        assert_eq!(Rate::from_units(Rate::SCALE).to_string(), "1.000000000000");
        assert_eq!(
            Rate::from_db_units(1).units(),
            Rate::DB_SCALE_FACTOR,
            "the finest value the database can express must survive conversion"
        );
    }

    #[test]
    fn a_per_gb_rate_scales_by_binary_gigabytes() {
        let rate = Rate::from_db_units(1_000_000); // $0.01/GB
        assert_eq!(rate.per_gb(1_073_741_824), rate, "exactly one GiB");
        assert_eq!(rate.per_gb(0), Rate::from_units(0));
        // A 100 TB object would overflow a naive u64 multiply; saturating keeps the
        // ordering sane instead of wrapping to something cheap-looking.
        assert!(rate.per_gb(u64::MAX) > rate);
    }

    #[test]
    fn a_single_request_charge_does_not_truncate_away() {
        // S3 GET pricing: $0.0004 per 1,000 requests. At the database's own 1e-8 scale this
        // divides to zero, which would drop request costs out of every estimate and make
        // two pools differing only in request price compare equal.
        let per_1k = Rate::from_db_units(40);
        assert_ne!(
            per_1k.per_requests(1),
            Rate::default(),
            "one request must cost something"
        );
        assert_eq!(
            per_1k.per_requests(1000),
            per_1k,
            "1,000 of them is the rate"
        );
    }

    #[test]
    fn the_pool_prefix_is_joined_without_doubling_the_separator() {
        let mut spec = PoolSpec {
            id: Uuid::nil(),
            name: "hot".into(),
            driver: Driver::S3,
            endpoint: None,
            region: None,
            bucket: "b".into(),
            prefix: "damrs/".into(),
            force_path_style: false,
            credentials_ref: "env:AWS".into(),
            storage_class: StorageClass::Standard,
            latency_class: LatencyClass::Instant,
            immutable: false,
            min_duration_days: 0,
            min_billable_bytes: 0,
            cost_per_gb_month: Rate::default(),
            cost_per_gb_retrieval: Rate::default(),
            cost_per_1k_requests: Rate::default(),
            enabled: true,
        };
        let key = Key::new("t/o/aa").expect("key");
        assert_eq!(spec.object_path(&key), "damrs/t/o/aa");
        spec.prefix = String::new();
        assert_eq!(spec.object_path(&key), "t/o/aa");
    }

    #[test]
    fn every_driver_round_trips_through_its_database_spelling() {
        for driver in [Driver::S3, Driver::Azure, Driver::Fs, Driver::Tape] {
            assert_eq!(
                driver.as_str().parse::<Driver>().expect("round trip"),
                driver
            );
        }
        assert!("gcs".parse::<Driver>().is_err());
    }
}
