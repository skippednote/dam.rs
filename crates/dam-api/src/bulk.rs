//! The bulk-operation endpoints (`POST /bulk/preview`, `POST /bulk`, `GET /bulk/{id}`).
//!
//! ## The target list is filtered through the caller's own predicate, in every request
//!
//! A bulk request arrives as a list of ids assembled by a client, and a client is not trusted about which
//! assets those are — a caller scoped to one group could otherwise bulk-delete another group's assets by
//! guessing ids. So the ids go through `assets::visible_among` under the caller's **Manage** predicate, and
//! anything that falls out is simply not part of the operation. Not an error: a stale grid legitimately
//! holds an id whose asset was re-scoped a moment ago, and refusing the whole request over it would make
//! bulk work flaky exactly when the library is busy.
//!
//! The consequence worth stating: the caller learns nothing about *why* an id fell out — not visible and
//! not existing look identical, which is §7's posture applied to writes.
//!
//! ## Preview is the same computation as creation
//!
//! `POST /bulk/preview` filters the ids exactly as `POST /bulk` will, so the number in the confirmation
//! dialog is the number that will be touched. Two implementations of "which of these may I act on" would
//! drift, and the drift is a dialog that says 40 and an operation that does 38.

use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::{TenantConn, assets, bulk};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the bulk endpoints need.
pub struct BulkState {
    pub global: PgPool,
}

impl std::fmt::Debug for BulkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkState").finish_non_exhaustive()
    }
}

/// The bulk routes.
pub fn router(state: BulkState) -> Router {
    Router::new()
        .route("/bulk/preview", post(preview))
        .route("/bulk", post(create))
        .route("/bulk/{operation_id}", get(status))
        .with_state(Arc::new(state))
}

/// The kinds a client may start.
///
/// Narrower than the schema's vocabulary, matching what `dam_pipeline::bulk_exec` can actually apply. The
/// executor refuses unknown kinds too, but a 422 here is a message in the requester's face; a dead job is a
/// message in a queue nobody watches.
const EXECUTABLE: &[&str] = &["metadata_set", "delete"];

/// What a client asks for.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BulkRequest {
    /// `metadata_set` or `delete`.
    pub kind: String,
    /// The selection. Deduplicated server-side; ids the caller may not manage fall out silently.
    pub asset_ids: Vec<Uuid>,
    /// Kind-specific parameters — for `metadata_set`, `{ "values": { … } }` with the same patch semantics
    /// as the single-asset endpoint: `null` clears, absent leaves alone.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// What a preview reports.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BulkPreview {
    pub kind: String,
    /// How many assets the operation would touch — after the caller's own scope is applied, so it is the
    /// number that will actually be touched.
    pub target_count: i64,
    /// A sample of the targets, for a dialog to name a few rather than list thousands.
    pub sample: Vec<Uuid>,
    /// How many of the submitted ids fell out of scope. Reported as a count and nothing more: which ones,
    /// and whether they exist at all, is exactly what §7 says a caller must not learn.
    pub out_of_scope: i64,
}

/// One failed row, for the report.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BulkFailure {
    pub asset_id: Uuid,
    pub reason: Option<String>,
}

/// An operation as the UI polls it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BulkStatus {
    pub id: Uuid,
    pub kind: String,
    /// `queued`, `running`, `completed`, `partial`, `failed`, `cancelled`.
    pub state: String,
    pub target_count: i64,
    pub done_count: i64,
    pub failed_count: i64,
    /// Whether the state is final, so a client polls this instead of re-implementing the state vocabulary.
    pub terminal: bool,
    /// The first failures, row by row — "must report exactly which rows did not apply".
    pub failures: Vec<BulkFailure>,
}

/// Previews a bulk operation without recording anything.
#[utoipa::path(
    post,
    path = "/bulk/preview",
    request_body = BulkRequest,
    responses(
        (status = 200, description = "What the operation would touch, under the caller's own scope", body = BulkPreview),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 422, description = "The kind is not executable"),
    ),
    tag = "bulk",
)]
pub async fn preview(
    State(state): State<Arc<BulkState>>,
    headers: HeaderMap,
    Json(request): Json<BulkRequest>,
) -> Result<Json<BulkPreview>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    require_executable(&request.kind)?;

    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let visible =
        assets::visible_among(conn.executor(), &caller.predicate, &request.asset_ids).await?;
    conn.commit().await?;

    let submitted = deduplicated(&request.asset_ids);
    let dry = bulk::dry_run(&request.kind, &visible);
    Ok(Json(BulkPreview {
        kind: dry.kind,
        target_count: dry.target_count,
        sample: dry.sample,
        out_of_scope: submitted - dry.target_count,
    }))
}

/// Creates a bulk operation and queues its execution.
#[utoipa::path(
    post,
    path = "/bulk",
    request_body = BulkRequest,
    responses(
        (status = 202, description = "Accepted; poll the returned operation", body = BulkStatus),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 422, description = "The kind is not executable, or nothing in the selection is manageable"),
    ),
    tag = "bulk",
)]
pub async fn create(
    State(state): State<Arc<BulkState>>,
    headers: HeaderMap,
    Json(request): Json<BulkRequest>,
) -> Result<(StatusCode, Json<BulkStatus>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    require_executable(&request.kind)?;

    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let visible =
        assets::visible_among(conn.executor(), &caller.predicate, &request.asset_ids).await?;
    if visible.is_empty() {
        // 422 with a reason rather than an instantly-completed no-op: the caller's history is where somebody
        // looks to find what they actually ran, and a "completed" operation over nothing hides the mistake.
        return Err(Failure::Unprocessable(
            "nothing in the selection is yours to manage".to_owned(),
        ));
    }

    let operation = bulk::create_on(
        conn.executor(),
        &bulk::OperationSpec {
            kind: &request.kind,
            actor_id: caller.identity_id,
            predicate: None,
            params: request.params,
        },
        &visible,
    )
    .await?;
    conn.commit().await?;

    dam_pipeline::worker::enqueue_bulk(&state.global, caller.tenant_id, operation.id)
        .await
        .map_err(|error| {
            // The row exists and nothing will run it. Loud, because the alternative is a progress bar that
            // never moves and a user who blames themselves.
            tracing::error!(%error, operation = %operation.id, "created a bulk operation but could not queue it");
            Failure::Internal
        })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(BulkStatus {
            id: operation.id,
            kind: operation.kind,
            state: operation.state,
            target_count: operation.target_count,
            done_count: operation.done_count,
            failed_count: operation.failed_count,
            terminal: false,
            failures: Vec::new(),
        }),
    ))
}

/// The progress of one operation.
#[utoipa::path(
    get,
    path = "/bulk/{operation_id}",
    params(("operation_id" = Uuid, Path, description = "The operation")),
    responses(
        (status = 200, body = BulkStatus),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "No such operation"),
    ),
    tag = "bulk",
)]
pub async fn status(
    State(state): State<Arc<BulkState>>,
    headers: HeaderMap,
    Path(operation_id): Path<Uuid>,
) -> Result<Json<BulkStatus>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;

    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let operation = bulk::load_on(conn.executor(), operation_id).await?;
    let Some(operation) = operation else {
        return Err(Failure::NotFound);
    };
    let failures = bulk::failures_on(conn.executor(), operation_id, 50).await?;
    conn.commit().await?;

    Ok(Json(BulkStatus {
        terminal: operation.is_terminal(),
        id: operation.id,
        kind: operation.kind,
        state: operation.state,
        target_count: operation.target_count,
        done_count: operation.done_count,
        failed_count: operation.failed_count,
        failures: failures
            .into_iter()
            .map(|item| BulkFailure {
                asset_id: item.asset_id,
                reason: item.reason,
            })
            .collect(),
    }))
}

fn require_executable(kind: &str) -> Result<(), Failure> {
    if EXECUTABLE.contains(&kind) {
        Ok(())
    } else {
        Err(Failure::Unprocessable(format!(
            "bulk operations of kind {kind:?} are not executable yet; use one of {EXECUTABLE:?}"
        )))
    }
}

fn deduplicated(ids: &[Uuid]) -> i64 {
    let mut unique: Vec<Uuid> = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    i64::try_from(unique.len()).unwrap_or(i64::MAX)
}

/// Everything that can go wrong here.
#[derive(Debug)]
pub enum Failure {
    Refused(caller::Refusal),
    NotFound,
    Unprocessable(String),
    Internal,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Refused(refusal) => refusal.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unprocessable(message) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "message": message })),
            )
                .into_response(),
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<caller::Refusal> for Failure {
    fn from(refusal: caller::Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<dam_db::Error> for Failure {
    fn from(error: dam_db::Error) -> Self {
        tracing::error!(%error, "bulk endpoint database error");
        Self::Internal
    }
}
