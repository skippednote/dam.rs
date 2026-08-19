//! Share links: management for the tenant, and the portal an external recipient lands on.
//!
//! ## Two audiences, two auth models, one table
//!
//! The management routes (`POST /shares`, `GET /shares`, `DELETE /shares/{id}`) authenticate the tenant's
//! own people with the usual bearer key and the **Manage** action. The portal routes (`POST /share/{token}`,
//! `POST /share/{token}/download`) authenticate nobody: **the token is the credential** — 256 random bits,
//! stored as a digest, resolved in one indexed lookup — exactly like a signed URL is, and the recipient is
//! by definition someone with no account here.
//!
//! ## Sharing does not bypass rights, and that is the design rather than a limitation
//!
//! Every byte the portal hands out goes through `delivery::issue_for_share`, which evaluates rights at issue
//! and again at delivery, with the share's id inside the signature so revoking the share kills the URLs it
//! already minted. An *internal preview* is refused for share links outright (`signed_url::Purpose`'s third
//! restriction), so the portal's preview is a full **Distribution** — an unlicensed asset shows its name and
//! a refusal, never its pixels. A share is a door, not a skeleton key.
//!
//! ## The portal reads the delivery tenant's schema
//!
//! Same posture as delivery itself: a token arrives with no tenant attached, and `damd` refuses to start
//! when the delivery tenant is ambiguous. When 3.x moves the tenant into signed material, the share token
//! gains a tenant prefix and this note goes away.
//!
//! ## `requires_eula` fails closed
//!
//! The column exists; the acceptance machinery (the EULA text, the recorded acceptance) does not. A share
//! created with it set is refused by the portal with a message saying so — enforcing nothing while the flag
//! reads as protection would be worse than the missing feature.

use crate::caller;
use crate::delivery::{self, DeliveryState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dam_core::policy::Action;
use dam_core::rights_eval::Usage;
use dam_db::shares::{self, ShareRefusal};
use dam_db::{TenantConn, assets};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the share endpoints need.
pub struct ShareState {
    pub global: PgPool,
    /// The delivery state: the portal mints real delivery URLs, and they must be signed with the same
    /// keyring and clock the delivery route verifies with.
    pub delivery: Arc<DeliveryState>,
}

impl std::fmt::Debug for ShareState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShareState").finish_non_exhaustive()
    }
}

/// The share routes — management and portal together, so the router is the complete list of what a share
/// token can reach.
pub fn router(state: ShareState) -> Router {
    Router::new()
        .route("/shares", post(create).get(list))
        .route("/shares/{share_id}", delete(revoke))
        .route("/share/{token}", post(portal))
        .route("/share/{token}/download", post(download))
        .with_state(Arc::new(state))
}

/// How long a portal's preview URL lives. Short: the page re-mints on reload, and the token — not this URL —
/// is the thing the recipient keeps.
const PORTAL_PREVIEW_TTL: ChronoDuration = ChronoDuration::minutes(30);
/// How long a download URL lives: long enough to click, too short to republish.
const PORTAL_DOWNLOAD_TTL: ChronoDuration = ChronoDuration::minutes(5);

// ─── management ──────────────────────────────────────────────────────────────

/// What a client asks for when sharing an asset.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ShareRequest {
    pub asset_id: Uuid,
    /// Hours until the link stops working. Absent means no expiry — revocation is then the only way it ends.
    pub expires_in_hours: Option<i64>,
    pub max_downloads: Option<i32>,
    /// Plaintext; hashed with argon2id server-side and never stored or returned.
    pub passcode: Option<String>,
    /// Whether the recipient may fetch the original, rather than only the web rendition.
    #[serde(default)]
    pub allow_original: bool,
}

/// A created share. The token appears here and nowhere else, exactly like an issued API key.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreatedShare {
    pub id: Uuid,
    /// The one-time token. The table holds a digest, so a lost link cannot be recovered — revoke and
    /// re-create, which is the same posture as an API key and for the same reason.
    pub token: String,
    /// The path a recipient opens, for the client to compose onto its own origin.
    pub portal_path: String,
}

/// One row in the management list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShareRow {
    pub id: Uuid,
    pub filename: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub max_downloads: Option<i32>,
    pub download_count: i32,
    pub allow_original: bool,
    pub has_passcode: bool,
    pub revoked: bool,
    /// Whether the link still works, so the list does not make a reader re-derive it from four columns.
    pub live: bool,
}

/// Shares an asset.
#[utoipa::path(
    post,
    path = "/shares",
    request_body = ShareRequest,
    responses(
        (status = 201, description = "Created; the token appears here and nowhere else", body = CreatedShare),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "No such asset, or not one this caller may manage"),
    ),
    tag = "shares",
)]
pub async fn create(
    State(state): State<Arc<ShareState>>,
    headers: HeaderMap,
    Json(request): Json<ShareRequest>,
) -> Result<(StatusCode, Json<CreatedShare>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;

    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Sharing is distribution of an asset the caller manages, so the predicate applies exactly as it does to
    // a bulk write: an asset outside the caller's scope answers 404, indistinguishable from absent.
    let visible =
        assets::visible_among(conn.executor(), &caller.predicate, &[request.asset_id]).await?;
    if visible.is_empty() {
        return Err(Failure::NotFound);
    }

    let created = shares::create_on(
        conn.executor(),
        &shares::ShareSpec {
            kind: "asset",
            target_id: Some(request.asset_id),
            search_query: None,
            passcode: request.passcode.as_deref(),
            expires_at: request
                .expires_in_hours
                .map(|hours| Utc::now() + ChronoDuration::hours(hours.clamp(1, 24 * 365))),
            max_downloads: request.max_downloads,
            allow_original: request.allow_original,
            requires_eula: false,
            created_by: caller.identity_id,
        },
    )
    .await?;
    conn.commit().await?;

    let portal_path = format!("/share/{}", created.token());
    Ok((
        StatusCode::CREATED,
        Json(CreatedShare {
            id: created.id,
            token: created.token().to_owned(),
            portal_path,
        }),
    ))
}

/// The tenant's share links, newest first.
#[utoipa::path(
    get,
    path = "/shares",
    responses(
        (status = 200, body = Vec<ShareRow>),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "shares",
)]
pub async fn list(
    State(state): State<Arc<ShareState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ShareRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let now = state.delivery.now();

    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let listed = shares::list_on(conn.executor(), 200).await?;
    conn.commit().await?;

    Ok(Json(
        listed
            .into_iter()
            .map(|row| ShareRow {
                live: row.share.revoked_at.is_none()
                    && row.share.expires_at.is_none_or(|at| now < at)
                    && row
                        .share
                        .max_downloads
                        .is_none_or(|max| row.share.download_count < max),
                id: row.share.id,
                filename: row.filename,
                created_at: row.created_at,
                expires_at: row.share.expires_at,
                max_downloads: row.share.max_downloads,
                download_count: row.share.download_count,
                allow_original: row.share.allow_original,
                has_passcode: row.share.has_passcode,
                revoked: row.share.revoked_at.is_some(),
            })
            .collect(),
    ))
}

/// Revokes a share. Takes effect on URLs the share already minted, because the share id is in their
/// signatures and delivery re-checks it on every request.
#[utoipa::path(
    delete,
    path = "/shares/{share_id}",
    params(("share_id" = Uuid, Path, description = "The share")),
    responses(
        (status = 204, description = "Revoked, or already was — revocation is idempotent"),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "shares",
)]
pub async fn revoke(
    State(state): State<Arc<ShareState>>,
    headers: HeaderMap,
    Path(share_id): Path<Uuid>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Idempotent, and an unknown id gets the same 204 as an already-revoked one: "make this stop working"
    // has succeeded in every one of those states, and distinguishing them tells a prober which ids exist.
    shares::revoke_on(conn.executor(), share_id, state.delivery.now()).await?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── the portal ──────────────────────────────────────────────────────────────

/// What a recipient sends. POST rather than GET even for the first look, so the passcode travels in a body —
/// a passcode in a query string is a passcode in every proxy log on the path.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct PortalRequest {
    pub passcode: Option<String>,
}

/// What the portal shows.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PortalView {
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// A short-lived delivery URL for the web rendition — a full rights-checked distribution, `None` when
    /// rights refuse or no rendition exists yet. The share existing does not entitle anyone to pixels.
    pub preview_url: Option<String>,
    /// Why there is no preview, when there is a stateable reason.
    pub preview_unavailable: Option<String>,
    pub download_allowed: bool,
    pub downloads_remaining: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// What the portal needs to say about the file: filename, mime, bytes, width, height.
type AssetFacts = (String, String, i64, Option<i32>, Option<i32>);

/// Resolves a share for viewing. Does not consume a download.
#[utoipa::path(
    post,
    path = "/share/{token}",
    request_body = PortalRequest,
    responses(
        (status = 200, body = PortalView),
        (status = 401, description = "A passcode is required or wrong; the body's `reason` says which"),
        (status = 404, description = "No such share — or revoked, expired, exhausted; the body's `reason` says which"),
    ),
    tag = "shares",
)]
pub async fn portal(
    State(state): State<Arc<ShareState>>,
    Path(token): Path<String>,
    Json(request): Json<PortalRequest>,
) -> Result<Json<PortalView>, Failure> {
    let now = state.delivery.now();
    let share = resolve_for_portal(&state, &token, request.passcode.as_deref(), now).await?;
    let asset_id = share.target_id.ok_or_else(|| {
        // A collection or search share. The portal renders assets; the wider kinds need their own view.
        Failure::Portal(
            StatusCode::NOT_FOUND,
            "this link shares something this portal cannot show yet".to_owned(),
        )
    })?;

    let row: Option<AssetFacts> = sqlx::query_as(
        "SELECT filename, mime, bytes, width, height FROM assets \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(state.delivery.pool())
    .await
    .map_err(dam_db::Error::from)?;
    let Some((filename, mime, bytes, width, height)) = row else {
        // The shared asset was deleted after the share was made. The same flat answer as a dead token: the
        // recipient learns the link no longer works, not what used to be behind it.
        return Err(ShareRefusal::NotFound.into());
    };

    // A full distribution, with the share in the signature. Rights refusing is a normal outcome the portal
    // states, not an error: the share's creator may not have had the licence they thought.
    let (preview_url, preview_unavailable) = match delivery::issue_for_share(
        &state.delivery,
        asset_id,
        "web-2048",
        &portal_usage(),
        None,
        Some(share.id),
        PORTAL_PREVIEW_TTL,
        now,
    )
    .await
    {
        Ok(token) => (Some(state.delivery.url_for(&token)), None),
        Err(delivery::Refusal::RightsDenied { .. }) => (
            None,
            Some("this asset is not licensed for distribution".to_owned()),
        ),
        Err(delivery::Refusal::NotDeliverable) => {
            (None, Some("no preview has been rendered yet".to_owned()))
        }
        Err(_) => return Err(Failure::Internal),
    };

    Ok(Json(PortalView {
        filename,
        mime,
        bytes,
        width,
        height,
        preview_url,
        preview_unavailable,
        download_allowed: true,
        downloads_remaining: share
            .max_downloads
            .map(|max| (max - share.download_count).max(0)),
        expires_at: share.expires_at,
    }))
}

/// The download response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PortalDownload {
    /// A short-lived delivery URL. Fetch it promptly; the token, not this URL, is what the recipient keeps.
    pub url: String,
    pub downloads_remaining: Option<i32>,
}

/// Consumes one download and mints the URL for it.
#[utoipa::path(
    post,
    path = "/share/{token}/download",
    request_body = PortalRequest,
    responses(
        (status = 200, body = PortalDownload),
        (status = 401, description = "A passcode is required or wrong"),
        (status = 403, description = "Rights refuse distribution of this asset"),
        (status = 404, description = "No such share — or revoked, expired, exhausted"),
    ),
    tag = "shares",
)]
pub async fn download(
    State(state): State<Arc<ShareState>>,
    Path(token): Path<String>,
    Json(request): Json<PortalRequest>,
) -> Result<Json<PortalDownload>, Failure> {
    let now = state.delivery.now();
    let share = resolve_for_portal(&state, &token, request.passcode.as_deref(), now).await?;
    let asset_id = share.target_id.ok_or(Failure::Portal(
        StatusCode::NOT_FOUND,
        "this link shares something this portal cannot download yet".to_owned(),
    ))?;

    // Rights first, *then* the download is consumed: a refusal must not spend one of the recipient's three
    // downloads on bytes they never received.
    let transform = if share.allow_original {
        "original"
    } else {
        "web-2048"
    };
    let minted = delivery::issue_for_share(
        &state.delivery,
        asset_id,
        transform,
        &portal_usage(),
        None,
        Some(share.id),
        PORTAL_DOWNLOAD_TTL,
        now,
    )
    .await
    .map_err(|refusal| match refusal {
        delivery::Refusal::RightsDenied { .. } => Failure::Portal(
            StatusCode::FORBIDDEN,
            "this asset is not licensed for distribution".to_owned(),
        ),
        delivery::Refusal::NotDeliverable => Failure::Portal(
            StatusCode::NOT_FOUND,
            "nothing is available to download yet".to_owned(),
        ),
        _ => Failure::Internal,
    })?;

    let count = shares::consume_download(state.delivery.pool(), share.id, now).await?;

    Ok(Json(PortalDownload {
        url: state.delivery.url_for(&minted),
        downloads_remaining: share.max_downloads.map(|max| (max - count).max(0)),
    }))
}

/// Resolve + passcode + the EULA fail-closed gate, shared by both portal routes.
async fn resolve_for_portal(
    state: &ShareState,
    token: &str,
    passcode: Option<&str>,
    now: DateTime<Utc>,
) -> Result<shares::Share, Failure> {
    let share = shares::resolve(state.delivery.pool(), token, now).await?;
    shares::check_passcode(state.delivery.pool(), share.id, passcode).await?;

    if share.requires_eula {
        // Fail closed: the flag exists, the acceptance machinery does not, and enforcing nothing while the
        // flag reads as protection would be worse than the missing feature.
        return Err(Failure::Portal(
            StatusCode::NOT_FOUND,
            "this link requires a licence acceptance flow that is not available yet".to_owned(),
        ));
    }
    Ok(share)
}

/// The usage a portal delivery is evaluated under.
///
/// `web`/`WORLD` — the widest read, deliberately: the portal cannot know where its recipient sits, so the
/// licence must permit the unrestricted case. A geo-scoped share would need the recipient's territory, which
/// is a feature with its own design, not a default.
fn portal_usage() -> Usage {
    Usage {
        channel: "web".to_owned(),
        territory: "WORLD".to_owned(),
    }
}

/// Everything that can go wrong here.
#[derive(Debug)]
pub enum Failure {
    Refused(caller::Refusal),
    NotFound,
    /// A portal-facing refusal with a message a recipient can act on.
    Portal(StatusCode, String),
    Internal,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Refused(refusal) => refusal.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Portal(status, reason) => {
                (status, Json(serde_json::json!({ "reason": reason }))).into_response()
            }
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<ShareRefusal> for Failure {
    fn from(refusal: ShareRefusal) -> Self {
        // The refusal reason goes to the recipient, deliberately. A share token is 256 random bits — nobody
        // enumerates one — so the person holding it is the person it was sent to, and "expired" tells them to
        // ask for a new link while "not found" sends them to re-type a URL that was never wrong. Passcode
        // refusals are 401 so a portal can re-prompt; the rest are 404 so a dead link stays uninformative
        // about *what* it was.
        let status = match refusal {
            ShareRefusal::PasscodeRequired | ShareRefusal::PasscodeWrong => {
                StatusCode::UNAUTHORIZED
            }
            _ => StatusCode::NOT_FOUND,
        };
        Self::Portal(status, refusal.to_string())
    }
}

impl From<caller::Refusal> for Failure {
    fn from(refusal: caller::Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<dam_db::Error> for Failure {
    fn from(error: dam_db::Error) -> Self {
        tracing::error!(%error, "share endpoint database error");
        Self::Internal
    }
}
