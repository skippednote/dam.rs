//! Span field conventions: tenant id on every span, tenant *data* on none.
//!
//! The distinction is the point. `tenant_id` is an opaque uuid and is safe to
//! attach to every span — it is what makes a trace attributable when a customer
//! reports a problem. An asset filename, a metadata value, or a search query is
//! tenant data and must not be there, because spans are exported to a collector
//! that sits outside the tenant's isolation boundary.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::config::{LogFormat, TelemetryConfig};
use dam_telemetry::{CaptureWriter, RequestContext, subscriber};
use uuid::Uuid;

fn cfg() -> TelemetryConfig {
    TelemetryConfig {
        log_format: LogFormat::Json,
        otlp_endpoint: None,
        service_name: "damrs-test".into(),
    }
}

#[test]
fn tenant_and_request_ids_appear_on_events_inside_the_span() {
    let tenant = Uuid::now_v7();
    let request = Uuid::now_v7();
    let cap = CaptureWriter::new();
    let sub = subscriber(&cfg(), cap.clone(), "info");
    tracing::subscriber::with_default(sub, || {
        let ctx = RequestContext::new(request).with_tenant(tenant);
        ctx.span().in_scope(|| tracing::info!("handling"));
    });
    let out = cap.contents();
    assert!(out.contains(&tenant.to_string()), "no tenant_id in {out}");
    assert!(out.contains(&request.to_string()), "no request_id in {out}");
}

#[test]
fn ids_propagate_into_a_child_span() {
    let tenant = Uuid::now_v7();
    let cap = CaptureWriter::new();
    let sub = subscriber(&cfg(), cap.clone(), "info");
    tracing::subscriber::with_default(sub, || {
        let ctx = RequestContext::new(Uuid::now_v7()).with_tenant(tenant);
        ctx.span().in_scope(|| {
            tracing::info_span!("derive_thumbnail").in_scope(|| tracing::info!("inner"));
        });
    });
    let out = cap.contents();
    assert!(
        out.contains(&tenant.to_string()),
        "child lost tenant_id: {out}"
    );
    assert!(out.contains("derive_thumbnail"), "{out}");
}

#[test]
fn an_anonymous_request_has_no_tenant_field_rather_than_a_placeholder() {
    // Pre-auth requests genuinely have no tenant. Emitting "unknown" or a nil uuid
    // would make a real tenant id indistinguishable from a missing one when
    // querying traces.
    let cap = CaptureWriter::new();
    let sub = subscriber(&cfg(), cap.clone(), "info");
    tracing::subscriber::with_default(sub, || {
        RequestContext::new(Uuid::now_v7())
            .span()
            .in_scope(|| tracing::info!("pre-auth"));
    });
    let out = cap.contents();
    assert!(
        !out.contains("00000000-0000"),
        "nil uuid placeholder: {out}"
    );
    assert!(!out.contains("\"unknown\""), "string placeholder: {out}");
}
