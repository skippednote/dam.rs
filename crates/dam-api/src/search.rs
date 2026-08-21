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
///
/// `Deserialize` as well as `Serialize` because it travels *inside* another response now — a translated
/// question reports the parser's own refusal rather than inventing a second vocabulary for the same failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct QueryProblem {
    pub message: String,
    /// A stable machine-readable code, when the parser gave one. A UI maps it to a message in the user's
    /// language, which prose cannot be.
    pub code: Option<String>,
    /// One-based column the parser stopped at, when it has one. A filter rail underlines from here; without it
    /// a user gets "invalid query" and no idea which word.
    pub at: Option<usize>,
    /// The name the parser thinks was meant, when one is close enough (Q.17).
    ///
    /// A suggestion beside a refusal, not a correction of it: the query still failed. The client offers it as
    /// a one-click fix, which is the difference between "no field named `brnad`" and a search somebody can
    /// finish.
    pub suggestion: Option<String>,
}

/// The search routes.
pub fn router(state: SearchState) -> Router {
    router_from(Arc::new(state))
}

/// The same routes, over a state somebody else is also holding — see `downloads::router_from`.
pub fn router_from(state: Arc<SearchState>) -> Router {
    Router::new()
        .route("/search", get(search))
        .route("/search/facets", get(facets))
        .route("/search/suggest", get(suggest))
        .route("/search/export.csv", get(export))
        .with_state(state)
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
    Ok(Json(run(&state, &caller, &params).await?))
}

/// Runs a search for a caller who has already been authorised.
///
/// Split out from the route for the MCP server (§8.5: "over the **same ABAC layer**"), and that sharing is
/// the point rather than a convenience. This function is where the query is parsed, the predicate is
/// composed into the plan, the relational clauses are routed to SQL and the index results are re-checked
/// against Postgres. A second implementation for agents would be a second chance to get any of those wrong,
/// and the one that matters — the predicate — would fail silently by returning *more*.
pub async fn run(
    state: &SearchState,
    caller: &Caller,
    params: &SearchParams,
) -> Result<AssetPage, Failure> {
    let offset = params.offset.clamp(0, MAX_SEARCH_DEPTH);
    let limit = params.limit.clamp(1, 200);

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let (planned, defs) = plan(conn.executor(), caller, &params.q).await?;

    // Some clauses are joins, not index terms: category and collection membership. `dam_search` refuses them
    // by name rather than dropping them, because a dropped filter returns *more* than the caller asked for —
    // the wrong direction to be wrong in over a governed library. So they are answered in SQL instead, which
    // can express the whole query language; the cost is that SQL has no relevance score, so the page comes
    // back in browse order and says so.
    if is_relational(planned.query()) {
        let page = assets::page_matching(
            conn.executor(),
            &planned,
            dam_db::assets::Order::Newest,
            offset,
            limit,
        )
        .await?;
        // The same engagement read the grid does, so a search result draws its star exactly as a browse result
        // does (Q.5b·3). Without this a favourited asset appears unfavourited the moment somebody searches for
        // it, and the star flips back when they clear the query — which reads as data loss.
        let ids: Vec<uuid::Uuid> = page.items.iter().map(|item| item.id).collect();
        let engagement = crate::assets::page_engagement(caller, conn.executor(), &ids)
            .await
            .map_err(|_| Failure::Internal)?;
        conn.commit().await?;
        return Ok(AssetPage {
            items: page
                .items
                .iter()
                .map(|row| crate::assets::summary_with_engagement(row, &engagement))
                .collect(),
            total: page.total,
            offset,
            ranked: false,
            // No value suggestion on this path, and it is not an omission. A did-you-mean is only made for a
            // lone field equality (see `sole_text_equality`), and an equality is never what sends a query
            // here — this path exists for the clauses the index cannot answer. Computing one would be a query
            // per empty page that could not produce an answer.
            did_you_mean: None,
        });
    }

    // Cloned rather than moved: the did-you-mean below needs the definitions to find the field a value
    // belonged to, and a search that matched nothing is the only path that reads them again.
    let index_schema = IndexSchema::new(defs.clone());
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

    let engagement = crate::assets::page_engagement(caller, conn.executor(), &window)
        .await
        .map_err(|_| Failure::Internal)?;

    let mut items = Vec::with_capacity(window.len());
    for asset_id in &window {
        // Per id rather than one `IN` query, because the ranking is the order and a set query returns rows
        // in whatever order it likes. The window is at most 200 rows.
        if let Some(found) = assets::detail(conn.executor(), &caller.predicate, *asset_id).await? {
            items.push(crate::assets::summary_with_engagement(
                &found.summary,
                &engagement,
            ));
        }
    }
    // A cost guard rather than a behaviour one, and worth saying so before somebody simplifies it away
    // expecting a difference: `nearer_query` refuses to suggest a value that already exists, so a page with
    // results would come back with `None` anyway. What this saves is the lookup — a query per search that
    // could not produce an answer.
    let did_you_mean = if total == 0 {
        nearer_query(conn.executor(), caller, &planned, &defs, &params.q).await?
    } else {
        None
    };
    conn.commit().await?;

    Ok(AssetPage {
        items,
        // The number of *ranked and visible* results, which is not the library total and is capped by the
        // overfetch depth. A grid uses it for `aria-rowcount`; it is honest about being a ranked-result count
        // rather than pretending to be exhaustive.
        total,
        offset,
        ranked: true,
        did_you_mean,
    })
}

/// A query worth trying instead, for a search that matched nothing (Q.17).
///
/// Only for the shape where a suggestion can be *checked*: exactly one clause comparing a field to a literal
/// piece of text. That is the typo people actually make — `brand:acmee`, `client:northwnd` — and it is the only
/// one where a candidate can be looked up and confirmed to exist in the caller's own library before being
/// offered. Anything else returns `None`, because "no results" with no suggestion is an honest answer and a
/// guess sends somebody round a second empty loop.
///
/// The rewrite is textual, on the query the caller sent. The value came *from* that string, so replacing its
/// first occurrence puts the correction exactly where the clause was — and the caller gets back something they
/// can read and edit rather than a reconstruction of their query in the parser's own spelling.
async fn nearer_query(
    conn: &mut sqlx::PgConnection,
    caller: &caller::Caller,
    planned: &Planned,
    defs: &[dam_core::fields::FieldDef],
    asked: &str,
) -> Result<Option<String>, Failure> {
    let Some((key, typed)) = sole_text_equality(planned.query()) else {
        return Ok(None);
    };
    let Some(def) = defs.iter().find(|def| def.key == key) else {
        return Ok(None);
    };

    // Over the whole visible library rather than this query's results, which are empty by definition. Somebody
    // who typed `brand:acmee` is asking about the brands they can see.
    let everything = Planned::new(dam_core::query::Query::All, caller.predicate.clone(), defs)
        .map_err(|_| Failure::Internal)?;
    let Some(nearest) = dam_db::suggest::nearest_value(conn, &everything, def, &typed).await?
    else {
        return Ok(None);
    };
    if nearest.eq_ignore_ascii_case(&typed) {
        // The value already exists and the search still found nothing, so suggesting it back would send
        // somebody to the same empty page. Case-insensitively, which was tried the other way first: offering
        // the correctly-cased value looked like a kindness until the corrected query was run and came back
        // empty too, because an equality on a text field is answered by the *index* and a long value is a row
        // of tokens there rather than one term. A suggestion that leads to a second empty page is worse than
        // none.
        //
        // Hard to reach through the single-clause shape above — if a value is visible to this lookup it is
        // visible to the search — and kept because the alternative is a silent absurdity if that ever stops
        // being true.
        return Ok(None);
    }
    Ok(Some(asked.replacen(&typed, &nearest, 1)))
}

/// The one clause a value suggestion can be made for, or `None`.
///
/// Deliberately narrow. A conjunction of two field clauses has two candidates and no way to know which one was
/// mistyped, and offering both would be two queries neither of which the user asked for.
fn sole_text_equality(query: &dam_core::query::Query) -> Option<(String, String)> {
    use dam_core::query::{Comparison, Literal, Query as Q};
    match query {
        Q::Field {
            key,
            op: Comparison::Equals(Literal::Text(value)),
        } => Some((key.clone(), value.clone())),
        // A single-clause `and` is what a one-term query parses to in some shapes, so it is followed; anything
        // with two children is ambiguous and is left alone.
        Q::And(children) | Q::Or(children) if children.len() == 1 => {
            sole_text_equality(&children[0])
        }
        _ => None,
    }
}

/// Whether a query contains a clause the index cannot answer.
///
/// Category and collection membership are joins. Recursive because a relational clause nested inside an `and`,
/// an `or` or a `not` is still relational — and a check that only looked at the top level would send
/// `in:exterior harbour` to the index, where the category clause would be refused rather than answered.
fn is_relational(query: &dam_core::query::Query) -> bool {
    use dam_core::query::{Comparison, Query as Q};
    // No wildcard arm, deliberately: a new `Query` variant should not silently default to "the index can
    // answer this", because the consequence of being wrong in that direction is a refused clause rather than a
    // filtered one. `InCollection` is unreachable *through this endpoint* today — the shorthand has no
    // `collection:` selector, and saved searches already render through SQL — so it is listed as a statement
    // about the type rather than a branch this file's tests can reach.
    match query {
        // `Rating` is an aggregate over a table and `Mine` is per-caller: neither can be an index field, so both
        // route through SQL for the same reason taxonomy membership does.
        // Q.15's three join the list for the same reason: a status is a column, an orientation is derived from
        // two more, and an attachment is a row pointing back — none of them is in the index, and each would
        // have to be kept in step with what it duplicates if it were.
        Q::Term { .. }
        | Q::InCollection(_)
        | Q::Rating(_)
        | Q::Mine(_)
        | Q::Status(_)
        | Q::Orientation(_)
        | Q::HasAttachment
        | Q::Filename(_) => true,
        Q::And(children) | Q::Or(children) => children.iter().any(is_relational),
        Q::Not(inner) => is_relational(inner),
        // A substring or a prefix over a metadata field goes to SQL, which is where the only agreed answer
        // lives: the index refuses these by name because an `ILIKE` and a Tantivy automaton disagree at the
        // margins, and §12 forbids an approximate answer that differs between back ends. Found by running
        // `caption:*harbour*` through this endpoint after Q.16 added the syntax — the parser produced the
        // clause, the index refused it, and the caller got a 501 for a query the database can answer exactly.
        Q::Field {
            op: Comparison::Contains(_) | Comparison::StartsWith(_),
            ..
        } => true,
        Q::All | Q::Text(_) | Q::Field { .. } => false,
    }
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

    // Every facetable field, plus every taxonomy — each paired with the rail entry that names it, so the
    // tenant's own order can be applied below (Q.19). Which fields are facetable is the tenant's decision,
    // recorded on the field definition; a hard-coded list here would be a second schema.
    let mut requests: Vec<(String, dam_db::facets::FacetRequest)> = defs
        .iter()
        .filter(|def| def.facetable)
        .map(|def| {
            (
                format!("field:{}", def.key),
                dam_db::facets::FacetRequest::Field {
                    key: def.key.clone(),
                    limit: FACET_BUCKETS,
                },
            )
        })
        .collect();
    // Vocabularies only. A *category* tree has its own surface — a hierarchy with rollup counts — and
    // emitting it here as well rendered it twice in the rail, the second time as a flat list of leaves under a
    // heading that was the taxonomy's UUID, because a facet key is an id and no label can be derived from one.
    // Found by opening the page after the first real category tree existed.
    let taxonomies: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM taxonomies WHERE kind <> 'category' ORDER BY label")
            .fetch_all(conn.executor())
            .await
            .map_err(dam_db::Error::from)?;
    requests.extend(taxonomies.into_iter().map(|taxonomy_id| {
        (
            format!("taxonomy:{taxonomy_id}"),
            dam_db::facets::FacetRequest::Taxonomy {
                taxonomy_id,
                limit: FACET_BUCKETS,
            },
        )
    }));
    // The four every library has, whatever its schema (Q.15). Not configurable: none of them can be marked
    // facetable on a field definition, because none of them is a field — status is a column, orientation is
    // derived from two more, a rating is an aggregate over another table, and an attachment is a row pointing
    // back. A tenant who does not want one hides it in the rail, which is presentation.
    requests.extend(dam_db::facets::Builtin::ALL.into_iter().map(|builtin| {
        (
            format!("builtin:{}", builtin.key()),
            dam_db::facets::FacetRequest::Builtin(builtin),
        )
    }));

    // The tenant's own order, and the entries it has switched off (Q.19). Applied *before* counting rather
    // than after: a disabled facet that was counted and then dropped is three queries nobody reads, and on a
    // library where counting is the expensive part that is the whole cost of the feature.
    let configured = dam_db::rail::read(conn.executor()).await?;
    let requests = dam_db::rail::arrange(&requests, &configured);

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

/// What somebody is probably about to type (Q.17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Suggestion {
    /// `field`, `term` or `filename` — what the client groups by.
    pub source: String,
    /// What to show.
    pub label: String,
    /// The field key or taxonomy label it came from. Absent for a filename.
    pub within: Option<String>,
    /// The query fragment to insert. The client's job is to put a string in a box; a suggestion it had to
    /// assemble would be a second place where the query language is spoken and can be got wrong.
    pub fragment: String,
    /// How many visible assets carry it. The list is ordered by this within each source.
    pub count: i64,
}

/// Query parameters for a type-ahead.
#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct SuggestParams {
    /// The word being typed. Fewer than two characters returns nothing — one character is every value in the
    /// library, which is a list nobody reads and three queries to produce it.
    #[serde(default)]
    pub typed: String,
    /// The query already in the box, so suggestions narrow as the search narrows. Empty is the whole visible
    /// library.
    #[serde(default)]
    pub q: String,
}

/// Suggestions for a partially typed word, over the caller's visible library.
///
/// Access-filtered, and that is the point rather than a detail. A facet count needs a reader to infer
/// something from a number; a suggestion *names* the value, so offering one for an asset somebody cannot see
/// hands them the fact directly — which is the disclosure §7 is about.
#[utoipa::path(
    get,
    path = "/search/suggest",
    params(SuggestParams),
    responses(
        (status = 200, description = "What to offer, most common first within each source", body = Vec<Suggestion>),
        (status = 400, description = "The query already in the box does not parse", body = QueryProblem),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no read scope"),
    ),
    tag = "search",
)]
pub async fn suggest(
    State(state): State<Arc<SearchState>>,
    headers: HeaderMap,
    Query(params): Query<SuggestParams>,
) -> Result<Json<Vec<Suggestion>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // The query already in the box, so a type-ahead two clauses into a search offers what is left rather than
    // what the library holds — the same reason the facet rail counts over the current query.
    let (planned, defs) = plan(conn.executor(), &caller, &params.q).await?;
    let found =
        dam_db::suggest::for_prefix(conn.executor(), &planned, &defs, &params.typed).await?;
    conn.commit().await?;

    Ok(Json(
        found
            .into_iter()
            .map(|one| Suggestion {
                source: one.source.as_str().to_owned(),
                label: one.label,
                within: one.within,
                fragment: one.fragment,
                count: one.count,
            })
            .collect(),
    ))
}

/// How many rows an interactive export may carry (Q.18).
///
/// The same order of magnitude as the ranked search's own depth cap, because this export is "the results I am
/// looking at" rather than "everything that matches". A set larger than this is refused with its size rather
/// than truncated: a CSV that silently stops at ten thousand rows opens perfectly in a spreadsheet, and the
/// person who re-imports it never learns that thirty thousand assets were left out.
pub const EXPORT_MAX: i64 = 10_000;

/// The caller's current search as a CSV (Q.18).
///
/// **Answered in SQL, always, even for a query the index would rank.** An export is a *set*, not a ranking: it
/// is a file somebody re-imports, audits, or hands to a client, and the two failure modes of the ranked path
/// are both silent omission. The index is eventually consistent, so an asset edited a moment ago may not be in
/// it; and the ranked path's total is capped by the overfetch depth, so a large set cannot even be measured
/// there — which is how the first version of this endpoint came to export nothing at all from an index that had
/// never been built.
///
/// The cost is stated rather than hidden: for a free-text query, SQL matches substrings where the index matches
/// tokens, so a text export can differ slightly from the ranked grid it was taken from. Every structured query
/// — a field, a facet click, a category, a filename — is identical in both. Completeness is the property an
/// export needs, and a file that quietly omits rows is worse than one that includes a near-miss.
///
/// Read scope. An export of metadata somebody can already read is not a disclosure — see `orders::metadata_csv`
/// on why the same file is not offered to an unauthenticated recipient.
#[utoipa::path(
    get,
    path = "/search/export.csv",
    params(SearchParams),
    responses(
        (status = 200, description = "text/csv", content_type = "text/csv"),
        (status = 400, description = "The query does not parse", body = QueryProblem),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no read scope"),
        (status = 422, description = "The result set is larger than an interactive export carries; the body says how large"),
    ),
    tag = "search",
)]
pub async fn export(
    State(state): State<Arc<SearchState>>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<axum::response::Response, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // The same parse and the same predicate as `/search`, so a query that is refused there is refused here with
    // the same sentence. `offset` is ignored: an export of "page 3" is a file nobody asked for.
    let (planned, _defs) = plan(conn.executor(), &caller, &params.q).await?;

    // Paged, because a page is capped at `assets::MAX_LIMIT` and asking for ten thousand rows in one call
    // silently returns five hundred. That was the first version of this: a file of exactly 500 rows, which
    // opens perfectly and is wrong in the one way an export must never be.
    let mut ids: Vec<uuid::Uuid> = Vec::new();
    let mut offset = 0i64;
    loop {
        let page = assets::page_matching(
            conn.executor(),
            &planned,
            dam_db::assets::Order::Newest,
            offset,
            dam_db::assets::MAX_LIMIT,
        )
        .await?;
        if offset == 0 && page.total > EXPORT_MAX {
            conn.commit().await?;
            return Err(Failure::TooLarge(format!(
                "that search matches {} assets and an export carries {EXPORT_MAX}; narrow the query",
                page.total
            )));
        }
        let fetched = page.items.len();
        ids.extend(page.items.iter().map(|item| item.id));
        // A short page is the last page. The `>=` guard is the belt to that brace: a set that grows under the
        // loop must not turn an export into an unbounded read.
        if fetched < usize::try_from(dam_db::assets::MAX_LIMIT).unwrap_or(usize::MAX)
            || i64::try_from(ids.len()).unwrap_or(i64::MAX) >= EXPORT_MAX
        {
            break;
        }
        offset += dam_db::assets::MAX_LIMIT;
    }
    let fields = dam_db::fields::load(conn.executor()).await?;
    let rows: Vec<crate::csv_export::Row> = sqlx::query_as(crate::csv_export::SELECT)
        .bind(&ids)
        .fetch_all(conn.executor())
        .await
        .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    // `ids` decides the row order, so the file reads in the same order as the grid's default. The rows were
    // fetched over ids the access-filtered query produced, which is what makes a second check here unnecessary
    // rather than forgotten.
    let document = crate::csv_export::document(&fields, &rows, &ids);
    Ok((crate::csv_export::headers("search-results.csv"), document).into_response())
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
    // `search_schema_on` rather than assembling here: it also loads the tenant's category paths, which is
    // what lets `in:exterior.yellow` resolve while parsing. Assembling the two halves separately is how a
    // fresh alias gets paired with a stale field list.
    let parse_schema = dam_db::fields::search_schema_on(&mut *conn).await?;

    let parsed = if q.trim().is_empty() {
        dam_core::query::Query::All
    } else {
        shorthand::parse(q, &parse_schema).map_err(|e| {
            Failure::BadQuery(QueryProblem {
                message: e.detail.clone(),
                code: Some(e.code.to_owned()),
                at: Some(e.column),
                suggestion: e.suggestion.clone(),
            })
        })?
    };

    // `Planned::new` is the only constructor and it takes the predicate, which is §7/§12 expressed in the
    // type system: there is no value of this type without an access filter, so no renderer can omit one.
    //
    // The viewer is named here too, because `is:favourite` is about whoever is asking. It goes beside the
    // predicate rather than into the parsed tree deliberately: a saved search stores the tree, so an identity
    // baked into it would make a shared search return the author's favourites.
    let planned = Planned::new(parsed, caller.predicate.clone(), &defs).map_err(|rejections| {
        Failure::BadQuery(QueryProblem {
            message: rejections
                .iter()
                .map(|r| format!("{}: {}", r.key, r.detail))
                .collect::<Vec<_>>()
                .join("; "),
            code: rejections.first().map(|r| r.code.to_owned()),
            at: None,
            // Validation refusals are about a comparison being wrong for a kind, not about a name being
            // misspelled — there is nothing to suggest, and inventing something would be noise.
            suggestion: None,
        })
    })?;
    let planned = match caller.identity_id {
        Some(identity) => planned.viewed_by(identity),
        // Unreachable through `authorize`, which refuses a key with no identity. Left unnamed rather than
        // defaulted, so a personal clause fails loudly instead of quietly matching nothing.
        None => planned,
    };
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
    /// A request this endpoint will not answer at this size — an export larger than `EXPORT_MAX` (Q.18). The
    /// body carries the count, because "too many" without a number is not something a caller can act on.
    TooLarge(String),
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
            Self::TooLarge(message) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({"message": message})),
            )
                .into_response(),
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
                // Nothing was misspelled: the clause is well formed and the index cannot answer it.
                suggestion: None,
            }),
            other => {
                tracing::error!(error = %other, "search index error");
                Self::Internal
            }
        }
    }
}
