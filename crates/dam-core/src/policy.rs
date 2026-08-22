//! The access predicate: RBAC × ABAC compiled once, rendered by three consumers (§12).
//!
//! A role carries verbs (`permissions`) crossed with scope (`asset_group_ids` / `all_asset_groups`),
//! plus a validity window and an optional EULA gate. This module turns a caller's set of roles into
//! one [`AccessPredicate`] value that SQL, Tantivy and MCP each render into their own dialect.
//!
//! ## Why one value rather than three checks
//!
//! §12 requires "one implementation, three consumers", and the reason is not tidiness. If the search
//! index and the database each decided independently what "visible" meant, they would eventually
//! disagree — and the disagreement would not surface as a bug report. It would surface as a caller
//! seeing a facet count for assets they cannot open, which is a disclosure.
//!
//! ## Two gates, not one
//!
//! Visibility and usability are separate questions, and conflating them is the mistake this module is
//! shaped to avoid. An expired asset must stay *findable* — somebody has to locate it to renew its
//! licence — while being undownloadable. So [`compile`] answers "which assets may this caller see for
//! this action", and [`evaluate`] answers "may this caller do this to this specific asset, and if not,
//! why". The refusal carries a machine-readable reason because a UI cannot branch on a sentence.
//!
//! ## What an administrator does not bypass
//!
//! `all_asset_groups` bypasses group scoping and release windows: an administrator manages the library,
//! so unreleased assets have to be reachable. It does **not** bypass expiry, legal hold, or a denied
//! or unevaluated `rights_state`. Those are legal facts about an asset rather than permissions anyone
//! holds — and if "administrator" also meant "may commit a rights violation", the download would look
//! authorised in the audit log, which is precisely the failure D12 exists to prevent.

use crate::RightsState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What a caller is trying to do.
///
/// Coarse on purpose. The fine-grained permission strings live in `roles.permissions`; this is the axis
/// the *gates* differ along, and there are only three shapes of gate: seeing a thing, taking a copy of
/// it, and changing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Appear in search results, facets and counts.
    Read,
    /// Take the bytes: original download, derivative delivery, embed, share.
    Download,
    /// Change metadata, membership or lifecycle.
    Manage,
}

impl Action {
    /// The permission string a role must carry for this action.
    fn permission(self) -> &'static str {
        match self {
            Self::Read => "asset:read",
            Self::Download => "asset:download",
            Self::Manage => "asset:manage",
        }
    }

    /// Whether this action takes a copy of the asset, and so passes the rights gates.
    ///
    /// This is the line D12 draws: rights are enforced at the point of distribution. Reading is not
    /// distribution, which is why an expired asset stays findable.
    fn is_distribution(self) -> bool {
        matches!(self, Self::Download)
    }
}

/// One role, as it applies to one caller.
///
/// `eula_accepted` is resolved per caller before it gets here — the role says an acceptance is
/// *required*, the caller's acceptance record says whether they gave one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub permissions: Vec<String>,
    pub asset_group_ids: Vec<Uuid>,
    pub all_asset_groups: bool,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub requires_eula: bool,
    pub eula_accepted: bool,
}

impl Grant {
    /// Whether the role is in force. Checked at query time, not at assignment time — a window checked
    /// when the role was granted is not a window.
    fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        self.valid_from.is_none_or(|from| now >= from)
            && self.valid_until.is_none_or(|until| now <= until)
    }

    fn permits(&self, action: Action) -> bool {
        self.permissions
            .iter()
            .any(|held| grants_permission(held, action.permission()))
    }
}

/// Whether a held permission string covers `wanted`.
///
/// Exact match, or a `namespace:*` wildcard covering that namespace. The wildcard exists because the seeded
/// `admin` role is written as `asset:*`, `metadata:*`, `tenant:*`, `rights:*` — and until this function existed
/// nothing expanded it, so a person given the built-in administrator role and *not* flagged as a tenant admin
/// on their membership held no asset permissions at all. The failure was in the safe direction and invisible:
/// every test and every live check reached admin access through the tenant-admin path instead, so the role row
/// was never consulted for the case its wildcards were written for.
///
/// No bare `*`, and no wildcard in the middle. A permission string is `namespace:verb`, so those are the only
/// two shapes that mean anything — and a matcher that accepted more would be a matcher somebody could widen by
/// typo.
#[must_use]
pub fn grants_permission(held: &str, wanted: &str) -> bool {
    if held == wanted {
        return true;
    }
    match held.split_once(':') {
        Some((namespace, "*")) => wanted
            .split_once(':')
            .is_some_and(|(asked, _)| asked == namespace),
        _ => false,
    }
}

/// Every role a caller holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants(Vec<Grant>);

impl From<Vec<Grant>> for Grants {
    fn from(grants: Vec<Grant>) -> Self {
        Self(grants)
    }
}

impl Grants {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn active(&self, now: DateTime<Utc>) -> impl Iterator<Item = &Grant> {
        self.0.iter().filter(move |g| g.is_active_at(now))
    }

    /// Every fine-grained permission string an active role carries, sorted and deduplicated.
    ///
    /// `Action` is the axis the *gates* differ along and is deliberately coarse; this is the other half of that
    /// sentence — the strings a feature can name when it needs to be narrower than "may download". Q.11's
    /// conversions are the first user: a "Print TIFF" format may require `conversion:print`.
    ///
    /// **Only for narrowing, and only after a gate.** Nothing here grants anything: a caller reaches a question
    /// about permissions *after* the compiled predicate has already allowed the action for the asset, and a
    /// permission can only remove a choice from what that allowed. A feature that consulted this instead of the
    /// predicate would be deciding access in a place nobody would look — §12's argument, applied to the strings.
    ///
    /// Active roles only, for the same reason the predicate uses them: a window checked when the role was
    /// granted is not a window. An expired role's permissions are gone the moment it expires.
    pub fn permissions_at(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut held: Vec<String> = self
            .active(now)
            .flat_map(|grant| grant.permissions.iter().cloned())
            .collect();
        held.sort_unstable();
        held.dedup();
        held
    }
}

/// The compiled visibility scope for one caller and one action.
///
/// This is what a query renders. It deliberately holds no reference to the caller: two callers with the
/// same effective scope produce the same predicate, which is what makes it cacheable and comparable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPredicate {
    action: Action,
    /// True when a role grants `all_asset_groups`, so group scoping does not apply.
    all_groups: bool,
    /// Union of every granted group, sorted and deduplicated so the value is canonical — an unstable
    /// order would defeat caching and make two identical predicates compare unequal.
    allowed_groups: Vec<Uuid>,
    /// Whether any active role grants the verb at all.
    has_permission: bool,
    /// True when every role that grants this action requires an unaccepted EULA.
    eula_blocks: bool,
}

impl AccessPredicate {
    /// Whether the caller holds the verb at all. A `false` here means the query should return nothing —
    /// and must still be a query with a false condition rather than a skipped filter.
    pub fn permits_action(&self) -> bool {
        self.has_permission
    }

    pub fn all_groups(&self) -> bool {
        self.all_groups
    }

    pub fn allowed_groups(&self) -> &[Uuid] {
        &self.allowed_groups
    }

    pub fn action(&self) -> Action {
        self.action
    }

    /// Whether an unaccepted EULA blocks this action.
    pub fn eula_blocks(&self) -> bool {
        self.eula_blocks
    }

    /// Whether the predicate can match anything at all.
    ///
    /// A caller with the verb but no groups matches nothing — which is different from not holding the
    /// verb, and both must render as a false condition rather than an omitted filter.
    pub fn matches_nothing(&self) -> bool {
        !self.has_permission || (!self.all_groups && self.allowed_groups.is_empty())
    }
}

/// Compiles the caller's grants into a visibility scope for `action`.
///
/// Roles combine as a **union**: a role granting {A,B} and one granting {B,C} yield {A,B,C}. Under
/// intersection, granting somebody an extra role would *reduce* their access, which no administrator
/// expects and which makes roles non-composable.
pub fn compile(grants: &Grants, action: Action, now: DateTime<Utc>) -> AccessPredicate {
    let relevant: Vec<&Grant> = grants.active(now).filter(|g| g.permits(action)).collect();

    let mut allowed_groups: Vec<Uuid> = relevant
        .iter()
        .flat_map(|g| g.asset_group_ids.iter().copied())
        .collect();
    allowed_groups.sort_unstable();
    allowed_groups.dedup();

    // The EULA gate is per-role, so it blocks only when *every* role that would permit the action
    // requires an unaccepted acceptance. A caller holding one gated role and one ungated role is not
    // blocked — the ungated role is sufficient on its own.
    let eula_blocks =
        !relevant.is_empty() && relevant.iter().all(|g| g.requires_eula && !g.eula_accepted);

    AccessPredicate {
        action,
        all_groups: relevant.iter().any(|g| g.all_asset_groups),
        allowed_groups,
        has_permission: !relevant.is_empty(),
        eula_blocks,
    }
}

/// The facts about one asset that the gates read.
///
/// Deliberately a plain value rather than a database row: the same evaluation has to be reachable from
/// a Tantivy hit and from an MCP tool call, neither of which has a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetFacts {
    pub asset_groups: Vec<Uuid>,
    pub release_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub legal_hold: bool,
    pub rights_state: RightsState,
    pub deleted: bool,
}

/// Why an action was refused.
///
/// Every variant has a stable [`Refusal::code`] because a UI cannot branch on a sentence and a support
/// engineer cannot grep for one. The codes are part of the API contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The asset is in no group the caller was granted. Deliberately says nothing about which groups
    /// exist or which the asset is in — that would disclose the asset it is hiding.
    OutsideGrantedGroups,
    MissingPermission,
    EulaNotAccepted,
    NotYetReleased {
        release_at: DateTime<Utc>,
    },
    Expired {
        expired_at: DateTime<Utc>,
    },
    RightsDenied {
        state: RightsState,
    },
    LegalHold,
    Deleted,
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutsideGrantedGroups => "outside_granted_groups",
            Self::MissingPermission => "missing_permission",
            Self::EulaNotAccepted => "eula_not_accepted",
            Self::NotYetReleased { .. } => "not_yet_released",
            Self::Expired { .. } => "rights_expired",
            Self::RightsDenied { .. } => "rights_denied",
            Self::LegalHold => "legal_hold",
            Self::Deleted => "deleted",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Prose for a log line, and nothing identifying. A refusal message that named the group or the
        // asset would disclose exactly what the refusal exists to hide.
        let message = match self {
            Self::OutsideGrantedGroups => "the asset is outside the caller's granted groups",
            Self::MissingPermission => "the caller holds no role granting this action",
            Self::EulaNotAccepted => "the end-user licence has not been accepted",
            Self::NotYetReleased { .. } => "the asset has not been released yet",
            Self::Expired { .. } => "the asset's rights have expired",
            Self::RightsDenied { .. } => "the asset's rights do not permit distribution",
            Self::LegalHold => "the asset is under legal hold",
            Self::Deleted => "the asset does not exist",
        };
        f.write_str(message)
    }
}

/// The outcome of evaluating one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Refused(Refusal),
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Allowed => None,
            Self::Refused(refusal) => Some(refusal),
        }
    }
}

/// Evaluates one asset against the caller's grants.
///
/// Gate order is deliberate, cheapest and most absolute first, so a bug in a later gate cannot override
/// an earlier one: existence, then the verb, then scope, then the distribution gates. The distribution
/// gates apply only to [`Action::Download`] — that is the D12 line, and it is what keeps an expired
/// asset findable so its licence can be renewed.
pub fn evaluate(
    grants: &Grants,
    action: Action,
    asset: &AssetFacts,
    now: DateTime<Utc>,
) -> Decision {
    // A deleted asset is gone for everyone, administrators included. This is the one case where
    // invisibility is right: it is not an asset with a problem.
    if asset.deleted {
        return Decision::Refused(Refusal::Deleted);
    }

    let predicate = compile(grants, action, now);
    if !predicate.permits_action() {
        return Decision::Refused(Refusal::MissingPermission);
    }

    if !predicate.all_groups()
        && !asset
            .asset_groups
            .iter()
            .any(|g| predicate.allowed_groups().contains(g))
    {
        return Decision::Refused(Refusal::OutsideGrantedGroups);
    }

    if !action.is_distribution() {
        // Visibility stops here. Everything below is about taking a copy.
        return Decision::Allowed;
    }

    if predicate.eula_blocks() {
        return Decision::Refused(Refusal::EulaNotAccepted);
    }

    // Legal hold and rights bind everyone. An administrator manages the library; a lapsed licence is a
    // fact about the asset.
    if asset.legal_hold {
        return Decision::Refused(Refusal::LegalHold);
    }
    if matches!(
        asset.rights_state,
        RightsState::Denied | RightsState::Unknown
    ) {
        // Unknown blocks alongside denied: the schema's AI gate says unevaluated rights are not
        // permission, and distribution is the same case.
        return Decision::Refused(Refusal::RightsDenied {
            state: asset.rights_state,
        });
    }
    if let Some(expires_at) = asset.expires_at
        && now > expires_at
    {
        // Strictly after: at the expiry instant the asset is still usable, or an asset would be
        // undownloadable for the last second of its own licence.
        return Decision::Refused(Refusal::Expired {
            expired_at: expires_at,
        });
    }

    // Release windows are the one gate an administrator does bypass — an unreleased asset has to be
    // reachable by whoever is preparing it.
    if !predicate.all_groups()
        && let Some(release_at) = asset.release_at
        && now < release_at
    {
        return Decision::Refused(Refusal::NotYetReleased { release_at });
    }

    Decision::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_download_passes_the_distribution_gates() {
        // The D12 line, asserted directly: if reading ever became distribution, an expired asset would
        // vanish from search and nobody would renew it.
        assert!(Action::Download.is_distribution());
        assert!(!Action::Read.is_distribution());
        assert!(!Action::Manage.is_distribution());
    }

    #[test]
    fn each_action_maps_to_its_permission_string() {
        assert_eq!(Action::Read.permission(), "asset:read");
        assert_eq!(Action::Download.permission(), "asset:download");
        assert_eq!(Action::Manage.permission(), "asset:manage");
    }

    #[test]
    fn a_permission_from_an_expired_role_is_not_held() {
        // The same rule the predicate follows, for the same reason: a window checked when the role was granted
        // is not a window. Q.11's download formats gate on these strings, so an expired role that still handed
        // out `conversion:print` would keep offering a format somebody's access to had ended.
        let now = Utc::now();
        let grants = Grants::from(vec![
            Grant {
                permissions: vec!["asset:download".into(), "conversion:web".into()],
                asset_group_ids: vec![],
                all_asset_groups: true,
                valid_from: None,
                valid_until: None,
                requires_eula: false,
                eula_accepted: true,
            },
            Grant {
                permissions: vec!["conversion:print".into()],
                asset_group_ids: vec![],
                all_asset_groups: true,
                valid_from: None,
                valid_until: Some(now - chrono::Duration::days(1)),
                requires_eula: false,
                eula_accepted: true,
            },
            Grant {
                permissions: vec!["conversion:future".into()],
                asset_group_ids: vec![],
                all_asset_groups: true,
                valid_from: Some(now + chrono::Duration::days(1)),
                valid_until: None,
                requires_eula: false,
                eula_accepted: true,
            },
        ]);

        let held = grants.permissions_at(now);
        assert_eq!(
            held,
            vec!["asset:download".to_owned(), "conversion:web".to_owned()],
            "an expired or not-yet-active role's permissions are still held"
        );

        // And the expired one comes back when the clock is inside its window, which is what proves the filter
        // is the window rather than something about that particular role.
        let earlier = grants.permissions_at(now - chrono::Duration::days(2));
        assert!(
            earlier.contains(&"conversion:print".to_owned()),
            "{earlier:?}"
        );
    }

    #[test]
    fn held_permissions_are_deduplicated_and_ordered() {
        // Two roles carrying the same string is ordinary — every role grants `asset:read`. A caller holding it
        // twice is not more permitted, and the duplicate would show up in any diagnostic that prints the set.
        let role = |permission: &str| Grant {
            permissions: vec!["asset:read".into(), permission.into()],
            asset_group_ids: vec![],
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        };
        let grants = Grants::from(vec![role("conversion:web"), role("conversion:print")]);
        assert_eq!(
            grants.permissions_at(Utc::now()),
            vec![
                "asset:read".to_owned(),
                "conversion:print".to_owned(),
                "conversion:web".to_owned()
            ]
        );
    }

    #[test]
    fn a_predicate_with_the_verb_but_no_groups_matches_nothing() {
        // Distinct from lacking the verb, and both must render as a false condition rather than an
        // omitted filter — an omitted group filter is a full scan of another tenant's library.
        let grants = Grants::from(vec![Grant {
            permissions: vec!["asset:read".into()],
            asset_group_ids: vec![],
            all_asset_groups: false,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: false,
        }]);
        let predicate = compile(&grants, Action::Read, Utc::now());
        assert!(predicate.permits_action());
        assert!(predicate.matches_nothing());
    }

    #[test]
    fn one_ungated_role_is_enough_to_lift_the_eula_gate() {
        // The gate is per-role, so it blocks only when every role that would permit the action is
        // gated. Blocking whenever *any* role is gated would let one restrictive role veto access the
        // caller legitimately has by another route.
        let gated = Grant {
            permissions: vec!["asset:download".into()],
            asset_group_ids: vec![Uuid::from_u128(1)],
            all_asset_groups: false,
            valid_from: None,
            valid_until: None,
            requires_eula: true,
            eula_accepted: false,
        };
        let ungated = Grant {
            requires_eula: false,
            ..gated.clone()
        };
        assert!(
            compile(
                &Grants::from(vec![gated.clone()]),
                Action::Download,
                Utc::now()
            )
            .eula_blocks()
        );
        assert!(
            !compile(
                &Grants::from(vec![gated, ungated]),
                Action::Download,
                Utc::now()
            )
            .eula_blocks()
        );
    }

    #[test]
    fn the_allowed_group_list_is_canonical() {
        // Sorted and deduplicated, so two callers with the same effective scope produce equal
        // predicates — which is what makes the value cacheable and comparable at all.
        let grants = Grants::from(vec![
            Grant {
                permissions: vec!["asset:read".into()],
                asset_group_ids: vec![Uuid::from_u128(3), Uuid::from_u128(1)],
                all_asset_groups: false,
                valid_from: None,
                valid_until: None,
                requires_eula: false,
                eula_accepted: false,
            },
            Grant {
                permissions: vec!["asset:read".into()],
                asset_group_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
                all_asset_groups: false,
                valid_from: None,
                valid_until: None,
                requires_eula: false,
                eula_accepted: false,
            },
        ]);
        assert_eq!(
            compile(&grants, Action::Read, Utc::now()).allowed_groups(),
            &[Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
        );
    }
}

#[cfg(test)]
mod wildcard_tests {
    use super::*;

    #[test]
    fn a_namespace_wildcard_covers_its_own_verbs_and_nothing_else() {
        // The seeded `admin` role is written this way, and until `grants_permission` existed nothing expanded
        // it: a person holding that role without the tenant-admin flag held no asset permissions at all.
        assert!(grants_permission("asset:*", "asset:read"));
        assert!(grants_permission("asset:*", "asset:download"));
        assert!(grants_permission("metadata:*", "metadata:write"));
        // A wildcard is scoped to its namespace. One that leaked across namespaces would make `metadata:*` a
        // download permission, which is the widening nobody asked for.
        assert!(!grants_permission("metadata:*", "asset:read"));
        // Exact matches still work, and a bare star is not a permission: a matcher that accepted one could be
        // widened to everything by a typo in a seed.
        assert!(grants_permission("asset:read", "asset:read"));
        assert!(!grants_permission("*", "asset:read"));
        assert!(!grants_permission("asset:rea*", "asset:read"));
        assert!(!grants_permission("asset:read", "asset:download"));
    }

    #[test]
    fn a_wildcard_role_compiles_to_a_predicate_that_permits() {
        // The end-to-end shape of the bug: a grant carrying only wildcards must permit the action, or the
        // predicate matches nothing and the caller is refused before any query runs.
        let grants = Grants::from(vec![Grant {
            permissions: vec!["asset:*".to_owned()],
            asset_group_ids: vec![],
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]);
        let predicate = compile(&grants, Action::Read, Utc::now());
        assert!(predicate.permits_action());
        assert!(!predicate.matches_nothing());
    }
}
