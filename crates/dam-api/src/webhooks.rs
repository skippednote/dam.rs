//! Webhook subscriptions over HTTP (Q.20c, §11).
//!
//! The outbox and the sender are `dam_db::webhooks` and `dam_connect::webhooks`. This is the administration:
//! register an endpoint, see what has been delivered to it, and retry what was abandoned.
//!
//! ## Manage, and the secret is shown exactly once
//!
//! The signing key is returned by `POST` and never again. It has to be shown once, because the receiver cannot
//! verify anything without it; showing it on every read would put it in the response of an endpoint an
//! integration polls, in logs, and in anybody's browser history. Same discipline as an API key.
//!
//! ## The URL is validated on the way in
//!
//! `https` only outside development, no credentials in the URL, and a host that is not a loopback or
//! link-local address. That last one is the important one: a subscription is a server-side request to a URL a
//! tenant chose, which is the definition of SSRF — without it a tenant could point a webhook at
//! `http://169.254.169.254/` and have damrs fetch cloud instance credentials on their behalf.
//!
//! ## Deliveries are read-only except for a retry
//!
//! An operator can see the log and re-queue a dead letter. They cannot edit a payload or fabricate a delivery:
//! the outbox records what happened, and a writable log is not a record.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::webhooks;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// How many log rows a read returns by default.
const LOG_PAGE: i64 = 50;

pub struct WebhookState {
    pub global: PgPool,
    /// Whether to insist on `https`. False in development, where a receiver on localhost is the normal case.
    pub require_https: bool,
}

impl std::fmt::Debug for WebhookState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookState")
            .field("require_https", &self.require_https)
            .finish_non_exhaustive()
    }
}

pub fn router(state: WebhookState) -> Router {
    Router::new()
        .route("/webhooks", get(list).post(create))
        .route("/webhooks/{id}", axum::routing::delete(remove))
        .route("/webhooks/{id}/enable", post(enable))
        .route("/webhooks/{id}/deliveries", get(deliveries))
        .route("/webhooks/{id}/deliveries/{delivery_id}/retry", post(retry))
        .with_state(Arc::new(state))
}

/// One subscription. Never carries the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SubscriptionView {
    pub id: Uuid,
    pub url: String,
    /// The event kinds wanted. Empty means all of them.
    pub event_kinds: Vec<String>,
    pub active: bool,
    /// Why the system disabled it. Present only when it did.
    pub disabled_reason: Option<String>,
    /// Deliveries abandoned in a row. Reset by any success, and by enabling it again.
    pub consecutive_failures: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<webhooks::Subscription> for SubscriptionView {
    fn from(row: webhooks::Subscription) -> Self {
        Self {
            id: row.id,
            url: row.url,
            event_kinds: row.event_kinds,
            active: row.active,
            disabled_reason: row.disabled_reason,
            consecutive_failures: row.consecutive_failures,
            created_at: row.created_at,
        }
    }
}

#[utoipa::path(
    get,
    path = "/webhooks",
    responses((status = 200, description = "Every subscription, without secrets", body = [SubscriptionView])),
    tag = "webhooks",
)]
pub async fn list(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<SubscriptionView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = webhooks::subscriptions(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(rows.into_iter().map(SubscriptionView::from).collect()))
}

/// An endpoint to register.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct NewSubscriptionBody {
    pub url: String,
    /// Which events to send. Omit or leave empty for all of them.
    #[serde(default)]
    pub event_kinds: Vec<String>,
}

/// A newly created subscription, with its signing key.
///
/// The only response that ever carries the secret. A receiver cannot verify a delivery without it, so it has
/// to be shown once — and once only, for the same reason an API key is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreatedView {
    #[serde(flatten)]
    pub subscription: SubscriptionView,
    /// Shown now and never again. Store it: it is what verifies every delivery's signature.
    pub secret: String,
    /// How to check a delivery, so the answer does not have to be looked up elsewhere.
    pub signature_note: String,
}

#[utoipa::path(
    post,
    path = "/webhooks",
    request_body = NewSubscriptionBody,
    responses(
        (status = 201, description = "Registered; the secret is in this response only", body = CreatedView),
        (status = 422, description = "The URL is not one this server will post to"),
    ),
    tag = "webhooks",
)]
pub async fn create(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Json(body): Json<NewSubscriptionBody>,
) -> Result<(StatusCode, Json<CreatedView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let url = body.url.trim();
    check_url(url, state.require_https).map_err(Failure::Unprocessable)?;

    // Generated here, not accepted from the caller. A client-chosen secret is a client-chosen *weak* secret,
    // and there is no reason to allow one: the value is opaque to everybody except the two ends.
    let secret = dam_db::auth::ApiKey::generate().into_plaintext();

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let id = webhooks::subscribe(
        conn.executor(),
        &webhooks::NewSubscription {
            connector_id: None,
            url,
            secret: &secret,
            event_kinds: &body.event_kinds,
        },
    )
    .await?;
    let made = webhooks::subscriptions(conn.executor())
        .await?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or(Failure::Internal)?;
    conn.commit().await?;

    // Started only now, so a deployment where nobody has registered a webhook runs no dispatch at all — which
    // is most deployments. Enqueued after the commit deliberately: the job is in the *global* schema, so it
    // cannot share this transaction, and a job that ran before the subscription committed would find nothing.
    dam_pipeline::worker::enqueue_webhook_dispatch(&state.global, caller.tenant_id)
        .await
        .map_err(|_| Failure::Internal)?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedView {
            subscription: SubscriptionView::from(made),
            secret,
            signature_note: format!(
                "Each delivery carries {sig} as \"{version}=<hex>\", the HMAC-SHA256 of \
                 \"<{stamp}>.<body>\" under this secret. Reject a {stamp} older than a few minutes: the \
                 timestamp is what stops a captured delivery being replayed.",
                sig = dam_connect::webhooks::SIGNATURE_HEADER,
                version = dam_connect::webhooks::SIGNATURE_VERSION,
                stamp = dam_connect::webhooks::TIMESTAMP_HEADER,
            ),
        }),
    ))
}

#[utoipa::path(
    delete,
    path = "/webhooks/{id}",
    responses(
        (status = 204, description = "Removed, with its queued deliveries"),
        (status = 404, description = "No such subscription"),
    ),
    tag = "webhooks",
)]
pub async fn remove(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let removed = webhooks::unsubscribe(conn.executor(), id).await?;
    conn.commit().await?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Failure::NotFound)
    }
}

/// Re-enables a subscription the system disabled.
#[utoipa::path(
    post,
    path = "/webhooks/{id}/enable",
    responses(
        (status = 200, description = "Enabled, with its failure count forgiven", body = SubscriptionView),
        (status = 404, description = "No such subscription"),
    ),
    tag = "webhooks",
)]
pub async fn enable(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<SubscriptionView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if !webhooks::reactivate(conn.executor(), id).await? {
        return Err(Failure::NotFound);
    }
    let row = webhooks::subscriptions(conn.executor())
        .await?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or(Failure::NotFound)?;
    conn.commit().await?;

    // The chain may have stopped while this was disabled, so restart it. Deduped, so a second enable is not a
    // second chain.
    dam_pipeline::worker::enqueue_webhook_dispatch(&state.global, caller.tenant_id)
        .await
        .map_err(|_| Failure::Internal)?;
    Ok(Json(SubscriptionView::from(row)))
}

/// One delivery attempt, as an operator sees it. No payload — see [`deliveries`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeliveryView {
    pub id: Uuid,
    pub event_kind: String,
    pub asset_id: Option<Uuid>,
    /// `pending`, `delivering`, `delivered`, `failed` or `dead`.
    pub state: String,
    pub attempts: i32,
    /// The HTTP status, when there was one. Absent for a timeout or a connection failure, which is a different
    /// diagnosis and must not read as a zero.
    pub response_status: Option<i32>,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the next attempt is due. In the past for one that is waiting to be picked up.
    pub next_attempt_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
pub struct LogParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

const fn default_limit() -> i64 {
    LOG_PAGE
}

/// The recent deliveries for one subscription, newest first.
///
/// **No payloads.** It is the largest column, on the query a screen runs most often, and returning them would
/// make this the cheapest way to read a tenant's whole change history in one request.
#[utoipa::path(
    get,
    path = "/webhooks/{id}/deliveries",
    params(("id" = Uuid, Path, description = "The subscription"), LogParams),
    responses((status = 200, description = "Recent deliveries, newest first", body = [DeliveryView])),
    tag = "webhooks",
)]
pub async fn deliveries(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(params): Query<LogParams>,
) -> Result<Json<Vec<DeliveryView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = webhooks::log(conn.executor(), id, params.limit).await?;
    conn.commit().await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| DeliveryView {
                id: row.id,
                event_kind: row.event_kind,
                asset_id: row.asset_id,
                state: row.state,
                attempts: row.attempts,
                response_status: row.response_status,
                last_error: row.last_error,
                created_at: row.created_at,
                delivered_at: row.delivered_at,
                next_attempt_at: row.next_attempt_at,
            })
            .collect(),
    ))
}

/// Re-queues an abandoned delivery.
///
/// Only a `dead` one, so this cannot be used to jump the queue for something in flight — which would break the
/// per-asset ordering the outbox exists to keep.
#[utoipa::path(
    post,
    path = "/webhooks/{id}/deliveries/{delivery_id}/retry",
    responses(
        (status = 202, description = "Queued for another round of attempts"),
        (status = 404, description = "No such delivery, or it was not abandoned"),
    ),
    tag = "webhooks",
)]
pub async fn retry(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Path((id, delivery_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // Checked against the subscription in the path, or the id there would be decoration and a guessed
    // delivery id would confirm it exists.
    let belongs = webhooks::log(conn.executor(), id, 500)
        .await?
        .into_iter()
        .any(|row| row.id == delivery_id);
    if !belongs {
        return Err(Failure::NotFound);
    }
    if !webhooks::revive(conn.executor(), delivery_id).await? {
        return Err(Failure::NotFound);
    }
    conn.commit().await?;

    dam_pipeline::worker::enqueue_webhook_dispatch(&state.global, caller.tenant_id)
        .await
        .map_err(|_| Failure::Internal)?;
    Ok(StatusCode::ACCEPTED)
}

/// Refuses a URL this server should not be posting to.
///
/// A subscription is a server-side request to an address the tenant chose, which is the definition of SSRF.
/// Without this a tenant could point a webhook at the cloud metadata service and have damrs fetch instance
/// credentials on their behalf — and the delivery log would show them the response body.
///
/// Host-based rather than resolution-based, and that limit is worth stating: a hostname that *resolves* to a
/// private address still passes here. Closing that properly means resolving at send time and checking the
/// address actually connected to, which belongs in the sender rather than in a validation function that runs
/// once. This blocks the literal forms, which is what a mistake looks like; a determined operator with DNS
/// they control is a different threat and needs the egress rules a deployment already has to have.
fn check_url(url: &str, require_https: bool) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("{url:?} is not a URL: {error}"))?;

    match parsed.scheme() {
        "https" => {}
        "http" if !require_https => {}
        "http" => {
            return Err(
                "a webhook URL must be https; a delivery carries a signature and a payload \
                        over the open internet"
                    .to_owned(),
            );
        }
        other => return Err(format!("{other:?} is not a scheme this server posts to")),
    }

    // Credentials in the URL would be written to the subscriptions table in the clear and echoed in every log
    // line that names the endpoint. A receiver that needs authentication has the signature.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "a webhook URL must not carry credentials; the signature is how a delivery is authenticated"
                .to_owned(),
        );
    }

    let Some(host) = parsed.host() else {
        return Err("a webhook URL needs a host".to_owned());
    };
    if is_internal(&host) {
        return Err(format!(
            "{host} is a loopback, private or link-local address, and this server will not post to one"
        ));
    }
    Ok(())
}

/// Whether a host is one no tenant should be able to make this server talk to.
fn is_internal(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(address) => {
            // 169.254/16 is the one that matters most: it carries the cloud metadata service.
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_broadcast()
        }
        url::Host::Ipv6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                // Unique-local (fc00::/7) and link-local (fe80::/10), which the standard library has no
                // stable predicate for on this edition.
                || (address.segments()[0] & 0xfe00) == 0xfc00
                || (address.segments()[0] & 0xffc0) == 0xfe80
        }
        // `localhost` and anything ending in it, which resolves to loopback everywhere. Other names are not
        // resolved here — see the note on this function's limits.
        url::Host::Domain(name) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
    }
}
