//! Rights and provenance **vocabulary** — the values, not the policy.
//!
//! These mirror `assets.rights_state` and `assets.provenance_state` exactly, because a state the
//! database can store but the API cannot name is a value the UI receives and cannot render — and an
//! asset that renders with no rights indicator reads as "no restriction", which is the most dangerous
//! default available.
//!
//! ## What is deliberately absent
//!
//! There is no `blocks_distribution()` here, and no method that answers whether a state permits
//! anything. That is *enforcement*, it belongs at the distribution chokepoint (D12), and the
//! predicate that decides it is task 0.10 — which is stopped pending the five decisions in
//! `NEEDS-REVIEW.md`. Adding a convenience method here would quietly become the definition, in the
//! one layer that has no idea who is asking or what they are asking for.

use serde::{Deserialize, Serialize};

/// Rights evaluation outcome for an asset. Mirrors the `assets.rights_state` CHECK.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum RightsState {
    /// A licence covers the intended use.
    Allowed,
    /// Licensed, but the coverage ends soon. Still usable — distinct from expired.
    Expiring,
    /// A licence forbids it, or none applies.
    Denied,
    /// Not yet evaluated. The default, matching the column default — an asset arrives unevaluated,
    /// and unevaluated is not permission.
    #[default]
    Unknown,
}

impl RightsState {
    /// The wire and database spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Expiring => "expiring",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for RightsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RightsState {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allowed" => Ok(Self::Allowed),
            "expiring" => Ok(Self::Expiring),
            "denied" => Ok(Self::Denied),
            "unknown" => Ok(Self::Unknown),
            other => Err(crate::Error::Validation {
                field: "rights_state".into(),
                reason: format!("unknown rights state {other:?}"),
            }),
        }
    }
}

/// Content-credential verification outcome. Mirrors the `assets.provenance_state` CHECK.
///
/// Four states rather than a boolean because they call for different responses: `None` is a file that
/// never had a credential, `Invalid` is one whose credential no longer matches its bytes, and
/// `Untrusted` is an intact credential from a signer outside any trust list. Collapsing them loses
/// the distinction between "no claim was made" and "a claim was made and broken".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ProvenanceState {
    /// The file carries no content credential. The default: absence is not a claim.
    #[default]
    None,
    Valid,
    Invalid,
    Untrusted,
}

impl ProvenanceState {
    /// The value stored in `assets.provenance_state`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Untrusted => "untrusted",
        }
    }

    /// The value stored in `provenance_manifests.validation_state`.
    ///
    /// The same states, and the two columns spell the absent case differently: `assets` says `none`,
    /// `provenance_manifests` says `absent`. Both CHECK constraints are already deployed, so this is a
    /// rendering difference rather than a second enum — the alternative is two Rust types for one
    /// concept, which is the drift the wire vocabulary lives in `dam-core` to prevent.
    pub fn as_validation_state(self) -> &'static str {
        match self {
            Self::None => "absent",
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Untrusted => "untrusted",
        }
    }

    /// Whether this state means credentials failed rather than were missing.
    ///
    /// The distinction the suspect index exists for: `invalid` and `untrusted` are findings, `none` is
    /// the ordinary case, and showing them the same way would bury every real signal.
    pub fn is_finding(self) -> bool {
        matches!(self, Self::Invalid | Self::Untrusted)
    }
}

impl std::fmt::Display for ProvenanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProvenanceState {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "valid" => Ok(Self::Valid),
            "invalid" => Ok(Self::Invalid),
            "untrusted" => Ok(Self::Untrusted),
            other => Err(crate::Error::Validation {
                field: "provenance_state".into(),
                reason: format!("unknown provenance state {other:?}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unevaluated_asset_defaults_to_unknown_rather_than_allowed() {
        // Matches the column default. A default of `Allowed` would make every asset licensed until
        // proven otherwise, which is backwards.
        assert_eq!(RightsState::default(), RightsState::Unknown);
        assert_eq!(ProvenanceState::default(), ProvenanceState::None);
    }

    #[test]
    fn every_state_round_trips_through_its_database_spelling() {
        for state in [
            RightsState::Allowed,
            RightsState::Expiring,
            RightsState::Denied,
            RightsState::Unknown,
        ] {
            assert_eq!(state.as_str().parse::<RightsState>().expect("parse"), state);
        }
        for state in [
            ProvenanceState::None,
            ProvenanceState::Valid,
            ProvenanceState::Invalid,
            ProvenanceState::Untrusted,
        ] {
            assert_eq!(
                state.as_str().parse::<ProvenanceState>().expect("parse"),
                state
            );
        }
    }

    #[test]
    fn an_unrecognised_value_is_refused_rather_than_defaulted() {
        // Silently mapping an unknown value onto a default is how a new database state becomes
        // invisible: the row says `revoked`, the API says `unknown`, and nobody notices.
        assert!("revoked".parse::<RightsState>().is_err());
        assert!("".parse::<ProvenanceState>().is_err());
    }
}
