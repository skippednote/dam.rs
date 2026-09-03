//! A public URL segment that names its own tenant (G22c).
//!
//! `/portal/{key}` and `/share/{token}` reach tables that live in a tenant schema, and neither URL used to say
//! which tenant. The process therefore had to be told once, at startup, through `server.delivery_tenant` — so
//! one damd served exactly one library's public surface however many tenants its database held.
//!
//! ## Why the tenant goes in the segment rather than in the path
//!
//! The alternative shapes were a globally unique key registry and a tenant path segment or subdomain. A global
//! registry makes portal keys first-come-first-served across customers, so the second customer to want
//! `press-kit` is told it is taken and never learns why. A path segment or subdomain changes every portal URL
//! anybody has already published. Encoding it into the segment keeps the key a per-tenant name, needs no data
//! migration, and leaves the route shape alone — `/portal/{ref}` is still one segment.
//!
//! ## The separator is `.` because a slug cannot contain one
//!
//! [`crate::TenantSlug`] is `^[a-z][a-z0-9_]{1,38}$` — lowercase, digits and underscore. No dot. So splitting
//! on the *first* dot is unambiguous and total: everything before it is the slug, everything after is the
//! caller's own key, dots included. A portal key containing a dot survives the round trip, which matters
//! because keys are chosen by people and `spring.2026` is a name somebody will pick.
//!
//! That also means this is not a format that needs versioning to stay parseable. A future scheme can prefix a
//! marker that is not a valid slug — a digit or a dot first — and be told apart from this one by the same
//! parse, because a slug's first character must be a lowercase letter.
//!
//! ## What this deliberately is not
//!
//! It is not a credential and not a signature. Naming a tenant here only chooses which schema to look in; the
//! token or key still has to be found in it, and a share token is still 256 bits of CSPRNG. Guessing a tenant
//! slug gets an attacker a lookup that fails, which is what naming a tenant that does not exist already did.

use crate::error::Error;
use crate::tenant::TenantSlug;
use std::fmt;

/// The separator between the tenant and the rest. See the module docs.
const SEPARATOR: char = '.';

/// A public URL segment carrying the tenant it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicRef {
    tenant: TenantSlug,
    rest: String,
}

impl PublicRef {
    /// Builds a reference from a tenant and the key or token it qualifies.
    ///
    /// # Errors
    /// [`Error::Validation`] if `rest` is empty. An empty key would render as a segment ending in the
    /// separator, which parses back to an empty key and looks up nothing — better refused where it is built.
    pub fn new(tenant: TenantSlug, rest: &str) -> Result<Self, Error> {
        if rest.is_empty() {
            return Err(Error::Validation {
                field: "public_ref".to_owned(),
                reason: "a public reference needs a key or token after the tenant".to_owned(),
            });
        }
        Ok(Self {
            tenant,
            rest: rest.to_owned(),
        })
    }

    /// Reads a reference out of a URL segment.
    ///
    /// # Errors
    /// [`Error::InvalidSlug`] if there is no separator or the part before it is not a tenant slug —
    /// the same error either way, because "no tenant here" and "not a tenant" are the same failure to a
    /// caller and distinguishing them would describe the format to somebody probing it.
    pub fn parse(segment: &str) -> Result<Self, Error> {
        let (slug, rest) = segment.split_once(SEPARATOR).ok_or(Error::InvalidSlug)?;
        let tenant = TenantSlug::new(slug)?;
        Self::new(tenant, rest)
    }

    /// The tenant whose schema this segment resolves in.
    #[must_use]
    pub fn tenant(&self) -> &TenantSlug {
        &self.tenant
    }

    /// The key or token, exactly as it was before the tenant was attached.
    #[must_use]
    pub fn rest(&self) -> &str {
        &self.rest
    }

    /// Renders a `{tenant}.{rest}` public segment directly, for a caller that only needs the string.
    ///
    /// Three handlers were each doing this inline and disagreeing about the impossible case — one returned an
    /// error, two fell back to the bare `rest`, each a `PublicRef::new(...).map_or/map_err` from scratch. This
    /// is the single spelling. `rest` is empty only for a caller that lost its own token or key, which the
    /// paths building these URLs never do, so the branch is unreachable; it falls back to the bare `rest`
    /// because a URL missing its tenant is a visible 404 while a 500 on an impossible branch is a mystery.
    #[must_use]
    pub fn qualify(tenant: &TenantSlug, rest: &str) -> String {
        match Self::new(tenant.clone(), rest) {
            Ok(reference) => reference.to_string(),
            Err(_) => rest.to_owned(),
        }
    }
}

impl fmt::Display for PublicRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}{}", self.tenant.as_str(), SEPARATOR, self.rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(s: &str) -> TenantSlug {
        TenantSlug::new(s).expect("a valid slug")
    }

    #[test]
    fn qualify_renders_the_same_string_as_a_built_reference() {
        // The one spelling every emitted URL uses. It must match what parsing back would expect.
        let rendered = PublicRef::qualify(&slug("acme"), "press-kit");
        assert_eq!(rendered, "acme.press-kit");
        let read = PublicRef::parse(&rendered).expect("round trips");
        assert_eq!(read.tenant().as_str(), "acme");
        assert_eq!(read.rest(), "press-kit");
    }

    #[test]
    fn a_reference_round_trips_through_its_rendering() {
        let made = PublicRef::new(slug("acme"), "press-kit").expect("valid");
        let read = PublicRef::parse(&made.to_string()).expect("parses");
        assert_eq!(read, made);
        assert_eq!(read.tenant().as_str(), "acme");
        assert_eq!(read.rest(), "press-kit");
    }

    #[test]
    fn a_key_may_contain_the_separator_because_only_the_first_one_splits() {
        // `spring.2026` is a name somebody picks. Splitting on the last dot, or on every dot, would either
        // lose part of the key or refuse a legitimate one.
        let made = PublicRef::new(slug("acme"), "spring.2026.launch").expect("valid");
        let read = PublicRef::parse(&made.to_string()).expect("parses");
        assert_eq!(read.rest(), "spring.2026.launch");
        assert_eq!(read.tenant().as_str(), "acme");
    }

    #[test]
    fn a_hex_share_token_round_trips_unchanged() {
        let token = "0".repeat(64);
        let read = PublicRef::parse(&format!("acme.{token}")).expect("parses");
        assert_eq!(read.rest(), token);
    }

    #[test]
    fn a_segment_with_no_tenant_is_refused_rather_than_defaulted() {
        // The whole point of the change: a bare key must not resolve, or the single-tenant assumption
        // survives as a fallback and nothing ever stops depending on it.
        assert!(matches!(
            PublicRef::parse("press-kit"),
            Err(Error::InvalidSlug)
        ));
    }

    #[test]
    fn a_leading_part_that_is_not_a_slug_is_refused() {
        for segment in [
            "Acme.press-kit",  // uppercase
            "1acme.press-kit", // leading digit
            "a.press-kit",     // one character, below the minimum
            "ac-me.press-kit", // hyphen is not in the slug alphabet
            ".press-kit",      // empty tenant
        ] {
            assert!(
                PublicRef::parse(segment).is_err(),
                "{segment} must not parse as a tenant reference"
            );
        }
    }

    #[test]
    fn an_empty_key_is_refused_at_both_ends() {
        assert!(PublicRef::new(slug("acme"), "").is_err());
        assert!(PublicRef::parse("acme.").is_err());
    }

    #[test]
    fn a_future_scheme_can_be_told_apart_by_the_same_parse() {
        // A slug must start with a lowercase letter, so any marker that does not is unambiguously not this
        // format. Stated as a test because it is the property that lets this format be replaced without a
        // version field in it.
        for other in ["2.press-kit", ".v2.press-kit", "_v2.press-kit"] {
            assert!(
                PublicRef::parse(other).is_err(),
                "{other} must not be mistaken for a tenant reference"
            );
        }
    }
}
