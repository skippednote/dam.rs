//! The tier a user sees, derived once (F.4's wire type).
//!
//! The UI shows one of five tiers, but the database stores two independent facts: the object's storage
//! class and the state of any restore. Deriving the tier in the frontend would mean reimplementing
//! that mapping in TypeScript — and the mapping contains the trap the schema comments warn about
//! twice: an *expired* restore of an archived object must read as archived, not as restored. Conflate
//! them and the download button stays enabled until the day someone presses it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use dam_core::{AssetTier, RestoreState, StorageClass};

#[test]
fn a_standard_object_is_hot() {
    assert_eq!(
        AssetTier::of(StorageClass::Standard, RestoreState::None),
        AssetTier::Hot
    );
}

#[test]
fn the_instant_but_billed_classes_are_cool_rather_than_hot() {
    // Glacier IR and IA read instantly, so they are not archive — but they carry a retrieval fee, and
    // a user deciding whether to bulk-download 2,000 originals needs to see the difference.
    for class in [
        StorageClass::StandardIa,
        StorageClass::OnezoneIa,
        StorageClass::GlacierIr,
        StorageClass::IntelligentTiering,
    ] {
        assert_eq!(
            AssetTier::of(class, RestoreState::None),
            AssetTier::Cool,
            "{class}"
        );
    }
}

#[test]
fn the_classes_that_need_a_restore_are_archive() {
    for class in [StorageClass::Glacier, StorageClass::DeepArchive] {
        assert_eq!(
            AssetTier::of(class, RestoreState::None),
            AssetTier::Archive,
            "{class}"
        );
    }
}

#[test]
fn a_restore_in_flight_reads_as_restoring() {
    for state in [RestoreState::Requested, RestoreState::Ongoing] {
        assert_eq!(
            AssetTier::of(StorageClass::DeepArchive, state),
            AssetTier::Restoring,
            "{state}"
        );
    }
}

#[test]
fn a_live_restore_reads_as_restored() {
    assert_eq!(
        AssetTier::of(StorageClass::Glacier, RestoreState::Available),
        AssetTier::Restored
    );
}

#[test]
fn an_expired_restore_reads_as_archive_again_not_as_restored() {
    // The trap. `restore_state` and `storage_class` are separate columns precisely because a restore
    // does not change the class — and the schema says so twice. If an expired restore rendered as
    // restored, the download button would stay enabled and 403 the day someone pressed it.
    assert_eq!(
        AssetTier::of(StorageClass::DeepArchive, RestoreState::Expired),
        AssetTier::Archive
    );
}

#[test]
fn a_restore_state_on_a_class_that_never_needed_one_is_ignored() {
    // Defensive: a stale `restore_state` left on an object that has since been transitioned back to
    // Standard must not make a hot object look like it is thawing.
    for state in [
        RestoreState::Ongoing,
        RestoreState::Available,
        RestoreState::Expired,
    ] {
        assert_eq!(
            AssetTier::of(StorageClass::Standard, state),
            AssetTier::Hot,
            "{state}"
        );
    }
}

#[test]
fn every_tier_reports_whether_bytes_are_available_now() {
    // What the download button branches on. Archive and restoring cannot serve the original.
    assert!(AssetTier::Hot.original_available());
    assert!(AssetTier::Cool.original_available());
    assert!(AssetTier::Restored.original_available());
    assert!(!AssetTier::Archive.original_available());
    assert!(!AssetTier::Restoring.original_available());
}

#[test]
fn tiers_round_trip_through_their_wire_spelling() {
    for tier in [
        AssetTier::Hot,
        AssetTier::Cool,
        AssetTier::Archive,
        AssetTier::Restoring,
        AssetTier::Restored,
    ] {
        assert_eq!(tier.as_str().parse::<AssetTier>().expect("parse"), tier);
    }
    assert!("frozen".parse::<AssetTier>().is_err());
}
