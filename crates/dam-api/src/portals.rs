//! Portals: a named, branded share of a set, for people without accounts (Q.14).
//!
//! ## What a portal adds, and what it deliberately does not
//!
//! It adds a name, a look and a set. It adds **no access mechanism at all**: every visit resolves the portal's
//! share link through `dam_db::shares::resolve` and every preview and download goes through the delivery
//! chokepoint, exactly as the single-asset share and the order pickup do. That is the whole reason a portal is
//! cheap to trust — there is no second answer to "may this person have these bytes".
//!
//! Acquia's four types (Standard, Brand, Video, Channel) are presentation. `kind` chooses a layout and, for
//! video and channel, a media-class filter over the same set. None of them changes who may see or take what, and
//! the naming invites the opposite assumption strongly enough to be worth repeating here.
//!
//! ## Only a collection, for now
//!
//! The schema anticipates three sources — a collection, a saved search, a media class — and this API accepts one.
//! A collection is *explicit*: somebody with Manage put each asset in it, which is a decision about publication.
//! A saved search is a **live query**, and a portal backed by one would make every future asset that happens to
//! match it anonymously visible without anybody deciding. That is an access-control question ARCHITECTURE does
//! not settle (§ "Whether Portals ships at all" is a product question; this is a policy one), so it is written up
//! in NEEDS-REVIEW.md and refused here rather than guessed at.
//!
//! ## Searching inside a portal narrows
//!
//! `allow_search` lets a visitor filter what they were given: the term is matched against the filenames and
//! metadata of the portal's own set. It cannot reach outside it — the set is the outer bound, and the query is a
//! filter over the rows that were already going to be listed.

use crate::assets::Failure;
use crate::caller;
// The visitor routes answer in the share portal's vocabulary — flat 404s that say nothing about what a dead
// link used to be, 401 for a passcode — because a portal visitor is a share recipient. The admin routes answer
// in the asset one. Two failure types in one module, and the alternative is one of the two audiences getting
// answers shaped for the other.
use crate::delivery::{self, DeliveryState};
use crate::shares::Failure as VisitorFailure;
use crate::shares::PortalItem;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use dam_core::policy::Action;
use dam_core::rights_eval::Usage;
use dam_db::portals::{self, Kind, NewPortal, Portal, PortalRefusal, Presentation, Source};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// How long a portal preview URL lives. The same short window the share portal uses: the page re-mints on
/// reload, and the token — not this URL — is what the visitor keeps.
const PREVIEW_TTL: Duration = Duration::minutes(15);

/// The most assets one portal page returns.
///
/// A portal is a page somebody reads, not a bulk endpoint: past this the request is a scrape and the reader has
/// scrolled past anything they were going to look at.
const MAX_ITEMS: i64 = 120;

/// What the portal endpoints need.
pub struct PortalState {
    pub global: PgPool,
    /// The delivery state, for the same reason the share portal holds it: previews are signed here and verified
    /// there, and two keyrings would mean tokens that fail verification.
    pub delivery: Arc<DeliveryState>,
}

impl std::fmt::Debug for PortalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalState").finish_non_exhaustive()
    }
}

/// The portal routes: administration and the public page together, so the router is the complete list of what a
/// portal exposes.
pub fn router(state: PortalState) -> Router {
    Router::new()
        .route("/portals", post(create).get(list))
        .route("/portals/{id}", patch(present).delete(retire))
        // The public pages. Two ways in, one enforcement path: a slug for a portal that has been made public,
        // and the share token for every portal.
        .route("/portal/{key}", get(by_key))
        .route("/share/{token}/portal", post(by_token))
        .with_state(Arc::new(state))
}

/// One portal, as an administrator sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PortalView {
    pub id: Uuid,
    /// The slug. A URL only when `is_public` — see the module note.
    pub key: String,
    pub title: String,
    pub intro: String,
    pub kind: String,
    pub collection_id: Option<Uuid>,
    pub logo_asset_id: Option<Uuid>,
    pub accent: String,
    pub is_public: bool,
    pub allow_search: bool,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
    /// Whether a live share link still reaches it. False on a retired portal, and on one whose link somebody
    /// revoked by hand — which is a state worth being able to see rather than inferring from silence.
    pub reachable: bool,
}

impl PortalView {
    fn of(portal: Portal, reachable: bool) -> Self {
        Self {
            collection_id: match &portal.source {
                Source::Collection(id) => Some(*id),
                _ => None,
            },
            id: portal.id,
            key: portal.key,
            title: portal.title,
            intro: portal.intro,
            kind: portal.kind,
            logo_asset_id: portal.logo_asset_id,
            accent: portal.accent,
            is_public: portal.is_public,
            allow_search: portal.allow_search,
            created_at: portal.created_at,
            retired_at: portal.retired_at,
            reachable,
        }
    }
}

/// A portal to create.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NewPortalRequest {
    /// The URL name: lower-case letters, digits and hyphens.
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub intro: String,
    /// `standard`, `brand`, `video` or `channel`. Presentation only.
    #[serde(default = "standard")]
    pub kind: String,
    /// The collection whose members the portal shows.
    pub collection_id: Option<Uuid>,
    /// A saved search as the source. Accepted by the schema, refused by this build — see the module note and
    /// NEEDS-REVIEW.md. Named in the request rather than absent from it, so asking gets an answer about the
    /// decision instead of "missing field `collection_id`".
    pub saved_search_id: Option<Uuid>,
    /// A media class ("every video") as the source. Refused for the same reason.
    pub media_class: Option<String>,
    pub logo_asset_id: Option<Uuid>,
    #[serde(default = "default_accent")]
    pub accent: String,
    /// Whether the slug resolves. False means the portal is reachable only by its token.
    #[serde(default)]
    pub is_public: bool,
    #[serde(default = "yes")]
    pub allow_search: bool,
    /// The access half, handed straight to the share machinery.
    pub passcode: Option<String>,
    pub expires_in_days: Option<i64>,
    pub max_downloads: Option<i32>,
    #[serde(default)]
    pub allow_original: bool,
}

fn standard() -> String {
    "standard".to_owned()
}

fn default_accent() -> String {
    "#2563eb".to_owned()
}

fn yes() -> bool {
    true
}

/// What creation produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreatedPortal {
    pub portal: PortalView,
    /// The token URL, readable exactly once: the token is stored as a digest, so this response is the only copy.
    pub url: String,
    /// The public URL, when the portal is public. `null` otherwise, rather than a URL that would 404 — a link
    /// that does not work is worse than no link.
    pub public_url: Option<String>,
}

/// Changes to how a portal looks and reads.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PresentRequest {
    pub title: String,
    #[serde(default)]
    pub intro: String,
    pub kind: String,
    pub logo_asset_id: Option<Uuid>,
    pub accent: String,
    pub is_public: bool,
    pub allow_search: bool,
}

/// What a visitor sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PortalPage {
    pub title: String,
    pub intro: String,
    pub kind: String,
    pub accent: String,
    /// A preview URL for the logo, minted like any other: a logo is an asset, and an asset is delivered through
    /// the chokepoint even when it is decoration.
    pub logo_url: Option<String>,
    pub allow_search: bool,
    /// What the visitor searched for, echoed so the page can show it.
    pub query: Option<String>,
    pub items: Vec<PortalItem>,
    /// How many assets the set holds after the search, which is not `items.len()` when the page is capped.
    pub total: i64,
    pub downloads_remaining: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A visitor's request.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct VisitRequest {
    pub passcode: Option<String>,
    /// A term to narrow the set by. Ignored when the portal does not allow searching.
    pub q: Option<String>,
}

/// The same, for the slug route, which is a `GET` because a public portal is a page a browser opens.
#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct VisitParams {
    pub q: Option<String>,
    pub passcode: Option<String>,
}

/// Creates a portal and the link that reaches it.
#[utoipa::path(
    post,
    path = "/portals",
    request_body = NewPortalRequest,
    responses(
        (status = 201, body = CreatedPortal),
        (status = 409, description = "That name is taken"),
        (status = 422, description = "A field the portal cannot hold, or a source this build does not accept"),
    ),
    tag = "portals",
)]
pub async fn create(
    State(state): State<Arc<PortalState>>,
    headers: HeaderMap,
    Json(request): Json<NewPortalRequest>,
) -> Result<(StatusCode, Json<CreatedPortal>), Failure> {
    // Manage: publishing part of the library under the tenant's name is an editorial act, and the widest one in
    // this system — a portal is visible to people who have no account at all.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let kind = Kind::parse(&request.kind).ok_or_else(|| {
        Failure::Unprocessable(format!(
            "`{}` is not a portal kind; use standard, brand, video or channel",
            request.kind
        ))
    })?;

    let collection_id = source_of(&request)?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // The collection must exist and be one this caller can see. Without this a portal could be created over an
    // id somebody guessed, which is the one place in this feature where a mistake publishes the wrong assets.
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM collections WHERE id = $1")
        .bind(collection_id)
        .fetch_optional(conn.executor())
        .await
        .map_err(dam_db::Error::from)?;
    if exists.is_none() {
        conn.commit().await?;
        return Err(Failure::NotFound);
    }

    let created = portals::create(
        conn.executor(),
        &NewPortal {
            key: request.key.trim().to_lowercase(),
            title: request.title,
            intro: request.intro,
            kind,
            source: Source::Collection(collection_id),
            logo_asset_id: request.logo_asset_id,
            accent: request.accent,
            is_public: request.is_public,
            allow_search: request.allow_search,
        },
        &dam_db::shares::ShareSpec {
            kind: "portal",
            target_id: None,
            search_query: None,
            passcode: request.passcode.as_deref(),
            expires_at: request
                .expires_in_days
                .map(|days| Utc::now() + Duration::days(days.clamp(1, 3_650))),
            max_downloads: request.max_downloads,
            allow_original: request.allow_original,
            requires_eula: false,
            created_by: caller.identity_id,
        },
        caller.identity_id,
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;

    let public_url = created
        .portal
        .is_public
        .then(|| self::public_url(&state, &created.portal.key));
    Ok((
        StatusCode::CREATED,
        Json(CreatedPortal {
            url: token_url(&state, &created.token),
            public_url,
            portal: PortalView::of(created.portal, true),
        }),
    ))
}

/// Which of the three sources was asked for, or the refusal that says why not.
///
/// The schema holds three and this build shows one. Both halves of that are deliberate: the column exists because
/// the other two are a slice away, and the refusal exists because *which* assets a live query publishes to the
/// public internet is a decision ARCHITECTURE.md does not settle. See NEEDS-REVIEW.md.
fn source_of(request: &NewPortalRequest) -> Result<Uuid, Failure> {
    let asked = usize::from(request.collection_id.is_some())
        + usize::from(request.saved_search_id.is_some())
        + usize::from(request.media_class.is_some());
    if asked != 1 {
        return Err(Failure::Unprocessable(
            "a portal shows one set: give exactly one of collection_id, saved_search_id or media_class"
                .to_owned(),
        ));
    }
    if request.saved_search_id.is_some() || request.media_class.is_some() {
        return Err(Failure::Unprocessable(
            "this build publishes a collection, where somebody put each asset there on purpose. A saved search \
             or a media class is a live query, so the portal would publish every future asset that happens to \
             match it — nobody decides, a rule does. See NEEDS-REVIEW.md."
                .to_owned(),
        ));
    }
    request
        .collection_id
        .ok_or_else(|| Failure::Unprocessable("a portal needs a collection_id".to_owned()))
}

/// Every portal, retired ones included.
#[utoipa::path(
    get,
    path = "/portals",
    responses((status = 200, body = Vec<PortalView>)),
    tag = "portals",
)]
pub async fn list(
    State(state): State<Arc<PortalState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<PortalView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = portals::all(conn.executor()).await?;
    let mut views = Vec::with_capacity(rows.len());
    for portal in rows {
        let reachable = portals::share_of(conn.executor(), portal.id)
            .await?
            .is_some();
        views.push(PortalView::of(portal, reachable));
    }
    conn.commit().await?;
    Ok(Json(views))
}

/// Changes how a portal looks and reads.
///
/// Not what it shows: a portal that swapped its collection would show a different library to everyone holding
/// the old URL, which is a new portal wearing an old name.
#[utoipa::path(
    patch,
    path = "/portals/{id}",
    request_body = PresentRequest,
    responses(
        (status = 200, body = PortalView),
        (status = 404, description = "No such portal, or it is retired"),
    ),
    tag = "portals",
)]
pub async fn present(
    State(state): State<Arc<PortalState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<PresentRequest>,
) -> Result<Json<PortalView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let kind = Kind::parse(&request.kind).ok_or_else(|| {
        Failure::Unprocessable(format!(
            "`{}` is not a portal kind; use standard, brand, video or channel",
            request.kind
        ))
    })?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let updated = portals::present(
        conn.executor(),
        id,
        &Presentation {
            title: request.title,
            intro: request.intro,
            kind,
            logo_asset_id: request.logo_asset_id,
            accent: request.accent,
            is_public: request.is_public,
            allow_search: request.allow_search,
        },
    )
    .await
    .map_err(Refused)?;
    let reachable = portals::share_of(conn.executor(), id).await?.is_some();
    conn.commit().await?;
    Ok(Json(PortalView::of(updated, reachable)))
}

/// Retires a portal and revokes the link that reaches it.
#[utoipa::path(
    delete,
    path = "/portals/{id}",
    responses(
        (status = 200, body = PortalView),
        (status = 404, description = "No such portal, or it is already retired"),
    ),
    tag = "portals",
)]
pub async fn retire(
    State(state): State<Arc<PortalState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<PortalView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let retired = portals::retire(conn.executor(), id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(PortalView::of(retired, false)))
}

/// A public portal, by name.
#[utoipa::path(
    get,
    path = "/portal/{key}",
    params(VisitParams),
    responses(
        (status = 200, body = PortalPage),
        (status = 401, description = "A passcode is required or wrong"),
        (status = 404, description = "No such portal — or private, retired, expired or exhausted"),
    ),
    tag = "portals",
)]
pub async fn by_key(
    State(state): State<Arc<PortalState>>,
    Path(key): Path<String>,
    Query(params): Query<VisitParams>,
) -> Result<Json<PortalPage>, VisitorFailure> {
    let mut conn = state
        .delivery
        .pool()
        .acquire()
        .await
        .map_err(dam_db::Error::from)?;
    // Public and live only — the narrow lookup, so a private portal is not even a 403 by name.
    let portal = portals::by_public_key(&mut conn, &key).await?;
    let Some(portal) = portal else {
        return Err(VisitorFailure::Portal(
            StatusCode::NOT_FOUND,
            "there is no portal at that address".to_owned(),
        ));
    };
    let Some(share_id) = portals::share_of(&mut conn, portal.id).await? else {
        // Public, live, and its link was revoked by hand. The same flat answer: the visitor learns the address
        // does not work, not that it used to.
        return Err(VisitorFailure::Portal(
            StatusCode::NOT_FOUND,
            "there is no portal at that address".to_owned(),
        ));
    };

    render(
        &state,
        portal,
        share_id,
        params.passcode.as_deref(),
        params.q.as_deref(),
    )
    .await
}

/// A portal by its share token — the way a private one is reached.
#[utoipa::path(
    post,
    path = "/share/{token}/portal",
    request_body = VisitRequest,
    responses(
        (status = 200, body = PortalPage),
        (status = 401, description = "A passcode is required or wrong"),
        (status = 404, description = "No such link, or it is not a portal"),
    ),
    tag = "portals",
)]
pub async fn by_token(
    State(state): State<Arc<PortalState>>,
    Path(token): Path<String>,
    Json(request): Json<VisitRequest>,
) -> Result<Json<PortalPage>, VisitorFailure> {
    let now = state.delivery.now();
    let share = dam_db::shares::resolve(state.delivery.pool(), &token, now).await?;
    let portal_id = match (share.kind.as_str(), share.target_id) {
        ("portal", Some(id)) => id,
        // The same flat answer a dead token gets: the holder of an asset link learns nothing about what other
        // kinds of link exist.
        _ => {
            return Err(VisitorFailure::Portal(
                StatusCode::NOT_FOUND,
                "this link is not a portal".to_owned(),
            ));
        }
    };

    let mut conn = state
        .delivery
        .pool()
        .acquire()
        .await
        .map_err(dam_db::Error::from)?;
    let portal = portals::read(&mut conn, portal_id).await?;
    let Some(portal) = portal.filter(Portal::is_live) else {
        return Err(VisitorFailure::Portal(
            StatusCode::NOT_FOUND,
            "this portal is no longer available".to_owned(),
        ));
    };

    render(
        &state,
        portal,
        share.id,
        request.passcode.as_deref(),
        request.q.as_deref(),
    )
    .await
}

/// Resolves the share, then renders the set.
///
/// Both routes end here, which is the point: the slug and the token are two addresses for one page, and the
/// checks — passcode, expiry, cap, revocation — happen once, in the share machinery, whichever address was used.
async fn render(
    state: &PortalState,
    portal: Portal,
    share_id: Uuid,
    passcode: Option<&str>,
    query: Option<&str>,
) -> Result<Json<PortalPage>, VisitorFailure> {
    let now = state.delivery.now();
    let share = dam_db::shares::by_id(state.delivery.pool(), share_id)
        .await?
        .ok_or(VisitorFailure::Portal(
            StatusCode::NOT_FOUND,
            "there is no portal at that address".to_owned(),
        ))?;
    if !share.is_live(now) {
        // The share's own vocabulary, through the same refusal type the share portal uses, so a revoked portal
        // and a revoked asset link read alike.
        return Err(VisitorFailure::from(
            dam_db::shares::ShareRefusal::from_share(&share, now),
        ));
    }
    dam_db::shares::check_passcode(state.delivery.pool(), share.id, passcode).await?;

    let Source::Collection(collection_id) = portal.source else {
        // The schema allows two more sources and this build accepts one — see the module note and
        // NEEDS-REVIEW.md. Said plainly rather than rendered empty: an empty portal reads as a mistake.
        return Err(VisitorFailure::Portal(
            StatusCode::NOT_FOUND,
            "this portal's source is not one this build can show".to_owned(),
        ));
    };

    let searching = portal
        .allow_search
        .then(|| query.unwrap_or("").trim())
        .filter(|q| !q.is_empty());
    let media_class = match portal.kind() {
        // The two presentation kinds that also narrow: a video portal showing stills would be a video portal in
        // name only. Standard and Brand show whatever the collection holds.
        Some(Kind::Video) => Some("video"),
        Some(Kind::Channel) => None,
        _ => None,
    };

    let rows = dam_db::portals::members(
        state.delivery.pool(),
        collection_id,
        searching,
        media_class,
        MAX_ITEMS,
    )
    .await?;
    let total =
        dam_db::portals::member_count(state.delivery.pool(), collection_id, searching, media_class)
            .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        // A preview per item, rights-checked on its own — the same rule the order pickup follows. A collection
        // of forty where two are unlicensed is a portal of thirty-eight, and the two are still named.
        let (preview_url, preview_unavailable) = match delivery::issue_for_share(
            &state.delivery,
            row.asset_id,
            "web-2048",
            &portal_usage(),
            None,
            Some(share.id),
            PREVIEW_TTL,
            now,
        )
        .await
        {
            Ok(minted) => (Some(state.delivery.url_for(&minted)), None),
            Err(delivery::Refusal::RightsDenied { .. }) => (
                None,
                Some("this asset is not licensed for distribution".to_owned()),
            ),
            Err(delivery::Refusal::NotDeliverable) => {
                (None, Some("no preview has been rendered yet".to_owned()))
            }
            Err(_) => return Err(VisitorFailure::Internal),
        };
        items.push(PortalItem {
            asset_id: row.asset_id,
            filename: row.filename.clone(),
            mime: Some(row.mime.clone()),
            bytes: Some(row.bytes),
            preview_url,
            preview_unavailable,
        });
    }

    let logo_url = match portal.logo_asset_id {
        Some(asset_id) => match delivery::issue_for_share(
            &state.delivery,
            asset_id,
            "web-2048",
            &portal_usage(),
            None,
            Some(share.id),
            PREVIEW_TTL,
            now,
        )
        .await
        {
            Ok(minted) => Some(state.delivery.url_for(&minted)),
            // A logo that cannot be delivered is a missing logo, not a broken portal.
            Err(_) => None,
        },
        None => None,
    };

    Ok(Json(PortalPage {
        title: portal.title,
        intro: portal.intro,
        kind: portal.kind,
        accent: portal.accent,
        logo_url,
        allow_search: portal.allow_search,
        query: searching.map(str::to_owned),
        items,
        total,
        downloads_remaining: share
            .max_downloads
            .map(|max| (max - share.download_count).max(0)),
        expires_at: share.expires_at,
    }))
}

/// The usage a portal delivery is evaluated under: the widest read, because a portal cannot know where its
/// visitor sits. Same as the share portal's, and deliberately the same words.
fn portal_usage() -> Usage {
    Usage {
        channel: "web".to_owned(),
        territory: "WORLD".to_owned(),
    }
}

/// The URL a token reaches the portal at.
fn token_url(state: &PortalState, token: &str) -> String {
    match state.delivery.public_origin() {
        Some(origin) => format!("{origin}/share/{token}"),
        None => format!("/share/{token}"),
    }
}

/// The URL a public portal answers at.
fn public_url(state: &PortalState, key: &str) -> String {
    match state.delivery.public_origin() {
        Some(origin) => format!("{origin}/portal/{key}"),
        None => format!("/portal/{key}"),
    }
}

/// Turns a store refusal into an HTTP one.
struct Refused(PortalRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            PortalRefusal::Unknown(_) => Self::NotFound,
            PortalRefusal::Taken(key) => Self::Conflict(format!(
                "the name `{key}` is already taken by another portal"
            )),
            PortalRefusal::Invalid(constraint) => Self::Unprocessable(constraint),
            PortalRefusal::Database(error) => Self::from(error),
        }
    }
}
