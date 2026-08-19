//! Upload profiles over HTTP (Q.3b).
//!
//! ## Listing is Read, editing is Manage
//!
//! Reading is deliberately open to any reader, because the *uploader* has to render the profile picker and
//! honour the required-field rule before it can upload anything. A client that could not list profiles could
//! not obey them, and the rule would exist only in the database. Editing is Manage: a profile decides what is
//! true of everything arriving from a source, which is a content decision rather than a view preference.
//!
//! ## An invalid default is 422 with the field named
//!
//! The whole point of validating defaults at save time is that the person who typed the value is still looking
//! at it. So the refusal names the field, which is what lets a form put the error where the value was entered —
//! a profile accepted here and failing at upload time would break every intake from that source, and the person
//! who could fix it would never see why.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::upload_profiles::{self, NewProfile, ProfileRefusal, UploadProfile};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the upload-profile endpoints need.
pub struct UploadProfileState {
    pub global: PgPool,
}

impl std::fmt::Debug for UploadProfileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadProfileState").finish_non_exhaustive()
    }
}

/// The upload-profile routes.
pub fn router(state: UploadProfileState) -> Router {
    Router::new()
        .route("/upload-profiles", get(list).post(create))
        .route(
            "/upload-profiles/{id}",
            axum::routing::patch(amend).delete(remove),
        )
        .with_state(Arc::new(state))
}

/// A profile as a client sees it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProfileRow {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    /// The form uploads under this profile get. `None` lets the file's media class decide.
    pub metadata_type_id: Option<Uuid>,
    /// Metadata applied to everything arriving under this profile, filling only what the upload omits.
    pub defaults: serde_json::Value,
    /// Whether the uploader should insist on required fields before proceeding.
    ///
    /// A rule for the client, not a server-side gate: by the time an upload finalises its bytes are staged, and
    /// refusing then would strand them over metadata a person could have supplied.
    pub require_complete: bool,
    pub ai_tags_enabled: bool,
    pub is_default: bool,
}

/// A profile to create.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProfileRequest {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub metadata_type_id: Option<Uuid>,
    #[serde(default)]
    pub defaults: Option<serde_json::Value>,
    #[serde(default)]
    pub require_complete: bool,
    /// Defaults to true: tagging on is the ordinary case, and a profile that silently disabled enrichment
    /// would be a surprising default to inherit.
    #[serde(default = "yes")]
    pub ai_tags_enabled: bool,
    #[serde(default)]
    pub is_default: bool,
}

fn yes() -> bool {
    true
}

/// What to change. An omitted member is left alone.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AmendProfileRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, with = "double_option")]
    pub metadata_type_id: Option<Option<Uuid>>,
    #[serde(default)]
    pub defaults: Option<serde_json::Value>,
    #[serde(default)]
    pub require_complete: Option<bool>,
    #[serde(default)]
    pub ai_tags_enabled: Option<bool>,
    #[serde(default)]
    pub is_default: Option<bool>,
}

/// Distinguishes "absent" from "present and null" in a JSON body.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}

/// Every profile, in display order.
#[utoipa::path(
    get,
    path = "/upload-profiles",
    responses((status = 200, body = Vec<ProfileRow>)),
    tag = "uploads",
)]
pub async fn list(
    State(state): State<Arc<UploadProfileState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProfileRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let profiles = upload_profiles::list_on(conn.executor())
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(profiles.into_iter().map(present).collect()))
}

/// Creates a profile.
#[utoipa::path(
    post,
    path = "/upload-profiles",
    request_body = CreateProfileRequest,
    responses(
        (status = 201, body = ProfileRow),
        (status = 409, description = "The key is already taken"),
        (status = 422, description = "The defaults do not validate; `reason` names the fields"),
    ),
    tag = "uploads",
)]
pub async fn create(
    State(state): State<Arc<UploadProfileState>>,
    headers: HeaderMap,
    Json(request): Json<CreateProfileRequest>,
) -> Result<(StatusCode, Json<ProfileRow>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let created = upload_profiles::create_on(
        conn.executor(),
        NewProfile {
            key: request.key,
            label: request.label,
            metadata_type_id: request.metadata_type_id,
            defaults: request.defaults.unwrap_or_else(|| serde_json::json!({})),
            require_complete: request.require_complete,
            ai_tags_enabled: request.ai_tags_enabled,
            is_default: request.is_default,
        },
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;
    Ok((StatusCode::CREATED, Json(present(created))))
}

/// Amends a profile.
#[utoipa::path(
    patch,
    path = "/upload-profiles/{id}",
    request_body = AmendProfileRequest,
    responses(
        (status = 200, body = ProfileRow),
        (status = 404, description = "No such profile"),
        (status = 422, description = "The defaults do not validate"),
    ),
    tag = "uploads",
)]
pub async fn amend(
    State(state): State<Arc<UploadProfileState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<AmendProfileRequest>,
) -> Result<Json<ProfileRow>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let amended = upload_profiles::amend_on(
        conn.executor(),
        id,
        upload_profiles::Amendment {
            label: request.label,
            metadata_type_id: request.metadata_type_id,
            defaults: request.defaults,
            require_complete: request.require_complete,
            ai_tags_enabled: request.ai_tags_enabled,
            is_default: request.is_default,
        },
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(present(amended)))
}

/// Removes a profile. Assets that arrived under it keep everything but the reference.
#[utoipa::path(
    delete,
    path = "/upload-profiles/{id}",
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "No such profile"),
    ),
    tag = "uploads",
)]
pub async fn remove(
    State(state): State<Arc<UploadProfileState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    upload_profiles::remove_on(conn.executor(), id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn present(profile: UploadProfile) -> ProfileRow {
    ProfileRow {
        id: profile.id,
        key: profile.key,
        label: profile.label,
        metadata_type_id: profile.metadata_type_id,
        defaults: profile.defaults,
        require_complete: profile.require_complete,
        ai_tags_enabled: profile.ai_tags_enabled,
        is_default: profile.is_default,
    }
}

/// Maps a [`ProfileRefusal`] onto a status.
struct Refused(ProfileRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            ProfileRefusal::UnknownProfile(_) => Self::NotFound,
            ProfileRefusal::DuplicateKey(_) => Self::Conflict(refusal.to_string()),
            // 422 with the fields named, so a form can put each error where the value was typed.
            ProfileRefusal::InvalidDefaults(_) => Self::Unprocessable(refusal.to_string()),
            ProfileRefusal::Database(error) => error.into(),
        }
    }
}
