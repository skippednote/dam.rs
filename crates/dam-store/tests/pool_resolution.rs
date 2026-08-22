//! Pool and placement resolution (§6.3).
//!
//! Pure decision logic — no container, no clock beyond the one passed in. The whole point
//! of this layer is that choosing *where to read from* and *where to write to* is a policy
//! decision with money and latency attached, and getting it wrong is invisible until a bill
//! or a 12-hour wait arrives.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, TimeZone, Utc};
use dam_core::{LatencyClass, PlacementState, RestoreState, StorageClass};
use dam_store::{
    Key, PlacementRef, PoolRegistry, PoolSpec, Rate, ReadPlan,
    pool::{Driver, WriteIntent},
};
use uuid::Uuid;

fn at(hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, hour, 0, 0)
        .single()
        .expect("fixed timestamp")
}

/// A pool with the fields resolution actually reads; the rest are plausible defaults.
fn pool(name: &str, class: StorageClass, latency: LatencyClass, retrieval: u64) -> PoolSpec {
    PoolSpec {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        driver: Driver::S3,
        endpoint: None,
        region: Some("us-east-1".into()),
        bucket: format!("damrs-{name}"),
        prefix: String::new(),
        force_path_style: false,
        credentials_ref: "env:AWS".into(),
        storage_class: class,
        latency_class: latency,
        immutable: false,
        min_duration_days: class.min_duration_days(),
        min_billable_bytes: if matches!(class, StorageClass::Standard) {
            0
        } else {
            131_072
        },
        // Rates as the database stores them: numeric(12,8) multiplied out.
        cost_per_gb_month: Rate::from_db_units(2_300_000),
        cost_per_gb_retrieval: Rate::from_db_units(retrieval),
        cost_per_1k_requests: Rate::from_db_units(40),
        enabled: true,
    }
}

fn hot() -> PoolSpec {
    pool("hot", StorageClass::Standard, LatencyClass::Instant, 0)
}

fn cool() -> PoolSpec {
    // Glacier IR: cheap to store, instant to read, but retrieval is billed per GB.
    pool(
        "cool",
        StorageClass::GlacierIr,
        LatencyClass::Instant,
        1_000_000,
    )
}

fn archive() -> PoolSpec {
    pool(
        "archive",
        StorageClass::DeepArchive,
        LatencyClass::Hours,
        200_000,
    )
}

fn present(spec: &PoolSpec, size: u64) -> PlacementRef {
    PlacementRef {
        pool_id: spec.id,
        size_bytes: size,
        state: PlacementState::Present,
        storage_class: spec.storage_class,
        restore_state: RestoreState::None,
        restore_expires_at: None,
    }
}

const GB: u64 = 1_073_741_824;

#[test]
fn an_unknown_pool_id_is_an_error_and_never_a_fallback() {
    let hot = hot();
    let registry = PoolRegistry::new(vec![hot.clone()]).expect("registry");
    let ghost = Uuid::new_v4();

    assert!(
        registry.get(ghost).is_err(),
        "an unknown pool must not resolve to anything"
    );

    // The dangerous case: a placement pointing at a pool the registry does not know. Both
    // placements are 'present', so skipping the unknown one would quietly serve the object
    // from the other pool and hide a configuration drift that also affects writes.
    let mut ghost_ref = present(&hot, GB);
    ghost_ref.pool_id = ghost;
    let err = registry
        .resolve_read(&[ghost_ref, present(&hot, GB)], at(12))
        .expect_err("a placement in an unknown pool must be a hard error");
    assert!(
        format!("{err}").contains(&ghost.to_string()),
        "the error must name the pool so the drift is fixable: {err}"
    );
}

#[test]
fn the_cheapest_readable_placement_wins() {
    let (hot, cool) = (hot(), cool());
    let registry = PoolRegistry::new(vec![hot.clone(), cool.clone()]).expect("registry");

    let plan = registry
        .resolve_read(&[present(&cool, GB), present(&hot, GB)], at(12))
        .expect("resolve");
    match plan {
        ReadPlan::Ready { pool_id, .. } => assert_eq!(
            pool_id, hot.id,
            "hot has no per-GB retrieval charge, so it is cheaper to read"
        ),
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn a_cheaper_archive_placement_loses_to_a_more_expensive_instant_one() {
    // This is the case that makes "cheapest wins" wrong if taken literally. Deep Archive
    // is the cheapest thing in the estate to *store* and its per-GB retrieval here is
    // lower than Glacier IR's — but reading it costs a 12-hour wait. A resolver that
    // ranked on price alone would turn every download into a restore ticket.
    let (cool, archive) = (cool(), archive());
    let registry = PoolRegistry::new(vec![cool.clone(), archive.clone()]).expect("registry");

    let plan = registry
        .resolve_read(&[present(&archive, GB), present(&cool, GB)], at(12))
        .expect("resolve");
    match plan {
        ReadPlan::Ready { pool_id, .. } => assert_eq!(
            pool_id, cool.id,
            "readability outranks price — a restore is not a cheaper read, it is a \
             different outcome"
        ),
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn an_archive_placement_with_a_live_restore_is_readable() {
    let archive = archive();
    let registry = PoolRegistry::new(vec![archive.clone()]).expect("registry");

    let mut restored = present(&archive, GB);
    restored.restore_state = RestoreState::Available;
    restored.restore_expires_at = Some(at(18));

    match registry.resolve_read(&[restored], at(12)).expect("resolve") {
        ReadPlan::Ready { pool_id, .. } => assert_eq!(pool_id, archive.id),
        other => panic!("a live restore must read directly, got {other:?}"),
    }
}

#[test]
fn a_restore_that_expires_exactly_now_is_not_readable() {
    // The boundary is exclusive, matching S3's `expiry-date` and `FakeS3Store`. Serving
    // one request past the expiry is how this becomes an intermittent production 403.
    let archive = archive();
    let registry = PoolRegistry::new(vec![archive.clone()]).expect("registry");

    let mut lapsing = present(&archive, GB);
    lapsing.restore_state = RestoreState::Available;
    lapsing.restore_expires_at = Some(at(12));

    match registry
        .resolve_read(&[lapsing.clone()], at(12) - Duration::seconds(1))
        .expect("resolve")
    {
        ReadPlan::Ready { .. } => {}
        other => panic!("one second before expiry it is still readable, got {other:?}"),
    }
    match registry.resolve_read(&[lapsing], at(12)).expect("resolve") {
        ReadPlan::Restore { .. } => {}
        other => panic!("at the expiry instant it must need a new restore, got {other:?}"),
    }
}

#[test]
fn a_missing_or_corrupt_placement_is_never_chosen() {
    let (hot, cool) = (hot(), cool());
    let registry = PoolRegistry::new(vec![hot.clone(), cool.clone()]).expect("registry");

    let mut broken = present(&hot, GB);
    broken.state = PlacementState::Corrupt;
    match registry
        .resolve_read(&[broken.clone(), present(&cool, GB)], at(12))
        .expect("resolve")
    {
        ReadPlan::Ready { pool_id, .. } => assert_eq!(
            pool_id, cool.id,
            "the cheaper pool is corrupt, so the pricier intact copy wins — this is what \
             replication is for"
        ),
        other => panic!("expected Ready, got {other:?}"),
    }

    let mut uploading = present(&cool, GB);
    uploading.state = PlacementState::Uploading;
    let err = registry
        .resolve_read(&[broken, uploading], at(12))
        .expect_err("nothing readable");
    let msg = format!("{err}");
    assert!(
        msg.contains("corrupt") && msg.contains("uploading"),
        "the error must say WHY each copy was unusable, or an operator cannot act on it: \
         {msg}"
    );
}

#[test]
fn no_placements_at_all_is_an_error_rather_than_a_default_pool() {
    let registry = PoolRegistry::new(vec![hot()]).expect("registry");
    assert!(
        registry.resolve_read(&[], at(12)).is_err(),
        "an object with no placements is missing, not readable from somewhere"
    );
}

#[test]
fn all_cold_yields_a_restore_plan_for_the_cheapest_restorable_copy() {
    let archive = archive();
    let mut pricier = archive.clone();
    pricier.id = Uuid::new_v4();
    pricier.name = "archive-eu".into();
    pricier.cost_per_gb_retrieval = Rate::from_db_units(900_000);
    let registry = PoolRegistry::new(vec![archive.clone(), pricier.clone()]).expect("registry");

    match registry
        .resolve_read(&[present(&pricier, GB), present(&archive, GB)], at(12))
        .expect("resolve")
    {
        ReadPlan::Restore {
            pool_id,
            estimated_cost,
            ..
        } => {
            assert_eq!(pool_id, archive.id, "cheapest retrieval wins among equals");
            // $0.002/GB over one GiB, plus a single request at $0.0004/1,000. The request
            // term must survive the division rather than truncating to nothing — the
            // estimate is what the UI shows before a user confirms a restore (§6.5).
            assert_eq!(
                estimated_cost.units(),
                2_000_000_000 + 400,
                "estimate was {estimated_cost}"
            );
        }
        other => panic!("expected Restore, got {other:?}"),
    }
}

#[test]
fn a_disabled_pool_can_still_be_read_but_never_written_to() {
    // Disabling a pool retires it from new placements. If it also blocked reads, disabling
    // would take every object that lives there offline — which is a data outage dressed up
    // as a configuration change.
    let mut retired = hot();
    retired.name = "hot-retired".into();
    retired.enabled = false;
    let live = cool();
    let registry = PoolRegistry::new(vec![retired.clone(), live.clone()]).expect("registry");

    match registry
        .resolve_read(&[present(&retired, GB)], at(12))
        .expect("resolve")
    {
        ReadPlan::Ready { pool_id, .. } => assert_eq!(pool_id, retired.id),
        other => panic!("a retired pool must still serve its data, got {other:?}"),
    }

    let chosen = registry
        .resolve_write(
            WriteIntent::Original,
            &Key::new("t/o/aa/bb/cc").expect("key"),
        )
        .expect("write target");
    assert_eq!(
        chosen.id, live.id,
        "a disabled pool must never receive a new object"
    );
}

#[test]
fn a_new_object_is_written_to_an_instant_pool_never_an_archive_one() {
    let (hot, archive) = (hot(), archive());
    let registry = PoolRegistry::new(vec![archive.clone(), hot.clone()]).expect("registry");

    let chosen = registry
        .resolve_write(
            WriteIntent::Original,
            &Key::new("t/o/aa/bb/cc").expect("key"),
        )
        .expect("write target");
    assert_eq!(
        chosen.id, hot.id,
        "ingesting straight into Deep Archive would start a 180-day minimum charge on an \
         object nobody has even previewed yet"
    );
}

#[test]
fn a_tier_exempt_key_avoids_a_pool_with_a_minimum_billable_size() {
    // A 20 KB thumbnail in a pool with a 128 KiB minimum billable size is billed as
    // 128 KiB — more than it would cost in Standard. Glacier IR is *instant*, so a
    // latency check alone would happily put thumbnails there.
    let (hot, cool) = (hot(), cool());
    let registry = PoolRegistry::new(vec![cool.clone(), hot.clone()]).expect("registry");
    let thumb = Key::thumbnail(Uuid::nil(), &"a".repeat(64), 400).expect("key");
    assert!(thumb.is_tier_exempt(), "precondition");

    let chosen = registry
        .resolve_write(WriteIntent::Derived, &thumb)
        .expect("write target");
    assert_eq!(
        chosen.id, hot.id,
        "a small, permanently-hot object must land in a pool with no size or duration \
         minimum"
    );
}

#[test]
fn an_estate_with_no_pool_fit_to_write_to_is_an_error_not_a_guess() {
    let registry = PoolRegistry::new(vec![archive()]).expect("registry");
    let err = registry
        .resolve_write(
            WriteIntent::Original,
            &Key::new("t/o/aa/bb/cc").expect("key"),
        )
        .expect_err("no instant pool exists");
    assert!(
        format!("{err}").contains("instant"),
        "the error must name the requirement that could not be met: {err}"
    );
}

#[test]
fn equal_cost_ties_break_by_pool_name_so_the_choice_is_reproducible() {
    let mut a = hot();
    a.name = "hot-b".into();
    let mut b = hot();
    b.name = "hot-a".into();
    let registry = PoolRegistry::new(vec![a.clone(), b.clone()]).expect("registry");

    // Twice, with the candidates in both orders: an unstable tiebreak would make a cache
    // key or an audit log differ between two identical requests.
    for candidates in [
        vec![present(&a, GB), present(&b, GB)],
        vec![present(&b, GB), present(&a, GB)],
    ] {
        match registry.resolve_read(&candidates, at(12)).expect("resolve") {
            ReadPlan::Ready { pool_id, .. } => assert_eq!(pool_id, b.id, "'hot-a' sorts first"),
            other => panic!("expected Ready, got {other:?}"),
        }
    }
}

#[test]
fn duplicate_pool_ids_or_names_are_refused_at_construction() {
    let hot = hot();
    let mut same_id = cool();
    same_id.id = hot.id;
    assert!(
        PoolRegistry::new(vec![hot.clone(), same_id]).is_err(),
        "two pools with one id makes resolution non-deterministic"
    );

    let mut same_name = cool();
    same_name.name = hot.name.clone();
    assert!(
        PoolRegistry::new(vec![hot, same_name]).is_err(),
        "a name is how an operator refers to a pool; two of them makes a policy ambiguous"
    );
}

#[test]
fn the_cost_estimate_scales_with_the_object() {
    let cool = cool();
    let registry = PoolRegistry::new(vec![cool.clone()]).expect("registry");
    let small = registry
        .resolve_read(&[present(&cool, GB / 1024)], at(12))
        .expect("resolve");
    let large = registry
        .resolve_read(&[present(&cool, GB * 100)], at(12))
        .expect("resolve");
    match (small, large) {
        (ReadPlan::Ready { cost: s, .. }, ReadPlan::Ready { cost: l, .. }) => assert!(
            l > s,
            "a 100 GB read must estimate above a 1 MB one: {l:?} vs {s:?}"
        ),
        other => panic!("expected two Ready plans, got {other:?}"),
    }
}
