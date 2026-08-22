//! Tenant identity.
//!
//! [`TenantSlug`] is validated at construction, and constructing one is the only way
//! to obtain a schema name. The slug reaches DDL (`CREATE SCHEMA "t_acme"`) and
//! `SET LOCAL search_path`, neither of which takes bind parameters — so validation
//! cannot be deferred to the query layer. Making the checked form the only form moves
//! this from a rule people have to remember to something the type system enforces.

use crate::Error;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Schemas a slug must never be able to produce or shadow.
///
/// `extensions` and `dam_global` matter because a tenant schema that shadowed either
/// would break every schema-qualified type reference in the tenant migrations.
const RESERVED: &[&str] = &[
    "public",
    "pg_catalog",
    "pg_toast",
    "information_schema",
    "extensions",
    "dam_global",
    "tenant_template",
];

/// A validated tenant slug: `^[a-z][a-z0-9_]{1,38}$`.
///
/// The shape is the same one `dam_global.tenants.slug` enforces with a CHECK
/// constraint. Keeping them identical means neither layer accepts what the other
/// refuses — `the_length_limit_matches_the_database_check_constraint` asserts it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct TenantSlug(String);

impl TenantSlug {
    /// Validates and wraps. This is the only constructor.
    pub fn new(s: &str) -> Result<Self, Error> {
        // 2..=39 characters: one leading letter plus 1..=38 more.
        if !(2..=39).contains(&s.len()) {
            return Err(Error::InvalidSlug);
        }
        let mut chars = s.chars();
        let Some(first) = chars.next() else {
            return Err(Error::InvalidSlug);
        };
        if !first.is_ascii_lowercase() {
            return Err(Error::InvalidSlug);
        }
        if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(Error::InvalidSlug);
        }
        if RESERVED.contains(&s) {
            return Err(Error::InvalidSlug);
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The Postgres schema holding this tenant's data.
    ///
    /// Safe to interpolate into DDL because [`Self::new`] has already restricted the
    /// character set to lowercase ASCII, digits, and underscore. Still quote it at
    /// the call site — defence in depth costs nothing.
    pub fn schema_name(&self) -> String {
        format!("t_{}", self.0)
    }
}

impl fmt::Display for TenantSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for TenantSlug {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Deserialises through [`Self::new`], so a slug arriving from JSON or TOML is
/// validated on the way in rather than trusted.
impl<'de> Deserialize<'de> for TenantSlug {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialisation_validates() {
        let ok: Result<TenantSlug, _> = serde_json::from_str("\"acme\"");
        assert!(ok.is_ok());
        let bad: Result<TenantSlug, _> = serde_json::from_str("\"Acme; DROP\"");
        assert!(bad.is_err(), "deserialisation must not bypass validation");
    }

    #[test]
    fn round_trip_through_serde_preserves_the_slug() {
        let s = TenantSlug::new("acme_corp").expect("valid");
        let json = serde_json::to_string(&s).expect("serialise");
        let back: TenantSlug = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(s, back);
    }
}
