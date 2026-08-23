//! The admin worklists over HTTP (Q.20, Q.2c·3).
//!
//! Two endpoints: the list of worklists with their sizes, and a page of one of them in the same row shape the
//! grid already draws. The queries themselves live in `dam_db::worklists`, which explains why they are SQL and
//! not search clauses.
//!
//! ## Read, not Manage
//!
//! These are the library's own gaps, and the person who fixes an uncategorised asset is whoever can edit it —
//! not necessarily an administrator. Gating on Manage would put the *finding* behind a permission the *fixing*
//! does not need, which is how a queue becomes one person's job. Every count is already scoped, so a reader
//! sees only their own work.
//!
//! ## The counts are stated as the caller's
//!
//! Two people legitimately see different numbers here, because a worklist is filtered by what each can read.
//! That is a fact worth saying out loud on the screen: a to-do list that counted work the reader cannot reach
//! would send them looking for an asset that 404s.
//!
//! ## The page reuses the grid's row
//!
//! Same `AssetPage`, same thumbnails, same engagement — so a worklist opens into something that looks and
//! behaves like the library rather than a table of uuids, and the fix is one click from the finding.

use crate::assets::Failure;
use crate::caller;
use crate::dto::{AssetPage, AssetSummary};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::worklists::Worklist;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// What the worklist endpoints need.
pub struct WorklistState {
    pub global: PgPool,
    /// For thumbnails on the page, through the same signing path as the grid.
    pub delivery: Option<Arc<crate::delivery::DeliveryState>>,
}

impl std::fmt::Debug for WorklistState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorklistState").finish_non_exhaustive()
    }
}

pub fn router(state: WorklistState) -> Router {
    Router::new()
        .route("/worklists", get(list))
        .route("/worklists/{key}", get(page))
        .with_state(Arc::new(state))
}

/// One worklist, and how much of it there is for this caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WorklistView {
    /// The stable name, used in the URL.
    pub key: String,
    /// What to call it on screen.
    pub label: String,
    /// What being on this list means, and what to do about it. Sent from the server rather than written into
    /// the client, so the sentence and the SQL that decides it live in the same place.
    pub explanation: String,
    /// How many assets *this caller* can see on it.
    pub count: i64,
    /// Whether the list is worth leading with when it is not empty.
    ///
    /// Two of these are exposure rather than tidiness — an asset served past its expiry date, and a licence
    /// about to lapse — and a screen that sorted by count would bury them under a thousand missing captions.
    pub urgent: bool,
}

fn describe(worklist: Worklist) -> (&'static str, &'static str, bool) {
    match worklist {
        Worklist::Expired => (
            "Past its scheduled expiry",
            "The retention date set on the asset has passed and it is still active, so it is still being \
             served. Archive it, or move the date.",
            true,
        ),
        Worklist::RightsExpiring => (
            "Licence coverage ending",
            "A licence term is inside its own renewal notice window — 60 days by default, and longer for a \
             contract that says so. This is the same reading as the “Expiring” badge on the asset itself.",
            true,
        ),
        Worklist::RightsDenied => (
            "Use not permitted",
            "Paperwork exists and forbids the intended use. Narrower than “no licence recorded”: something \
             was recorded, and it says no.",
            true,
        ),
        Worklist::ExpiringSoon => (
            "Scheduled to expire within 30 days",
            "The retention date set on the asset falls in the next 30 days. Distinct from licence coverage \
             above: this is a date somebody put on the file, not a contract term.",
            false,
        ),
        // Important, listed high, and deliberately *not* urgent. Every asset arrives with no licence, so on a
        // fresh tenant this list is the entire library — and a badge that fires on 100% of rows from day one is
        // not a signal, it is background. What the urgent flag marks is a *change*: a contract running out, a
        // use that has become forbidden. An absence that has always been there is a programme of work, not an
        // alarm. Seen on the dev library, where this read 180 of 182 and outlined itself in red.
        Worklist::NoLicence => (
            "No licence recorded",
            "No paperwork at all is attached, so nothing can say whether a use is permitted. This is the \
             absence of a record, not a refusal — a download may still be allowed on the tenant's default, \
             and every asset starts here.",
            false,
        ),
        Worklist::MissingRequired => (
            "Missing required metadata",
            "A field the asset's metadata type marks required is empty. Required is not enforced at upload: \
             refusing the bytes over a caption would strand the file, so it lands here instead.",
            false,
        ),
        Worklist::Uncategorised => (
            "In no category",
            "Nothing in the taxonomy points at this asset, so it is reachable by search and by nothing else.",
            false,
        ),
        Worklist::Embargoed => (
            "Not released yet",
            "Held until a future date. Expected for an unannounced campaign, and worth checking when the date \
             has been forgotten rather than chosen.",
            false,
        ),
        Worklist::EnrichmentFailed => (
            "Enrichment failed",
            "The AI pass stopped rather than queued. The asset is fine; the suggestions are missing.",
            false,
        ),
        Worklist::NoThumbnail => (
            "No thumbnail",
            "No thumbnail was ever rendered, so this asset is a grey square in every grid it appears in. \
             Usually a pipeline failure on a format the toolchain could not read.",
            false,
        ),
    }
}

#[utoipa::path(
    get,
    path = "/worklists",
    responses(
        (status = 200, description = "Every worklist with the caller's own count", body = [WorklistView]),
        (status = 403, description = "The credential holds no read scope"),
    ),
    tag = "worklists",
)]
pub async fn list(
    State(state): State<Arc<WorklistState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<WorklistView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let counts = dam_db::worklists::counts(conn.executor(), &caller.predicate).await?;
    conn.commit().await?;

    Ok(Json(
        counts
            .into_iter()
            .map(|(worklist, count)| {
                let (label, explanation, urgent) = describe(worklist);
                WorklistView {
                    key: worklist.key().to_owned(),
                    label: label.to_owned(),
                    explanation: explanation.to_owned(),
                    count,
                    urgent,
                }
            })
            .collect(),
    ))
}

/// Paging, matching `/assets` so a worklist behaves like the grid it opens into.
#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
pub struct PageParams {
    #[serde(default)]
    pub offset: i64,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

const fn default_limit() -> i64 {
    60
}

#[utoipa::path(
    get,
    path = "/worklists/{key}",
    params(("key" = String, Path, description = "The worklist"), PageParams),
    responses(
        (status = 200, description = "One page of the worklist", body = AssetPage),
        (status = 404, description = "No such worklist"),
    ),
    tag = "worklists",
)]
pub async fn page(
    State(state): State<Arc<WorklistState>>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Query(params): Query<PageParams>,
) -> Result<Json<AssetPage>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    // Resolved before the transaction: an unknown key is a 404 about the *worklist*, and opening a
    // transaction to discover that would be work for nothing.
    let worklist = Worklist::from_key(&key).ok_or(Failure::NotFound)?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let page = dam_db::worklists::page(
        conn.executor(),
        &caller.predicate,
        worklist,
        // Oldest first, which is the opposite of the grid's default and deliberate: a worklist is a backlog,
        // and the asset that has been waiting longest is the one to fix. Newest-first would show the same top
        // rows to everybody who ever opens it while the old work sank.
        dam_db::assets::Order::Oldest,
        params.offset,
        params.limit,
    )
    .await?;

    let ids: Vec<Uuid> = page.items.iter().map(|item| item.id).collect();
    let with_thumbnails =
        dam_db::derivatives::which_have(conn.executor(), &ids, &crate::assets::thumb_op_hash())
            .await?;
    let with_attachments =
        dam_db::attachments::which_have(conn.executor(), &ids, &caller.predicate)
            .await
            .map_err(|_| Failure::Internal)?;
    conn.commit().await?;

    let items: Vec<AssetSummary> = page
        .items
        .iter()
        .map(|row| {
            let mut summary = crate::assets::summary_with_extras(row, &[], &with_attachments);
            if with_thumbnails.contains(&row.id) {
                summary.thumbnail_url =
                    crate::assets::thumbnail_url(state.delivery.as_deref(), &caller, row.id);
            }
            summary
        })
        .collect();

    Ok(Json(AssetPage {
        did_you_mean: None,
        items,
        total: page.total,
        offset: page.offset,
        ranked: false,
    }))
}
