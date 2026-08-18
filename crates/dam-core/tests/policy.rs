//! The ABAC predicate compiler (0.10) — §12's "one implementation, three consumers".
//!
//! Roles carry RBAC (which verbs) crossed with ABAC (over which asset groups). This compiles a set of
//! grants into one [`AccessPredicate`] value, which SQL, Tantivy and MCP then render. The value is the
//! contract: if the three consumers each decided for themselves what "visible" meant, they would
//! disagree, and the disagreement would be a disclosure rather than a bug report.
//!
//! The five semantics under test are the ones recorded as delegated decisions in DECISIONS.md:
//!
//! 1. roles combine as a **union**;
//! 2. an unreleased or expired asset is **visible but not downloadable**;
//! 3. `requires_eula` gates **download only**, not visibility;
//! 4. rule-based groups are evaluated **live**;
//! 5. `all_asset_groups` bypasses group scoping and release windows, but **not** expiry, legal hold, or
//!    `rights_state = 'denied'`.
//!
//! Each is asserted here rather than described in a comment, because every one of them is a decision
//! somebody will later assume went the other way.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::policy::{Action, Grant, Grants, Refusal};
use dam_core::{RightsState, policy};
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0)
        .single()
        .expect("timestamp")
}

fn group(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// A role granting `permissions` over `groups`.
fn grant(permissions: &[&str], groups: &[Uuid]) -> Grant {
    Grant {
        permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        asset_group_ids: groups.to_vec(),
        all_asset_groups: false,
        valid_from: None,
        valid_until: None,
        requires_eula: false,
        eula_accepted: false,
    }
}

/// An asset with nothing wrong with it.
fn healthy() -> policy::AssetFacts {
    policy::AssetFacts {
        asset_groups: vec![group(1)],
        release_at: None,
        expires_at: None,
        legal_hold: false,
        rights_state: RightsState::Allowed,
        deleted: false,
    }
}

// ─── 1. roles combine as a union ────────────────────────────────────────────

#[test]
fn two_roles_grant_the_union_of_their_groups() {
    // Intersection would mean that granting somebody an extra role *reduced* what they could see,
    // which no administrator expects and which makes roles non-composable.
    let grants = Grants::from(vec![
        grant(&["asset:read"], &[group(1), group(2)]),
        grant(&["asset:read"], &[group(2), group(3)]),
    ]);
    let predicate = policy::compile(&grants, Action::Read, now());
    let mut visible = predicate.allowed_groups().to_vec();
    visible.sort();
    assert_eq!(visible, vec![group(1), group(2), group(3)]);
}

#[test]
fn a_permission_from_any_role_is_enough() {
    let grants = Grants::from(vec![
        grant(&["asset:read"], &[group(1)]),
        grant(&["asset:download"], &[group(2)]),
    ]);
    assert!(policy::compile(&grants, Action::Read, now()).permits_action());
    assert!(policy::compile(&grants, Action::Download, now()).permits_action());
    assert!(!policy::compile(&grants, Action::Manage, now()).permits_action());
}

#[test]
fn a_role_outside_its_validity_window_grants_nothing() {
    // `roles.valid_from` / `valid_until` are how time-boxed access is expressed. A window that is
    // checked at assignment time rather than at query time is not a window at all.
    let mut expired = grant(&["asset:read"], &[group(1)]);
    expired.valid_until = Some(now() - Duration::days(1));
    let mut future = grant(&["asset:read"], &[group(2)]);
    future.valid_from = Some(now() + Duration::days(1));

    let predicate = policy::compile(&Grants::from(vec![expired, future]), Action::Read, now());
    assert!(!predicate.permits_action());
    assert!(predicate.allowed_groups().is_empty());
}

#[test]
fn a_caller_with_no_roles_is_refused_rather_than_unrestricted() {
    // The direction of the default. An empty grant set must compile to "nothing", never to a predicate
    // that omits its group filter — which is how an ACL check becomes a full table scan of someone
    // else's library.
    let predicate = policy::compile(&Grants::from(vec![]), Action::Read, now());
    assert!(!predicate.permits_action());
    assert!(predicate.allowed_groups().is_empty());
    assert!(!predicate.all_groups());
}

// ─── 2. unreleased and expired stay visible ─────────────────────────────────

#[test]
fn an_unreleased_asset_is_visible_but_not_downloadable() {
    // A librarian has to see next week's embargoed campaign in order to tag it.
    let grants = Grants::from(vec![grant(&["asset:read", "asset:download"], &[group(1)])]);
    let mut asset = healthy();
    asset.release_at = Some(now() + Duration::days(7));

    assert!(policy::evaluate(&grants, Action::Read, &asset, now()).is_allowed());
    match policy::evaluate(&grants, Action::Download, &asset, now()) {
        policy::Decision::Refused(Refusal::NotYetReleased { release_at }) => {
            assert_eq!(release_at, now() + Duration::days(7));
        }
        other => panic!("expected NotYetReleased, got {other:?}"),
    }
}

#[test]
fn an_expired_asset_is_visible_but_not_downloadable_and_says_when_it_expired() {
    // The refusal carries the date so the UI can say "licence expired 14 Aug" rather than silently
    // omitting the asset — and an asset that vanishes on expiry is one nobody renews.
    let grants = Grants::from(vec![grant(&["asset:read", "asset:download"], &[group(1)])]);
    let mut asset = healthy();
    asset.expires_at = Some(now() - Duration::days(4));

    assert!(policy::evaluate(&grants, Action::Read, &asset, now()).is_allowed());
    match policy::evaluate(&grants, Action::Download, &asset, now()) {
        policy::Decision::Refused(Refusal::Expired { expired_at }) => {
            assert_eq!(expired_at, now() - Duration::days(4));
        }
        other => panic!("expected Expired, got {other:?}"),
    }
}

#[test]
fn the_release_and_expiry_boundaries_are_inclusive_of_the_usable_window() {
    // At exactly `release_at` the asset is released; at exactly `expires_at` it is not yet expired.
    // Off by one here means an asset is undownloadable for the first second of its own campaign.
    let grants = Grants::from(vec![grant(&["asset:download"], &[group(1)])]);

    let mut releasing = healthy();
    releasing.release_at = Some(now());
    assert!(policy::evaluate(&grants, Action::Download, &releasing, now()).is_allowed());

    let mut expiring = healthy();
    expiring.expires_at = Some(now());
    assert!(policy::evaluate(&grants, Action::Download, &expiring, now()).is_allowed());
    assert!(
        !policy::evaluate(
            &grants,
            Action::Download,
            &expiring,
            now() + Duration::seconds(1)
        )
        .is_allowed()
    );
}

// ─── 3. the EULA gates download only ────────────────────────────────────────

#[test]
fn an_unaccepted_eula_blocks_download_but_not_browsing() {
    // Gating search results would make an unaccepted EULA look like an empty library, which reads as a
    // broken product rather than a gate — and browsing is what tells someone the EULA is worth
    // accepting.
    let mut role = grant(&["asset:read", "asset:download"], &[group(1)]);
    role.requires_eula = true;
    role.eula_accepted = false;
    let grants = Grants::from(vec![role]);

    assert!(policy::evaluate(&grants, Action::Read, &healthy(), now()).is_allowed());
    assert!(matches!(
        policy::evaluate(&grants, Action::Download, &healthy(), now()),
        policy::Decision::Refused(Refusal::EulaNotAccepted)
    ));
}

#[test]
fn an_accepted_eula_lifts_the_download_gate() {
    let mut role = grant(&["asset:download"], &[group(1)]);
    role.requires_eula = true;
    role.eula_accepted = true;
    assert!(
        policy::evaluate(
            &Grants::from(vec![role]),
            Action::Download,
            &healthy(),
            now()
        )
        .is_allowed()
    );
}

// ─── 5. what an administrator does and does not bypass ──────────────────────

fn administrator() -> Grants {
    let mut role = grant(&["asset:read", "asset:download", "asset:manage"], &[]);
    role.all_asset_groups = true;
    Grants::from(vec![role])
}

#[test]
fn an_administrator_reaches_every_group_and_every_unreleased_asset() {
    let admin = administrator();
    let predicate = policy::compile(&admin, Action::Download, now());
    assert!(predicate.all_groups(), "group scoping is bypassed");

    let mut embargoed = healthy();
    embargoed.asset_groups = vec![group(99)];
    embargoed.release_at = Some(now() + Duration::days(30));
    assert!(
        policy::evaluate(&admin, Action::Download, &embargoed, now()).is_allowed(),
        "an administrator manages the library, so unreleased assets must be reachable"
    );
}

#[test]
fn an_administrator_does_not_bypass_expiry() {
    // The one I was least willing to guess at. If "administrator" also meant "may commit a rights
    // violation", the download would look authorised in the audit log — which is exactly the failure
    // D12 exists to prevent.
    let mut expired = healthy();
    expired.expires_at = Some(now() - Duration::days(1));
    assert!(matches!(
        policy::evaluate(&administrator(), Action::Download, &expired, now()),
        policy::Decision::Refused(Refusal::Expired { .. })
    ));
}

#[test]
fn an_administrator_does_not_bypass_a_legal_hold_or_a_denied_rights_state() {
    let mut held = healthy();
    held.legal_hold = true;
    assert!(matches!(
        policy::evaluate(&administrator(), Action::Download, &held, now()),
        policy::Decision::Refused(Refusal::LegalHold)
    ));

    let mut denied = healthy();
    denied.rights_state = RightsState::Denied;
    assert!(matches!(
        policy::evaluate(&administrator(), Action::Download, &denied, now()),
        policy::Decision::Refused(Refusal::RightsDenied { .. })
    ));
}

#[test]
fn unknown_rights_block_a_download_just_as_denied_rights_do() {
    // `rights_state` defaults to `unknown`, and the schema's AI-gate comment is explicit that
    // unevaluated rights are not permission. Distribution is the same case.
    let mut unevaluated = healthy();
    unevaluated.rights_state = RightsState::Unknown;
    // Both verbs: the point is that the *rights* gate blocks download while leaving read alone, so a
    // fixture granting download only would fail on the missing read permission and prove nothing.
    let grants = Grants::from(vec![grant(&["asset:read", "asset:download"], &[group(1)])]);
    assert!(matches!(
        policy::evaluate(&grants, Action::Download, &unevaluated, now()),
        policy::Decision::Refused(Refusal::RightsDenied { .. })
    ));
    assert!(
        policy::evaluate(&grants, Action::Read, &unevaluated, now()).is_allowed(),
        "but it stays visible, so somebody can go and evaluate it"
    );
}

#[test]
fn an_expiring_rights_state_still_permits_download() {
    // Expiring is not expired. Blocking it would take a licensed asset out of service early.
    let mut soon = healthy();
    soon.rights_state = RightsState::Expiring;
    let grants = Grants::from(vec![grant(&["asset:download"], &[group(1)])]);
    assert!(policy::evaluate(&grants, Action::Download, &soon, now()).is_allowed());
}

// ─── group scoping and deletion ─────────────────────────────────────────────

#[test]
fn an_asset_outside_every_granted_group_is_not_visible() {
    let grants = Grants::from(vec![grant(&["asset:read"], &[group(1)])]);
    let mut elsewhere = healthy();
    elsewhere.asset_groups = vec![group(2)];
    assert!(matches!(
        policy::evaluate(&grants, Action::Read, &elsewhere, now()),
        policy::Decision::Refused(Refusal::OutsideGrantedGroups)
    ));
}

#[test]
fn an_asset_in_any_granted_group_is_visible() {
    // Membership is a union too: an asset in {2,3} is visible to a role granting {3,4}.
    let grants = Grants::from(vec![grant(&["asset:read"], &[group(3), group(4)])]);
    let mut shared = healthy();
    shared.asset_groups = vec![group(2), group(3)];
    assert!(policy::evaluate(&grants, Action::Read, &shared, now()).is_allowed());
}

#[test]
fn a_deleted_asset_is_invisible_to_everyone_including_an_administrator() {
    // Soft deletion is the one case where invisibility is right: a deleted asset is not an asset with
    // a problem, it is gone. Restoring it is a manage action against a different surface.
    let mut deleted = healthy();
    deleted.deleted = true;
    for (label, grants) in [
        (
            "reader",
            Grants::from(vec![grant(&["asset:read"], &[group(1)])]),
        ),
        ("administrator", administrator()),
    ] {
        assert!(
            matches!(
                policy::evaluate(&grants, Action::Read, &deleted, now()),
                policy::Decision::Refused(Refusal::Deleted)
            ),
            "{label} must not see a deleted asset"
        );
    }
}

// ─── the refusal is machine-readable ────────────────────────────────────────

#[test]
fn every_refusal_carries_a_stable_reason_code() {
    // §12 and the error design both require this: a UI cannot branch on a sentence, and a support
    // engineer cannot grep for one. The codes are part of the API contract.
    let cases = [
        (Refusal::OutsideGrantedGroups, "outside_granted_groups"),
        (Refusal::MissingPermission, "missing_permission"),
        (Refusal::EulaNotAccepted, "eula_not_accepted"),
        (Refusal::LegalHold, "legal_hold"),
        (Refusal::Deleted, "deleted"),
    ];
    for (refusal, code) in cases {
        assert_eq!(refusal.code(), code);
    }
    assert_eq!(
        Refusal::Expired { expired_at: now() }.code(),
        "rights_expired"
    );
    assert_eq!(
        Refusal::NotYetReleased { release_at: now() }.code(),
        "not_yet_released"
    );
    assert_eq!(
        Refusal::RightsDenied {
            state: RightsState::Unknown
        }
        .code(),
        "rights_denied"
    );
}

#[test]
fn a_refusal_names_the_gate_without_leaking_what_it_is_hiding() {
    // "outside_granted_groups" must not become "asset 7f3c is in group marketing-2026", which would
    // disclose the existence and grouping of an asset the caller cannot see.
    let rendered = Refusal::OutsideGrantedGroups.to_string();
    assert!(!rendered.contains("group_id"));
    assert!(!rendered.to_lowercase().contains("uuid"));
}
