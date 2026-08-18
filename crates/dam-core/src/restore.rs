//! Planning a restore (3.4, ARCHITECTURE §6.5).
//!
//! A download that resolves to an archived object cannot be served: the bytes are in Glacier or Deep
//! Archive and a `GET` fails until a restore completes. §6.5 makes that a `202` with an ETA and a cost
//! estimate rather than an error, and this module is the arithmetic behind that response.
//!
//! ## The estimate exists because the spread is tenfold
//!
//! Expedited against Bulk is roughly 10× on price and 100× on latency. A user picking a tier without seeing
//! either number is guessing, and the guess is billed to their employer — so §6.5 requires the estimate
//! *before* they confirm, and an approval step above a threshold.
//!
//! ## Expedited on Deep Archive is refused, not downgraded
//!
//! Deep Archive has no Expedited tier. Silently substituting Standard would answer a request for "five
//! minutes" with twelve hours and no explanation — the user would sit waiting for something that was never
//! going to happen on that timescale. Refusing names the constraint and lets them choose Standard knowingly,
//! or move the asset to a warmer class.
//!
//! ## A restore is a temporary copy
//!
//! The object's storage class does not change. That is why `restore_state` and `restore_expires_at` are
//! separate fields, and why [`Plan::expires_at`] matters: a delivery URL that outlives the temporary copy
//! must stop working, or it 403s from S3 with nothing explaining why.

use crate::storage::{RestoreTier, StorageClass};
use chrono::{DateTime, Duration, Utc};

/// How long a restored copy is kept warm by default.
///
/// Seven days rather than one: the common case is a person restoring an archived shoot to work on it, and a
/// copy that vanishes overnight means restoring twice and paying twice.
pub const DEFAULT_KEEP_WARM_DAYS: i64 = 7;

/// What a restore is expected to cost and take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The tier that will actually be used.
    pub tier: RestoreTier,
    /// Bytes to be restored, across every object in the batch.
    pub bytes: u64,
    /// Objects in the batch.
    pub objects: u64,
    /// Estimated cost in whole cents, rounded **up**.
    ///
    /// Up, because an estimate a user approves should not come in over. A restore that costs a cent more
    /// than the number somebody signed off is a conversation nobody wants to have.
    pub est_cost_cents: u64,
    /// When the copy is expected to be available. Derived from the class and tier, not measured.
    pub eta_at: DateTime<Utc>,
    /// When the temporary copy lapses.
    pub expires_at: DateTime<Utc>,
    /// Whether this needs an administrator before it proceeds.
    pub needs_approval: bool,
}

/// Why a restore cannot be planned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RestoreRefusal {
    #[error(
        "{class} objects are already instantly readable, so there is nothing to restore — asking for one \
         means something upstream resolved the wrong placement"
    )]
    NotArchived { class: StorageClass },

    #[error(
        "{class} has no {tier} tier; choose Standard or Bulk, or move the asset to a warmer class. \
         Substituting a slower tier silently would answer a request for minutes with hours"
    )]
    TierUnavailable {
        class: StorageClass,
        tier: RestoreTier,
    },

    #[error(
        "this restore would cost about {est_cents} cents, over the {budget_cents}-cent limit for a single \
         request"
    )]
    OverRequestBudget { est_cents: u64, budget_cents: u64 },

    #[error(
        "this tenant has spent {spent_cents} of {budget_cents} cents on restores this month, and this \
         request would add about {est_cents}"
    )]
    OverMonthlyBudget {
        spent_cents: u64,
        budget_cents: u64,
        est_cents: u64,
    },

    #[error("a restore of nothing is not a restore")]
    Empty,
}

/// The retrieval prices a pool charges, in units of 1e-12 of the billing currency per GB.
///
/// Supplied by the caller from `storage_pools` rather than hard-coded: prices differ per region and per
/// provider, and a constant here would be wrong for everyone within a year.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetrievalPrices {
    pub expedited_per_gb: u64,
    pub standard_per_gb: u64,
    pub bulk_per_gb: u64,
    /// Per 1,000 requests, since a restore is billed per object as well as per byte. At 400 objects this is
    /// no longer a rounding error, which is exactly the case a collection restore hits.
    pub per_1000_requests: u64,
}

impl RetrievalPrices {
    fn per_gb(self, tier: RestoreTier) -> u64 {
        match tier {
            RestoreTier::Expedited => self.expedited_per_gb,
            RestoreTier::Standard => self.standard_per_gb,
            RestoreTier::Bulk => self.bulk_per_gb,
        }
    }
}

/// The guardrails §6.5 asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Refused outright above this.
    pub per_request_cents: Option<u64>,
    /// Refused when the month's spend plus this request would exceed it.
    pub monthly_cents: Option<u64>,
    /// Held for an administrator above this. Distinct from a refusal: a large restore is often legitimate,
    /// and the answer is "somebody senior confirms" rather than "no".
    pub approval_threshold_cents: Option<u64>,
    /// What the tenant has already spent this month.
    pub spent_this_month_cents: u64,
}

impl Default for Budget {
    /// Deliberately permissive, and deliberately not unlimited.
    ///
    /// A default of "no budget at all" makes the guardrail opt-in, and the failure mode of an opt-in cost
    /// control is a surprise invoice. A default of zero would make every restore need approval before anyone
    /// had configured anything, which teaches people to disable it.
    fn default() -> Self {
        Self {
            per_request_cents: None,
            monthly_cents: None,
            // About fifty dollars. Large enough that ordinary work proceeds, small enough that a
            // mis-clicked expedited restore of a whole archive stops for a human.
            approval_threshold_cents: Some(5_000),
            spent_this_month_cents: 0,
        }
    }
}

/// One object to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub bytes: u64,
    pub class: StorageClass,
}

/// Plans a restore of `candidates` at `tier`.
///
/// Every object must be in the same class: a batch spanning Glacier and Deep Archive has two ETAs and two
/// prices, and averaging them would give a number that is wrong for both. The caller groups by class, which
/// is also how the S3 calls have to be issued.
pub fn plan(
    candidates: &[Candidate],
    tier: RestoreTier,
    prices: RetrievalPrices,
    budget: &Budget,
    keep_warm_days: i64,
    now: DateTime<Utc>,
) -> Result<Plan, RestoreRefusal> {
    let first = candidates.first().ok_or(RestoreRefusal::Empty)?;
    let class = first.class;

    if !class.requires_restore() {
        // Reaching here means something upstream resolved a placement that did not need restoring, and
        // quietly succeeding would hide that.
        return Err(RestoreRefusal::NotArchived { class });
    }
    if !tier.is_available_for(class) {
        return Err(RestoreRefusal::TierUnavailable { class, tier });
    }

    let bytes: u64 = candidates.iter().map(|c| c.bytes).sum();
    let objects = candidates.len() as u64;

    let est_cost_cents = estimate_cents(bytes, objects, tier, prices);

    if let Some(limit) = budget.per_request_cents
        && est_cost_cents > limit
    {
        return Err(RestoreRefusal::OverRequestBudget {
            est_cents: est_cost_cents,
            budget_cents: limit,
        });
    }
    if let Some(limit) = budget.monthly_cents
        && budget.spent_this_month_cents.saturating_add(est_cost_cents) > limit
    {
        return Err(RestoreRefusal::OverMonthlyBudget {
            spent_cents: budget.spent_this_month_cents,
            budget_cents: limit,
            est_cents: est_cost_cents,
        });
    }

    let needs_approval = budget
        .approval_threshold_cents
        .is_some_and(|threshold| est_cost_cents > threshold);

    let eta_at = now + worst_case_latency(class, tier);
    Ok(Plan {
        tier,
        bytes,
        objects,
        est_cost_cents,
        eta_at,
        expires_at: eta_at + Duration::days(keep_warm_days.max(1)),
        needs_approval,
    })
}

/// The cost of restoring `bytes` across `objects`, in whole cents rounded up.
pub fn estimate_cents(bytes: u64, objects: u64, tier: RestoreTier, prices: RetrievalPrices) -> u64 {
    const GB: u128 = 1_073_741_824;
    /// 1e-12 units per cent.
    const UNITS_PER_CENT: u128 = 10_000_000_000;

    let per_gb = u128::from(prices.per_gb(tier));
    let byte_cost = per_gb.saturating_mul(u128::from(bytes)) / GB;
    // Per object as well as per byte. At 400 objects the request term stops being a rounding error, which is
    // exactly the shape of a collection restore.
    let request_cost =
        u128::from(prices.per_1000_requests).saturating_mul(u128::from(objects)) / 1_000;

    let total = byte_cost.saturating_add(request_cost);
    // Rounded **up**: an estimate somebody approves should not be exceeded by the actual charge.
    let cents = total.div_ceil(UNITS_PER_CENT);
    u64::try_from(cents).unwrap_or(u64::MAX)
}

/// The slow end of the documented window for a class and tier.
///
/// The slow end, not the middle. An ETA is a promise, and a promise made from an average is broken half the
/// time — which is worse than a pessimistic one met early.
pub fn worst_case_latency(class: StorageClass, tier: RestoreTier) -> Duration {
    match (class, tier) {
        // Glacier: Expedited 1–5 min, Standard 3–5 h, Bulk 5–12 h.
        (StorageClass::Glacier, RestoreTier::Expedited) => Duration::minutes(5),
        (StorageClass::Glacier, RestoreTier::Standard) => Duration::hours(5),
        (StorageClass::Glacier, RestoreTier::Bulk) => Duration::hours(12),
        // Deep Archive: no Expedited, Standard ~12 h, Bulk ~48 h. The Expedited arm is unreachable through
        // `plan`, and answering with the Standard window rather than something absurd keeps a direct caller
        // from reading a nonsense ETA.
        (StorageClass::DeepArchive, RestoreTier::Standard | RestoreTier::Expedited) => {
            Duration::hours(12)
        }
        (StorageClass::DeepArchive, RestoreTier::Bulk) => Duration::hours(48),
        // Not archived, so nothing to wait for. `plan` refuses these before asking.
        _ => Duration::zero(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_eta_uses_the_slow_end_of_the_window() {
        // A promise made from an average is broken half the time.
        assert_eq!(
            worst_case_latency(StorageClass::Glacier, RestoreTier::Standard),
            Duration::hours(5)
        );
        assert_eq!(
            worst_case_latency(StorageClass::DeepArchive, RestoreTier::Bulk),
            Duration::hours(48)
        );
    }

    #[test]
    fn the_cost_estimate_rounds_up() {
        // An estimate somebody approved must not be exceeded by the charge. A cost of 1.2 cents is quoted as
        // 2, not 1.
        let prices = RetrievalPrices {
            standard_per_gb: 12_000_000_000, // 1.2 cents per GB
            ..RetrievalPrices::default()
        };
        assert_eq!(
            estimate_cents(1_073_741_824, 1, RestoreTier::Standard, prices),
            2
        );
    }

    #[test]
    fn the_per_object_term_survives_at_scale() {
        // A collection restore is hundreds of objects. Dropping the request term makes a 400-object restore
        // look free, which is the kind of estimate that stops being believed.
        let prices = RetrievalPrices {
            // 0.4 cents per 1,000 requests.
            per_1000_requests: 4_000_000_000,
            ..RetrievalPrices::default()
        };
        assert_eq!(estimate_cents(0, 1_000, RestoreTier::Bulk, prices), 1);
        assert_eq!(
            estimate_cents(0, 1, RestoreTier::Bulk, prices),
            1,
            "and a single request still rounds up rather than vanishing"
        );
    }
}
