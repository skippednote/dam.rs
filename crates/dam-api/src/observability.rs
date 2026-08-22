//! `/metrics` and `/ready`, and the middleware that feeds the first one.
//!
//! ## Two endpoints because they answer two different questions
//!
//! `/health` already exists and says nothing: it is liveness, and a process that can answer it at all is a
//! process worth leaving alone. `/ready` is the other question — *should traffic come here* — and it can only
//! be answered by touching the things a request will touch. A load balancer pointing at a replica whose
//! database is unreachable is a load balancer serving 500s it could have avoided.
//!
//! So `/ready` checks Postgres and the object store, with a short budget, and names which one failed. It
//! deliberately does **not** check the search index: a tenant's index is opened lazily per request and a
//! missing one is rebuildable, so failing readiness over it would take a replica out of rotation for
//! something that does not stop uploads, downloads or metadata.
//!
//! ## `/metrics` is fail-closed
//!
//! No token configured, no endpoint — 404, not 401, so a scan cannot distinguish "off" from "protected". The
//! module docs on `/health` make the argument already: it is the first thing anybody scans, and route
//! templates plus per-route request counts are a map of the API and a usage profile. The default that ends up
//! exposed is the one that serves openly and trusts a firewall.

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use dam_core::Secret;
use dam_telemetry::metrics::Metrics;
use std::sync::Arc;
use std::time::Instant;

/// What the observability routes need.
pub struct ObservabilityState {
    pub global: sqlx::PgPool,
    pub store: Arc<dyn dam_store::BlobStore>,
    pub metrics: Metrics,
    /// `None` disables `/metrics` entirely. See the module docs.
    pub metrics_token: Option<Secret<String>>,
}

impl std::fmt::Debug for ObservabilityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservabilityState").finish_non_exhaustive()
    }
}

pub fn router(state: ObservabilityState) -> Router {
    Router::new()
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .with_state(Arc::new(state))
}

/// Records every request into the registry.
///
/// Uses `MatchedPath` — the route *template* — and falls back to a fixed string rather than the URI when
/// there is no match. That fallback is the important part: an unmatched request is a 404, and labelling those
/// with their URIs would let anybody create unbounded series by requesting random paths.
pub async fn record(State(metrics): State<Metrics>, request: Request, next: Next) -> Response {
    let method = match *request.method() {
        axum::http::Method::GET => "GET",
        axum::http::Method::POST => "POST",
        axum::http::Method::PUT => "PUT",
        axum::http::Method::PATCH => "PATCH",
        axum::http::Method::DELETE => "DELETE",
        axum::http::Method::HEAD => "HEAD",
        axum::http::Method::OPTIONS => "OPTIONS",
        // A `&'static str` label, so anything exotic collapses into one series rather than allocating one.
        _ => "other",
    };
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_owned());

    let started = Instant::now();
    let response = next.run(request).await;
    metrics.request(
        method,
        &route,
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    response
}

/// What a readiness probe reports.
#[derive(Debug, serde::Serialize)]
struct Readiness {
    database: &'static str,
    storage: &'static str,
}

async fn ready(State(state): State<Arc<ObservabilityState>>) -> Response {
    // Both checks always run, rather than short-circuiting on the first failure. A probe that stops at the
    // database tells an operator the database is down and nothing about whether the store is *also* down,
    // which is exactly the question during a wide outage.
    let database = match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.global)
        .await
    {
        Ok(_) => "ok",
        Err(error) => {
            tracing::warn!(%error, "readiness: database");
            "unreachable"
        }
    };
    // A listing rather than a write: readiness must not leave objects behind, and a bucket that lists is a
    // bucket credentials and network both reach.
    let storage = match state.store.list("readiness/", 1).await {
        Ok(_) => "ok",
        Err(error) => {
            tracing::warn!(%error, "readiness: storage");
            "unreachable"
        }
    };

    let body = Readiness { database, storage };
    let code = if database == "ok" && storage == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, axum::Json(body)).into_response()
}

async fn metrics(State(state): State<Arc<ObservabilityState>>, headers: HeaderMap) -> Response {
    let Some(expected) = state.metrics_token.as_ref() else {
        // 404, not 401. See the module docs.
        return StatusCode::NOT_FOUND.into_response();
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();

    if !constant_time_eq(presented.as_bytes(), expected.expose().as_bytes()) {
        // Also 404: a wrong token learns the same thing as no token, which is nothing.
        return StatusCode::NOT_FOUND.into_response();
    }

    // Refreshed on scrape rather than on a timer. Queue depth is a fact about the database, not something this
    // process accumulates, and a background refresher would be a second schedule to reason about for one
    // cheap grouped query over a table that is small by construction — the queue is work not yet done.
    //
    // A failure here does not fail the scrape: the HTTP metrics are still worth having, and a monitoring
    // endpoint that returns 500 because one gauge could not be read is a monitoring endpoint that goes dark
    // exactly when the database is the thing going wrong.
    match queue_depth(&state.global).await {
        Ok(series) => state.metrics.set_gauge("damrs_jobs", series),
        Err(error) => tracing::warn!(%error, "metrics: reading queue depth"),
    }

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

/// Jobs by kind and state.
///
/// The `dead` and `failed` counts are the ones worth alerting on, and they are invisible without this: a
/// worker that fails every derivative looks identical from the outside to one with nothing to do.
async fn queue_depth(global: &sqlx::PgPool) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT kind, state, count(*) FROM dam_global.jobs GROUP BY kind, state ORDER BY kind, state",
    )
    .fetch_all(global)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, state, count)| (format!(r#"kind="{kind}",state="{state}""#), count))
        .collect())
}

/// Length-independent comparison.
///
/// The lengths themselves are compared first and short-circuit, which leaks the token *length* and nothing
/// else — the standard trade, and the alternative (hashing both sides) is more machinery than a scrape
/// credential warrants.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_rejects_the_obvious_cases() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrer"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
