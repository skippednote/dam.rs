//! Domain errors.
//!
//! `thiserror` per crate, `anyhow` only in binaries. This is the base error type;
//! other crates define their own and wrap this one where a domain error surfaces
//! through an infrastructure operation.
//!
//! Two rules the variants encode:
//!
//! 1. **Error messages must not carry secrets or tenant data.** Errors are logged,
//!    returned over the API, and attached to spans. Identifiers and field names are
//!    fine; values are not — hence `Validation { field, reason }` rather than a
//!    formatted message with the offending value in it.
//! 2. **A denial says why, in machine-readable form.** `Forbidden` and
//!    `RightsDenied` carry reason codes rather than prose, because the API has to
//!    explain a refusal to a user (ARCHITECTURE §11.3, D12) and a UI cannot branch
//!    on a sentence.

use std::fmt;

/// The kind of thing an operation was looking for. Kept as a small enum rather
/// than a string so `NotFound` cannot be constructed with a typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Asset,
    Collection,
    Derivative,
    FieldDef,
    License,
    Pool,
    Release,
    Role,
    Taxonomy,
    TaxonomyTerm,
    Tenant,
    User,
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Asset => "asset",
            Self::Collection => "collection",
            Self::Derivative => "derivative",
            Self::FieldDef => "field definition",
            Self::License => "license",
            Self::Pool => "storage pool",
            Self::Release => "release",
            Self::Role => "role",
            Self::Taxonomy => "taxonomy",
            Self::TaxonomyTerm => "taxonomy term",
            Self::Tenant => "tenant",
            Self::User => "user",
        };
        f.write_str(s)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration could not be loaded or is internally inconsistent.
    #[error("configuration error: {0}")]
    Config(String),

    /// A value failed schema validation. Carries the field and a reason code —
    /// never the value, which may be tenant data.
    #[error("invalid value for field `{field}`: {reason}")]
    Validation { field: String, reason: String },

    /// A tenant slug did not match `^[a-z][a-z0-9_]{1,38}$`. Its own variant
    /// rather than a `Validation` because it gates schema-name construction, and
    /// a caller that ignores it produces a SQL injection rather than a bad row.
    #[error("invalid tenant slug: must match ^[a-z][a-z0-9_]{{1,38}}$")]
    InvalidSlug,

    #[error("{kind} not found: {id}")]
    NotFound { kind: ResourceKind, id: String },

    #[error("{kind} already exists: {id}")]
    Conflict { kind: ResourceKind, id: String },

    /// The caller's grants do not permit this. `reasons` are stable codes the API
    /// and UI can branch on.
    #[error("forbidden: {}", reasons.join(", "))]
    Forbidden { reasons: Vec<String> },

    /// Rights evaluation denied the operation for this channel and territory.
    /// Distinct from `Forbidden`: that is about who you are, this is about what the
    /// licence permits (D12).
    #[error("rights denied for {channel}/{territory}: {}", reasons.join(", "))]
    RightsDenied {
        channel: String,
        territory: String,
        reasons: Vec<String>,
    },

    /// The operation is understood but not permitted in the current state — an
    /// archived tenant, a session at its budget, an asset mid-restore.
    #[error("invalid state: {0}")]
    InvalidState(String),

    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl Error {
    pub fn validation(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            reason: reason.into(),
        }
    }

    pub fn not_found(kind: ResourceKind, id: impl fmt::Display) -> Self {
        Self::NotFound {
            kind,
            id: id.to_string(),
        }
    }

    pub fn conflict(kind: ResourceKind, id: impl fmt::Display) -> Self {
        Self::Conflict {
            kind,
            id: id.to_string(),
        }
    }

    /// True when retrying the identical request could plausibly succeed. Used by
    /// the job queue to decide between a retry and the dead-letter state.
    pub fn is_retryable(&self) -> bool {
        // Every variant here is a caller or data problem. Infrastructure errors
        // live in the crates that own the infrastructure, and those decide their
        // own retryability.
        false
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_names_the_field_but_not_the_value() {
        let e = Error::validation("brand", "not in vocabulary");
        let msg = e.to_string();
        assert!(msg.contains("brand"));
        assert!(msg.contains("not in vocabulary"));
    }

    #[test]
    fn denials_render_their_reason_codes() {
        let e = Error::RightsDenied {
            channel: "paid_social".into(),
            territory: "DE".into(),
            reasons: vec!["channel_not_licensed".into(), "release_expired".into()],
        };
        let msg = e.to_string();
        assert!(msg.contains("paid_social/DE"));
        assert!(msg.contains("channel_not_licensed"));
        assert!(msg.contains("release_expired"));
    }

    #[test]
    fn not_found_renders_the_kind_readably() {
        let e = Error::not_found(ResourceKind::TaxonomyTerm, "abc");
        assert_eq!(e.to_string(), "taxonomy term not found: abc");
    }
}
