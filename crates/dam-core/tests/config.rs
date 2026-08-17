//! Config precedence.
//!
//! Written before the implementation (D17). Precedence is the whole point of
//! having a config layer: an operator setting `DAMRS_DATABASE__URL` in a container
//! must beat the checked-in TOML, and both must beat the built-in default. Getting
//! the order wrong is a bug that only shows up in production, where the file wins
//! and nobody can work out why.
//!
//! `figment::Jail` isolates the process environment and working directory under a
//! mutex, so these run in parallel without leaking into each other — and without a
//! test-only code path in `Config` itself.

// Panicking IS the assertion in a test, so the workspace's `unwrap_used` /
// `expect_used` denials are relaxed here only. `result_large_err` fires on
// figment::Jail closures, whose Err type we do not control.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::config::{Config, Environment};
use figment::Jail;

#[test]
fn defaults_load_with_no_file_and_no_env() {
    Jail::expect_with(|_| {
        let cfg = Config::load(None::<&str>).expect("defaults must load");
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.database.max_connections, 16);
        assert_eq!(cfg.environment, Environment::Development);
        Ok(())
    });
}

#[test]
fn file_overrides_default() {
    Jail::expect_with(|jail| {
        jail.create_file("damrs.toml", "[server]\nport = 9999\n")?;
        let cfg = Config::load(Some("damrs.toml")).expect("load");
        assert_eq!(cfg.server.port, 9999, "file must beat the default");
        assert_eq!(
            cfg.database.max_connections, 16,
            "keys absent from the file keep their default"
        );
        Ok(())
    });
}

#[test]
fn env_overrides_file() {
    Jail::expect_with(|jail| {
        jail.create_file("damrs.toml", "[server]\nport = 9999\n")?;
        jail.set_env("DAMRS_SERVER__PORT", 7777);
        let cfg = Config::load(Some("damrs.toml")).expect("load");
        assert_eq!(cfg.server.port, 7777, "env must beat the file");
        Ok(())
    });
}

#[test]
fn env_reaches_nested_keys_via_double_underscore() {
    Jail::expect_with(|jail| {
        jail.set_env("DAMRS_DATABASE__MAX_CONNECTIONS", 64);
        let cfg = Config::load(None::<&str>).expect("load");
        assert_eq!(cfg.database.max_connections, 64);
        Ok(())
    });
}

#[test]
fn a_missing_config_file_is_not_an_error() {
    // Figment's Toml::file is lenient about absence by design, and that is the
    // behaviour we want: a container with no mounted config should start on
    // defaults plus env rather than refusing to boot.
    Jail::expect_with(|_| {
        let cfg = Config::load(Some("definitely-not-here.toml")).expect("must fall back");
        assert_eq!(cfg.server.port, 8080);
        Ok(())
    });
}

#[test]
fn invalid_value_is_a_config_error_not_a_panic() {
    Jail::expect_with(|jail| {
        jail.set_env("DAMRS_SERVER__PORT", "not-a-number");
        let err = Config::load(None::<&str>).expect_err("a non-numeric port must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("port"),
            "the error must name the offending key, got: {msg}"
        );
        Ok(())
    });
}

#[test]
fn an_unknown_key_in_the_file_is_rejected() {
    // `deny_unknown_fields`. A typo'd key that is silently ignored is how an
    // operator ends up convinced they changed a setting when they did not.
    Jail::expect_with(|jail| {
        jail.create_file("damrs.toml", "[server]\nprot = 9999\n")?;
        let err = Config::load(Some("damrs.toml")).expect_err("typo must be rejected");
        assert!(err.to_string().contains("prot"), "{err}");
        Ok(())
    });
}

#[test]
fn production_environment_rejects_the_dev_placeholder_signing_key() {
    // A dev placeholder reaching production means every signed URL is forgeable by
    // anyone who has read the source. That fails startup; it does not warn.
    // ARCHITECTURE §12.
    Jail::expect_with(|jail| {
        jail.set_env("DAMRS_ENVIRONMENT", "production");
        jail.set_env("DAMRS_TELEMETRY__LOG_FORMAT", "json");
        let err = Config::load(None::<&str>)
            .expect_err("the dev placeholder must not be accepted in production");
        assert!(err.to_string().contains("signing"), "{err}");
        Ok(())
    });
}
