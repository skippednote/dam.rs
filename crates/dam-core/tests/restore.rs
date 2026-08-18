//! Planning a restore (3.4, §6.5).
//!
//! The numbers here reach a user before they spend money, so the interesting cases are the ones where a
//! plausible implementation quotes something wrong or promises something it cannot deliver.
//!
//! Expedited against Bulk is roughly **10× on price and 100× on latency**. That spread is why §6.5 requires
//! the estimate before confirmation and an approval step above a threshold — and why every guardrail here is
//! about being wrong in the safe direction: round the cost up, quote the slow end of the latency window, and
//! refuse a tier that does not exist rather than substituting a slower one.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::restore::{self, Budget, Candidate, Plan, RestoreRefusal, RetrievalPrices};
use dam_core::storage::{RestoreTier, StorageClass};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

const GB: u64 = 1_073_741_824;

/// Prices roughly in the shape of S3's, expressed in 1e-12 units per GB.
///
/// Bulk about a tenth of Expedited, which is the spread the guardrails exist for.
fn prices() -> RetrievalPrices {
    RetrievalPrices {
        expedited_per_gb: 30_000_000_000, // 3 cents/GB
        standard_per_gb: 10_000_000_000,  // 1 cent/GB
        bulk_per_gb: 2_500_000_000,       // 0.25 cents/GB
        per_1000_requests: 5_000_000_000, // 0.5 cents/1000
    }
}

fn glacier(gb: u64) -> Vec<Candidate> {
    vec![Candidate {
        bytes: gb * GB,
        class: StorageClass::Glacier,
    }]
}

fn plan_with(
    candidates: &[Candidate],
    tier: RestoreTier,
    budget: &Budget,
) -> Result<Plan, RestoreRefusal> {
    restore::plan(candidates, tier, prices(), budget, 7, now())
}

fn permissive() -> Budget {
    Budget {
        per_request_cents: None,
        monthly_cents: None,
        approval_threshold_cents: None,
        spent_this_month_cents: 0,
    }
}

// ─── tiers ──────────────────────────────────────────────────────────────────

#[test]
fn expedited_on_deep_archive_is_refused_not_quietly_downgraded() {
    // Deep Archive has no Expedited tier. Substituting Standard would answer a request for five minutes with
    // twelve hours and no explanation — the user waits for something that was never going to happen on that
    // timescale. Refusing names the constraint so they can choose Standard knowingly or move the asset.
    let deep = vec![Candidate {
        bytes: GB,
        class: StorageClass::DeepArchive,
    }];
    let refused = plan_with(&deep, RestoreTier::Expedited, &permissive()).expect_err("must refuse");
    assert_eq!(
        refused,
        RestoreRefusal::TierUnavailable {
            class: StorageClass::DeepArchive,
            tier: RestoreTier::Expedited,
        }
    );
    // The message has to say what to do instead, or it is just a rejection.
    let message = refused.to_string();
    assert!(
        message.contains("Standard") && message.contains("Bulk"),
        "got {message}"
    );
}

#[test]
fn every_tier_glacier_offers_is_plannable() {
    for tier in [
        RestoreTier::Expedited,
        RestoreTier::Standard,
        RestoreTier::Bulk,
    ] {
        plan_with(&glacier(1), tier, &permissive())
            .unwrap_or_else(|e| panic!("glacier must offer {tier:?}: {e}"));
    }
}

#[test]
fn restoring_something_already_instant_is_refused() {
    // Reaching here means something upstream resolved a placement that needed no restore. Quietly succeeding
    // would hide that, and the user would wait for a job with nothing to do.
    for class in [StorageClass::Standard, StorageClass::GlacierIr] {
        let refused = plan_with(
            &[Candidate { bytes: GB, class }],
            RestoreTier::Standard,
            &permissive(),
        )
        .expect_err("must refuse");
        assert_eq!(refused, RestoreRefusal::NotArchived { class });
    }
}

#[test]
fn an_empty_restore_is_refused() {
    assert_eq!(
        plan_with(&[], RestoreTier::Bulk, &permissive()).expect_err("must refuse"),
        RestoreRefusal::Empty
    );
}

// ─── the estimate ───────────────────────────────────────────────────────────

#[test]
fn the_tenfold_spread_between_tiers_shows_up_in_the_estimate() {
    // The whole reason the estimate is shown before confirmation. If the numbers did not differ, there would
    // be nothing to choose between and nothing to approve.
    let expedited = plan_with(&glacier(100), RestoreTier::Expedited, &permissive())
        .expect("plan")
        .est_cost_cents;
    let bulk = plan_with(&glacier(100), RestoreTier::Bulk, &permissive())
        .expect("plan")
        .est_cost_cents;
    assert!(
        expedited >= bulk * 10,
        "expedited {expedited} should be about ten times bulk {bulk}"
    );
}

#[test]
fn the_estimate_rounds_up_so_the_charge_cannot_exceed_it() {
    // A restore that costs a cent more than the figure somebody signed off is a conversation nobody wants.
    let prices = RetrievalPrices {
        standard_per_gb: 1_000_000_000, // 0.1 cents/GB
        per_1000_requests: 0,
        ..RetrievalPrices::default()
    };
    // 0.1 cents must quote as 1, not 0.
    assert_eq!(
        restore::estimate_cents(GB, 1, RestoreTier::Standard, prices),
        1
    );
}

#[test]
fn a_large_object_count_is_priced_not_ignored() {
    // A collection restore is hundreds of objects, and S3 bills per object as well as per byte. Dropping the
    // request term makes a 400-object restore of small files look free.
    let many = vec![
        Candidate {
            bytes: 1_024,
            class: StorageClass::Glacier,
        };
        400
    ];
    let plan = plan_with(&many, RestoreTier::Bulk, &permissive()).expect("plan");
    assert_eq!(plan.objects, 400);
    assert!(
        plan.est_cost_cents > 0,
        "400 objects must cost something even when the bytes round to nothing"
    );
}

#[test]
fn an_absurd_size_saturates_rather_than_overflowing() {
    // A cost estimate must not panic on absurd input — this runs on a request path, and a panic there is a
    // denial of service reachable by anyone who can name a big enough object.
    let huge = vec![Candidate {
        bytes: u64::MAX,
        class: StorageClass::Glacier,
    }];
    let plan = plan_with(&huge, RestoreTier::Expedited, &permissive()).expect("plan");
    assert!(plan.est_cost_cents > 0);
}

// ─── budgets and approval ───────────────────────────────────────────────────

#[test]
fn a_restore_over_the_per_request_budget_is_refused_with_both_numbers() {
    let budget = Budget {
        per_request_cents: Some(50),
        ..permissive()
    };
    let refused =
        plan_with(&glacier(100), RestoreTier::Expedited, &budget).expect_err("must refuse");
    match refused {
        RestoreRefusal::OverRequestBudget {
            est_cents,
            budget_cents,
        } => {
            assert_eq!(budget_cents, 50);
            assert!(est_cents > 50, "the estimate must be reported too");
        }
        other => panic!("expected a request-budget refusal, got {other:?}"),
    }
}

#[test]
fn the_monthly_budget_counts_what_has_already_been_spent() {
    // A per-request limit alone does not stop a hundred small restores adding up to a large bill, which is the
    // shape a runaway integration actually takes.
    let budget = Budget {
        monthly_cents: Some(1_000),
        spent_this_month_cents: 990,
        ..permissive()
    };
    let refused =
        plan_with(&glacier(100), RestoreTier::Standard, &budget).expect_err("must refuse");
    match refused {
        RestoreRefusal::OverMonthlyBudget {
            spent_cents,
            budget_cents,
            ..
        } => {
            assert_eq!((spent_cents, budget_cents), (990, 1_000));
        }
        other => panic!("expected a monthly-budget refusal, got {other:?}"),
    }

    // And a small restore inside the remaining headroom proceeds, so the limit is a limit rather than a wall.
    let small = plan_with(
        &[Candidate {
            bytes: 1_024,
            class: StorageClass::Glacier,
        }],
        RestoreTier::Bulk,
        &budget,
    );
    assert!(small.is_ok(), "got {small:?}");
}

#[test]
fn a_large_restore_needs_approval_rather_than_being_refused() {
    // A big restore is often legitimate. The answer is "somebody senior confirms", not "no" — refusing
    // outright is what teaches people to route around the control.
    let budget = Budget {
        approval_threshold_cents: Some(100),
        ..permissive()
    };
    let plan = plan_with(&glacier(100), RestoreTier::Expedited, &budget).expect("plan");
    assert!(plan.needs_approval);
    assert!(plan.est_cost_cents > 100);

    let small = plan_with(
        &[Candidate {
            bytes: 1_024,
            class: StorageClass::Glacier,
        }],
        RestoreTier::Bulk,
        &budget,
    )
    .expect("plan");
    assert!(
        !small.needs_approval,
        "ordinary work must not queue behind an administrator"
    );
}

#[test]
fn the_default_budget_asks_for_approval_but_does_not_block() {
    // A default of "no budget at all" makes the guardrail opt-in, and the failure mode of an opt-in cost
    // control is a surprise invoice. A default of zero would make everything need approval before anyone had
    // configured anything, which teaches people to disable it.
    let default = Budget::default();
    assert!(default.per_request_cents.is_none());
    assert!(default.monthly_cents.is_none());
    assert!(default.approval_threshold_cents.is_some());

    // An ordinary restore proceeds without approval.
    let ordinary = plan_with(&glacier(1), RestoreTier::Bulk, &default).expect("plan");
    assert!(!ordinary.needs_approval);

    // A mis-clicked expedited restore of a large archive stops for a human.
    let enormous = plan_with(&glacier(10_000), RestoreTier::Expedited, &default).expect("plan");
    assert!(enormous.needs_approval);
}

// ─── the temporary copy ─────────────────────────────────────────────────────

#[test]
fn the_copy_expires_after_the_keep_warm_window_measured_from_availability() {
    // Measured from the ETA, not from now. A 48-hour Bulk restore with a 7-day window that expired 7 days
    // after the *request* would give the user five days, not seven — and the difference is a second restore.
    let plan = restore::plan(
        &glacier(1),
        RestoreTier::Bulk,
        prices(),
        &permissive(),
        7,
        now(),
    )
    .expect("plan");
    assert_eq!(plan.eta_at, now() + Duration::hours(12));
    assert_eq!(plan.expires_at, plan.eta_at + Duration::days(7));
}

#[test]
fn a_zero_day_keep_warm_window_is_clamped_to_a_day() {
    // A copy that expires the instant it becomes available is not a restore. Clamping beats refusing, since
    // the caller's intent is obvious and a misconfigured window should not lose the request.
    let plan = restore::plan(
        &glacier(1),
        RestoreTier::Standard,
        prices(),
        &permissive(),
        0,
        now(),
    )
    .expect("plan");
    assert_eq!(plan.expires_at, plan.eta_at + Duration::days(1));
}

#[test]
fn deep_archive_quotes_its_own_much_longer_windows() {
    let deep = vec![Candidate {
        bytes: GB,
        class: StorageClass::DeepArchive,
    }];
    let standard = plan_with(&deep, RestoreTier::Standard, &permissive()).expect("plan");
    assert_eq!(standard.eta_at, now() + Duration::hours(12));

    let bulk = plan_with(&deep, RestoreTier::Bulk, &permissive()).expect("plan");
    assert_eq!(
        bulk.eta_at,
        now() + Duration::hours(48),
        "48 hours is the documented Deep Archive Bulk window, and quoting less would break the promise"
    );
}
