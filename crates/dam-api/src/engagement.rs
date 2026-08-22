//! Ratings, favourites and watches over HTTP (Q.5b).
//!
//! ## These need a person, not a key
//!
//! Every route here needs an identity behind the key. A rating is somebody's opinion, a favourite is somebody's
//! list, and a watch is somebody's request to be told — a service account has none of those, and recording one
//! against a bare key would write a row nobody can ever own or clear.
//!
//! `caller::authorize` already enforces this for every endpoint in the system: no identity means no membership,
//! so no grants, so 403 before a handler runs. [`person`] is therefore a fail-closed unwrap rather than the check
//! that produces the refusal — worth knowing, because mutation-testing it shows the mutant surviving, and the
//! reason is that the guarantee lives upstream and not that the rule is untested.
//!
//! ## Read, not Manage
//!
//! Rating something you can see is not administration. The dam-db layer already refuses an asset the caller
//! cannot see, so `Read` is exactly the right bar: whoever may look at an asset may have an opinion about it.
//! Requiring Manage would mean only administrators could favourite anything, which is the opposite of the point.
//!
//! ## Every response is the asset's engagement afterwards
//!
//! Not 204. A star widget has to redraw the average, and the average moved *because* of this request — so
//! returning it saves a round trip and, more importantly, means the number on screen came from the write rather
//! than from a read that raced it.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, put};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_core::query::{Planned, Query as AssetQuery};
use dam_db::engagement::{self, Engagement, EngagementRefusal, List};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// What the engagement endpoints need.
pub struct EngagementState {
    pub global: PgPool,
}

impl std::fmt::Debug for EngagementState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngagementState").finish_non_exhaustive()
    }
}

/// The engagement routes.
pub fn router(state: EngagementState) -> Router {
    Router::new()
        .route("/assets/{id}/rating", put(set_rating).delete(clear_rating))
        .route(
            "/assets/{id}/favourite",
            put(add_favourite).delete(remove_favourite),
        )
        .route("/assets/{id}/watch", put(add_watch).delete(remove_watch))
        .route("/favourites", get(favourites))
        .route("/watches", get(watches))
        .with_state(Arc::new(state))
}

/// An asset's engagement, as the caller may see it.
///
/// `Deserialize` as well as `Serialize` because `AssetDetail` embeds it and that type round-trips in the tests
/// and in the generated client; `PartialEq` for the same reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EngagementView {
    pub asset_id: Uuid,
    /// The mean of every rating, or null when nobody has rated it.
    ///
    /// Null rather than zero: "unrated" and "rated badly by everyone" are different facts, and a widget that
    /// drew them the same way would be lying about one of them.
    pub average_stars: Option<f64>,
    pub rating_count: i64,
    pub favourite_count: i64,
    /// This caller's own rating. Never anybody else's.
    pub my_stars: Option<i16>,
    pub is_favourite: bool,
    pub is_watched: bool,
}

/// A rating to set.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RatingRequest {
    /// 1 to 5. There is no zero — clearing a rating is `DELETE`, because "no opinion" is not a low score.
    pub stars: i16,
}

/// One page of a private list.
#[derive(Debug, Deserialize, IntoParams)]
pub struct PageParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

/// A page of the caller's own favourites or watches.
///
/// Whole assets, not ids. Ids were the first shape here, on the reasoning that a grid already knows how to render
/// a set of assets — but there is no endpoint that fetches assets *by id set*, so a client holding fifty ids had
/// fifty requests to make. Returning the same `AssetPage` the browse and search endpoints return means one
/// request and one renderer, which is what the ids were supposed to achieve.
#[derive(Debug, Serialize, ToSchema)]
pub struct ListPage {
    /// Ordered by when *this caller* added each one, newest first — which is the order that makes a private list
    /// legible, and is not the order any other endpoint can produce.
    pub items: Vec<crate::dto::AssetSummary>,
    /// How many the caller has *and can still see* — the same predicate the page came from.
    pub total: i64,
    /// Zero-based index of the first item within the full list.
    pub offset: i64,
}

/// Sets the caller's rating.
#[utoipa::path(
    put,
    path = "/assets/{id}/rating",
    request_body = RatingRequest,
    responses(
        (status = 200, body = EngagementView),
        (status = 403, description = "The key has no person behind it"),
        (status = 404, description = "No such asset, or not one this caller may see"),
        (status = 422, description = "Not 1 to 5 stars"),
    ),
    tag = "engagement",
)]
pub async fn set_rating(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(request): Json<RatingRequest>,
) -> Result<Json<EngagementView>, Failure> {
    act(&state, &headers, asset_id, Op::Rate(request.stars)).await
}

/// Clears the caller's rating. Clearing one that is not there is not an error.
#[utoipa::path(
    delete,
    path = "/assets/{id}/rating",
    responses(
        (status = 200, body = EngagementView),
        (status = 403, description = "The key has no person behind it"),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "engagement",
)]
pub async fn clear_rating(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<EngagementView>, Failure> {
    act(&state, &headers, asset_id, Op::Unrate).await
}

/// Adds the asset to the caller's favourites. Idempotent.
#[utoipa::path(
    put,
    path = "/assets/{id}/favourite",
    responses(
        (status = 200, body = EngagementView),
        (status = 403, description = "The key has no person behind it"),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "engagement",
)]
pub async fn add_favourite(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<EngagementView>, Failure> {
    act(&state, &headers, asset_id, Op::Favourite).await
}

/// Removes it from the caller's favourites. Idempotent.
#[utoipa::path(
    delete,
    path = "/assets/{id}/favourite",
    responses(
        (status = 200, body = EngagementView),
        (status = 403, description = "The key has no person behind it"),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "engagement",
)]
pub async fn remove_favourite(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<EngagementView>, Failure> {
    act(&state, &headers, asset_id, Op::Unfavourite).await
}

/// Starts watching the asset. Idempotent.
#[utoipa::path(
    put,
    path = "/assets/{id}/watch",
    responses(
        (status = 200, body = EngagementView),
        (status = 403, description = "The key has no person behind it"),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "engagement",
)]
pub async fn add_watch(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<EngagementView>, Failure> {
    act(&state, &headers, asset_id, Op::Watch).await
}

/// Stops watching. Idempotent.
#[utoipa::path(
    delete,
    path = "/assets/{id}/watch",
    responses(
        (status = 200, body = EngagementView),
        (status = 403, description = "The key has no person behind it"),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "engagement",
)]
pub async fn remove_watch(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<EngagementView>, Failure> {
    act(&state, &headers, asset_id, Op::Unwatch).await
}

/// The caller's own favourites, newest first.
#[utoipa::path(
    get,
    path = "/favourites",
    params(PageParams),
    responses(
        (status = 200, body = ListPage),
        (status = 403, description = "The key has no person behind it"),
    ),
    tag = "engagement",
)]
pub async fn favourites(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Query(page): Query<PageParams>,
) -> Result<Json<ListPage>, Failure> {
    read_list(&state, &headers, List::Favourites, page).await
}

/// The caller's own watches, newest first.
#[utoipa::path(
    get,
    path = "/watches",
    params(PageParams),
    responses(
        (status = 200, body = ListPage),
        (status = 403, description = "The key has no person behind it"),
    ),
    tag = "engagement",
)]
pub async fn watches(
    State(state): State<Arc<EngagementState>>,
    headers: HeaderMap,
    Query(page): Query<PageParams>,
) -> Result<Json<ListPage>, Failure> {
    read_list(&state, &headers, List::Watches, page).await
}

/// Which engagement write a handler is asking for.
///
/// An enum rather than a closure passed into [`act`]: a closure taking the connection and the plan by reference
/// needs a higher-ranked bound to express, and the six operations are a closed set anyway. Naming them also makes
/// the dispatch below a list one can read against the routes.
#[derive(Debug, Clone, Copy)]
enum Op {
    Rate(i16),
    Unrate,
    Favourite,
    Unfavourite,
    Watch,
    Unwatch,
}

impl Op {
    async fn run(
        self,
        conn: &mut sqlx::PgConnection,
        asset_id: Uuid,
        identity: Uuid,
        planned: &Planned,
    ) -> Result<Engagement, EngagementRefusal> {
        match self {
            Self::Rate(stars) => engagement::rate(conn, asset_id, identity, stars, planned).await,
            Self::Unrate => engagement::unrate(conn, asset_id, identity, planned).await,
            Self::Favourite => engagement::favourite(conn, asset_id, identity, planned).await,
            Self::Unfavourite => engagement::unfavourite(conn, asset_id, identity, planned).await,
            Self::Watch => engagement::watch(conn, asset_id, identity, planned).await,
            Self::Unwatch => engagement::unwatch(conn, asset_id, identity, planned).await,
        }
    }
}

/// Authorises, resolves the person, and runs one engagement write inside a tenant transaction.
///
/// Six handlers differing only in which dam-db call they make, so the authorisation, the identity check and the
/// transaction live here once. Six copies of this preamble would be six chances to omit the identity check.
async fn act(
    state: &Arc<EngagementState>,
    headers: &HeaderMap,
    asset_id: Uuid,
    op: Op,
) -> Result<Json<EngagementView>, Failure> {
    let caller = caller::authorize(&state.global, headers, Action::Read).await?;
    let identity = person(&caller)?;
    // `Query::All` and the caller's own predicate: there is no user query here, only the access filter — which
    // is the whole point, since it is what decides whether this asset exists as far as this caller is concerned.
    let planned = Planned::new(AssetQuery::All, caller.predicate.clone(), &[])
        .map_err(|_| Failure::Internal)?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let after = op
        .run(conn.executor(), asset_id, identity, &planned)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(present(after)))
}

/// The same preamble for the two list reads.
async fn read_list(
    state: &Arc<EngagementState>,
    headers: &HeaderMap,
    which: List,
    page: PageParams,
) -> Result<Json<ListPage>, Failure> {
    let caller = caller::authorize(&state.global, headers, Action::Read).await?;
    let identity = person(&caller)?;
    let planned = Planned::new(AssetQuery::All, caller.predicate.clone(), &[])
        .map_err(|_| Failure::Internal)?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let (total, asset_ids) = engagement::mine(
        conn.executor(),
        which,
        identity,
        &planned,
        page.limit,
        page.offset,
    )
    .await
    .map_err(Refused)?;

    // Hydrated in the order the list gave, which is the order the caller added them — so a per-id read rather
    // than one set query, exactly as the ranked search path does and for the same reason: a set query returns
    // rows in whatever order it likes, and the order *is* the answer here.
    let engaged = crate::assets::page_engagement(&caller, conn.executor(), &asset_ids).await?;
    let mut items = Vec::with_capacity(asset_ids.len());
    for asset_id in &asset_ids {
        if let Some(found) =
            dam_db::assets::detail(conn.executor(), &caller.predicate, *asset_id).await?
        {
            items.push(crate::assets::summary_with_engagement(
                &found.summary,
                &engaged,
            ));
        }
    }
    conn.commit().await?;
    Ok(Json(ListPage {
        items,
        total,
        offset: page.offset.max(0),
    }))
}

/// The person behind the key.
///
/// Unreachable in practice: `caller::authorize` refuses a key with no identity before any handler runs, so
/// `Caller::identity_id` is always `Some` by the time it gets here. Kept as a fail-closed unwrap rather than an
/// `expect`, because the alternative to a 403 would be inventing an identity — and a row keyed to a fabricated
/// person can never be found, owned or cleared by anyone.
///
/// The `Option` is a wider smell: three other call sites re-check the same guarantee. Recorded in TASKS.md
/// rather than refactored here, since narrowing the type touches every handler.
fn person(caller: &caller::Caller) -> Result<Uuid, Failure> {
    caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))
}

/// An [`EngagementView`] for an asset, from an engagement row that may not be there.
///
/// The absent case is a caller with no identity, for whom "nothing is favourited" is simply true. Zeroes rather
/// than a null object, so a panel never has to decide what a missing engagement means.
#[must_use]
pub fn view_of(state: Option<Engagement>, asset_id: Uuid) -> EngagementView {
    state.map_or(
        EngagementView {
            asset_id,
            average_stars: None,
            rating_count: 0,
            favourite_count: 0,
            my_stars: None,
            is_favourite: false,
            is_watched: false,
        },
        present,
    )
}

fn present(state: Engagement) -> EngagementView {
    EngagementView {
        asset_id: state.asset_id,
        average_stars: state.average_stars,
        rating_count: state.rating_count,
        favourite_count: state.favourite_count,
        my_stars: state.my_stars,
        is_favourite: state.is_favourite,
        is_watched: state.is_watched,
    }
}

/// Maps an [`EngagementRefusal`] onto a status.
struct Refused(EngagementRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            // 404 for both "no such asset" and "not yours to see" — the dam-db layer already collapses them, and
            // splitting them here would rebuild the existence oracle it exists to prevent.
            EngagementRefusal::UnknownAsset(_) => Self::NotFound,
            EngagementRefusal::OutOfRange(_) => Self::Unprocessable(refusal.to_string()),
            EngagementRefusal::Database(error) => error.into(),
        }
    }
}
