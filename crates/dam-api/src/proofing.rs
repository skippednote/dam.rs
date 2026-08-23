//! Proofing rounds over HTTP (M6b).
//!
//! ## Who may do what
//!
//! Opening a round needs `Manage`: it asks named people to spend time, and it snapshots a set of assets. Giving
//! a verdict needs only `Read` — a reviewer is somebody asked to look at pictures, and requiring `Manage` to
//! answer would mean only administrators could ever be asked. The round's own reviewer list is the
//! authorisation for deciding, which is why [`decide`] does not check a permission beyond being able to see the
//! assets.
//!
//! ## A round names people, and this surface resolves them
//!
//! Like a comment thread. Within a tenant that discloses nothing new — the same names are already on every
//! comment and every share — and "waiting on Ada" is the whole value of a review list.
//!
//! ## It gates nothing
//!
//! There is no endpoint here that changes an asset. A round records that people agreed; whether an unapproved
//! asset may be published is a rights question, and answering it here would put a collaboration table in the
//! delivery path.

use crate::assets::Failure;
use crate::caller;
use crate::comments::PersonView;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::proofing::{self, NewRound, ProofRefusal, Verdict};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

pub struct ProofingState {
    pub global: PgPool,
    /// For thumbnails on the round's assets, through the same signing path as the grid. A review screen that
    /// listed filenames would be asking people to approve pictures they cannot see.
    pub delivery: Option<Arc<crate::delivery::DeliveryState>>,
}

impl std::fmt::Debug for ProofingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofingState").finish_non_exhaustive()
    }
}

pub fn router(state: ProofingState) -> Router {
    Router::new()
        .route("/proofing", get(list).post(open))
        .route("/proofing/mine", get(mine))
        .route("/proofing/{id}", get(read))
        .route("/proofing/{id}/assets", get(round_assets))
        .route("/proofing/{id}/verdict", post(decide))
        .route("/proofing/{id}/cancel", post(cancel))
        .with_state(Arc::new(state))
}

/// One reviewer's standing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReviewerView {
    pub person: PersonView,
    /// `pending`, `approved` or `changes_requested`.
    pub verdict: String,
    /// Their covering note. The specifics live in comments on the assets.
    pub note: String,
    /// Absent while pending — and a pending reviewer has no decision moment, which the schema enforces.
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RoundView {
    pub id: Uuid,
    pub title: String,
    pub brief: String,
    /// Round 1, 2, 3 of a sequence.
    pub number: i32,
    /// The round this one follows, when it is a second pass.
    pub supersedes: Option<Uuid>,
    pub due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub requested_by: Option<PersonView>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub closed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `open`, `approved`, `changes_requested` or `cancelled`.
    ///
    /// **Derived from the verdicts, never stored** — see `dam_db::proofing`. `changes_requested` wins over any
    /// number of approvals.
    pub outcome: String,
    /// How many assets are in it. Shrinks if one is deleted.
    pub asset_count: i64,
    pub reviewers: Vec<ReviewerView>,
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
    path = "/proofing",
    params(ListParams),
    responses((status = 200, description = "Rounds you can see, newest first", body = [RoundView])),
    tag = "proofing",
)]
pub async fn list(
    State(state): State<Arc<ProofingState>>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<RoundView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rounds = proofing::list(conn.executor(), &caller.predicate, params.limit)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    present(&state, rounds).await
}

/// The rounds waiting on the caller, most urgent first.
#[utoipa::path(
    get,
    path = "/proofing/mine",
    responses((status = 200, description = "Rounds waiting on you, dated ones first", body = [RoundView])),
    tag = "proofing",
)]
pub async fn mine(
    State(state): State<Arc<ProofingState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<RoundView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    // A machine key holds no identity, so nothing can be waiting on it. An empty list rather than a refusal:
    // "nothing is waiting on you" is true and useful, and a 403 would read as a permission problem.
    let Some(identity) = caller.identity_id else {
        return Ok(Json(vec![]));
    };
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rounds = proofing::waiting_on(conn.executor(), identity, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    present(&state, rounds).await
}

#[utoipa::path(
    get,
    path = "/proofing/{id}",
    responses(
        (status = 200, body = RoundView),
        (status = 404, description = "No such round, or one over assets you cannot all see"),
    ),
    tag = "proofing",
)]
pub async fn read(
    State(state): State<Arc<ProofingState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<RoundView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let round = proofing::read(conn.executor(), id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    let mut presented = present(&state, vec![round]).await?;
    presented.0.pop().map(Json).ok_or(Failure::Internal)
}

/// One asset in a round, in snapshot order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RoundAssetView {
    pub asset_id: Uuid,
    pub position: i32,
    pub filename: String,
    pub mime: String,
    /// A short-lived preview link, present only when the asset has a thumbnail derivative *and* delivery is
    /// configured. Absent is not an error — it means there is nothing rendered yet — and the screen says so
    /// rather than showing a broken image.
    pub thumbnail_url: Option<String>,
}

/// The assets a round is about.
///
/// Separate from the round itself because the round is what a list screen draws — twenty of them, with their
/// reviewers — and its assets are what one detail screen draws. Folding the pictures into every row of a list
/// would sign a preview URL for every asset of every open round on a page nobody has opened yet.
#[utoipa::path(
    get,
    path = "/proofing/{id}/assets",
    responses(
        (status = 200, description = "The snapshot, in order", body = [RoundAssetView]),
        (status = 404, description = "No such round, or not all of its assets are visible to you"),
    ),
    tag = "proofing",
)]
pub async fn round_assets(
    State(state): State<Arc<ProofingState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<RoundAssetView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Through `read` first, and this is the whole access check: it refuses a round whose assets the caller
    // cannot *all* see, so the list below needs no second filter. Calling `items` directly would hand a
    // scoped curator the filenames of everything in somebody else's round.
    proofing::read(conn.executor(), id, &caller.predicate)
        .await
        .map_err(Refused)?;
    let items = proofing::items(conn.executor(), id)
        .await
        .map_err(Refused)?;
    let ids: Vec<Uuid> = items.iter().map(|item| item.asset_id).collect();
    // One query for the batch, as on the grid — "do you have a thumbnail" asked per row would be a round trip
    // per picture.
    let with_thumbnails =
        dam_db::derivatives::which_have(conn.executor(), &ids, &crate::assets::thumb_op_hash())
            .await?;
    conn.commit().await?;

    Ok(Json(
        items
            .into_iter()
            .map(|item| RoundAssetView {
                thumbnail_url: with_thumbnails
                    .contains(&item.asset_id)
                    .then(|| {
                        crate::assets::thumbnail_url(
                            state.delivery.as_deref(),
                            &caller,
                            item.asset_id,
                        )
                    })
                    .flatten(),
                asset_id: item.asset_id,
                position: item.position,
                filename: item.filename,
                mime: item.mime,
            })
            .collect(),
    ))
}

/// A round to open.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct OpenRoundBody {
    pub title: String,
    #[serde(default)]
    pub brief: String,
    /// The assets to review, in the order to show them. Snapshotted: a round cannot be widened afterwards.
    pub asset_ids: Vec<Uuid>,
    /// Who to ask. At least one, or nobody is being asked anything.
    pub reviewer_ids: Vec<Uuid>,
    #[serde(default)]
    pub due_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The round this follows, when it is a second pass. Its number is taken from that one plus one.
    #[serde(default)]
    pub supersedes: Option<Uuid>,
}

#[utoipa::path(
    post,
    path = "/proofing",
    request_body = OpenRoundBody,
    responses(
        (status = 201, body = RoundView),
        (status = 422, description = "No assets, no reviewers, or assets you cannot see"),
    ),
    tag = "proofing",
)]
pub async fn open(
    State(state): State<Arc<ProofingState>>,
    headers: HeaderMap,
    Json(body): Json<OpenRoundBody>,
) -> Result<(StatusCode, Json<RoundView>), Failure> {
    // Manage: opening a round asks named people to spend time and snapshots a set of assets.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    if body.title.trim().is_empty() {
        return Err(Failure::Unprocessable(
            "a round needs a title; it is what the reviewers will see in their list".to_owned(),
        ));
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let id = proofing::open(
        conn.executor(),
        &NewRound {
            title: &body.title,
            brief: &body.brief,
            asset_ids: &body.asset_ids,
            reviewer_ids: &body.reviewer_ids,
            due_at: body.due_at,
            requested_by: caller.identity_id,
            supersedes: body.supersedes,
        },
        &caller.predicate,
    )
    .await
    .map_err(Refused)?;
    let round = proofing::read(conn.executor(), id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;

    let mut presented = present(&state, vec![round]).await?;
    presented
        .0
        .pop()
        .map(|view| (StatusCode::CREATED, Json(view)))
        .ok_or(Failure::Internal)
}

/// A verdict.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct VerdictBody {
    /// `approved` or `changes_requested`. Not `pending` — that is a starting state, not an answer.
    pub verdict: String,
    /// A covering note. The specifics belong in comments on the assets, where they can be pinned to a region.
    #[serde(default)]
    pub note: String,
}

/// Records the caller's verdict on a round.
///
/// **Only `Read`**, deliberately. A reviewer is somebody asked to look at pictures, and requiring `Manage` to
/// answer would mean only administrators could ever be asked to review anything. The round's own reviewer list
/// is the authorisation: somebody not on it is refused, and the assets still have to be visible.
#[utoipa::path(
    post,
    path = "/proofing/{id}/verdict",
    request_body = VerdictBody,
    responses(
        (status = 200, description = "Recorded, with the round's new outcome", body = RoundView),
        (status = 403, description = "Not a reviewer on this round"),
        (status = 409, description = "The round is closed; a further review is a new round"),
        (status = 422, description = "Not a verdict"),
    ),
    tag = "proofing",
)]
pub async fn decide(
    State(state): State<Arc<ProofingState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<VerdictBody>,
) -> Result<Json<RoundView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let Some(identity) = caller.identity_id else {
        // A machine key is on no reviewer list, so it cannot be one. Forbidden rather than 422: the request is
        // well formed and the credential is the problem.
        return Err(Failure::Refused(caller::Refusal::Forbidden));
    };
    let verdict = Verdict::parse_decision(body.verdict.trim()).ok_or_else(|| {
        Failure::Unprocessable(format!(
            "{:?} is not a verdict; use approved or changes_requested",
            body.verdict
        ))
    })?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Read first, through the caller's predicate: a verdict on a round whose assets they cannot all see would
    // be an approval of pictures they have never opened.
    proofing::read(conn.executor(), id, &caller.predicate)
        .await
        .map_err(Refused)?;
    proofing::decide(conn.executor(), id, identity, verdict, &body.note)
        .await
        .map_err(Refused)?;
    let round = proofing::read(conn.executor(), id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;

    let mut presented = present(&state, vec![round]).await?;
    presented.0.pop().map(Json).ok_or(Failure::Internal)
}

/// Withdraws a round. Verdicts already given are kept.
#[utoipa::path(
    post,
    path = "/proofing/{id}/cancel",
    responses(
        (status = 200, description = "Withdrawn", body = RoundView),
        (status = 404, description = "No such round, or already withdrawn"),
    ),
    tag = "proofing",
)]
pub async fn cancel(
    State(state): State<Arc<ProofingState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<RoundView>, Failure> {
    // Manage, matching `open`: withdrawing a review is the requester's act, not a reviewer's.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    proofing::read(conn.executor(), id, &caller.predicate)
        .await
        .map_err(Refused)?;
    if !proofing::cancel(conn.executor(), id, caller.identity_id)
        .await
        .map_err(Refused)?
    {
        return Err(Failure::NotFound);
    }
    let round = proofing::read(conn.executor(), id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    let mut presented = present(&state, vec![round]).await?;
    presented.0.pop().map(Json).ok_or(Failure::Internal)
}

/// Resolves the names on a batch of rounds, in one read.
///
/// One query for every identity mentioned across every round, rather than one per reviewer per round: a list of
/// twenty rounds with four reviewers each is eighty lookups done as one.
async fn present(
    state: &ProofingState,
    rounds: Vec<proofing::Round>,
) -> Result<Json<Vec<RoundView>>, Failure> {
    let mut ids: Vec<Uuid> = Vec::new();
    for round in &rounds {
        if let Some(requester) = round.requested_by {
            ids.push(requester);
        }
        ids.extend(round.reviewers.iter().map(|r| r.identity_id));
    }
    ids.sort_unstable();
    ids.dedup();

    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;
    let named: std::collections::HashMap<Uuid, PersonView> = people
        .into_iter()
        .map(|person| {
            (
                person.id,
                PersonView {
                    id: person.id,
                    // `display_name`, and the db layer already falls back to the email when nobody set one —
                    // so a round never shows a blank where a person should be.
                    name: person.display_name,
                    email: person.email,
                },
            )
        })
        .collect();

    // A name that cannot be resolved is a deleted identity. Rendered as the id rather than dropped: a verdict
    // by somebody since removed is still a verdict, and losing the row would make the tally wrong.
    let unknown = |id: Uuid| PersonView {
        id,
        name: format!("{id}"),
        email: String::new(),
    };

    Ok(Json(
        rounds
            .into_iter()
            .map(|round| RoundView {
                id: round.id,
                title: round.title,
                brief: round.brief,
                number: round.number,
                supersedes: round.supersedes,
                due_at: round.due_at,
                requested_by: round
                    .requested_by
                    .map(|id| named.get(&id).cloned().unwrap_or_else(|| unknown(id))),
                created_at: round.created_at,
                closed_at: round.closed_at,
                outcome: round.outcome.as_str().to_owned(),
                asset_count: round.asset_count,
                reviewers: round
                    .reviewers
                    .into_iter()
                    .map(|reviewer| ReviewerView {
                        person: named
                            .get(&reviewer.identity_id)
                            .cloned()
                            .unwrap_or_else(|| unknown(reviewer.identity_id)),
                        verdict: reviewer.verdict.as_str().to_owned(),
                        note: reviewer.note,
                        decided_at: reviewer.decided_at,
                    })
                    .collect(),
            })
            .collect(),
    ))
}

/// Maps a [`ProofRefusal`] onto a status.
struct Refused(ProofRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            // 404 for a round that does not exist and for one whose assets the caller cannot all see. The same
            // refusal, because distinguishing them would confirm the round exists.
            ProofRefusal::UnknownRound(_) => Self::NotFound,
            // 403: the request is fine and the caller is not on the list. Reachable only for a round they can
            // already read, so it discloses nothing new.
            ProofRefusal::NotAReviewer(_) => Self::Refused(caller::Refusal::Forbidden),
            // 409: something is in the way rather than wrong with the request, and the sentence says what to do
            // instead — open a new round.
            ProofRefusal::AlreadyClosed => Self::Conflict(refusal.to_string()),
            ProofRefusal::AssetsOutOfScope(_)
            | ProofRefusal::NoAssets
            | ProofRefusal::NoReviewers
            | ProofRefusal::NotAVerdict(_) => Self::Unprocessable(refusal.to_string()),
            ProofRefusal::Database(error) => error.into(),
        }
    }
}
