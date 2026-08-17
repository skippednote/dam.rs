//! The lifecycle engine (task 1.10) — §6.4.
//!
//! damrs drives transitions itself rather than delegating to S3 lifecycle rules, so this is the code
//! that decides to move a customer's masters into a tier they cannot read back for 48 hours. Three
//! properties matter more than the arithmetic:
//!
//! - **Dry run is the default.** A policy that tiers on its first run moves the whole library before
//!   anyone has read the plan.
//! - **Nothing is capped silently.** A run that stops at its object limit must say so, or a policy
//!   that only ever processes its first thousand objects looks like it is working.
//! - **The two billing traps are respected.** Minimum duration charges and minimum residency are the
//!   same counter, and ignoring it means paying 180 days for an object held three.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::{PlacementState, RestoreState, StorageClass};
use dam_store::{
    Key,
    lifecycle::{self, Candidate, HaltReason, LifecyclePolicy, SkipReason, Verdict},
};
use uuid::Uuid;

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, day, 12, 0, 0)
        .single()
        .expect("timestamp")
}

const HASH: &str = "9f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

fn tenant() -> Uuid {
    Uuid::from_u128(0x0da3_0000_0000_0000_0000_0000_0000_0010)
}

/// A current original, hot, placed and last touched 200 days before `now`.
fn original(now: DateTime<Utc>) -> Candidate {
    Candidate {
        object_key: Key::original(tenant(), HASH).expect("key"),
        pool_id: Uuid::from_u128(1),
        size_bytes: 40 * 1024 * 1024,
        state: PlacementState::Present,
        storage_class: StorageClass::Standard,
        restore_state: RestoreState::None,
        pinned: false,
        pin_reason: None,
        min_duration_until: None,
        placed_at: now - Duration::days(200),
        last_accessed_at: Some(now - Duration::days(200)),
    }
}

/// The default policy from §6.4: current originals go cool after 90 days.
fn cool_after_90() -> LifecyclePolicy {
    LifecyclePolicy::new("cool-originals", StorageClass::GlacierIr, 90)
}

#[test]
fn a_new_policy_is_a_dry_run_until_someone_says_otherwise() {
    // The single most consequential default in the engine. A policy that executes on creation moves
    // every eligible object in the library before anybody has looked at what it would do.
    let policy = cool_after_90();
    assert!(
        policy.dry_run,
        "tiering must be opt-in; the first run exists to be read, not to move data"
    );

    let plan = lifecycle::plan(&policy, &[original(at(20))], at(20));
    assert!(plan.dry_run);
    assert_eq!(
        plan.transitions().count(),
        1,
        "the plan still says what it would do"
    );
}

#[test]
fn an_untouched_original_past_the_threshold_is_planned_for_the_target_class() {
    let now = at(20);
    let plan = lifecycle::plan(&cool_after_90(), &[original(now)], now);
    let action = plan.transitions().next().expect("one transition");
    assert_eq!(action.to, StorageClass::GlacierIr);
    assert_eq!(action.from, StorageClass::Standard);
    assert_eq!(plan.bytes_to_move(), 40 * 1024 * 1024);
}

#[test]
fn an_object_touched_recently_is_not_eligible_and_the_plan_says_when_it_will_be() {
    // "Untouched for N days" is the whole predicate. An operator asking why nothing moved needs the
    // date, not just an absence.
    let now = at(20);
    let mut recent = original(now);
    recent.last_accessed_at = Some(now - Duration::days(10));

    let plan = lifecycle::plan(&cool_after_90(), &[recent], now);
    assert_eq!(plan.transitions().count(), 0);
    match plan.skipped().next().expect("one skip").1 {
        SkipReason::NotYetEligible { eligible_at } => {
            assert_eq!(*eligible_at, now + Duration::days(80));
        }
        other => panic!("expected NotYetEligible, got {other:?}"),
    }
}

#[test]
fn an_object_never_accessed_is_aged_from_when_it_was_placed() {
    // A null `last_accessed_at` must mean neither "infinitely recent" (nothing ever tiers) nor
    // "epoch" (everything tiers immediately). Both are plausible readings and both are wrong.
    let now = at(20);
    let mut never = original(now);
    never.last_accessed_at = None;
    never.placed_at = now - Duration::days(120);
    assert_eq!(
        lifecycle::plan(&cool_after_90(), &[never.clone()], now)
            .transitions()
            .count(),
        1,
        "placed 120 days ago and never read is exactly what a tiering policy is for"
    );

    let mut fresh = never;
    fresh.placed_at = now - Duration::days(5);
    assert_eq!(
        lifecycle::plan(&cool_after_90(), &[fresh], now)
            .transitions()
            .count(),
        0,
        "and one placed five days ago must not move because nobody has read it yet"
    );
}

#[test]
fn a_pinned_object_is_never_tiered_whatever_else_matches() {
    // Pinned is how a legal hold, a live portal reference and a pin_hot collection all express
    // themselves. It is unconditional by design.
    let now = at(20);
    let mut pinned = original(now);
    pinned.pinned = true;
    pinned.pin_reason = Some("legal hold".into());

    let plan = lifecycle::plan(&cool_after_90(), &[pinned], now);
    assert_eq!(plan.transitions().count(), 0);
    match plan.skipped().next().expect("one skip").1 {
        SkipReason::Pinned { reason } => {
            assert_eq!(reason.as_deref(), Some("legal hold"));
        }
        other => panic!("expected Pinned, got {other:?}"),
    }
}

#[test]
fn the_search_substrate_is_never_tiered_even_if_a_policy_names_it() {
    // The §2 invariant, at the point where it could be violated. A proxy or thumbnail matching a
    // tiering rule must be skipped, not obeyed — and a 20 KB thumbnail in Glacier IR costs *more*
    // than in Standard because of the 128 KiB minimum billable size.
    let now = at(20);
    let exempt = [
        Key::proxy(tenant(), HASH, "jpg").expect("key"),
        Key::thumbnail(tenant(), HASH, 400).expect("key"),
        Key::manifest(tenant(), HASH).expect("key"),
    ];
    for key in exempt {
        let mut candidate = original(now);
        candidate.object_key = key.clone();
        candidate.size_bytes = 20 * 1024;

        let plan = lifecycle::plan(&cool_after_90(), &[candidate], now);
        assert_eq!(plan.transitions().count(), 0, "{key} must not tier");
        assert!(
            matches!(
                plan.skipped().next().expect("skip").1,
                SkipReason::TierExempt
            ),
            "{key} should be skipped as exempt"
        );
    }
}

#[test]
fn a_minimum_duration_that_has_not_elapsed_blocks_the_transition() {
    // The billing trap: the same counter charges a minimum on the class an object is *leaving* and
    // blocks a premature second hop. Moving early does not save money, it spends it twice.
    let now = at(20);
    let mut held = original(now);
    held.storage_class = StorageClass::StandardIa;
    held.min_duration_until = Some(now + Duration::days(5));

    let policy = LifecyclePolicy::new("deep", StorageClass::DeepArchive, 90);
    let plan = lifecycle::plan(&policy, &[held.clone()], now);
    match plan.skipped().next().expect("skip").1 {
        SkipReason::MinDurationNotElapsed { until } => {
            assert_eq!(*until, now + Duration::days(5));
        }
        other => panic!("expected MinDurationNotElapsed, got {other:?}"),
    }
}

#[test]
fn the_minimum_duration_boundary_is_inclusive() {
    // At the instant the minimum has elapsed, the charge is settled and the hop is free. One second
    // earlier it is not. Getting this backwards costs a full minimum period per object.
    let now = at(20);
    let mut held = original(now);
    held.storage_class = StorageClass::StandardIa;

    held.min_duration_until = Some(now + Duration::seconds(1));
    assert_eq!(
        lifecycle::plan(
            &LifecyclePolicy::new("deep", StorageClass::DeepArchive, 90),
            &[held.clone()],
            now
        )
        .transitions()
        .count(),
        0,
        "one second before the minimum elapses, the hop still costs double"
    );

    held.min_duration_until = Some(now);
    assert_eq!(
        lifecycle::plan(
            &LifecyclePolicy::new("deep", StorageClass::DeepArchive, 90),
            &[held],
            now
        )
        .transitions()
        .count(),
        1,
        "at the instant it elapses, the hop is free"
    );
}

#[test]
fn an_object_already_in_the_target_class_is_not_moved_again() {
    let now = at(20);
    let mut already = original(now);
    already.storage_class = StorageClass::GlacierIr;

    let plan = lifecycle::plan(&cool_after_90(), &[already], now);
    assert_eq!(plan.transitions().count(), 0);
    assert!(matches!(
        plan.skipped().next().expect("skip").1,
        SkipReason::AlreadyInClass
    ));
}

#[test]
fn a_policy_can_never_move_an_object_toward_a_warmer_tier() {
    // Cold to hot is a *restore*, not a transition: it costs retrieval fees and, for Glacier and
    // Deep Archive, takes hours. A tiering policy that could do it by accident would produce an
    // enormous surprise bill from a config typo.
    let now = at(20);
    let mut cold = original(now);
    cold.storage_class = StorageClass::DeepArchive;
    cold.min_duration_until = None;

    let policy = LifecyclePolicy::new("oops-warm", StorageClass::Standard, 1);
    let plan = lifecycle::plan(&policy, &[cold], now);
    assert_eq!(plan.transitions().count(), 0);
    match plan.skipped().next().expect("skip").1 {
        SkipReason::WouldWarm { from, to } => {
            assert_eq!(*from, StorageClass::DeepArchive);
            assert_eq!(*to, StorageClass::Standard);
        }
        other => panic!("expected WouldWarm, got {other:?}"),
    }
}

#[test]
fn an_object_that_is_not_present_is_not_a_candidate() {
    let now = at(20);
    for state in [
        PlacementState::Uploading,
        PlacementState::Missing,
        PlacementState::Corrupt,
        PlacementState::Deleting,
        PlacementState::Transitioning,
    ] {
        let mut broken = original(now);
        broken.state = state;
        let plan = lifecycle::plan(&cool_after_90(), &[broken], now);
        assert_eq!(plan.transitions().count(), 0, "{state} must not be tiered");
        assert!(matches!(
            plan.skipped().next().expect("skip").1,
            SkipReason::NotPresent { .. }
        ));
    }
}

#[test]
fn an_object_with_a_live_restore_is_left_alone() {
    // Transitioning an object whose temporary copy someone is downloading right now breaks that
    // download, and the restore fee has already been paid.
    let now = at(20);
    let mut restored = original(now);
    restored.storage_class = StorageClass::Glacier;
    restored.restore_state = RestoreState::Available;

    let policy = LifecyclePolicy::new("deeper", StorageClass::DeepArchive, 90);
    let plan = lifecycle::plan(&policy, &[restored], now);
    assert!(matches!(
        plan.skipped().next().expect("skip").1,
        SkipReason::RestoreInFlight { .. }
    ));
}

#[test]
fn a_run_that_hits_its_object_limit_says_so_rather_than_stopping_quietly() {
    // A silent cap is indistinguishable from a working policy: the run reports success, a thousand
    // objects move, and the other four million never do.
    let now = at(20);
    let candidates: Vec<Candidate> = (0..10)
        .map(|i| {
            let mut c = original(now);
            c.object_key = Key::new(format!("{}/o/aa/bb/{i:064}", tenant())).expect("key");
            c
        })
        .collect();

    let policy = LifecyclePolicy {
        max_objects_per_run: Some(3),
        ..cool_after_90()
    };
    let plan = lifecycle::plan(&policy, &candidates, now);
    assert_eq!(plan.transitions().count(), 3);
    match plan.halted {
        Some(HaltReason::ObjectLimit { limit, remaining }) => {
            assert_eq!(limit, 3);
            assert_eq!(remaining, 7, "the operator needs to know how much was left");
        }
        other => panic!("expected a reported halt, got {other:?}"),
    }
}

#[test]
fn a_run_within_its_limit_reports_no_halt() {
    let now = at(20);
    let policy = LifecyclePolicy {
        max_objects_per_run: Some(10),
        ..cool_after_90()
    };
    let plan = lifecycle::plan(&policy, &[original(now)], now);
    assert!(plan.halted.is_none());
}

#[test]
fn a_policy_below_its_minimum_size_skips_small_objects() {
    // Below roughly 128 KiB, IA and Glacier IR bill a minimum that makes tiering cost more than
    // Standard. A policy without a size floor quietly loses money on every small object.
    let now = at(20);
    let mut small = original(now);
    small.size_bytes = 40 * 1024;

    let policy = LifecyclePolicy {
        min_size_bytes: Some(128 * 1024),
        ..cool_after_90()
    };
    let plan = lifecycle::plan(&policy, &[small], now);
    match plan.skipped().next().expect("skip").1 {
        SkipReason::BelowMinimumSize { size, minimum } => {
            assert_eq!(*size, 40 * 1024);
            assert_eq!(*minimum, 128 * 1024);
        }
        other => panic!("expected BelowMinimumSize, got {other:?}"),
    }
}

#[test]
fn a_superseded_only_policy_is_reported_as_unsupported_rather_than_matching_nothing() {
    // §6.4 tiers superseded versions on their own schedule, and `lifecycle_policies.only_superseded`
    // expresses it — but `object_placements` has no version dimension, so the engine cannot identify
    // a noncurrent version. Matching nothing would look exactly like "no objects are due", and a
    // policy that appears configured and does nothing is the worst of the three outcomes.
    let now = at(20);
    let policy = LifecyclePolicy {
        only_superseded: true,
        ..cool_after_90()
    };
    let plan = lifecycle::plan(&policy, &[original(now)], now);
    assert_eq!(plan.transitions().count(), 0);
    match plan.halted {
        Some(HaltReason::Unsupported { what }) => {
            assert!(
                what.contains("superseded"),
                "the halt must name what is missing: {what}"
            );
        }
        other => panic!("expected an Unsupported halt, got {other:?}"),
    }
}

#[test]
fn a_disabled_policy_plans_nothing_at_all() {
    let now = at(20);
    let policy = LifecyclePolicy {
        enabled: false,
        ..cool_after_90()
    };
    let plan = lifecycle::plan(&policy, &[original(now)], now);
    assert_eq!(plan.transitions().count(), 0);
    assert!(matches!(plan.halted, Some(HaltReason::PolicyDisabled)));
}

#[test]
fn the_plan_is_deterministic_so_two_runs_can_be_compared() {
    // A dry run is only useful if the real run does the same thing. An unstable order also makes an
    // audit diff unreadable.
    let now = at(20);
    let candidates: Vec<Candidate> = ["cc", "aa", "bb"]
        .iter()
        .map(|tag| {
            let mut c = original(now);
            c.object_key = Key::new(format!("{}/o/{tag}/bb/{HASH}", tenant())).expect("key");
            c
        })
        .collect();

    let first = lifecycle::plan(&cool_after_90(), &candidates, now);
    let mut reversed = candidates.clone();
    reversed.reverse();
    let second = lifecycle::plan(&cool_after_90(), &reversed, now);

    let keys = |p: &lifecycle::Plan| -> Vec<String> {
        p.transitions()
            .map(|a| a.object_key.as_str().to_owned())
            .collect()
    };
    assert_eq!(keys(&first), keys(&second));
    assert_eq!(
        keys(&first),
        vec![
            format!("{}/o/aa/bb/{HASH}", tenant()),
            format!("{}/o/bb/bb/{HASH}", tenant()),
            format!("{}/o/cc/bb/{HASH}", tenant()),
        ],
        "ordered by key, so the plan reads the same as a bucket listing"
    );
}

#[test]
fn every_candidate_appears_in_the_plan_exactly_once() {
    // The property that makes a dry run trustworthy: an object that is neither moved nor explained
    // has been silently dropped, and nobody can tell the difference from a bucket listing.
    let now = at(20);
    let mut candidates = vec![original(now)];
    let mut pinned = original(now);
    pinned.object_key = Key::new(format!("{}/o/zz/bb/{HASH}", tenant())).expect("key");
    pinned.pinned = true;
    candidates.push(pinned);

    let plan = lifecycle::plan(&cool_after_90(), &candidates, now);
    assert_eq!(
        plan.transitions().count() + plan.skipped().count(),
        candidates.len(),
        "accounted: {} moved, {} skipped, {} in",
        plan.transitions().count(),
        plan.skipped().count(),
        candidates.len()
    );
}

#[test]
fn a_transition_records_the_minimum_duration_it_starts() {
    // Moving into Deep Archive starts a 180-day clock. The engine has to write that forward, because
    // the next policy run reads it — and an unset counter means the next hop looks free when it is
    // not.
    let now = at(20);
    let policy = LifecyclePolicy::new("deep", StorageClass::DeepArchive, 90);
    let plan = lifecycle::plan(&policy, &[original(now)], now);
    let action = plan.transitions().next().expect("transition");
    assert_eq!(
        action.min_duration_until,
        Some(now + Duration::days(180)),
        "Deep Archive's minimum is 180 days, and it starts now"
    );
}

#[test]
fn a_verdict_can_be_rendered_for_an_operator() {
    // A plan is read by a person deciding whether to enable a policy that will move terabytes.
    let now = at(20);
    let plan = lifecycle::plan(&cool_after_90(), &[original(now)], now);
    let rendered = plan.summary();
    assert!(rendered.contains("GLACIER_IR"), "got {rendered}");
    assert!(rendered.contains("dry run"), "got {rendered}");
}

#[test]
fn a_verdict_enum_covers_both_outcomes() {
    let now = at(20);
    let plan = lifecycle::plan(&cool_after_90(), &[original(now)], now);
    let verdicts: Vec<&Verdict> = plan.verdicts().collect();
    assert_eq!(verdicts.len(), 1);
    assert!(matches!(verdicts[0], Verdict::Transition { .. }));
}
