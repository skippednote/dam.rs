//! Log output must never carry a secret or tenant data.
//!
//! Written before the implementation (D17). This is the test that matters most in
//! this crate: `Secret` already refuses to render, but the guarantee is only real
//! if the subscriber that actually runs in production honours it. Asserting on
//! captured output proves the whole path rather than the type in isolation.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::{
    Secret,
    config::{LogFormat, TelemetryConfig},
};
use dam_telemetry::{CaptureWriter, subscriber};

const PLAINTEXT: &str = "hunter2-super-secret";

fn json_cfg() -> TelemetryConfig {
    TelemetryConfig {
        log_format: LogFormat::Json,
        otlp_endpoint: None,
        service_name: "damrs-test".into(),
    }
}

#[test]
fn a_secret_field_is_redacted_in_json_output() {
    let cap = CaptureWriter::new();
    let sub = subscriber(&json_cfg(), cap.clone(), "info");
    tracing::subscriber::with_default(sub, || {
        let key = Secret::new(PLAINTEXT.to_owned());
        tracing::info!(signing_key = ?key, "loaded config");
    });
    let out = cap.contents();
    assert!(!out.contains(PLAINTEXT), "secret leaked into logs: {out}");
    assert!(
        out.contains("REDACTED"),
        "expected redaction marker in {out}"
    );
}

#[test]
fn json_output_is_one_valid_json_object_per_line() {
    let cap = CaptureWriter::new();
    let sub = subscriber(&json_cfg(), cap.clone(), "info");
    tracing::subscriber::with_default(sub, || {
        tracing::info!(count = 3, "first");
        tracing::info!("second");
    });
    let out = cap.contents();
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "expected 2 lines, got: {out}");
    for line in lines {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON {line:?}: {e}"));
        assert!(
            v.get("fields").is_some() || v.get("message").is_some(),
            "{v}"
        );
    }
}

#[test]
fn the_filter_drops_events_below_the_configured_level() {
    let cap = CaptureWriter::new();
    let sub = subscriber(&json_cfg(), cap.clone(), "warn");
    tracing::subscriber::with_default(sub, || {
        tracing::debug!("should not appear");
        tracing::info!("should not appear either");
        tracing::warn!("this one should");
    });
    let out = cap.contents();
    assert!(!out.contains("should not appear"), "{out}");
    assert!(out.contains("this one should"), "{out}");
}

#[test]
fn a_per_target_filter_directive_is_honoured() {
    let cap = CaptureWriter::new();
    // Mirrors the shipped RUST_LOG: our own crates verbose, dependencies quiet.
    let sub = subscriber(&json_cfg(), cap.clone(), "warn,redaction=debug");
    tracing::subscriber::with_default(sub, || {
        tracing::debug!("crate-local debug is kept");
    });
    assert!(cap.contents().contains("crate-local debug is kept"));
}
