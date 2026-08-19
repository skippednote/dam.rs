//! Taking a copy: the download the DAM's own users never had (Q.11c).
//!
//! Until now the only path that minted a delivery URL was the share portal. Somebody signed in could see an
//! asset, preview it, and read a panel saying the original was available — with nothing to press. This is the
//! press.
//!
//! ## It mints a URL rather than serving bytes
//!
//! `POST /assets/{id}/download` returns a signed `/d/{token}` URL, which is the one chokepoint every byte
//! passes through (3.1). Rights are evaluated there *and* here: at delivery because that is what makes a lapsed
//! licence stop an already-issued link, and here because a link that looks valid and fails when somebody clicks
//! it puts the error in front of the wrong person.
//!
//! ## A format that has not been rendered yet is 202, not 404
//!
//! A conversion's bytes are produced on demand. The honest answer for the first person to ask is "this is being
//! made", with the render queued — not a dead URL, and not a synchronous wait while a 40 MB TIFF is resampled
//! inside a request. The client asks again; the second call returns the URL.
//!
//! Deduplicated on `(asset, conversion)`, so twenty people choosing the same format is one render.
//!
//! ## Two gates, asset first
//!
//! Download for the asset, then the conversion's own permission. Naming a format the caller has no permission
//! for is a 403 that says which permission — deliberately unlike the asset rule, where hidden and absent
//! collapse. What is being withheld here is *tenant configuration*, which says nothing about anybody's library,
//! and a person who cannot use a format is better served by knowing what to ask for. See DECISIONS.md.

use crate::assets::Failure;
use crate::caller;
use crate::delivery::{self, DeliveryState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use chrono::Duration as ChronoDuration;
use dam_core::policy::Action;
use dam_core::rights_eval::Usage;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the download endpoint needs.
pub struct DownloadState {
    pub global: PgPool,
    /// The signer. `None` in tests that exercise the refusals rather than the minting, which is also why the
    /// field is optional on `AssetState` — see the note there.
    pub delivery: Option<Arc<DeliveryState>>,
}

impl std::fmt::Debug for DownloadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadState").finish_non_exhaustive()
    }
}

/// The download route.
pub fn router(state: DownloadState) -> Router {
    Router::new()
        .route("/assets/{asset_id}/download", post(download))
        .with_state(Arc::new(state))
}

/// How long a download URL lives.
///
/// Long enough to click, short enough that a URL pasted into a chat has stopped working by the time anybody
/// scrolls back to it. Clamped again by `delivery::MAX_TOKEN_TTL`.
const DOWNLOAD_TTL_MINUTES: i64 = 15;

/// What to download, and what for.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DownloadRequest {
    /// `original`, or a conversion's key. Defaults to the original, because that is what "download" means
    /// without further information.
    #[serde(default = "original")]
    pub format: String,
    /// Where the copy is going, which is what rights are evaluated against.
    ///
    /// Defaulted rather than required *for now*: every existing caller of the rights evaluator passes a usage,
    /// and asking the person is Q.12's intended-use capture. The default is the narrowest honest thing — a
    /// generic internal channel and worldwide territory — and it is a default a licence can refuse.
    #[serde(default = "internal_channel")]
    pub channel: String,
    #[serde(default = "worldwide")]
    pub territory: String,
}

fn original() -> String {
    dam_media::profiles::ORIGINAL.to_owned()
}
fn internal_channel() -> String {
    "internal".to_owned()
}
fn worldwide() -> String {
    "WORLD".to_owned()
}

/// A URL to fetch, or word that the format is being made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DownloadIssued {
    /// The signed URL. Absent while a conversion is still being rendered.
    pub url: Option<String>,
    /// `ready` or `rendering`. A client polls on `rendering`.
    pub status: String,
    /// What was asked for, echoed so a client holding several requests can tell them apart.
    pub format: String,
}

/// Mints a download URL for an asset, in the original or a named format.
#[utoipa::path(
    post,
    path = "/assets/{asset_id}/download",
    request_body = DownloadRequest,
    responses(
        (status = 200, body = DownloadIssued, description = "A signed URL"),
        (status = 202, body = DownloadIssued, description = "The format is being rendered; ask again"),
        (status = 403, description = "The rights refuse this usage, or the format needs a permission the \
                                      caller does not hold"),
        (status = 404, description = "No such asset, or not one this caller may download"),
    ),
    tag = "conversions",
)]
pub async fn download(
    State(state): State<Arc<DownloadState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(request): Json<DownloadRequest>,
) -> Result<(StatusCode, Json<DownloadIssued>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Download).await?;

    // The asset gate first, and by the same read the options endpoint uses: an asset outside the caller's scope
    // is absent before any question about formats is asked.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let asset = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id).await?;
    let Some(asset) = asset else {
        conn.commit().await?;
        return Err(Failure::NotFound);
    };

    // `original` is not a conversion, so it skips the whole table — see `profiles::ORIGINAL`.
    let transform = if request.format == dam_media::profiles::ORIGINAL {
        conn.commit().await?;
        request.format.clone()
    } else {
        let Some(conversion) =
            dam_db::conversions::by_key(conn.executor(), &request.format).await?
        else {
            conn.commit().await?;
            return Err(Failure::NotFound);
        };
        conn.commit().await?;

        if !conversion.permitted_for(&caller.permissions) {
            // Named rather than hidden. See the module docs on why this differs from the asset rule.
            return Err(Failure::Forbidden(match &conversion.required_permission {
                Some(permission) => format!(
                    "the {} format needs the {permission} permission",
                    conversion.key
                ),
                // Unreachable: `permitted_for` is true whenever no permission is named. Stated rather than
                // unwrapped, because a panic in a refusal path is the worst place to have one.
                None => format!("the {} format is not available to you", conversion.key),
            }));
        }

        // The class is checked here, not only when the format was offered: a caller can name any key, and an
        // image recipe over a PDF would queue a job that can only fail.
        let class = dam_db::conversions::class_of(&asset.summary.mime);
        if class != conversion.media_class {
            return Err(Failure::Unprocessable(format!(
                "the {} format applies to {} and this asset is {class}",
                conversion.key, conversion.media_class
            )));
        }

        let Some(op_hash) = conversion.op_hash() else {
            // A recipe this build cannot render. 422 rather than 500: the request named something real that
            // this binary does not understand, which is a fact about the deployment and not a crash.
            return Err(Failure::Unprocessable(format!(
                "the {} format names a rendition this build cannot produce",
                conversion.key
            )));
        };

        let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
        let rendered = dam_db::derivatives::by_op_hash(conn.executor(), asset_id, &op_hash).await?;
        conn.commit().await?;

        if rendered.is_none() {
            // Queued, and the caller is told so. A synchronous render inside the request would hold a
            // connection open for however long a large TIFF takes, and a 404 would say the format does not
            // exist when what is true is that its bytes do not exist yet.
            dam_pipeline::worker::enqueue_conversion(
                &state.global,
                caller.tenant_id,
                asset_id,
                &conversion.key,
            )
            .await
            .map_err(|error| {
                tracing::error!(%error, %asset_id, key = %conversion.key, "queueing a conversion render");
                Failure::Internal
            })?;

            return Ok((
                StatusCode::ACCEPTED,
                Json(DownloadIssued {
                    url: None,
                    status: "rendering".to_owned(),
                    format: conversion.key,
                }),
            ));
        }

        conversion.key
    };

    // Rights, and then the signature. Evaluated here as well as at delivery: see the module docs.
    let Some(delivery) = state.delivery.as_ref() else {
        tracing::error!("a download was asked for with no delivery state configured");
        return Err(Failure::Internal);
    };
    let now = delivery.now();
    // `issue` returns the token; the URL is built by the state that knows the public origin. Returning the bare
    // token would be handing a client an unfetchable string — the mistake `sign_preview` documents having made.
    let token = delivery::issue(
        delivery,
        asset_id,
        &transform,
        &Usage {
            channel: request.channel.clone(),
            territory: request.territory.clone(),
        },
        caller.identity_id,
        ChronoDuration::minutes(DOWNLOAD_TTL_MINUTES),
        now,
    )
    .await
    .map_err(Denied)?;

    Ok((
        StatusCode::OK,
        Json(DownloadIssued {
            url: Some(delivery.url_for(&token)),
            status: "ready".to_owned(),
            format: transform,
        }),
    ))
}

/// Maps a delivery refusal onto a status.
struct Denied(delivery::Refusal);

impl From<Denied> for Failure {
    fn from(Denied(refusal): Denied) -> Self {
        match refusal {
            // The rights verdict, with its codes, because the caller has established they may see the asset —
            // this is the one refusal that is more useful explained than collapsed.
            delivery::Refusal::RightsDenied { state, codes } => Self::Forbidden(format!(
                "rights refuse this download ({}){}",
                state.as_str(),
                if codes.is_empty() {
                    String::new()
                } else {
                    format!(": {}", codes.join(", "))
                }
            )),
            // A token that could not be minted for any other reason answers like an absent asset. The caller
            // learns nothing about whether the thing they named exists, which is the delivery module's rule and
            // stays its rule here.
            delivery::Refusal::NotDeliverable => Self::NotFound,
            delivery::Refusal::Internal => Self::Internal,
        }
    }
}
