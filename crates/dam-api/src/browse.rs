//! The asset picker's endpoint (M3d·3, §11.1).
//!
//! ## One call, because a picker is one dialog
//!
//! Results and the facet rail together. Two calls would be two round trips before an editor sees anything, and
//! would let the rail's counts disagree with the grid beside them — the same argument `/dashboard` makes about
//! its three parts.
//!
//! ## Two ways in, one scope
//!
//! A **server-side** caller uses the connector's API key: Drupal's module fetching results in PHP. A
//! **browser-side** picker uses a short-lived token the site signed itself with the secret it already holds
//! (`dam_connect::browse_token`), because putting a long-lived API key in JavaScript hands it to every editor
//! and every page the picker is embedded in.
//!
//! Both resolve through `caller::authorize_as`, so grants, predicate compilation and both of `authorize`'s
//! guards are the same code. A connector-shaped scope resolver would be a second place access is decided.
//!
//! ## CORS is per connector, never a wildcard
//!
//! The allowed origin is the connector's own `site_url`, which is what that column is for. A wildcard would
//! make this endpoint readable from any page that could obtain a token, and the token is the thing most likely
//! to leak — it lives in a browser.
//!
//! Only the token path gets CORS headers. A cross-origin request carrying an `Authorization` header would be a
//! site putting its API key in a browser, and answering it would be endorsing that.
//!
//! ## It is a picker, not a second search API
//!
//! No export, no saved searches, no `did_you_mean` chasing. What it adds over `/search` is being answerable with
//! a token, aggregated, and CORS-enabled; everything about *how* results are found is `search::run`, unchanged.

use crate::caller::{self, Authorized, Caller};
// `search::Failure` rather than `assets::Failure`, and it earns the choice: a picker has a search box, so a
// query that does not parse has to come back as the 400 that names the column it stopped at. An `assets`
// failure would flatten that into "bad request".
use crate::search::{Facet, Failure, SearchParams, SearchState};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::connectors::Connector;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

/// What the picker needs beyond the search router: a way to open connector secrets.
pub struct BrowseState {
    pub search: Arc<SearchState>,
    pub global: PgPool,
    /// Opens a connector's sealed signing secret, so a token it signed can be verified. `None` disables the
    /// token path entirely — fail-closed, exactly as the delivery route's does.
    pub connectors: Option<ConnectorAuth>,
}

/// The keyring and the tenant a connector's secret is sealed against.
#[derive(Clone)]
pub struct ConnectorAuth {
    pub sealing: dam_core::sealed::SealingKeyring,
    pub tenant_slug: dam_core::TenantSlug,
}

impl std::fmt::Debug for BrowseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowseState").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ConnectorAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectorAuth").finish_non_exhaustive()
    }
}

pub fn router(state: BrowseState) -> Router {
    Router::new()
        .route("/browse", get(browse).options(preflight))
        .with_state(Arc::new(state))
}

/// What a picker draws.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BrowseResult {
    pub results: crate::dto::AssetPage,
    /// The rail, counted over the same query — so a facet saying 40 and a grid showing 3 cannot happen.
    pub facets: Vec<Facet>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct BrowseParams {
    /// The shorthand query. Empty lists the library, newest first, which is what a picker opens on.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    /// A token the site signed itself, for a browser-side picker. Omit it and the `Authorization` header is
    /// used instead.
    #[serde(default)]
    pub token: Option<String>,
}

#[utoipa::path(
    get,
    path = "/browse",
    params(BrowseParams),
    responses(
        (status = 200, body = BrowseResult),
        (status = 401, description = "No usable credential, or a token that does not verify"),
        (status = 403, description = "Authenticated, and holds no read scope"),
    ),
    tag = "connectors",
)]
pub async fn browse(
    State(state): State<Arc<BrowseState>>,
    headers: HeaderMap,
    Query(params): Query<BrowseParams>,
) -> Result<Response, Failure> {
    let (caller, connector) = authorize_browser(&state, &headers, params.token.as_deref()).await?;

    let search = SearchParams {
        q: params.q.clone(),
        // The picker's own defaults, not the search API's: a dialog shows a grid of pictures and 60 is one
        // scroll, where `/search`'s 50 is a page of a list.
        limit: params.limit.unwrap_or(60),
        offset: params.offset.unwrap_or(0),
    };
    let results = crate::search::run(&state.search, &caller, &search).await?;
    let facets = crate::search::facets_for(&state.search, &caller, &params.q).await?;

    let body = Json(BrowseResult { results, facets });
    Ok(match connector {
        // Only the token path is answerable cross-origin. See the module docs.
        Some(connector) => (cors(&connector, &headers), body).into_response(),
        None => body.into_response(),
    })
}

/// The two ways a picker authenticates, resolved once.
///
/// Shared with the oEmbed provider, which has exactly the same two callers — a module fetching server-side with
/// the API key, and a browser-side plugin with a token the site signed. Two copies of this would be two places
/// a connector's identity is established, and one of them would eventually forget to check whether the key was
/// revoked.
pub async fn authorize_browser(
    state: &BrowseState,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<(Caller, Option<Connector>), Failure> {
    match token {
        Some(token) => {
            let (caller, connector) = by_token(state, token).await?;
            Ok((caller, Some(connector)))
        }
        // No token: the ordinary bearer path.
        None => Ok((
            caller::authorize(&state.global, headers, Action::Read).await?,
            None,
        )),
    }
}

/// Answers a preflight for the token path.
///
/// A browser sends this before a cross-origin `GET` with a non-simple header or a non-simple method — and the
/// picker's fetch has neither, so strictly this is not always needed. Present anyway because "sometimes needed"
/// is how an integration works in development and fails once somebody adds a header.
///
/// The connector is read from the *query* here, because a preflight carries no credential at all: it is the
/// browser asking whether it may make the real request. So this says which origins are allowed to try, and the
/// real request still has to present a token that verifies.
pub async fn preflight(
    State(state): State<Arc<BrowseState>>,
    headers: HeaderMap,
    Query(params): Query<BrowseParams>,
) -> Response {
    let Some(token) = params.token.as_deref() else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let Some(id) = dam_connect::browse_token::connector_of(token) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    // Unverified, deliberately, and it grants nothing: the answer is a set of headers saying which origin may
    // *attempt* the real request. A forged token gets a preflight it cannot then use.
    match connector_row(&state, id).await {
        Ok(Some(connector)) => (StatusCode::NO_CONTENT, cors(&connector, &headers)).into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Resolves a browse token into a caller, through the ordinary scope machinery.
async fn by_token(state: &BrowseState, token: &str) -> Result<(Caller, Connector), Failure> {
    // Every failure below is the same 401. Distinguishing "no such connector", "revoked", "bad signature" and
    // "expired" would tell whoever holds the token which connectors exist and what state they are in — the same
    // reasoning the delivery route's flat 404 rests on.
    let refused = || Failure::Refused(caller::Refusal::Unauthorized);

    let auth = state.connectors.as_ref().ok_or_else(|| {
        tracing::error!("a browse token arrived with no connector auth configured");
        refused()
    })?;
    let id = dam_connect::browse_token::connector_of(token).ok_or_else(refused)?;
    let connector = connector_row(state, id).await?.ok_or_else(refused)?;
    if !connector.status.may_render() {
        return Err(refused());
    }

    let aad = dam_db::connectors::associated_data(auth.tenant_slug.as_str(), connector.id);
    let now = chrono::Utc::now();
    let mut secrets = Vec::with_capacity(2);
    if let Ok(current) = auth.sealing.open(&connector.sealed_secret, &aad) {
        secrets.push(current);
    }
    // The superseded secret while it is inside its window, so a rotation does not close an open picker.
    if let Some(previous) = connector.live_previous(now)
        && let Ok(opened) = auth.sealing.open(previous, &aad)
    {
        secrets.push(opened);
    }
    dam_connect::browse_token::verify(secrets.iter(), token, now).map_err(|reason| {
        tracing::debug!(?reason, connector = %connector.id, "browse token rejected");
        refused()
    })?;

    // The connector's own key and identity, so the scope is exactly what a server-side call with that key would
    // get. A revoked or expired key stops the token too — otherwise revoking a credential would leave minted
    // tokens working for their full lifetime.
    let api_key_id = connector.api_key_id.ok_or_else(refused)?;
    let row: Option<(Option<uuid::Uuid>, uuid::Uuid, Vec<String>)> = sqlx::query_as(
        "SELECT k.identity_id, k.tenant_id, coalesce(m.role_names, '{}') \
         FROM dam_global.api_keys k \
         LEFT JOIN dam_global.tenant_members m \
                ON m.identity_id = k.identity_id AND m.tenant_id = k.tenant_id \
         WHERE k.id = $1 AND k.revoked_at IS NULL \
           AND (k.expires_at IS NULL OR k.expires_at > now())",
    )
    .bind(api_key_id)
    .fetch_optional(&state.global)
    .await
    .map_err(dam_db::Error::from)?;
    let Some((Some(identity_id), tenant_id, role_names)) = row else {
        return Err(refused());
    };

    let caller = caller::authorize_as(
        &state.global,
        &Authorized {
            tenant_id,
            tenant_slug: auth.tenant_slug.clone(),
            identity_id,
            api_key_id,
            // None. A browse token narrows nothing and widens nothing — see `browse_token`.
            scopes: Vec::new(),
            role_names,
        },
        Action::Read,
    )
    .await?;
    Ok((caller, connector))
}

async fn connector_row(state: &BrowseState, id: uuid::Uuid) -> Result<Option<Connector>, Failure> {
    let auth = state.connectors.as_ref();
    let slug = match auth {
        Some(auth) => auth.tenant_slug.clone(),
        None => return Ok(None),
    };
    let mut conn = dam_db::TenantConn::begin(&state.global, &slug).await?;
    let found = dam_db::connectors::by_id(conn.executor(), id).await?;
    conn.commit().await?;
    Ok(found)
}

/// The CORS headers for one connector, and only when the request's origin is that connector's own.
///
/// A mismatch produces no headers at all rather than a refusal: the browser will block the read, which is the
/// correct outcome and the one CORS is for. Answering 403 would tell a page it guessed the wrong origin.
fn cors(connector: &Connector, headers: &HeaderMap) -> [(header::HeaderName, HeaderValue); 3] {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let allowed = origin == connector.site_url.trim_end_matches('/');
    let value = if allowed {
        HeaderValue::from_str(origin).unwrap_or_else(|_| HeaderValue::from_static("null"))
    } else {
        // `null` rather than the connector's URL: echoing an origin the caller did not send would be a header
        // that looks permissive and matches nothing, which is harder to debug than a plain refusal to allow.
        HeaderValue::from_static("null")
    };
    [
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, value),
        (
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, OPTIONS"),
        ),
        // The token is in the query string, so no request header is needed and none is allowed. A picker that
        // wanted to send `Authorization` cross-origin is a picker holding a long-lived key.
        (
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        ),
    ]
}
