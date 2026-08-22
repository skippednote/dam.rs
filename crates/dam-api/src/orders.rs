//! Orders over HTTP (Q.13b).
//!
//! ## Asking is Read; deciding is Manage
//!
//! The whole point of an order is that somebody who may *see* assets but not take them can ask. So placing one
//! requires Read and nothing more — requiring Download would restrict the feature to exactly the people who do
//! not need it. Deciding is Manage: an approval is an act of authority over what leaves the library.
//!
//! ## An order grants nothing
//!
//! Approving does not give the requester a download right. It records a decision and opens a pickup window;
//! fulfilment then creates a share link, which is the machinery that already answers who may take what. See the
//! migration and NEEDS-REVIEW.md on the delegating design that was not taken.
//!
//! ## Your own orders, and the ones waiting for you
//!
//! Two lists, because they are two questions. `GET /orders` is a person's own history, newest first. `GET
//! /orders/queue` is what needs deciding, oldest first — a queue is worked through, and the longest wait is the
//! next thing to do.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::orders::{self, NewOrder, Order, OrderRefusal};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the order endpoints need.
pub struct OrderState {
    pub global: PgPool,
    /// Where the portal is reached from, for building a pickup URL. `None` yields a root-relative one, which is
    /// what a single-origin deployment wants.
    pub public_url: Option<String>,
}

impl std::fmt::Debug for OrderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderState").finish_non_exhaustive()
    }
}

/// The order routes.
pub fn router(state: OrderState) -> Router {
    Router::new()
        .route("/orders", get(mine).post(place))
        .route("/orders/queue", get(queue))
        .route("/orders/{id}", get(one))
        .route("/orders/{id}/approve", post(approve))
        .route("/orders/{id}/reject", post(reject))
        .route("/orders/{id}/cancel", post(cancel))
        .route("/orders/{id}/fulfil", post(fulfil))
        .route("/orders/{id}/metadata.csv", get(metadata_csv))
        .with_state(Arc::new(state))
}

/// How long a pickup window lasts, in days.
///
/// Two weeks: long enough for somebody to come back from leave, short enough that a link in an old email has
/// stopped working. Not configurable yet — a tenant-set window is a settings screen, and inventing one here would
/// be a column nothing writes.
const PICKUP_DAYS: i64 = 14;

/// One asset in an order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrderItemView {
    pub asset_id: Uuid,
    /// The name as asked for, so an order reads sensibly after a rename or a deletion.
    pub filename: String,
}

/// One order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrderView {
    pub id: Uuid,
    /// Human-quotable, per tenant: `ORD-000123`.
    pub reference: String,
    pub requested_by: Option<crate::comments::PersonView>,
    pub purpose: String,
    pub channel: Option<String>,
    pub territory: Option<String>,
    pub conversion_key: Option<String>,
    pub include_metadata: bool,
    pub recipients: Vec<String>,
    /// `submitted`, `approved`, `rejected`, `ready`, `collected`, `cancelled`.
    pub state: String,
    /// Whether the pickup window has closed. Derived from `expires_at` rather than stored — a stored `expired`
    /// would need a sweeper to stay true and would be wrong between sweeps.
    pub expired: bool,
    pub decided_by: Option<crate::comments::PersonView>,
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
    pub decision_note: Option<String>,
    /// Whether the requester decided their own order. Reported rather than prevented — see the db module.
    pub self_approved: bool,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub items: Vec<OrderItemView>,
    /// The pickup URL, **only** on the response that just minted it.
    ///
    /// A share token is stored as a digest, so it cannot be shown again — the same property that makes a leaked
    /// database not a leaked set of links. Anybody who needs the link again re-issues the pickup, which revokes
    /// the previous one; see [`fulfil`]. Absent on every ordinary read, which is why it is optional rather than a
    /// separate response type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pickup_url: Option<String>,
}

/// An order to place.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PlaceOrderRequest {
    /// The assets asked for. Narrowed to what this caller may see; see the db module on why silently.
    pub asset_ids: Vec<Uuid>,
    /// Why. The entire question an approver answers, so it is required.
    pub purpose: String,
    /// The intended use (Q.12), carried into the ledger when the pickup is collected.
    pub channel: Option<String>,
    pub territory: Option<String>,
    /// Which named format (Q.11). Absent means the original.
    pub conversion_key: Option<String>,
    #[serde(default)]
    pub include_metadata: bool,
    /// Who the delivery is for. Plural because an order is usually for a team.
    #[serde(default)]
    pub recipients: Vec<String>,
}

/// A decision.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DecisionRequest {
    /// Why it was refused, or a condition on an approval.
    pub note: Option<String>,
}

/// Places an order.
#[utoipa::path(
    post,
    path = "/orders",
    request_body = PlaceOrderRequest,
    responses(
        (status = 201, body = OrderView),
        (status = 422, description = "Nothing was asked for, or nothing asked for exists for this caller"),
    ),
    tag = "orders",
)]
pub async fn place(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
    Json(request): Json<PlaceOrderRequest>,
) -> Result<(StatusCode, Json<OrderView>), Failure> {
    // Read, deliberately. Somebody who may already download does not need an order; requiring Download here
    // would restrict the feature to exactly the people it is not for.
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let requester = caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))?;
    if request.purpose.trim().is_empty() {
        return Err(Failure::Unprocessable(
            "an order needs a reason: it is the question the approver answers".to_owned(),
        ));
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let placed = orders::place(
        conn.executor(),
        &NewOrder {
            requested_by: requester,
            purpose: request.purpose,
            channel: request.channel,
            territory: request.territory,
            conversion_key: request.conversion_key,
            include_metadata: request.include_metadata,
            recipients: request.recipients,
            asset_ids: request.asset_ids,
        },
        &caller.predicate,
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;

    let view = present(&state, vec![placed]).await?;
    Ok((
        StatusCode::CREATED,
        Json(view.into_iter().next().ok_or(Failure::Internal)?),
    ))
}

/// The caller's own orders, newest first.
#[utoipa::path(
    get,
    path = "/orders",
    responses((status = 200, body = Vec<OrderView>)),
    tag = "orders",
)]
pub async fn mine(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<OrderView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let requester = caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = orders::placed_by(conn.executor(), requester, 100).await?;
    conn.commit().await?;
    Ok(Json(present(&state, rows).await?))
}

/// Orders waiting for a decision, oldest first.
#[utoipa::path(
    get,
    path = "/orders/queue",
    responses(
        (status = 200, body = Vec<OrderView>),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "orders",
)]
pub async fn queue(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<OrderView>>, Failure> {
    // Manage: the queue is a list of decisions to make, and seeing what colleagues have asked for is part of the
    // authority to decide rather than something every reader needs.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = orders::awaiting_decision(conn.executor(), 100).await?;
    conn.commit().await?;
    Ok(Json(present(&state, rows).await?))
}

/// One order.
///
/// Readable by its requester and by anybody who may decide. Not by everybody: an order names what somebody
/// wanted and why, which is theirs and their approver's business.
#[utoipa::path(
    get,
    path = "/orders/{id}",
    responses(
        (status = 200, body = OrderView),
        (status = 404, description = "No such order, or not one this caller may read"),
    ),
    tag = "orders",
)]
pub async fn one(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = orders::read(conn.executor(), id).await?;
    conn.commit().await?;
    let Some(order) = found else {
        return Err(Failure::NotFound);
    };

    // Theirs, or somebody who may decide. A second `authorize` for the Manage case rather than a role check
    // here: whether a caller holds Manage is the caller module's question, and answering it twice differently is
    // how the two drift.
    let is_requester = caller.identity_id == Some(order.requested_by);
    if !is_requester
        && caller::authorize(&state.global, &headers, Action::Manage)
            .await
            .is_err()
    {
        // The same 404 an unknown order gets. "Not yours" would confirm the reference exists, and an order
        // reference is guessable — they are sequential.
        return Err(Failure::NotFound);
    }

    let view = present(&state, vec![order]).await?;
    view.into_iter().next().map(Json).ok_or(Failure::Internal)
}

/// Approves an order and opens its pickup window.
#[utoipa::path(
    post,
    path = "/orders/{id}/approve",
    request_body = DecisionRequest,
    responses(
        (status = 200, body = OrderView),
        (status = 403, description = "Some of the assets are outside the approver's scope"),
        (status = 404, description = "No such order"),
        (status = 409, description = "It has already been decided"),
    ),
    tag = "orders",
)]
pub async fn approve(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<DecisionRequest>,
) -> Result<Json<OrderView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let approver = caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let decided = orders::approve(
        conn.executor(),
        id,
        approver,
        request.note.as_deref(),
        &caller.predicate,
        PICKUP_DAYS,
        chrono::Utc::now(),
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;

    // The decision is committed. The pickup is a second write, deliberately: if it fails the order stays
    // `approved` and can be fulfilled again rather than the approver being asked to decide twice. A failure here
    // is therefore *not* an error for this request — the decision stands, and the state says what is missing.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let (ready, token) =
        match make_pickup(conn.executor(), id, caller.identity_id, chrono::Utc::now()).await {
            Ok((ready, token)) => {
                conn.commit().await?;
                (ready, Some(token))
            }
            Err(error) => {
                tracing::error!(%error, order = %id, "approved, but the pickup could not be made");
                (decided, None)
            }
        };
    let view = present(&state, vec![ready]).await?;
    let mut view = view.into_iter().next().ok_or(Failure::Internal)?;
    // Shown once, to the person who just approved it. The token is stored as a digest, so this response is the
    // only place it exists in readable form — which is why the approver is told to pass it on, and why
    // re-issuing exists for when they lose it.
    view.pickup_url = token.map(|token| pickup_url(&state, &token));
    Ok(Json(view))
}

/// Creates an approved order's pickup, moving it to `ready`.
///
/// Called automatically the moment an approval commits, and exposed so it can be *retried*: the decision and the
/// pickup are two writes, and an approver should not have to decide twice because the second one failed. That is
/// what keeps `approved` a meaningful state rather than a transient one.
///
/// The pickup is a share link — see the migration and NEEDS-REVIEW.md on why that rather than a new kind of
/// grant. It inherits the order's expiry, so the window an approver granted is the window the link has.
#[utoipa::path(
    post,
    path = "/orders/{id}/fulfil",
    responses(
        (status = 200, body = OrderView),
        (status = 404, description = "No such order"),
        (status = 409, description = "Not an approved order awaiting a pickup"),
    ),
    tag = "orders",
)]
pub async fn fulfil(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let (ready, token) = make_pickup(conn.executor(), id, caller.identity_id, chrono::Utc::now())
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    let view = present(&state, vec![ready]).await?;
    let mut view = view.into_iter().next().ok_or(Failure::Internal)?;
    view.pickup_url = Some(pickup_url(&state, &token));
    Ok(Json(view))
}

/// Where a recipient collects, from a token.
///
/// Built from the configured public URL when there is one, root-relative otherwise — the same rule
/// `DeliveryState::url_for` follows, and for the same reason: a hand-written path somewhere else is one rename
/// away from a dead link. The portal path is `/share/{token}`, and an order pickup is read at `/share/{token}/set`.
fn pickup_url(state: &Arc<OrderState>, token: &str) -> String {
    match &state.public_url {
        Some(base) => format!("{base}/share/{token}"),
        None => format!("/share/{token}"),
    }
}

/// Creates the share and marks the order ready, on a connection the caller has already scoped.
///
/// Shared by [`approve`] and [`fulfil`] so the two cannot make different pickups — the failure that would look
/// like "the retry produced a link with a different expiry".
async fn make_pickup(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    by: Option<Uuid>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Order, String), OrderRefusal> {
    let order = orders::read(&mut *conn, id)
        .await?
        .ok_or(OrderRefusal::Unknown(id))?;
    let items = order.items.len();

    // Re-issuing revokes the previous link, so an order never has two live pickups. That matters more than it
    // sounds: the point of a pickup is that revoking it stops the URLs it has already minted, and two live shares
    // would mean revoking one and believing the order was closed.
    if let Some(previous) = order.share_link_id {
        dam_db::shares::revoke_on(&mut *conn, previous, now).await?;
    }
    let share = dam_db::shares::create_on(
        &mut *conn,
        &dam_db::shares::ShareSpec {
            kind: "order",
            target_id: Some(order.id),
            search_query: None,
            // No passcode. An order was already addressed to named recipients by somebody who approved it, and a
            // passcode nobody was told is a link that does not work. A tenant that wants one is asking for a
            // setting, which is a decision rather than a default.
            passcode: None,
            expires_at: order.expires_at,
            // Two per item plus a handful: enough to fetch what was sent and retry a failure, not enough for the
            // link to become a general-purpose tap. Saturating, because an order of two billion assets is not a
            // thing but an overflow would be.
            max_downloads: Some(
                i32::try_from(items)
                    .unwrap_or(i32::MAX / 4)
                    .saturating_mul(2)
                    .saturating_add(5),
            ),
            // The original only if the order did not name a format: an approver who agreed to a 2048px JPEG did
            // not agree to the master.
            allow_original: order.conversion_key.is_none(),
            requires_eula: false,
            created_by: by,
        },
    )
    .await?;
    // Two functions, two transitions: `mark_ready` moves an `approved` order to `ready`, and `replace_pickup`
    // swaps the link on one that is already there. Each refuses every other state itself, so there is no third
    // arm here — an earlier version had one, and mutation testing showed the two guards masking each other: with
    // the arm present, nothing else ever reached `replace_pickup`, so removing *its* check changed nothing
    // observable. One mechanism, and a refusal that comes from the layer that owns the invariant.
    let ready = if order.state == "approved" {
        orders::mark_ready(&mut *conn, id, share.id).await?
    } else {
        orders::replace_pickup(&mut *conn, id, share.id).await?
    };
    Ok((ready, share.token().to_owned()))
}

/// Refuses an order.
#[utoipa::path(
    post,
    path = "/orders/{id}/reject",
    request_body = DecisionRequest,
    responses(
        (status = 200, body = OrderView),
        (status = 404, description = "No such order"),
        (status = 409, description = "It has already been decided"),
    ),
    tag = "orders",
)]
pub async fn reject(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<DecisionRequest>,
) -> Result<Json<OrderView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let approver = caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let decided = orders::reject(
        conn.executor(),
        id,
        approver,
        request.note.as_deref(),
        chrono::Utc::now(),
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;
    let view = present(&state, vec![decided]).await?;
    view.into_iter().next().map(Json).ok_or(Failure::Internal)
}

/// Withdraws an order, which only its requester may do, and only before a decision.
#[utoipa::path(
    post,
    path = "/orders/{id}/cancel",
    responses(
        (status = 200, body = OrderView),
        (status = 403, description = "Somebody else's order"),
        (status = 404, description = "No such order"),
        (status = 409, description = "It has already been decided"),
    ),
    tag = "orders",
)]
pub async fn cancel(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<OrderView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let requester = caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let cancelled = orders::cancel(conn.executor(), id, requester)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    let view = present(&state, vec![cancelled]).await?;
    view.into_iter().next().map(Json).ok_or(Failure::Internal)
}

/// The order's assets as a CSV, for the requester or an approver.
///
/// **Authenticated only, and deliberately not part of the pickup.** An export of descriptive metadata to an
/// external recipient is a disclosure decision nobody has made: `field_defs` has no notion of which fields an
/// outsider may see, so the honest options were to invent one or to keep the export inside the tenant. This is the
/// second. Somebody signed in exporting metadata they can already read is not a disclosure at all; the portal case
/// is written up in NEEDS-REVIEW.md.
///
/// Read scope, and the same audience as the order itself: its requester, or anybody who may decide.
#[utoipa::path(
    get,
    path = "/orders/{id}/metadata.csv",
    responses(
        (status = 200, description = "text/csv", content_type = "text/csv"),
        (status = 404, description = "No such order, or not one this caller may read"),
    ),
    tag = "orders",
)]
pub async fn metadata_csv(
    State(state): State<Arc<OrderState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = orders::read(conn.executor(), id).await?;

    let Some(order) = found else {
        conn.commit().await?;
        return Err(Failure::NotFound);
    };
    // The same audience rule as reading the order, and the same 404 rather than a 403: references are sequential.
    if caller.identity_id != Some(order.requested_by)
        && caller::authorize(&state.global, &headers, Action::Manage)
            .await
            .is_err()
    {
        conn.commit().await?;
        return Err(Failure::NotFound);
    }

    // The field definitions give the columns, so an export has the tenant's own vocabulary rather than whatever
    // keys happen to appear in the first row's JSON. Shared with the search export (Q.18): one CSV vocabulary,
    // because two would drift and the person who notices is the one whose re-import fails.
    let fields = dam_db::fields::load(conn.executor()).await?;
    let ids: Vec<Uuid> = order.items.iter().map(|item| item.asset_id).collect();
    let rows: Vec<crate::csv_export::Row> = sqlx::query_as(crate::csv_export::SELECT)
        .bind(&ids)
        .fetch_all(conn.executor())
        .await
        .map_err(dam_db::Error::from)?;
    // Read under the *caller's* predicate: an approver exporting an order must not receive metadata for an
    // asset they cannot see, even though the order names it.
    let visible = dam_db::assets::visible_among(conn.executor(), &caller.predicate, &ids).await?;
    conn.commit().await?;

    // The order's own sequence, narrowed to what this caller may see.
    let order_of_rows: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| visible.contains(id))
        .collect();
    let document = crate::csv_export::document(&fields, &rows, &order_of_rows);

    let filename = format!("{}-metadata.csv", order.reference);
    Ok((crate::csv_export::headers(&filename), document).into_response())
}

/// Resolves the people named on a set of orders in one lookup, and renders.
async fn present(
    state: &Arc<OrderState>,
    rows: Vec<Order>,
) -> Result<Vec<OrderView>, dam_db::Error> {
    let ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = rows
            .iter()
            .flat_map(|order| [Some(order.requested_by), order.decided_by])
            .flatten()
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;
    let named = |id: Option<Uuid>| {
        id.and_then(|id| {
            people
                .iter()
                .find(|person| person.id == id)
                .map(|person| crate::comments::PersonView {
                    id: person.id,
                    name: person.display_name.clone(),
                    email: person.email.clone(),
                })
        })
    };

    let now = chrono::Utc::now();
    Ok(rows
        .into_iter()
        .map(|order| OrderView {
            id: order.id,
            reference: order.reference.clone(),
            requested_by: named(Some(order.requested_by)),
            purpose: order.purpose.clone(),
            channel: order.channel.clone(),
            territory: order.territory.clone(),
            conversion_key: order.conversion_key.clone(),
            include_metadata: order.include_metadata,
            recipients: order.recipients.clone(),
            expired: order.is_expired(now),
            self_approved: order.self_approved(),
            decided_by: named(order.decided_by),
            decided_at: order.decided_at,
            decision_note: order.decision_note.clone(),
            expires_at: order.expires_at,
            created_at: order.created_at,
            items: order
                .items
                .iter()
                .map(|item| OrderItemView {
                    asset_id: item.asset_id,
                    filename: item.filename.clone(),
                })
                .collect(),
            state: order.state,
            // Never on a plain read: only the response that minted the token carries it.
            pickup_url: None,
        })
        .collect())
}

/// Maps an [`OrderRefusal`] onto a status.
struct Refused(OrderRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            OrderRefusal::Unknown(_) => Self::NotFound,
            // 409: the request is well formed and the world has moved on. Two approvers opening the same queue
            // is the commonest way here, and the message says what the order *is* so the second one is not left
            // refreshing a screen.
            OrderRefusal::WrongState(..) => Self::Conflict(refusal.to_string()),
            // 422: nothing was asked for, or nothing asked for exists here.
            OrderRefusal::Empty | OrderRefusal::NothingVisible => {
                Self::Unprocessable(refusal.to_string())
            }
            // 403 with the count: an approver being told they cannot judge an order needs to know that it is
            // about their scope rather than about the order being gone.
            OrderRefusal::Unjudgeable(_) | OrderRefusal::NotYours => {
                Self::Forbidden(refusal.to_string())
            }
            OrderRefusal::Database(error) => error.into(),
        }
    }
}
