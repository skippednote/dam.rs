//! The oEmbed provider (M3d·3, §11.1).
//!
//! ## Authenticated, which the oEmbed spec does not contemplate
//!
//! oEmbed assumes a public provider: a consumer fetches `/oembed?url=…` with no credential and gets back a
//! description of a public resource. A governed asset library cannot work that way — an unauthenticated
//! endpoint that turns an asset id into a filename, a size and a preview URL is an enumeration API for the
//! whole library, and §7's rule that a count is a disclosure applies rather more strongly to a picture.
//!
//! So this takes the connector's API key or a token it signed, exactly as `/browse` does. That is a deliberate
//! deviation from the spec and it costs nothing in practice: CKEditor's oEmbed fetch happens in Drupal's
//! server-side code, which holds the key.
//!
//! ## The URL it takes is the one damrs would show a person
//!
//! `<public_url>/assets/<id>`, because that is what an editor pastes. Parsing it rather than accepting a bare
//! id is the whole point of oEmbed — a consumer discovers the provider from the URL and hands it back
//! unchanged.
//!
//! ## `cache_age` is below the delivery URL's lifetime, deliberately
//!
//! The `url` in the response is a signed delivery token, so it expires. An oEmbed consumer that cached the
//! response for a day would serve a broken image for most of it. Reporting a `cache_age` shorter than the token's
//! own TTL is what makes the two agree, and it is the one field a caching consumer actually acts on.
//!
//! ## Photos are photos; everything else is a link
//!
//! `type: photo` needs a URL that *is* an image, which is true for a rendition of an image and false for a
//! video, a PDF or an audio file. Claiming `video` would require an embeddable player this does not have, so
//! anything that is not an image comes back as `link` with a thumbnail — honest, and still useful to an editor
//! who wanted a card rather than an inline image.

use crate::caller::Caller;
use crate::search::QueryProblem;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dam_core::rights_eval::Usage;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// How long the delivery URL in a response is good for.
///
/// Fifteen minutes. Long enough for a consumer to fetch the bytes and cache them itself, short enough that a
/// response copied out of a log is worth little.
const URL_TTL_MINUTES: i64 = 15;

/// What a consumer may cache the response for, in seconds.
///
/// Comfortably below the URL's own lifetime. A consumer that honours this refetches before the token expires;
/// one that ignores it gets a broken image, which is its own fault and not something a longer number would fix.
const CACHE_AGE_SECONDS: u32 = 600;

/// Everything that can go wrong answering an oEmbed request.
///
/// Its own type rather than `search::Failure`, because oEmbed's statuses are part of its spec and differ from
/// this codebase's usual mapping: **404** for a URL the provider does not recognise, and **501** for a format
/// it will not emit. A consumer implements against those, so flattening them into a generic 400 would make a
/// correct consumer look broken.
#[derive(Debug)]
pub enum Failure {
    Refused(crate::caller::Refusal),
    /// A URL this provider does not answer for, or an asset the caller cannot see. **One answer for both**, as
    /// everywhere else: distinguishing them would confirm an asset exists to somebody who cannot see it.
    NotFound,
    /// A format this provider does not emit. 501 is the spec's status for it.
    UnsupportedFormat(QueryProblem),
    /// A URL that is not one of ours at all. 400, because the consumer sent something malformed for *this*
    /// provider rather than asking about a resource that might exist.
    NotOurs(QueryProblem),
    Internal,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Refused(refusal) => refusal.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::UnsupportedFormat(problem) => {
                (StatusCode::NOT_IMPLEMENTED, Json(problem)).into_response()
            }
            Self::NotOurs(problem) => (StatusCode::BAD_REQUEST, Json(problem)).into_response(),
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<crate::caller::Refusal> for Failure {
    fn from(refusal: crate::caller::Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<dam_db::Error> for Failure {
    fn from(error: dam_db::Error) -> Self {
        tracing::error!(%error, "answering an oembed request");
        Self::Internal
    }
}

impl From<crate::search::Failure> for Failure {
    fn from(failure: crate::search::Failure) -> Self {
        match failure {
            crate::search::Failure::Refused(refusal) => Self::Refused(refusal),
            other => {
                tracing::error!(
                    ?other,
                    "an oembed request failed inside the browse authoriser"
                );
                Self::Internal
            }
        }
    }
}

pub struct OembedState {
    pub browse: Arc<crate::browse::BrowseState>,
    /// Mints the delivery URL. Without it a response carries no `url` at all rather than an unsigned one — the
    /// same choice the search results make about thumbnails.
    pub delivery: Option<Arc<crate::delivery::DeliveryState>>,
    /// The origin an asset URL is expected to be under, and the `provider_url` in the response.
    pub public_url: Option<String>,
}

impl std::fmt::Debug for OembedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OembedState").finish_non_exhaustive()
    }
}

pub fn router(state: OembedState) -> Router {
    Router::new()
        .route("/oembed", get(oembed))
        .with_state(Arc::new(state))
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct OembedParams {
    /// The asset's page URL, as an editor pasted it: `<origin>/assets/<id>`.
    pub url: String,
    /// The consumer's maximum width. Used to choose a rendition, not to scale one.
    #[serde(default)]
    pub maxwidth: Option<u32>,
    #[serde(default)]
    pub maxheight: Option<u32>,
    /// `json` only. XML is in the spec and nothing has asked for it; a stub returning JSON under an XML content
    /// type would be worse than a refusal.
    #[serde(default)]
    pub format: Option<String>,
    /// A token the site signed, for a browser-side editor plugin. Omit it and the `Authorization` header is used.
    #[serde(default)]
    pub token: Option<String>,
}

/// An oEmbed response.
///
/// Field names are the spec's, including the snake_case ones, because a consumer reads them by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Oembed {
    /// `photo` for an image, `link` for anything else. Never `video` — see the module docs.
    #[serde(rename = "type")]
    pub kind: String,
    pub version: String,
    pub title: String,
    pub provider_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_url: Option<String>,
    /// The image itself, for a `photo`. A signed delivery URL, so it expires — see `cache_age`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_height: Option<u32>,
    /// Seconds a consumer may cache this. Below the `url`'s own lifetime, deliberately.
    pub cache_age: u32,
}

#[utoipa::path(
    get,
    path = "/oembed",
    params(OembedParams),
    responses(
        (status = 200, body = Oembed),
        (status = 400, description = "Not an asset URL from this library, or a format other than json"),
        (status = 401, description = "No usable credential"),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "connectors",
)]
pub async fn oembed(
    State(state): State<Arc<OembedState>>,
    headers: HeaderMap,
    Query(params): Query<OembedParams>,
) -> Result<Json<Oembed>, Failure> {
    // `json` or nothing. The spec allows a consumer to ask for XML, and answering with JSON under an XML
    // content type would be a lie a consumer cannot detect.
    if let Some(format) = params.format.as_deref()
        && !format.eq_ignore_ascii_case("json")
    {
        return Err(Failure::UnsupportedFormat(QueryProblem {
            message: format!("{format:?} is not a format this provider emits; only json"),
            code: Some("unsupported_format".to_owned()),
            at: None,
            suggestion: None,
        }));
    }

    // The same two ways in as `/browse`, resolved by the same function: an API key for a server-side fetch, or
    // a token the site signed for a browser-side editor plugin.
    let (caller, _connector) =
        crate::browse::authorize_browser(&state.browse, &headers, params.token.as_deref()).await?;

    let asset_id = asset_id_from(&params.url, state.public_url.as_deref()).ok_or_else(|| {
        Failure::NotOurs(QueryProblem {
            message: format!(
                "{:?} is not an asset URL from this library; expected <origin>/assets/<id>",
                params.url
            ),
            code: Some("not_an_asset_url".to_owned()),
            at: None,
            suggestion: None,
        })
    })?;

    describe(&state, &caller, asset_id, &params).await.map(Json)
}

/// Pulls the asset id out of a page URL.
///
/// The path has to end `/assets/<uuid>` and, when the deployment knows its own origin, the origin has to
/// match. Checking the origin is not pedantry: a consumer that pasted another provider's URL should be told
/// so, and accepting any URL that happens to contain a uuid would make this endpoint answer for resources it
/// knows nothing about.
#[must_use]
pub fn asset_id_from(url: &str, public_url: Option<&str>) -> Option<Uuid> {
    let parsed = url::Url::parse(url).ok()?;
    if let Some(expected) = public_url
        && let Ok(expected) = url::Url::parse(expected)
        && (parsed.host_str() != expected.host_str() || parsed.scheme() != expected.scheme())
    {
        return None;
    }
    let mut segments = parsed.path_segments()?.filter(|part| !part.is_empty());
    let first = segments.next()?;
    let second = segments.next()?;
    if first != "assets" || segments.next().is_some() {
        return None;
    }
    Uuid::parse_str(second).ok()
}

async fn describe(
    state: &OembedState,
    caller: &Caller,
    asset_id: Uuid,
    params: &OembedParams,
) -> Result<Oembed, Failure> {
    let mut conn = dam_db::TenantConn::begin(&state.browse.global, &caller.tenant_slug).await?;
    // Through the caller's predicate, so an asset the connector may not see is absent rather than described.
    let found = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id).await?;
    conn.commit().await?;
    // 404 through `Refused`, the same answer a caller gets for an id that never existed.
    let asset = found.ok_or(Failure::NotFound)?;

    let is_image = asset.summary.mime.starts_with("image/");
    let profile = rendition_for(params.maxwidth, params.maxheight);

    let (url, width, height) = match (&state.delivery, is_image) {
        (Some(delivery), true) => {
            let minted = crate::delivery::issue(
                delivery,
                crate::delivery::Scope {
                    tenant_id: caller.tenant_id,
                    slug: &caller.tenant_slug,
                },
                asset_id,
                profile.name,
                // The connector's own channel and territory would be better, and there is nowhere to put them:
                // oEmbed has no parameter for either. `web`/`WORLD` is what a page embed is, and the rights
                // check at delivery is the thing that decides — as always.
                &Usage {
                    channel: "web".to_owned(),
                    territory: "WORLD".to_owned(),
                },
                Some(caller.identity_id),
                chrono::Duration::minutes(URL_TTL_MINUTES),
                delivery.now(),
            )
            .await;
            match minted {
                Ok(token) => (
                    Some(delivery.url_for(&token)),
                    Some(profile.rendition.width),
                    Some(profile.rendition.height),
                ),
                // **A `link`, not a 500.** Minting refuses for reasons that are ordinary states rather than
                // faults: the rendition has not been rendered yet (a fresh upload, a reindex), or the rights
                // check says no for this channel. Either way there is a real asset with a real title, and a
                // consumer that pasted its URL is better served by a card than by a server error it can do
                // nothing about.
                //
                // The refusal is logged rather than swallowed, because "every oEmbed came back as a link"
                // needs to be findable.
                Err(refusal) => {
                    tracing::debug!(
                        ?refusal,
                        %asset_id,
                        profile = profile.name,
                        "no oembed photo url; answering as a link",
                    );
                    (None, None, None)
                }
            }
        }
        // No delivery configured, or not an image. Either way there is no `url`, and the response says `link`
        // rather than claiming to be a photo with nothing to show.
        _ => (None, None, None),
    };

    Ok(Oembed {
        kind: if url.is_some() { "photo" } else { "link" }.to_owned(),
        version: "1.0".to_owned(),
        title: asset.summary.filename.clone(),
        // The library's own name would be better and lives in `branding`; this is the vendor default until an
        // oEmbed consumer has a reason to show it. Named here rather than left blank because the field is
        // required by the spec.
        provider_name: "damrs".to_owned(),
        provider_url: state.public_url.clone(),
        url,
        width,
        height,
        thumbnail_url: None,
        thumbnail_width: None,
        thumbnail_height: None,
        cache_age: CACHE_AGE_SECONDS,
    })
}

/// The smallest built-in rendition that satisfies the consumer's maximum.
///
/// Chosen rather than scaled: the renditions are the tenant's, already rendered and already cached, and
/// generating an arbitrary size per oEmbed request would put a render in the path of pasting a URL. A consumer
/// asking for something smaller than the smallest gets the smallest, which is the honest answer — oEmbed's
/// `maxwidth` is a maximum for *layout*, and a consumer that cannot scale down an image has other problems.
fn rendition_for(
    maxwidth: Option<u32>,
    maxheight: Option<u32>,
) -> &'static dam_media::profiles::Profile {
    let cap = [maxwidth, maxheight]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(u32::MAX);
    const LADDER: [&dam_media::profiles::Profile; 3] = [
        &dam_media::profiles::THUMB_256,
        &dam_media::profiles::PREVIEW_1024,
        &dam_media::profiles::WEB_2048,
    ];
    // The largest that fits, else the smallest there is. A consumer asking for less than 256 gets 256 rather
    // than nothing: `maxwidth` is a layout maximum, and a consumer that cannot scale an image down has other
    // problems.
    LADDER
        .into_iter()
        .rev()
        .find(|profile| profile.rendition.width <= cap)
        .unwrap_or(LADDER[0])
}
