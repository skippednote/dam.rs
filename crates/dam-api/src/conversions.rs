//! Named download formats over HTTP (Q.11b).
//!
//! Two audiences, and they ask different questions.
//!
//! **Somebody downloading** asks "what can I have this as?" — `GET /assets/{id}/download-options`. The answer is
//! the untransformed original plus the conversions that apply to this asset's media class and that this caller's
//! permissions allow, each with the description whoever configured it wrote for exactly this moment.
//!
//! **An administrator** asks "what do we offer?" — `GET/POST/PATCH /conversions`. That list includes withdrawn
//! formats, because somebody has to be able to see what they withdrew in order to restore it.
//!
//! ## Two gates, and the asset's is first
//!
//! Every route here requires the caller to hold the action for the *asset* before any conversion is considered:
//! Download for the options, Manage for administration. A conversion's own permission can only narrow what that
//! allowed — it is never consulted to widen anything, and a caller who cannot download the asset never reaches
//! the question of which formats exist for it.
//!
//! ## A format the caller may not use is absent from the offer
//!
//! Not shown-and-refused: a list of things you cannot have is a worse answer than a shorter list. What happens
//! when somebody names such a format *directly* is a question for the download route, which does not exist yet —
//! the answer is recorded in DECISIONS.md and implemented alongside it, rather than as an untested branch here.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::conversions::{self, Conversion, ConversionRefusal, NewConversion};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the conversion endpoints need.
pub struct ConversionState {
    pub global: PgPool,
}

impl std::fmt::Debug for ConversionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversionState").finish_non_exhaustive()
    }
}

/// The conversion routes.
pub fn router(state: ConversionState) -> Router {
    Router::new()
        .route("/conversions", get(list).post(create))
        .route("/conversions/{id}", patch(redefine))
        .route("/conversions/{id}/active", patch(set_active))
        .route("/assets/{asset_id}/download-options", get(options))
        .with_state(Arc::new(state))
}

/// One named format, as a person choosing sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConversionView {
    pub id: Uuid,
    /// The name a download request carries. Stable across relabelling.
    pub key: String,
    pub label: String,
    /// Written for whoever is choosing. The reason this table exists rather than a list of sizes.
    pub description: String,
    pub media_class: String,
    pub max_width: i32,
    pub max_height: i32,
    pub format: String,
    pub quality: i32,
    pub fit: String,
    pub background: String,
    /// The permission a role must carry. Present for administration; the download options never list a format
    /// the caller cannot use, so it is always `null` there.
    pub required_permission: Option<String>,
    pub is_active: bool,
    pub sort_order: i32,
}

impl From<Conversion> for ConversionView {
    fn from(row: Conversion) -> Self {
        Self {
            id: row.id,
            key: row.key,
            label: row.label,
            description: row.description,
            media_class: row.media_class,
            max_width: row.max_width,
            max_height: row.max_height,
            format: row.format,
            quality: row.quality,
            fit: row.fit,
            background: row.background,
            required_permission: row.required_permission,
            is_active: row.is_active,
            sort_order: row.sort_order,
        }
    }
}

/// What one asset can be had as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DownloadOptions {
    /// Whether the untransformed bytes can be fetched now.
    ///
    /// False for archived bytes and for a restore in flight: offering them would be offering a download that
    /// begins a wait nobody was told about.
    ///
    /// It says nothing about the conversions below it. A conversion that has *already* been rendered is its own
    /// object and does not tier, so it stays available while the original does not — but one that has not been
    /// rendered is produced from the original (`derive::asset` reads the original bytes), so for an archived
    /// asset it needs the same restore. This response does not yet distinguish those two cases: the readiness
    /// of a particular format is the download route's answer, and that route does not exist yet. Listing a
    /// format here means the tenant offers it, not that its bytes exist this second.
    pub original_available: bool,
    /// The asset's media class, so a client can say "no formats are configured for video yet" rather than
    /// showing an empty list with no explanation.
    pub media_class: String,
    /// The formats this caller may ask for, in the order somebody configured. See `original_available` on what
    /// this list does and does not promise.
    pub conversions: Vec<ConversionView>,
}

/// A conversion to create or redefine.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ConversionRequest {
    /// Required on create, ignored on redefine — a delivery token carries the key, so renaming one would
    /// strand links that were valid when they were sent.
    pub key: Option<String>,
    pub label: String,
    pub description: String,
    #[serde(default = "image_class")]
    pub media_class: String,
    pub max_width: i32,
    pub max_height: i32,
    pub format: String,
    pub quality: i32,
    #[serde(default = "contain")]
    pub fit: String,
    #[serde(default = "white")]
    pub background: String,
    pub required_permission: Option<String>,
    #[serde(default)]
    pub sort_order: i32,
}

fn image_class() -> String {
    "image".to_owned()
}
fn contain() -> String {
    "contain".to_owned()
}
fn white() -> String {
    "ffffff".to_owned()
}

impl ConversionRequest {
    fn into_new(self, key: String) -> NewConversion {
        NewConversion {
            key,
            label: self.label,
            description: self.description,
            media_class: self.media_class,
            max_width: self.max_width,
            max_height: self.max_height,
            format: self.format,
            quality: self.quality,
            fit: self.fit,
            background: self.background,
            required_permission: self.required_permission,
            sort_order: self.sort_order,
        }
    }
}

/// Whether a conversion is withdrawn or in use.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct ActiveRequest {
    pub is_active: bool,
}

/// Every conversion, withdrawn ones included.
#[utoipa::path(
    get,
    path = "/conversions",
    responses(
        (status = 200, body = Vec<ConversionView>),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "conversions",
)]
pub async fn list(
    State(state): State<Arc<ConversionState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ConversionView>>, Failure> {
    // Manage, not Read. The set of formats is configuration, and the recipe behind one — quality, dimensions,
    // which permission it needs — is administrative detail rather than something a reader needs.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = conversions::all(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(rows.into_iter().map(ConversionView::from).collect()))
}

/// Adds a named format.
#[utoipa::path(
    post,
    path = "/conversions",
    request_body = ConversionRequest,
    responses(
        (status = 201, body = ConversionView),
        (status = 409, description = "That key is already taken"),
        (status = 422, description = "The recipe is one nothing can render; the body names what was refused"),
    ),
    tag = "conversions",
)]
pub async fn create(
    State(state): State<Arc<ConversionState>>,
    headers: HeaderMap,
    Json(request): Json<ConversionRequest>,
) -> Result<(StatusCode, Json<ConversionView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let Some(key) = request.key.clone() else {
        return Err(Failure::Unprocessable(
            "a conversion needs a key: it is the name a download request carries".to_owned(),
        ));
    };
    // A key that shadows a built-in profile is refused, because the shadow never wins. Delivery resolves a name
    // against the built-in set first (see `delivery::op_hash_for`), so a conversion called `web-2048` would be
    // queued and rendered under its own recipe's hash and then *served* under the built-in's — a format that
    // reports ready and hands back a URL nobody can fetch. Found by adding the mint-time rendition check in
    // Q.14: the download suite had exactly this collision in its fixture and could not see it, because it
    // asserted a URL came back rather than following one.
    if dam_media::profiles::by_name(key.trim()).is_some() {
        return Err(Failure::Unprocessable(format!(
            "`{}` is the name of a built-in rendition; pick another key",
            key.trim()
        )));
    }
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let created = conversions::create(
        conn.executor(),
        &request.into_new(key),
        Some(caller.identity_id),
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// Replaces a format's definition.
///
/// Every asset's rendition under this format re-renders on next request, because the cache key is the recipe.
/// That is the intended behaviour and the reason there is no revision to bump.
#[utoipa::path(
    patch,
    path = "/conversions/{id}",
    request_body = ConversionRequest,
    responses(
        (status = 200, body = ConversionView),
        (status = 404, description = "No such conversion"),
        (status = 422, description = "The recipe is one nothing can render"),
    ),
    tag = "conversions",
)]
pub async fn redefine(
    State(state): State<Arc<ConversionState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<ConversionRequest>,
) -> Result<Json<ConversionView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // The key in the body is ignored; `redefine` does not write it. Passed as a placeholder so one request
    // type serves both routes — the alternative is two nearly identical bodies that drift.
    let updated = conversions::redefine(conn.executor(), id, &request.into_new(String::new()))
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(updated.into()))
}

/// Withdraws a format, or puts a withdrawn one back.
#[utoipa::path(
    patch,
    path = "/conversions/{id}/active",
    request_body = ActiveRequest,
    responses(
        (status = 200, body = ConversionView),
        (status = 404, description = "No such conversion"),
    ),
    tag = "conversions",
)]
pub async fn set_active(
    State(state): State<Arc<ConversionState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<ActiveRequest>,
) -> Result<Json<ConversionView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let updated = conversions::set_active(conn.executor(), id, request.is_active)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(updated.into()))
}

/// What this asset can be had as, for this caller.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/download-options",
    responses(
        (status = 200, body = DownloadOptions),
        (status = 404, description = "No such asset, or not one this caller may download"),
    ),
    tag = "conversions",
)]
pub async fn options(
    State(state): State<Arc<ConversionState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<DownloadOptions>, Failure> {
    // **Download, not Read.** Asking what formats an asset can be had in is asking about taking a copy of it,
    // so a caller who may only look never sees the list. Checked before any conversion is considered — the
    // reverse order would answer "may I have this asset" through the shape of "which formats exist".
    let caller = caller::authorize(&state.global, &headers, Action::Download).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // `assets::detail`, not a query of my own. The tier is *derived* — from the warmest present placement, by
    // one function so a badge cannot change when a panel opens — and a second derivation here would be a
    // second answer to "can this be fetched now". It also applies the predicate inside the query, which is
    // what makes an asset outside the caller's scope absent rather than checked-and-refused.
    let asset = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id).await?;
    conn.commit().await?;
    // Absent and unreachable answer the same way, which is the asset rule and stays the asset rule: this half
    // is about an asset, and only the *conversion* half departs from it.
    let Some(asset) = asset else {
        return Err(Failure::NotFound);
    };

    let media_class = conversions::class_of(&asset.summary.mime);
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let offered = conversions::offerable(conn.executor(), media_class, &caller.permissions).await?;
    conn.commit().await?;

    Ok(Json(DownloadOptions {
        // Archived bytes need a restore first, and offering them anyway would be offering a download that
        // fails after the person has already chosen it. `Cool` is offered — it is instant and merely billed —
        // and so is `Restored`, where a temporary copy exists now. The two that are not are the two where
        // pressing the button would start a wait nobody was told about.
        original_available: !matches!(
            asset.summary.tier,
            dam_core::storage::AssetTier::Archive | dam_core::storage::AssetTier::Restoring
        ),
        media_class: media_class.to_owned(),
        conversions: offered
            .into_iter()
            .map(|row| {
                let mut view = ConversionView::from(row);
                // Never in this answer. Every format here is one the caller may use, so naming the permission
                // would be telling them about a gate they have already passed.
                view.required_permission = None;
                view
            })
            .collect(),
    }))
}

/// Maps a [`ConversionRefusal`] onto a status.
struct Refused(ConversionRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            ConversionRefusal::Unknown(_) => Self::NotFound,
            // 409: the request is well formed and the world already contains that name.
            ConversionRefusal::DuplicateKey(_) => Self::Conflict(refusal.to_string()),
            // 422, carrying the constraint's own name. The database is the specification for a usable recipe,
            // so the message says which rule refused rather than a generic "invalid".
            ConversionRefusal::Invalid(named) => {
                Self::Unprocessable(format!("that recipe is refused by {named}"))
            }
            ConversionRefusal::Database(error) => error.into(),
        }
    }
}
