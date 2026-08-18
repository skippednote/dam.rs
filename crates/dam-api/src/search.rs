//! Search (`GET /search`) and facets (`GET /search/facets`).
//!
//! ## Tantivy ranks, Postgres authorises
//!
//! The index carries group membership so the access predicate can narrow the candidate set, and that filter
//! is an **optimisation, not the authority**: membership changes in Postgres the instant an administrator
//! saves, and the index catches up when the asset is reindexed. In between, the index is *permissive*, and a
//! permissive stale index used as the gate on a governed library is a leak.
//!
//! So this handler renders the predicate into the Tantivy query *and* hydrates the resulting ids through
//! Postgres with the same predicate. Both, deliberately.
//!
//! ## A clause the index cannot answer is refused, not dropped
//!
//! Taxonomy and collection membership are relational. `dam_search::query::render` refuses them by name
//! rather than dropping them, because a dropped filter clause returns **more** than the caller asked for —
//! and for a filter over a governed library that is the wrong direction to be wrong in. Those queries route
//! through SQL instead, which is what `dam_db::query_sql` is for.

use crate::caller::{self, Caller};
use crate::dto::AssetPage;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_core::query::Planned;
use dam_core::shorthand;
use dam_db::assets;
use dam_search::{IndexPool, IndexSchema};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

/// What the search endpoints need.
pub struct SearchState {
    pub global: PgPool,
    pub indexes: Arc<IndexPool>,
}

impl std::fmt::Debug for SearchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchState").finish_non_exhaustive()
    }
}

/// How many ids to ask the index for before hydrating.
///
/// Larger than the page, because hydration *drops* ids — an asset the caller may not see, or one deleted
/// since the last reindex. Asking for exactly one page would return a short page whenever anything was
/// dropped, and a grid would read that as the end of the results.
const OVERFETCH: usize = 4;

/// The deepest a caller may page into a ranked result.
///
/// Relevance ranking past a few hundred results is not meaningfully ordered, and `TopDocs` costs the full
/// depth on every request — so an offset of fifty thousand is a request to sort the whole library by a score
/// nobody will read. A caller who wants everything wants the list endpoint, which pages on an index.
pub const MAX_SEARCH_DEPTH: i64 = 1_000;

/// The query string.
#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct SearchParams {
    /// Shorthand: `bra:acme`, quoted phrases, ranges, negation. Empty matches everything the caller may see.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub offset: i64,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    50
}

/// One facetable field and its buckets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Facet {
    pub key: String,
    pub buckets: Vec<Bucket>,
    /// Whether buckets were dropped by the limit. Reported rather than left implicit — see [`FACET_BUCKETS`].
    pub truncated: bool,
}

/// Where a query was refused, so a UI can point at the character.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QueryProblem {
    pub message: String,
    /// A stable machine-readable code, when the parser gave one. A UI maps it to a message in the user's
    /// language, which prose cannot be.
    pub code: Option<String>,
    /// One-based column the parser stopped at, when it has one. A filter rail underlines from here; without it
    /// a user gets "invalid query" and no idea which word.
    pub at: Option<usize>,
}

/// The search routes.
pub fn router(state: SearchState) -> Router {
    Router::new()
        .route("/search", get(search))
        .route("/search/facets", get(facets))
        .with_state(Arc::new(state))
}

/// Ranked results for a shorthand query.
#[utoipa::path(
    get,
    path = "/search",
    params(SearchParams),
    responses(
        (status = 200, description = "One page of ranked results", body = AssetPage),
        (status = 400, description = "The query does not parse, or names a field that does not exist", body = QueryProblem),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no read scope"),
        (status = 501, description = "A clause the index cannot answer — refused rather than dropped, because dropping it would return more than was asked for", body = QueryProblem),
    ),
    tag = "search",
)]
pub async fn search(
    State(state): State<Arc<SearchState>>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<Json<AssetPage>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let offset = params.offset.clamp(0, MAX_SEARCH_DEPTH);
    let limit = params.limit.clamp(1, 200);

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let (planned, defs) = plan(conn.executor(), &caller, &params.q).await?;

    let index_schema = IndexSchema::new(defs);
    let open = state
        .indexes
        .get(&caller.tenant_slug, &index_schema)
        .await
        .map_err(Failure::from)?;
    // Once per request. A searcher held from a previous request would answer from the index as it was, and
    // an asset edited a moment ago would be missing from its own search results.
    open.reload().map_err(Failure::from)?;

    let depth = usize::try_from(offset + limit).unwrap_or(usize::MAX);
    let ranked = dam_search::query::search(
        &open,
        &index_schema,
        &planned,
        depth.saturating_mul(OVERFETCH).max(depth),
    )
    .map_err(Failure::from)?;

    // Hydrated through Postgres with the same predicate. See the module docs: this is what makes a stale,
    // permissive index harmless.
    let visible = assets::visible_among(conn.executor(), &caller.predicate, &ranked).await?;
    let total = i64::try_from(visible.len()).unwrap_or(i64::MAX);

    let window: Vec<uuid::Uuid> = visible
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .collect();

    let mut items = Vec::with_capacity(window.len());
    for asset_id in &window {
        // Per id rather than one `IN` query, because the ranking is the order and a set query returns rows
        // in whatever order it likes. The window is at most 200 rows.
        if let Some(found) = assets::detail(conn.executor(), &caller.predicate, *asset_id).await? {
            items.push(crate::assets::summary_of(&found.summary));
        }
    }
    conn.commit().await?;

    Ok(Json(AssetPage {
        items,
        // The number of *ranked and visible* results, which is not the library total and is capped by the
        // overfetch depth. A grid uses it for `aria-rowcount`; it is honest about being a ranked-result count
        // rather than pretending to be exhaustive.
        total,
        offset,
    }))
}

/// One facet bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Bucket {
    /// The value as text. A number or a boolean is rendered rather than typed, because a rail displays it and
    /// the round trip back into a query goes through the field's kind anyway.
    pub value: String,
    /// A stable identifier where one exists — a taxonomy term id. Absent for a metadata value, whose identity
    /// *is* its text.
    pub id: Option<uuid::Uuid>,
    pub count: i64,
}

/// Facet counts for the caller's visible library, narrowed by the same query the results were.
#[utoipa::path(
    get,
    path = "/search/facets",
    params(SearchParams),
    responses(
        (status = 200, description = "Every facetable field with its buckets and counts", body = Vec<Facet>),
        (status = 400, description = "The query does not parse", body = QueryProblem),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no read scope"),
    ),
    tag = "search",
)]
pub async fn facets(
    State(state): State<Arc<SearchState>>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<Facet>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // The *same* query the results were narrowed by, which is what makes the numbers mean anything: a rail
    // counting the whole library beside a filtered result set tells a user there are 240 outdoor assets and
    // then shows them three.
    let (planned, defs) = plan(conn.executor(), &caller, &params.q).await?;

    // Every facetable field, plus every taxonomy. Which fields are facetable is the tenant's decision,
    // recorded on the field definition — a hard-coded list here would be a second schema.
    let mut requests: Vec<dam_db::facets::FacetRequest> = defs
        .iter()
        .filter(|def| def.facetable)
        .map(|def| dam_db::facets::FacetRequest::Field {
            key: def.key.clone(),
            limit: FACET_BUCKETS,
        })
        .collect();
    let taxonomies: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM taxonomies ORDER BY label")
            .fetch_all(conn.executor())
            .await
            .map_err(dam_db::Error::from)?;
    requests.extend(taxonomies.into_iter().map(|taxonomy_id| {
        dam_db::facets::FacetRequest::Taxonomy {
            taxonomy_id,
            limit: FACET_BUCKETS,
        }
    }));

    let counted = dam_db::facets::count_on(conn.executor(), &planned, &defs, &requests).await?;
    conn.commit().await?;

    Ok(Json(
        counted
            .into_iter()
            .map(|facet| Facet {
                key: facet.key,
                truncated: facet.truncated,
                buckets: facet
                    .buckets
                    .into_iter()
                    .map(|bucket| Bucket {
                        value: bucket.value,
                        id: bucket.id,
                        count: bucket.count,
                    })
                    .collect(),
            })
            .collect(),
    ))
}

/// How many buckets a facet returns.
///
/// A rail shows a handful and a "more" affordance, and `truncated` says when there were others — a rail that
/// silently cuts off makes "no other brands" and "ninety other brands" look identical.
const FACET_BUCKETS: i64 = 20;

/// Parses `q` and binds it to the caller's predicate.
///
/// Shared by both handlers on purpose. Two copies of "parse, validate, plan" is how a facet rail ends up
/// counting a slightly different query from the one that produced the results beside it.
async fn plan(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    q: &str,
) -> Result<(Planned, Vec<dam_core::fields::FieldDef>), Failure> {
    let defs = dam_db::fields::load(&mut *conn).await?;
    let aliases = dam_db::fields::aliases(&mut *conn).await?;
    let parse_schema = shorthand::Schema::new(defs.clone(), aliases);

    let parsed = if q.trim().is_empty() {
        dam_core::query::Query::All
    } else {
        shorthand::parse(q, &parse_schema).map_err(|e| {
            Failure::BadQuery(QueryProblem {
                message: e.detail.clone(),
                code: Some(e.code.to_owned()),
                at: Some(e.column),
            })
        })?
    };

    // `Planned::new` is the only constructor and it takes the predicate, which is §7/§12 expressed in the
    // type system: there is no value of this type without an access filter, so no renderer can omit one.
    let planned = Planned::new(parsed, caller.predicate.clone(), &defs).map_err(|rejections| {
        Failure::BadQuery(QueryProblem {
            message: rejections
                .iter()
                .map(|r| format!("{}: {}", r.key, r.detail))
                .collect::<Vec<_>>()
                .join("; "),
            code: rejections.first().map(|r| r.code.to_owned()),
            at: None,
        })
    })?;
    Ok((planned, defs))
}

/// Everything that can go wrong in a search.
#[derive(Debug)]
pub enum Failure {
    Refused(caller::Refusal),
    BadQuery(QueryProblem),
    /// A clause the index cannot express. **501, not 400**: the caller's query is valid and this back end
    /// cannot answer it, which is a gap in the server rather than a mistake by the client — and a 400 would
    /// send somebody looking for a typo that is not there.
    Unsupported(QueryProblem),
    Internal,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Refused(refusal) => refusal.into_response(),
            Self::BadQuery(problem) => (StatusCode::BAD_REQUEST, Json(problem)).into_response(),
            Self::Unsupported(problem) => {
                (StatusCode::NOT_IMPLEMENTED, Json(problem)).into_response()
            }
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<caller::Refusal> for Failure {
    fn from(refusal: caller::Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<dam_db::Error> for Failure {
    fn from(error: dam_db::Error) -> Self {
        tracing::error!(%error, "search database error");
        Self::Internal
    }
}

impl From<dam_search::Error> for Failure {
    fn from(error: dam_search::Error) -> Self {
        match error {
            dam_search::Error::Unsupported(reason) => Self::Unsupported(QueryProblem {
                message: reason,
                code: Some("unsupported_clause".to_owned()),
                at: None,
            }),
            other => {
                tracing::error!(error = %other, "search index error");
                Self::Internal
            }
        }
    }
}
