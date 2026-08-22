//! Auto-import mappings over HTTP (Q.4).
//!
//! ## Everything here is Manage
//!
//! Unlike upload profiles, no client needs to *read* these to behave correctly — a mapping fires during ingest,
//! on the server, and an uploader that knew nothing about it would still be right. So there is no reason to widen
//! the read, and a mapping is a decision about what the tenant's fields mean.
//!
//! ## The source list is served, not documented
//!
//! `GET /auto-import-mappings/sources` returns the names the extractor can actually produce. A screen that asked
//! an administrator to type `exif.artist` from memory would produce rules that look correct in the table and
//! never fire, and a list in the documentation is a list that drifts. Serving it from the extractor's own tables
//! means the picker cannot offer a name nothing writes.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::auto_import::{self, Mapping, MappingRefusal, NewMapping};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the auto-import endpoints need.
pub struct AutoImportState {
    pub global: PgPool,
}

impl std::fmt::Debug for AutoImportState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoImportState").finish_non_exhaustive()
    }
}

/// The auto-import routes.
pub fn router(state: AutoImportState) -> Router {
    Router::new()
        .route("/auto-import-mappings", get(list).post(create))
        .route("/auto-import-mappings/sources", get(sources))
        .route(
            "/auto-import-mappings/{id}",
            axum::routing::patch(amend).delete(remove),
        )
        .with_state(Arc::new(state))
}

/// A mapping as a client sees it.
#[derive(Debug, Serialize, ToSchema)]
pub struct MappingRow {
    pub id: Uuid,
    /// The embedded name, as the extractor reports it — `exif.artist`, `xmp.creator`.
    pub source: String,
    pub field_key: String,
    /// Lower fires first when several sources feed one field.
    pub priority: i32,
    /// Whether this mapping may replace a value the asset already has.
    pub overwrite: bool,
    pub enabled: bool,
}

/// A mapping to create.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateMappingRequest {
    pub source: String,
    pub field_key: String,
    #[serde(default)]
    pub priority: i32,
    /// Defaults to false, which is the safe direction: see the note in `dam_db::auto_import`.
    #[serde(default)]
    pub overwrite: bool,
    /// Defaults to true: a mapping saved and then not applied would be a surprising thing to have to switch on.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// What to change. An omitted member is left alone.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AmendMappingRequest {
    #[serde(default)]
    pub overwrite: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Every mapping, best-first within each field.
#[utoipa::path(
    get,
    path = "/auto-import-mappings",
    responses((status = 200, body = Vec<MappingRow>)),
    tag = "schema",
)]
pub async fn list(
    State(state): State<Arc<AutoImportState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<MappingRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let mappings = auto_import::list_on(conn.executor())
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(mappings.into_iter().map(present).collect()))
}

/// The embedded names a mapping can be written against.
#[utoipa::path(
    get,
    path = "/auto-import-mappings/sources",
    responses((status = 200, body = Vec<String>)),
    tag = "schema",
)]
pub async fn sources(
    State(state): State<Arc<AutoImportState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, Failure> {
    caller::authorize(&state.global, &headers, Action::Manage).await?;
    // No tenant read at all: the list is a property of the extractor, not of this tenant's data.
    Ok(Json(
        dam_media::embedded::sources()
            .into_iter()
            .map(str::to_owned)
            .collect(),
    ))
}

/// Creates a mapping.
#[utoipa::path(
    post,
    path = "/auto-import-mappings",
    request_body = CreateMappingRequest,
    responses(
        (status = 201, body = MappingRow),
        (status = 404, description = "No field is defined with that key"),
        (status = 409, description = "That source already maps to that field"),
        (status = 422, description = "The source is malformed, or the field is read-only"),
    ),
    tag = "schema",
)]
pub async fn create(
    State(state): State<Arc<AutoImportState>>,
    headers: HeaderMap,
    Json(request): Json<CreateMappingRequest>,
) -> Result<(StatusCode, Json<MappingRow>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let created = auto_import::create_on(
        conn.executor(),
        NewMapping {
            source: request.source,
            field_key: request.field_key,
            priority: request.priority,
            overwrite: request.overwrite,
            enabled: request.enabled,
        },
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;
    Ok((StatusCode::CREATED, Json(present(created))))
}

/// Turns a mapping on or off, or changes whether it may overwrite.
///
/// Deliberately narrow: the source and the field are what a mapping *is*, and re-pointing one in place would make
/// an existing rule mean something else without any record that it changed. Delete and create instead.
#[utoipa::path(
    patch,
    path = "/auto-import-mappings/{id}",
    request_body = AmendMappingRequest,
    responses(
        (status = 200, body = MappingRow),
        (status = 404, description = "No such mapping"),
    ),
    tag = "schema",
)]
pub async fn amend(
    State(state): State<Arc<AutoImportState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<AmendMappingRequest>,
) -> Result<Json<MappingRow>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if let Some(enabled) = request.enabled {
        auto_import::set_enabled_on(conn.executor(), id, enabled)
            .await
            .map_err(Refused)?;
    }
    if let Some(overwrite) = request.overwrite {
        auto_import::set_overwrite_on(conn.executor(), id, overwrite)
            .await
            .map_err(Refused)?;
    }
    // Read back inside the same transaction, so the response is the row as stored rather than the request echoed
    // with the parts that were not sent guessed at.
    let mapping = auto_import::get_on(conn.executor(), id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(present(mapping)))
}

/// Removes a mapping. Values it already imported stay on their assets.
#[utoipa::path(
    delete,
    path = "/auto-import-mappings/{id}",
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "No such mapping"),
    ),
    tag = "schema",
)]
pub async fn remove(
    State(state): State<Arc<AutoImportState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    auto_import::remove_on(conn.executor(), id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn present(mapping: Mapping) -> MappingRow {
    MappingRow {
        id: mapping.id,
        source: mapping.source,
        field_key: mapping.field_key,
        priority: mapping.priority,
        overwrite: mapping.overwrite,
        enabled: mapping.enabled,
    }
}

/// Maps a [`MappingRefusal`] onto a status.
struct Refused(MappingRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            MappingRefusal::UnknownMapping(_) => Self::NotFound,
            // 404 rather than 422: the client named a resource that is not there, and a schema screen's field
            // picker is built from the same list, so this means the field was removed underneath it.
            MappingRefusal::UnknownField(_) => Self::NotFound,
            MappingRefusal::Duplicate { .. } => Self::Conflict(refusal.to_string()),
            // 422 with the reason: both of these are about the *content* of an otherwise well-formed request,
            // and the person who typed it is still looking at the form.
            MappingRefusal::MalformedSource(_) | MappingRefusal::ReadOnlyTarget(_) => {
                Self::Unprocessable(refusal.to_string())
            }
            MappingRefusal::Database(error) => error.into(),
        }
    }
}
