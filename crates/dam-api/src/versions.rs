//! Versions over HTTP (Q.8).
//!
//! ## Adding a version is joining an upload, not a second upload path
//!
//! `POST /assets/{id}/versions` takes the id of an asset the caller has *already uploaded* through the ordinary
//! route. So a version gets the same sniffing, the same probe, the same profile defaults and the same derivatives
//! as anything else — because it went through the same ingest. A dedicated multipart endpoint here would be a
//! second ingest path, and two ingest paths diverge.
//!
//! ## Manage, not Read
//!
//! Reading a history is Read: it is part of understanding an asset. Superseding one is Manage — it changes which
//! bytes everybody gets from that point on, which is a content decision rather than a view preference.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::versions::{self, Version, VersionRefusal};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the version endpoints need.
pub struct VersionState {
    pub global: PgPool,
}

impl std::fmt::Debug for VersionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionState").finish_non_exhaustive()
    }
}

/// The version routes.
pub fn router(state: VersionState) -> Router {
    Router::new()
        .route("/assets/{asset_id}/versions", get(history).post(add))
        .route("/assets/{asset_id}/versions/current", post(make_current))
        .with_state(Arc::new(state))
}

/// One version in a history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct VersionView {
    pub asset_id: Uuid,
    pub version_no: i32,
    /// Whether this is the version listings and downloads resolve to.
    pub is_current: bool,
    pub filename: String,
    pub bytes: i64,
    pub content_hash: String,
    pub replaces_id: Option<Uuid>,
    pub uploaded_by: Option<crate::comments::PersonView>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Which already-uploaded asset becomes the new version.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddVersionRequest {
    /// An asset this caller uploaded through the ordinary route. See the module docs on why it is not a file.
    pub new_asset_id: Uuid,
}

/// Every version of an asset, newest first.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/versions",
    responses(
        (status = 200, body = Vec<VersionView>),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "versions",
)]
pub async fn history(
    State(state): State<Arc<VersionState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Vec<VersionView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = versions::history(conn.executor(), asset_id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    present(&state, found).await
}

/// Supersedes an asset with one already uploaded.
#[utoipa::path(
    post,
    path = "/assets/{asset_id}/versions",
    request_body = AddVersionRequest,
    responses(
        (status = 200, body = Vec<VersionView>),
        (status = 404, description = "Either asset is unknown, or not one this caller may manage"),
        (status = 409, description = "That asset is not the current version; reload and retry"),
    ),
    tag = "versions",
)]
pub async fn add(
    State(state): State<Arc<VersionState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(request): Json<AddVersionRequest>,
) -> Result<Json<Vec<VersionView>>, Failure> {
    // Manage: this changes which bytes everybody gets from now on.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    versions::add(
        conn.executor(),
        asset_id,
        request.new_asset_id,
        &caller.predicate,
    )
    .await
    .map_err(Refused)?;

    // The event, in the same transaction. Recorded against the *new* asset, because that is the row that now
    // represents the group — and with the number, so a feed line can say which version it became.
    if let Some(actor) = caller.identity_id {
        dam_db::events::record(
            conn.executor(),
            dam_db::events::NewEvent::by(dam_db::events::Kind::Edited, request.new_asset_id, actor)
                .with(serde_json::json!({ "versioned": true, "replaces": asset_id })),
        )
        .await?;
    }

    let found = versions::history(conn.executor(), request.new_asset_id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    present(&state, found).await
}

/// Makes an earlier version current again.
#[utoipa::path(
    post,
    path = "/assets/{asset_id}/versions/current",
    responses(
        (status = 200, body = Vec<VersionView>),
        (status = 404, description = "No such asset, or not one this caller may manage"),
    ),
    tag = "versions",
)]
pub async fn make_current(
    State(state): State<Arc<VersionState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Vec<VersionView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = versions::restore(conn.executor(), asset_id, &caller.predicate)
        .await
        .map_err(Refused)?;
    if let Some(actor) = caller.identity_id {
        dam_db::events::record(
            conn.executor(),
            dam_db::events::NewEvent::by(dam_db::events::Kind::Edited, asset_id, actor)
                .with(serde_json::json!({ "made_current": true })),
        )
        .await?;
    }
    conn.commit().await?;
    present(&state, found).await
}

/// Resolves uploader names in one lookup and renders.
async fn present(
    state: &Arc<VersionState>,
    found: Vec<Version>,
) -> Result<Json<Vec<VersionView>>, Failure> {
    let ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = found
            .iter()
            .filter_map(|version| version.uploaded_by)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;

    Ok(Json(
        found
            .into_iter()
            .map(|version| VersionView {
                asset_id: version.asset_id,
                version_no: version.version_no,
                is_current: version.is_current,
                filename: version.filename,
                bytes: version.bytes,
                content_hash: version.content_hash,
                replaces_id: version.replaces_id,
                // `None` when nobody is recorded — an import, or somebody since deleted. A history line reads
                // without a name; inventing a placeholder would be claiming something.
                uploaded_by: version.uploaded_by.and_then(|id| {
                    people.iter().find(|person| person.id == id).map(|person| {
                        crate::comments::PersonView {
                            id: person.id,
                            name: person.display_name.clone(),
                            email: person.email.clone(),
                        }
                    })
                }),
                created_at: version.created_at,
            })
            .collect(),
    ))
}

/// Maps a [`VersionRefusal`] onto a status.
struct Refused(VersionRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            VersionRefusal::UnknownAsset(_) => Self::NotFound,
            // 409, not 422: the request is well formed and the *world* has moved on. A client's correct response is
            // to reload and retry, which is what a conflict means and what a 422 would not say.
            VersionRefusal::NotCurrent(_) => Self::Conflict(refusal.to_string()),
            VersionRefusal::Database(error) => error.into(),
        }
    }
}
