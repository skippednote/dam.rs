//! The lifecycle engine (§6.4): deciding what moves to which tier.
//!
//! damrs drives transitions itself rather than delegating to S3 lifecycle rules, for two reasons
//! §6.4 gives: cross-provider tiering cannot be expressed as an S3 rule at all, and self-driven
//! transitions keep `object_placements` authoritative instead of eventually-consistent.
//!
//! This module *plans*; it does not move anything. The separation is the point — a plan can be read,
//! diffed, and approved before terabytes of a customer's masters go somewhere they cannot be read
//! back from for 48 hours.
//!
//! ## Three defaults chosen to fail safely
//!
//! - **Dry run is on.** A policy that executed on creation would move every eligible object before
//!   anyone read what it would do.
//! - **Nothing is dropped silently.** Every candidate appears in the plan exactly once, either as a
//!   transition or as a skip *with a reason*. An object that is neither moved nor explained is
//!   indistinguishable from one the engine forgot.
//! - **A truncated run says so.** [`HaltReason::ObjectLimit`] carries how many were left, because a
//!   run that quietly stops at its limit looks exactly like a policy that is working.
//!
//! ## The billing traps
//!
//! `STANDARD_IA` bills a 30-day minimum, `GLACIER_IR` and `GLACIER` 90, `DEEP_ARCHIVE` 180. The same
//! counter charges a minimum on the class an object is *leaving* and blocks a premature second hop,
//! so `min_duration_until` is checked before any transition and written forward on every one.

use crate::Key;
use chrono::{DateTime, Duration, Utc};
use dam_core::{PlacementState, RestoreState, StorageClass};
use uuid::Uuid;

/// One `object_placements` row, as the engine reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub object_key: Key,
    pub pool_id: Uuid,
    pub size_bytes: u64,
    pub state: PlacementState,
    pub storage_class: StorageClass,
    pub restore_state: RestoreState,
    /// Blocks tiering unconditionally: legal hold, a live portal reference, a `pin_hot` collection.
    pub pinned: bool,
    pub pin_reason: Option<String>,
    /// When the minimum billable duration for the current class elapses.
    pub min_duration_until: Option<DateTime<Utc>>,
    pub placed_at: DateTime<Utc>,
    /// `None` means never read, which ages from `placed_at` — see [`Candidate::idle_since`].
    pub last_accessed_at: Option<DateTime<Utc>>,
}

impl Candidate {
    /// The instant from which idleness is measured.
    ///
    /// A null `last_accessed_at` must mean neither "infinitely recent" (nothing ever tiers) nor
    /// "epoch" (everything tiers at once). Both readings are plausible and both are wrong: an object
    /// nobody has ever opened is exactly what a tiering policy is for, aged from when it arrived.
    pub fn idle_since(&self) -> DateTime<Utc> {
        self.last_accessed_at.unwrap_or(self.placed_at)
    }
}

/// A tiering rule, mirroring `lifecycle_policies`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecyclePolicy {
    pub name: String,
    pub enabled: bool,
    pub target_class: StorageClass,
    /// Move once nothing has read the object for this many days.
    pub after_days: u32,
    /// Noncurrent versions only. Not executable yet — see [`HaltReason::Unsupported`].
    pub only_superseded: bool,
    /// Floor below which tiering costs more than it saves: IA and Glacier IR bill a 128 KiB minimum,
    /// so a 20 KB object is more expensive there than in Standard.
    pub min_size_bytes: Option<u64>,
    /// Cap on objects touched per run, so one pass cannot move a whole library.
    pub max_objects_per_run: Option<u32>,
    /// Plan only. **True by default**, and the only way to turn it off is to say so.
    pub dry_run: bool,
}

impl LifecyclePolicy {
    /// A policy that plans but does not execute.
    ///
    /// There is deliberately no `Default` and no builder that ends in execution: turning off the dry
    /// run is a decision that should appear in a diff.
    pub fn new(name: &str, target_class: StorageClass, after_days: u32) -> Self {
        Self {
            name: name.to_owned(),
            enabled: true,
            target_class,
            after_days,
            only_superseded: false,
            min_size_bytes: None,
            max_objects_per_run: None,
            dry_run: true,
        }
    }
}

/// What the engine decided about one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Transition {
        from: StorageClass,
        to: StorageClass,
    },
    Skipped(SkipReason),
}

/// Why an object was not moved.
///
/// Every variant carries what an operator needs to act. "Skipped" alone turns a plan into a mystery,
/// and the first question is always "why did nothing happen?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Pinned {
        reason: Option<String>,
    },
    /// A proxy, thumbnail, detached manifest or staging object — the §2 search substrate, which must
    /// stay hot whatever a policy says.
    TierExempt,
    NotYetEligible {
        eligible_at: DateTime<Utc>,
    },
    MinDurationNotElapsed {
        until: DateTime<Utc>,
    },
    AlreadyInClass,
    /// Cold to warm is a restore, not a transition. A tiering policy that could do it by accident
    /// would produce a very large surprise bill from a configuration typo.
    WouldWarm {
        from: StorageClass,
        to: StorageClass,
    },
    NotPresent {
        state: PlacementState,
    },
    /// Somebody may be downloading the temporary copy right now, and the retrieval fee is spent.
    RestoreInFlight {
        state: RestoreState,
    },
    BelowMinimumSize {
        size: u64,
        minimum: u64,
    },
}

/// Why a run stopped early or did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltReason {
    PolicyDisabled,
    ObjectLimit {
        limit: u32,
        /// How many candidates were left unexamined. Reported so a truncated run is visibly
        /// truncated.
        remaining: usize,
    },
    /// The policy is representable in the database but not executable yet.
    Unsupported {
        what: String,
    },
}

/// A planned transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTransition {
    pub object_key: Key,
    pub pool_id: Uuid,
    pub from: StorageClass,
    pub to: StorageClass,
    pub size_bytes: u64,
    /// The minimum-duration counter this move starts. Written forward so the next run knows the
    /// second hop is not free.
    pub min_duration_until: Option<DateTime<Utc>>,
}

/// The outcome of a planning pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub policy_name: String,
    pub dry_run: bool,
    pub halted: Option<HaltReason>,
    /// Every candidate, in key order, paired with what the engine decided.
    entries: Vec<(Key, Verdict)>,
    transitions: Vec<PlannedTransition>,
}

impl Plan {
    pub fn transitions(&self) -> impl Iterator<Item = &PlannedTransition> {
        self.transitions.iter()
    }

    pub fn skipped(&self) -> impl Iterator<Item = (&Key, &SkipReason)> {
        self.entries
            .iter()
            .filter_map(|(key, verdict)| match verdict {
                Verdict::Skipped(reason) => Some((key, reason)),
                Verdict::Transition { .. } => None,
            })
    }

    pub fn verdicts(&self) -> impl Iterator<Item = &Verdict> {
        self.entries.iter().map(|(_, v)| v)
    }

    pub fn bytes_to_move(&self) -> u64 {
        self.transitions.iter().map(|t| t.size_bytes).sum()
    }

    /// One line for an operator about to enable a policy that will move terabytes.
    pub fn summary(&self) -> String {
        let mut line = format!(
            "{}: {} object(s), {} bytes to {}{}",
            self.policy_name,
            self.transitions.len(),
            self.bytes_to_move(),
            self.transitions
                .first()
                .map_or_else(|| "nothing".to_owned(), |t| t.to.as_s3().to_owned()),
            if self.dry_run { " (dry run)" } else { "" }
        );
        if let Some(halt) = &self.halted {
            line.push_str(&format!("; halted: {halt:?}"));
        }
        line
    }
}

/// Decides what would move, without moving it.
///
/// Candidates are examined in key order, so a plan reads like a bucket listing and two runs over the
/// same data produce the same plan — which is what makes a dry run worth reading.
pub fn plan(policy: &LifecyclePolicy, candidates: &[Candidate], now: DateTime<Utc>) -> Plan {
    let mut plan = Plan {
        policy_name: policy.name.clone(),
        dry_run: policy.dry_run,
        halted: None,
        entries: Vec::new(),
        transitions: Vec::new(),
    };

    if !policy.enabled {
        plan.halted = Some(HaltReason::PolicyDisabled);
        return plan;
    }
    if policy.only_superseded {
        // Matching nothing would be indistinguishable from "no objects are due", and a policy that
        // looks configured while doing nothing is worse than one that refuses: nobody investigates a
        // quiet success. `object_placements` is keyed (object_key, pool_id) with no version
        // dimension, so a noncurrent version cannot be identified here at all.
        plan.halted = Some(HaltReason::Unsupported {
            what: "only_superseded: object_placements has no version dimension, so a superseded \
                   version cannot be identified — see the note against 1.10 in TASKS.md"
                .to_owned(),
        });
        return plan;
    }

    let mut ordered: Vec<&Candidate> = candidates.iter().collect();
    ordered.sort_by(|a, b| a.object_key.cmp(&b.object_key));

    let limit = policy.max_objects_per_run.map(|l| l as usize);
    for (index, candidate) in ordered.iter().enumerate() {
        if let Some(limit) = limit
            && plan.transitions.len() >= limit
        {
            plan.halted = Some(HaltReason::ObjectLimit {
                limit: limit as u32,
                remaining: ordered.len() - index,
            });
            break;
        }

        let verdict = examine(policy, candidate, now);
        if let Verdict::Transition { from, to } = &verdict {
            plan.transitions.push(PlannedTransition {
                object_key: candidate.object_key.clone(),
                pool_id: candidate.pool_id,
                from: *from,
                to: *to,
                size_bytes: candidate.size_bytes,
                min_duration_until: minimum_duration_from(*to, now),
            });
        }
        plan.entries.push((candidate.object_key.clone(), verdict));
    }

    plan
}

/// The decision for one object.
///
/// Ordered cheapest-and-most-absolute first: a pinned object is never tiered regardless of anything
/// else, so asking about eligibility first would let a bug in the arithmetic override a legal hold.
fn examine(policy: &LifecyclePolicy, candidate: &Candidate, now: DateTime<Utc>) -> Verdict {
    if candidate.pinned {
        return Verdict::Skipped(SkipReason::Pinned {
            reason: candidate.pin_reason.clone(),
        });
    }
    if candidate.object_key.is_tier_exempt() {
        return Verdict::Skipped(SkipReason::TierExempt);
    }
    if !matches!(candidate.state, PlacementState::Present) {
        return Verdict::Skipped(SkipReason::NotPresent {
            state: candidate.state,
        });
    }
    if matches!(
        candidate.restore_state,
        RestoreState::Requested | RestoreState::Ongoing | RestoreState::Available
    ) {
        return Verdict::Skipped(SkipReason::RestoreInFlight {
            state: candidate.restore_state,
        });
    }
    if candidate.storage_class == policy.target_class {
        return Verdict::Skipped(SkipReason::AlreadyInClass);
    }
    if is_warmer(policy.target_class, candidate.storage_class) {
        return Verdict::Skipped(SkipReason::WouldWarm {
            from: candidate.storage_class,
            to: policy.target_class,
        });
    }
    if let Some(minimum) = policy.min_size_bytes
        && candidate.size_bytes < minimum
    {
        return Verdict::Skipped(SkipReason::BelowMinimumSize {
            size: candidate.size_bytes,
            minimum,
        });
    }

    let eligible_at = candidate.idle_since() + Duration::days(i64::from(policy.after_days));
    if now < eligible_at {
        return Verdict::Skipped(SkipReason::NotYetEligible { eligible_at });
    }
    if let Some(until) = candidate.min_duration_until
        && now < until
    {
        // Inclusive boundary: at the instant the minimum elapses the charge is settled and the hop
        // is free. Rejecting *at* the boundary would cost a full minimum period per object.
        return Verdict::Skipped(SkipReason::MinDurationNotElapsed { until });
    }

    Verdict::Transition {
        from: candidate.storage_class,
        to: policy.target_class,
    }
}

/// Whether `target` is warmer than `current` — i.e. the move would be a restore, not a transition.
///
/// Ranked by retrieval latency and cost rather than by name, so a new class slots in by its
/// characteristics.
fn is_warmer(target: StorageClass, current: StorageClass) -> bool {
    coldness(target) < coldness(current)
}

fn coldness(class: StorageClass) -> u8 {
    match class {
        StorageClass::Standard => 0,
        StorageClass::IntelligentTiering => 1,
        StorageClass::OnezoneIa => 2,
        StorageClass::StandardIa => 3,
        StorageClass::GlacierIr => 4,
        StorageClass::Glacier => 5,
        StorageClass::DeepArchive => 6,
    }
}

/// When the minimum billable duration for `class` elapses, starting now.
fn minimum_duration_from(class: StorageClass, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let days = class.min_duration_days();
    (days > 0).then(|| now + Duration::days(i64::from(days)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coldness_is_a_total_order_from_standard_to_deep_archive() {
        let ordered = [
            StorageClass::Standard,
            StorageClass::IntelligentTiering,
            StorageClass::OnezoneIa,
            StorageClass::StandardIa,
            StorageClass::GlacierIr,
            StorageClass::Glacier,
            StorageClass::DeepArchive,
        ];
        for pair in ordered.windows(2) {
            assert!(
                coldness(pair[0]) < coldness(pair[1]),
                "{:?} should be warmer than {:?}",
                pair[0],
                pair[1]
            );
            assert!(is_warmer(pair[0], pair[1]));
            assert!(!is_warmer(pair[1], pair[0]));
        }
    }

    #[test]
    fn standard_starts_no_minimum_duration_clock() {
        let now = Utc::now();
        assert!(minimum_duration_from(StorageClass::Standard, now).is_none());
        assert_eq!(
            minimum_duration_from(StorageClass::DeepArchive, now),
            Some(now + Duration::days(180))
        );
    }

    #[test]
    fn a_new_policy_plans_rather_than_executes() {
        // Asserted here as well as in the integration suite, because this default is the difference
        // between a report and an incident.
        assert!(LifecyclePolicy::new("p", StorageClass::GlacierIr, 90).dry_run);
    }
}
