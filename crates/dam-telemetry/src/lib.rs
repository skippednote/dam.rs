//! Tracing, structured logs, and OTLP export.
//!
//! Its own crate rather than part of `dam-core` because `tracing-subscriber` and
//! `opentelemetry-otlp` are concrete infrastructure (the latter pulls in a gRPC
//! stack), and `dam-core` is deliberately free of that. All three binaries depend
//! on this so initialisation cannot drift between them.
//!
//! ## Field conventions
//!
//! **`tenant_id` on every span. Tenant data on none.**
//!
//! That distinction is the whole rule. `tenant_id` is an opaque uuid, and having it
//! on every span is what makes a trace attributable when a customer reports a
//! problem. An asset filename, a metadata value, or a search query is tenant data
//! and must never appear, because spans leave the process for a collector that sits
//! outside the tenant's isolation boundary — and unlike the database, a trace
//! backend has no schema-per-tenant guarantee.
//!
//! Secrets are typed as [`dam_core::Secret`], which refuses to render. The
//! [`redaction`](../tests/redaction.rs) suite asserts that end to end through the
//! real subscriber, not just on the type.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)
)]

use dam_core::config::{LogFormat, TelemetryConfig};
use std::{
    io,
    sync::{Arc, Mutex},
};
use tracing::Span;
use tracing_subscriber::{
    EnvFilter, Layer, layer::SubscriberExt, registry::Registry, util::SubscriberInitExt,
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid log filter directive: {0}")]
    Filter(String),
    #[error("otlp exporter: {0}")]
    Otlp(String),
    #[error("a global tracing subscriber is already installed")]
    AlreadyInitialised,
}

/// Identifiers carried on every span for one unit of work.
///
/// Constructed once per request (or per job) and used to open the root span.
/// Deliberately holds only opaque identifiers — there is no field here that could
/// carry tenant content, which is the cheapest way to enforce the convention above.
#[derive(Debug, Clone, Copy)]
pub struct RequestContext {
    request_id: Uuid,
    tenant_id: Option<Uuid>,
    actor_id: Option<Uuid>,
}

impl RequestContext {
    pub fn new(request_id: Uuid) -> Self {
        Self {
            request_id,
            tenant_id: None,
            actor_id: None,
        }
    }

    /// Generates a fresh request id. Used at the edge when the caller supplied none.
    pub fn generate() -> Self {
        Self::new(Uuid::now_v7())
    }

    pub fn with_tenant(mut self, tenant_id: Uuid) -> Self {
        self.tenant_id = Some(tenant_id);
        self
    }

    pub fn with_actor(mut self, actor_id: Uuid) -> Self {
        self.actor_id = Some(actor_id);
        self
    }

    pub fn request_id(&self) -> Uuid {
        self.request_id
    }

    pub fn tenant_id(&self) -> Option<Uuid> {
        self.tenant_id
    }

    /// Opens the root span. Absent ids are recorded as `tracing::field::Empty`
    /// rather than a placeholder string — a nil uuid or `"unknown"` would make a
    /// genuinely missing tenant indistinguishable from a real one when querying
    /// traces, and pre-auth requests legitimately have no tenant.
    pub fn span(&self) -> Span {
        let span = tracing::info_span!(
            "request",
            request_id = %self.request_id,
            tenant_id = tracing::field::Empty,
            actor_id = tracing::field::Empty,
        );
        if let Some(t) = self.tenant_id {
            span.record("tenant_id", tracing::field::display(t));
        }
        if let Some(a) = self.actor_id {
            span.record("actor_id", tracing::field::display(a));
        }
        span
    }
}

/// An in-memory [`tracing_subscriber::fmt::MakeWriter`] for tests.
///
/// Lives in the library rather than a test helper so the redaction suite exercises
/// the same subscriber construction production uses. A capture writer that only
/// exists in tests invites a subscriber that only exists in tests.
#[derive(Debug, Clone, Default)]
pub struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl CaptureWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Captured output as UTF-8. Lossy because log output is bytes and a panic here
    /// would obscure whatever the test was actually asserting.
    pub fn contents(&self) -> String {
        match self.0.lock() {
            Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
            // A poisoned lock means another thread panicked mid-write; the test is
            // already failing, so surface it as visible output rather than a second
            // panic that hides the first.
            Err(poisoned) => String::from_utf8_lossy(&poisoned.into_inner()).into_owned(),
        }
    }
}

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.lock() {
            Ok(mut inner) => {
                inner.extend_from_slice(buf);
                Ok(buf.len())
            }
            Err(_) => Err(io::Error::other("capture buffer poisoned")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Builds the subscriber without installing it globally.
///
/// Tests drive this through `tracing::subscriber::with_default`, so they exercise
/// exactly the layer stack production installs. An invalid filter directive falls
/// back to `info` rather than failing — losing observability is worse than losing
/// precision, and a typo in `RUST_LOG` should not stop a service booting.
pub fn subscriber<W>(
    cfg: &TelemetryConfig,
    writer: W,
    filter: &str,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));

    let fmt_layer = match cfg.log_format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            // `with_span_list(true)` is required, not cosmetic. `with_current_span`
            // alone emits only the INNERMOST span's fields, so an event inside a
            // child span (`derive_thumbnail` under `request`) loses `tenant_id`
            // entirely — precisely the attributability the field convention above
            // exists to guarantee. The cost is a more verbose line; the alternative
            // is traces that cannot be tied to a tenant, which makes the convention
            // decorative. Proved by `ids_propagate_into_a_child_span`.
            .with_span_list(true)
            .with_writer(writer)
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_writer(writer)
            .boxed(),
    };

    Registry::default().with(env_filter).with(fmt_layer)
}

/// Held by `main` for the lifetime of the process. Dropping it flushes any pending
/// OTLP spans — without it, the last trace of a short-lived job is lost.
///
/// opentelemetry 0.32 removed `global::shutdown_tracer_provider()`, so the guard
/// owns the provider and shuts it down directly. Losing that is easy and silent:
/// the process exits cleanly and the final spans simply never arrive.
#[derive(Debug)]
pub struct Guard {
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            if let Err(e) = provider.shutdown() {
                // Nothing useful to do this late — the subscriber may already be
                // torn down, so this goes to stderr rather than through tracing.
                eprintln!("otlp shutdown failed: {e}");
            }
        }
    }
}

/// Installs the global subscriber. Call once, from `main`.
///
/// `filter` is normally `std::env::var("RUST_LOG")`; the caller passes it in so this
/// function does not read the environment behind the config layer's back.
pub fn init(cfg: &TelemetryConfig, filter: &str) -> Result<Guard, Error> {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));

    let fmt_layer = match cfg.log_format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(true) // see the note in `subscriber()`
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer().with_target(true).boxed(),
    };

    let registry = Registry::default().with(env_filter).with(fmt_layer);

    match &cfg.otlp_endpoint {
        Some(endpoint) => {
            let (otel_layer, provider) = otlp_layer(endpoint, &cfg.service_name)?;
            registry
                .with(otel_layer)
                .try_init()
                .map_err(|_| Error::AlreadyInitialised)?;
            Ok(Guard {
                provider: Some(provider),
            })
        }
        None => {
            registry.try_init().map_err(|_| Error::AlreadyInitialised)?;
            Ok(Guard { provider: None })
        }
    }
}

type OtlpLayer<S> =
    tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>;

/// Returns the layer plus the provider, because the caller must keep the provider
/// alive to flush on shutdown (see [`Guard`]).
fn otlp_layer<S>(
    endpoint: &str,
    service_name: &str,
) -> Result<(OtlpLayer<S>, opentelemetry_sdk::trace::SdkTracerProvider), Error>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::{Resource, trace::SdkTracerProvider};

    // OTLP/HTTP-protobuf rather than gRPC: reuses the reqwest stack already present
    // for the Anthropic client instead of pulling tonic, and every collector
    // supports it (port 4318).
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e: opentelemetry_otlp::ExporterBuildError| Error::Otlp(e.to_string()))?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", service_name.to_owned())])
                .build(),
        )
        .build();

    let tracer = opentelemetry::trace::TracerProvider::tracer(&provider, "damrs");
    opentelemetry::global::set_tracer_provider(provider.clone());
    Ok((tracing_opentelemetry::layer().with_tracer(tracer), provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invalid_filter_falls_back_to_info_rather_than_failing() {
        let cfg = TelemetryConfig {
            log_format: LogFormat::Json,
            otlp_endpoint: None,
            service_name: "t".into(),
        };
        let cap = CaptureWriter::new();
        let sub = subscriber(&cfg, cap.clone(), "this=is=not=a=filter");
        tracing::subscriber::with_default(sub, || {
            tracing::info!("still logging");
            tracing::debug!("but not debug");
        });
        let out = cap.contents();
        assert!(out.contains("still logging"), "{out}");
        assert!(!out.contains("but not debug"), "{out}");
    }

    #[test]
    fn context_builders_are_additive() {
        let r = Uuid::now_v7();
        let t = Uuid::now_v7();
        let ctx = RequestContext::new(r).with_tenant(t);
        assert_eq!(ctx.request_id(), r);
        assert_eq!(ctx.tenant_id(), Some(t));
    }
}
