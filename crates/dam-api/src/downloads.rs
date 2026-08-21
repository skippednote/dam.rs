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
//! ## The intended use is captured, and a default is not a declaration (Q.12)
//!
//! `channel` and `territory` decide the rights answer, so a download is also the moment to ask what the copy is
//! for. Both fields are optional on the wire and the *presence* of them is what marks the record as declared —
//! not a flag the client sets, which a client could assert without anybody having answered.
//!
//! Every download is written to `rights_usage` **before** the URL is minted. That ledger is what
//! `license_scopes.max_downloads` is summed against, so an unrecorded download makes a cap under-count and
//! permits more than the licence allows; a recorded download that then failed to mint over-counts and permits
//! fewer. The first is a licence breach and the second is an inconvenience.
//!
//! ## Two gates, asset first
//!
//! Download for the asset, then the conversion's own permission. Naming a format the caller has no permission
//! for is a 403 that says which permission — deliberately unlike the asset rule, where hidden and absent
//! collapse. What is being withheld here is *tenant configuration*, which says nothing about anybody's library,
//! and a person who cannot use a format is better served by knowing what to ask for. See DECISIONS.md.

use crate::assets::Failure;
use crate::caller;
use crate::caller::Caller;
use crate::delivery::{self, DeliveryState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
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

/// The download routes.
pub fn router(state: DownloadState) -> Router {
    router_from(Arc::new(state))
}

/// The same routes, over a state somebody else is also holding.
///
/// The MCP server calls `issue` with this state, and one state is the point: two would be two pools and,
/// eventually, two answers about the same download.
pub fn router_from(state: Arc<DownloadState>) -> Router {
    Router::new()
        .route("/assets/{asset_id}/download", post(download))
        .route("/assets/{asset_id}/usage", get(ledger))
        .route("/usage-options", get(options))
        .with_state(state)
}

/// What a person may declare a download as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UsageOptions {
    /// Channels any of this tenant's licences reference, inclusions and exclusions alike.
    pub channels: Vec<String>,
    pub territories: Vec<String>,
    /// What a download is recorded as when nobody declares anything, so a client can show it as the default
    /// rather than inventing its own.
    pub default_channel: String,
    pub default_territory: String,
}

/// One line of the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UsageRecord {
    pub id: Uuid,
    pub channel: Option<String>,
    pub territory: Option<String>,
    /// Whether somebody stated this use, or the API defaulted it. The difference is the whole point of asking.
    pub declared: bool,
    /// Who took it. Absent for a download by a machine credential, which has no person behind it.
    pub person: Option<crate::comments::PersonView>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

/// The channels and territories a download may be declared as.
///
/// Derived from the tenant's licences rather than configured, so every option is one that can change a rights
/// answer. Read, not Download: a person filling in a form has not asked for bytes yet.
#[utoipa::path(
    get,
    path = "/usage-options",
    responses((status = 200, body = UsageOptions)),
    tag = "conversions",
)]
pub async fn options(
    State(state): State<Arc<DownloadState>>,
    headers: HeaderMap,
) -> Result<Json<UsageOptions>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let (channels, territories) = dam_db::usage::vocabulary(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(UsageOptions {
        channels,
        territories,
        default_channel: INTERNAL_CHANNEL.to_owned(),
        default_territory: WORLDWIDE.to_owned(),
    }))
}

/// What one asset has been taken for, newest first.
///
/// Read: how an asset has been used is part of its rights position, and somebody deciding whether they may use
/// it benefits from seeing that it went out under a print licence last month. It names colleagues, which the
/// comment threads and the activity feed already do within a tenant.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/usage",
    responses(
        (status = 200, body = Vec<UsageRecord>),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "conversions",
)]
pub async fn ledger(
    State(state): State<Arc<DownloadState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Vec<UsageRecord>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Visibility first, so an asset the caller cannot see is absent rather than shown as having no history.
    let visible =
        dam_db::assets::visible_among(conn.executor(), &caller.predicate, &[asset_id]).await?;
    if visible.is_empty() {
        conn.commit().await?;
        return Err(Failure::NotFound);
    }
    let rows = dam_db::usage::for_asset(conn.executor(), asset_id, &caller.predicate, 100).await?;
    conn.commit().await?;

    // Names in one lookup, as the dashboard and the history panel do.
    let ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = rows.iter().filter_map(|row| row.recorded_by).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;

    Ok(Json(
        rows.into_iter()
            .map(|row| UsageRecord {
                id: row.id,
                channel: row.channel,
                territory: row.territory,
                declared: row.declared,
                person: row.recorded_by.and_then(|id| {
                    people.iter().find(|person| person.id == id).map(|person| {
                        crate::comments::PersonView {
                            id: person.id,
                            name: person.display_name.clone(),
                            email: person.email.clone(),
                        }
                    })
                }),
                recorded_at: row.recorded_at,
            })
            .collect(),
    ))
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
    /// Optional, and its *presence* is the declaration: a request that names a channel is one where somebody
    /// answered, and the ledger row says so. Absent, the API supplies the narrowest honest default — a generic
    /// internal channel — and records that nobody was asked. See migration 0024 on why the difference is worth
    /// a column.
    ///
    /// Not required outright, because a machine integration genuinely has one fixed usage and making it restate
    /// it on every call would be ceremony. What must not happen is a default that *looks* like an answer.
    pub channel: Option<String>,
    pub territory: Option<String>,
}

impl DownloadRequest {
    /// The usage to evaluate, and whether anybody actually said so.
    ///
    /// Both fields together: half a declaration is not one. Somebody who named a channel and left the territory
    /// to a default has still had the question put to them, but the record would claim more than was asked, so
    /// this counts as declared only when the request carried both.
    fn usage(&self) -> (Usage, bool) {
        let declared = self.channel.is_some() && self.territory.is_some();
        (
            Usage {
                channel: self
                    .channel
                    .clone()
                    .unwrap_or_else(|| INTERNAL_CHANNEL.to_owned()),
                territory: self
                    .territory
                    .clone()
                    .unwrap_or_else(|| WORLDWIDE.to_owned()),
            },
            declared,
        )
    }
}

/// What an undeclared download is recorded as.
///
/// `internal` rather than something broader: a licence that restricts a channel should refuse an undeclared
/// download rather than wave it through under a permissive guess.
const INTERNAL_CHANNEL: &str = "internal";
const WORLDWIDE: &str = "WORLD";

fn original() -> String {
    dam_media::profiles::ORIGINAL.to_owned()
}

/// A URL to fetch, or word that the format is being made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DownloadIssued {
    /// The signed URL. Absent while a conversion is still being rendered.
    pub url: Option<String>,
    /// `ready`, `rendering`, or `archived`. A client polls on `rendering` and asks for a restore on
    /// `archived`.
    pub status: String,
    /// Where to ask for a restore, on `archived`. Absent otherwise.
    ///
    /// Present because a URL that will 202 is worse than no URL: the client has something that looks
    /// fetchable, hands it to a browser, and the wait shows up as a mystery in somebody else's response. The
    /// mint knows the bytes are cold, so it says so here rather than letting the delivery route say it later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_url: Option<String>,
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
    let (status, issued) = issue(&state, &caller, asset_id, &request).await?;
    Ok((status, Json(issued)))
}

/// Issues a download, for a caller who has already been authorised.
///
/// Split out from the route so the MCP server can reach it (§8.5: "over the **same ABAC layer**"). That is
/// not a convenience — it is the whole property. A second implementation of this path would be a second
/// place where rights are evaluated, the ledger is written and the token is minted, and the three would
/// drift in exactly the way a governed library cannot afford. The caller is passed in rather than derived,
/// so every entry point has to have authorised one first.
pub async fn issue(
    state: &DownloadState,
    caller: &Caller,
    asset_id: Uuid,
    request: &DownloadRequest,
) -> Result<(StatusCode, DownloadIssued), Failure> {
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
                DownloadIssued {
                    url: None,
                    status: "rendering".to_owned(),
                    restore_url: None,
                    format: conversion.key,
                },
            ));
        }

        conversion.key
    };

    let Some(delivery) = state.delivery.as_ref() else {
        tracing::error!("a download was asked for with no delivery state configured");
        return Err(Failure::Internal);
    };
    let now = delivery.now();
    let (usage, declared) = request.usage();

    // Rights evaluated here in full, rather than left to `issue`. Two reasons, and the first is the one that
    // matters: the evaluation is where the *consuming scope* comes from, and a ledger row with no scope counts
    // toward no cap. The second is that a refusal caught here never reaches the ledger, so a denied attempt is
    // not recorded as a download.
    //
    // `evaluate` rather than `effective`: the cached form returns a verdict and nothing else, and re-deriving
    // which licence permitted this from the verdict would be a second answer to a question the evaluator
    // already answers.
    let evaluation = dam_db::rights::evaluate(delivery.pool(), asset_id, &usage, now).await?;
    if !evaluation.permits_distribution() {
        return Err(Failure::Forbidden(format!(
            "rights refuse this download ({}){}",
            evaluation.verdict.as_str(),
            reasons_of(&evaluation)
        )));
    }

    // Cold bytes are reported here rather than left for the delivery route to refuse.
    //
    // The delivery route *does* refuse them, with a 202 and an ETA — but by then the client has been handed a
    // URL, called it fetchable, and quite possibly given it to a browser. The wait then surfaces as a mystery
    // in a response nothing on this side wrote. The mint already knows: `download-options` reports
    // `original_available` from exactly this fact, and this endpoint was the one place that knew and said
    // "ready" anyway.
    //
    // After rights, deliberately. Whether the caller may have the bytes at all is a stronger answer than
    // whether the bytes are warm, and telling somebody how to thaw an asset they are not licensed for would
    // be inviting them to spend money on a download that will still be refused.
    //
    // Not recorded against the licence either, because nothing was distributed — which is why this sits above
    // the ledger write below.
    if transform == original()
        && let Some(archived) =
            archived_original(&state.global, &caller.tenant_slug, asset_id).await?
    {
        return Ok((
            StatusCode::ACCEPTED,
            DownloadIssued {
                url: None,
                status: "archived".to_owned(),
                restore_url: Some(format!("/assets/{asset_id}/restore")),
                format: archived,
            },
        ));
    }

    // Recorded before the URL exists. An unrecorded download makes `max_downloads` under-count and permits more
    // than the licence allows; a recorded one that then fails to mint over-counts and permits fewer. A licence
    // breach is worse than an inconvenience, so the order is this way round — and a write failure refuses the
    // download rather than handing out an unaudited copy.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    dam_db::usage::record_download(
        conn.executor(),
        &dam_db::usage::NewDownload {
            asset_id,
            channel: usage.channel.clone(),
            territory: usage.territory.clone(),
            license_scope_id: evaluation.consuming_scope,
            declared,
            recorded_by: caller.identity_id,
        },
    )
    .await?;
    conn.commit().await?;

    // `issue` returns the token; the URL is built by the state that knows the public origin. Returning the bare
    // token would be handing a client an unfetchable string — the mistake `sign_preview` documents having made.
    //
    // It re-checks rights, which is not redundant with the check above: this is the one chokepoint every byte
    // passes through, and a caller reaching it by another route must be refused there too.
    let token = delivery::issue(
        delivery,
        asset_id,
        &transform,
        &usage,
        caller.identity_id,
        ChronoDuration::minutes(DOWNLOAD_TTL_MINUTES),
        now,
    )
    .await
    .map_err(Denied)?;

    Ok((
        StatusCode::OK,
        DownloadIssued {
            url: Some(delivery.url_for(&token)),
            status: "ready".to_owned(),
            restore_url: None,
            format: transform,
        },
    ))
}

/// The storage class of an asset's coldest original, when it needs a restore.
///
/// `None` for the common case, at the cost of one indexed read. Only the original is asked about: a rendered
/// conversion is its own object and does not tier, so a named format stays deliverable while the original it
/// came from is cold.
async fn archived_original(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    asset_id: Uuid,
) -> Result<Option<String>, Failure> {
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let class: Option<String> = sqlx::query_scalar(
        "SELECT storage_class FROM object_placements \
         WHERE asset_id = $1 AND derivative_id IS NULL AND state = 'present' \
           AND NOT (restore_state = 'available' AND restore_expires_at > now()) \
         ORDER BY CASE storage_class \
                      WHEN 'DEEP_ARCHIVE' THEN 0 WHEN 'GLACIER' THEN 1 ELSE 2 \
                  END, object_key \
         LIMIT 1",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    Ok(class.filter(|raw| {
        raw.parse::<dam_core::StorageClass>()
            .is_ok_and(dam_core::StorageClass::requires_restore)
    }))
}

/// The reason codes of a refusal, as a sentence fragment.
///
/// Shared between the two refusal paths — the explicit evaluation here and delivery's own — so a client sees the
/// same shape whichever refused. Empty when there is nothing to say, rather than a dangling colon.
fn reasons_of(evaluation: &dam_core::rights_eval::Evaluation) -> String {
    let codes: Vec<&str> = evaluation
        .reasons
        .iter()
        .map(|reason| reason.code)
        .collect();
    if codes.is_empty() {
        String::new()
    } else {
        format!(": {}", codes.join(", "))
    }
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
            // Archived bytes, on a route that has no `202` to give: this one answers with a URL or an error,
            // so the wait becomes a sentence naming what to do about it. `Unprocessable` rather than
            // `Forbidden` because nothing is being withheld — the request cannot be *completed* yet, and the
            // caller has an action available that fixes it.
            delivery::Refusal::Restoring(body) => Self::Unprocessable(format!(
                "the original is in {} and cannot be fetched yet; ask for a restore at {}",
                body.storage_class, body.restore_url,
            )),
            delivery::Refusal::Internal => Self::Internal,
        }
    }
}
