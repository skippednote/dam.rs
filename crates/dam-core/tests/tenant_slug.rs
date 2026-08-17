//! `TenantSlug` — validated at construction so a schema name cannot be built from
//! unchecked input.
//!
//! The slug is interpolated into DDL and into `SET LOCAL search_path`. Making the
//! validated form the only way to obtain one moves that from a rule people must
//! remember to a thing the type system enforces.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::{Error, TenantSlug};

#[test]
fn well_formed_slugs_are_accepted() {
    for s in ["acme", "ab", "acme_corp", "x9", "a1_b2"] {
        assert!(TenantSlug::new(s).is_ok(), "{s} should be valid");
    }
}

#[test]
fn the_schema_name_is_the_slug_with_a_t_prefix() {
    let s = TenantSlug::new("acme").expect("valid");
    assert_eq!(s.schema_name(), "t_acme");
    assert_eq!(s.as_str(), "acme");
}

#[test]
fn malformed_and_injection_shaped_slugs_are_rejected() {
    for s in [
        "",
        "1acme",
        "Acme",
        "acme-corp",
        "acme corp",
        "acme;DROP SCHEMA dam_global CASCADE",
        "acme\"",
        "_acme",
        "t_acme; --",
        "público",
    ] {
        assert!(
            matches!(TenantSlug::new(s), Err(Error::InvalidSlug)),
            "{s:?} should have been rejected"
        );
    }
}

#[test]
fn the_length_limit_matches_the_database_check_constraint() {
    // `tenants.slug` is CHECK (slug ~ '^[a-z][a-z0-9_]{1,38}$'): one leading letter
    // PLUS 1..=38 more, so 2..=39 characters. If these disagree, one layer accepts
    // what the other refuses — and the database wins, so it is the authority here.
    //
    // This assertion originally claimed a single character was valid. It is not:
    // `{1,38}` is a minimum of one *additional* character. Reading a quantifier as
    // covering the whole pattern is an easy mistake and exactly the sort of thing a
    // test pinned to the real constraint is for.
    assert!(TenantSlug::new("ab").is_ok(), "two chars is the minimum");
    assert!(TenantSlug::new("a").is_err(), "one char must be rejected");
    assert!(
        TenantSlug::new(&format!("a{}", "b".repeat(38))).is_ok(),
        "39 chars"
    );
    assert!(
        TenantSlug::new(&format!("a{}", "b".repeat(39))).is_err(),
        "40 chars must be rejected"
    );
}

#[test]
fn a_slug_cannot_collide_with_a_reserved_schema() {
    // `t_emplate` would produce schema `t_t_emplate`, which is fine — but a slug of
    // `emplate` must not be able to produce `tenant_template`. Belt and braces:
    // reserved names are refused outright.
    for s in ["public", "pg_catalog", "information_schema", "extensions"] {
        assert!(TenantSlug::new(s).is_err(), "{s} is reserved");
    }
}
