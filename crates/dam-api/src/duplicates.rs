//! The near-duplicate review queue, and the colour facet (M4, §8.1).
//!
//! ## A queue, never an automatic merge
//!
//! Migration 0003 states the rule and the reason: "auto-merging a crop that is actually a different licensed
//! deliverable is a rights problem, so a human decides". So this surface offers three verdicts and performs
//! none of them on the assets — it records what a person decided about a pair.
//!
//! `merged` is accepted and recorded and *merges nothing*. What a merge means — which asset survives, what
//! happens to the other's rights, shares and references — is a decision with consequences this table has no
//! way to express, and a button that silently picked one would be the worst possible place to make it.
//!
//! ## Both halves of a pair are filtered through the caller's scope
//!
//! A pair names two assets. Showing a pair where the caller can see only one would disclose that the other
//! exists, and showing one where they can see neither would be a row they cannot act on at all. So a candidate
//! appears only when both sides are visible to the reader — which means two people legitimately see different
//! queues, and the screen says so.
//!
//! ## Manage to resolve, Read to look
//!
//! Reading the queue is reading the library's own state, which anybody who can see the assets can already
//! work out. Recording a verdict changes what somebody else sees next, so it needs Manage.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

pub struct DuplicateState {
    pub global: PgPool,
    /// For the thumbnails, through the same signing path as the grid — comparing two pictures is the whole task.
    pub delivery: Option<Arc<crate::delivery::DeliveryState>>,
}

impl std::fmt::Debug for DuplicateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplicateState").finish_non_exhaustive()
    }
}

pub fn router(state: DuplicateState) -> Router {
    Router::new()
        .route("/duplicates", get(list))
        .route("/duplicates/{id}", post(resolve))
        .route("/colours", get(colours))
        .with_state(Arc::new(state))
}

/// One side of a candidate pair, with enough to look at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Side {
    pub asset_id: Uuid,
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
    /// Absent when nothing has rendered a thumbnail, rather than a link that 404s.
    pub thumbnail_url: Option<String>,
}

/// A pair awaiting a verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CandidateView {
    pub id: Uuid,
    pub left: Side,
    pub right: Side,
    /// Bits differing out of 64, from the closer of the two hashes. Lower is more alike.
    pub hamming: Option<i16>,
    /// Absent until embeddings exist — the model-dependent half of M4.
    pub cosine: Option<f32>,
    /// `near_identical` or `variant`. The finer relations need an embedding to tell apart.
    pub relation: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

const fn default_limit() -> i64 {
    50
}

#[utoipa::path(
    get,
    path = "/duplicates",
    params(ListParams),
    responses((status = 200, description = "Open pairs, most alike first", body = [CandidateView])),
    tag = "duplicates",
)]
pub async fn list(
    State(state): State<Arc<DuplicateState>>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<CandidateView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // Over-read, because the scope filter below removes rows: asking for exactly `limit` and then dropping
    // half would hand back a short page that looks like the end of the queue. Bounded so a narrow scope cannot
    // turn one request into a full scan.
    let raw = dam_db::similarity::open_candidates(conn.executor(), params.limit.clamp(1, 200) * 4)
        .await?;

    // Both sides, in one visibility check. A pair where the caller can see one asset and not the other would
    // disclose that the other exists.
    let mut ids: Vec<Uuid> = Vec::with_capacity(raw.len() * 2);
    for candidate in &raw {
        ids.push(candidate.asset_id);
        ids.push(candidate.other_id);
    }
    let visible: std::collections::HashSet<Uuid> =
        dam_db::assets::visible_among(conn.executor(), &caller.predicate, &ids)
            .await?
            .into_iter()
            .collect();

    let shown: Vec<&dam_db::similarity::Candidate> = raw
        .iter()
        .filter(|candidate| {
            visible.contains(&candidate.asset_id) && visible.contains(&candidate.other_id)
        })
        .take(usize::try_from(params.limit.clamp(1, 200)).unwrap_or(50))
        .collect();

    let mut wanted: Vec<Uuid> = Vec::with_capacity(shown.len() * 2);
    for candidate in &shown {
        wanted.push(candidate.asset_id);
        wanted.push(candidate.other_id);
    }
    let sides = sides_for(&state, &caller, conn.executor(), &wanted).await?;
    conn.commit().await?;

    Ok(Json(
        shown
            .into_iter()
            .filter_map(|candidate| {
                Some(CandidateView {
                    id: candidate.id,
                    left: sides.get(&candidate.asset_id)?.clone(),
                    right: sides.get(&candidate.other_id)?.clone(),
                    hamming: candidate.hamming,
                    cosine: candidate.cosine,
                    relation: candidate.relation.clone(),
                })
            })
            .collect(),
    ))
}

/// A verdict.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct ResolveBody {
    /// `confirmed`, `dismissed` or `merged`.
    ///
    /// **`merged` records a decision and merges nothing.** Which asset survives, and what happens to the
    /// other's rights and references, is not something this endpoint can decide — see the module docs.
    pub state: String,
}

#[utoipa::path(
    post,
    path = "/duplicates/{id}",
    request_body = ResolveBody,
    responses(
        (status = 204, description = "Recorded"),
        (status = 404, description = "No such open pair, or one you cannot see both sides of"),
        (status = 422, description = "Not a verdict"),
    ),
    tag = "duplicates",
)]
pub async fn resolve(
    State(state): State<Arc<DuplicateState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<ResolveBody>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // Checked against the caller's scope before resolving. Without it, somebody could dismiss a pair of assets
    // they cannot see — which is both a write outside their scope and a way to learn the pair exists.
    let open = dam_db::similarity::open_candidates(conn.executor(), 500).await?;
    let Some(candidate) = open.into_iter().find(|row| row.id == id) else {
        return Err(Failure::NotFound);
    };
    let visible = dam_db::assets::visible_among(
        conn.executor(),
        &caller.predicate,
        &[candidate.asset_id, candidate.other_id],
    )
    .await?;
    if visible.len() != 2 {
        return Err(Failure::NotFound);
    }

    let resolved =
        dam_db::similarity::resolve(conn.executor(), id, body.state.trim(), caller.identity_id)
            .await
            .map_err(|error| match error {
                dam_db::Error::Unsupported(reason) => Failure::Unprocessable(reason),
                other => other.into(),
            })?;
    conn.commit().await?;

    if resolved {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Failure::NotFound)
    }
}

/// One colour bucket and how many assets lead with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ColourBucket {
    /// `red`, `orange`, `grey`, `brown` and so on — a coarse name, so a facet groups into something clickable.
    pub bucket: String,
    /// Assets whose *primary* colour is in this bucket.
    pub count: i64,
}

/// The colour buckets present in the library.
///
/// **Not scoped to the caller**, and that is a deliberate exception worth stating. Every other count in this
/// codebase runs through the reader's predicate, because §7 says a count is a disclosure. A colour bucket is
/// the one place that reasoning does not reach: "eleven assets are mostly blue" names no asset, and the facet
/// exists to tell somebody whether clicking it is worth anything. The *results* of clicking it are scoped like
/// every other search, so the disclosure is a number with nothing behind it.
#[utoipa::path(
    get,
    path = "/colours",
    responses((status = 200, description = "Colour buckets, most common first", body = [ColourBucket])),
    tag = "duplicates",
)]
pub async fn colours(
    State(state): State<Arc<DuplicateState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ColourBucket>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let buckets = dam_db::similarity::colour_buckets(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(
        buckets
            .into_iter()
            .map(|(bucket, count)| ColourBucket { bucket, count })
            .collect(),
    ))
}

/// Reads the facts and mints the thumbnails for a set of assets, keyed by id.
async fn sides_for(
    state: &DuplicateState,
    caller: &caller::Caller,
    conn: &mut sqlx::PgConnection,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Side>, Failure> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<(Uuid, String, String, i64)> =
        sqlx::query_as("SELECT id, filename, mime, bytes FROM assets WHERE id = ANY($1)")
            .bind(ids.to_vec())
            .fetch_all(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?;

    // One query for the whole page, as on the grid: asking per asset would be a round trip per side.
    let with_thumbnails =
        dam_db::derivatives::which_have(&mut *conn, ids, &crate::assets::thumb_op_hash()).await?;

    Ok(rows
        .into_iter()
        .map(|(asset_id, filename, mime, bytes)| {
            let thumbnail_url = with_thumbnails
                .contains(&asset_id)
                .then(|| crate::assets::thumbnail_url(state.delivery.as_deref(), caller, asset_id))
                .flatten();
            (
                asset_id,
                Side {
                    asset_id,
                    filename,
                    mime,
                    bytes,
                    thumbnail_url,
                },
            )
        })
        .collect())
}
