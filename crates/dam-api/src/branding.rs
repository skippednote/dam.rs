//! Site branding over HTTP (Q.20d).
//!
//! ## Reading needs Read, writing needs Manage
//!
//! The app shell renders the tenant's name and accent on every page, so every authenticated reader needs this
//! — gating the read behind `Manage` would leave a curator looking at a header that says "damrs". Changing it
//! is administration: it is what the whole library calls itself, and what every new portal inherits.
//!
//! Nothing here is a disclosure. A tenant's own name, logo and colour are visible to everybody who can already
//! see the library, and the support address is one the tenant chose to publish on their portals.
//!
//! ## The name falls back to the tenant's display name
//!
//! Resolved here rather than in the column, because the column cannot see `dam_global.tenants` — 0002 forbids
//! cross-schema foreign keys — and a copy of the display name in the tenant schema would go stale the day
//! somebody changed it. So an empty `site_name` means "use theirs", and the response carries the resolved
//! value so a client never has to know the rule.
//!
//! ## The logo is an asset id, and it is checked
//!
//! Following 0030's argument for portals: a logo is an asset, already governed, and a second upload path for
//! it would be a second thing to back up and a second place for an unlicensed image to appear. Checked against
//! the caller's own predicate, because otherwise setting the logo to an id you cannot see would tell you it
//! exists — and would put an asset on every page of a library its own rules say you may not read.

use crate::assets::Failure;
use crate::caller;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

pub struct BrandingState {
    pub global: PgPool,
    /// For minting the logo's preview link, through the same signing path as any other thumbnail.
    pub delivery: Option<Arc<crate::delivery::DeliveryState>>,
}

impl std::fmt::Debug for BrandingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrandingState").finish_non_exhaustive()
    }
}

pub fn router(state: BrandingState) -> Router {
    Router::new()
        .route("/branding", get(read).put(write))
        .with_state(Arc::new(state))
}

/// What the library calls itself and what it looks like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BrandingView {
    /// The name to show. Already resolved: empty `site_name` becomes the tenant's display name, so a client
    /// never has to know the fallback rule.
    pub site_name: String,
    /// Whether that name is the tenant's own setting or the fallback. The settings form needs the difference —
    /// it must show an empty field rather than pre-filling the fallback and making it look chosen.
    pub site_name_is_default: bool,
    pub logo_asset_id: Option<Uuid>,
    /// A short-lived link to the logo's thumbnail, when it has one and delivery is configured.
    pub logo_url: Option<String>,
    /// Lowercase `#rrggbb`.
    pub accent: String,
    pub support_email: Option<String>,
}

#[utoipa::path(
    get,
    path = "/branding",
    responses(
        (status = 200, description = "The tenant's branding, with the name resolved", body = BrandingView),
        (status = 403, description = "The credential holds no read scope"),
    ),
    tag = "branding",
)]
pub async fn read(
    State(state): State<Arc<BrandingState>>,
    headers: HeaderMap,
) -> Result<Json<BrandingView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let display_name = tenant_display_name(&state.global, caller.tenant_id).await?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let branding = dam_db::branding::read(conn.executor()).await?;
    let logo_url = logo_link(&state, &caller, conn.executor(), branding.logo_asset_id).await?;
    conn.commit().await?;

    Ok(Json(view(&branding, &display_name, logo_url)))
}

/// What may be changed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct BrandingBody {
    /// Empty or absent means fall back to the tenant's display name.
    #[serde(default)]
    pub site_name: String,
    #[serde(default)]
    pub logo_asset_id: Option<Uuid>,
    pub accent: String,
    #[serde(default)]
    pub support_email: Option<String>,
}

#[utoipa::path(
    put,
    path = "/branding",
    request_body = BrandingBody,
    responses(
        (status = 200, description = "Saved, as stored", body = BrandingView),
        (status = 422, description = "The accent is not a colour, or the logo is not an asset you can see"),
    ),
    tag = "branding",
)]
pub async fn write(
    State(state): State<Arc<BrandingState>>,
    headers: HeaderMap,
    Json(body): Json<BrandingBody>,
) -> Result<Json<BrandingView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let display_name = tenant_display_name(&state.global, caller.tenant_id).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // Through the caller's own predicate. Without it, setting the logo to an id you cannot see would confirm
    // the asset exists, and would put it on every page of a library whose rules say you may not read it.
    if let Some(logo) = body.logo_asset_id {
        let visible =
            dam_db::assets::visible_among(conn.executor(), &caller.predicate, &[logo]).await?;
        if visible.is_empty() {
            return Err(Failure::Unprocessable(
                "that logo is not an asset you can see".to_owned(),
            ));
        }
    }

    let branding = dam_db::branding::Branding {
        site_name: body.site_name,
        logo_asset_id: body.logo_asset_id,
        accent: body.accent,
        support_email: body.support_email,
    };
    dam_db::branding::write(conn.executor(), &branding)
        .await
        .map_err(|error| match error {
            // The colour rule, as a sentence naming the format. A constraint violation would be a 500 for
            // something the person at the keyboard can fix in one keystroke.
            dam_db::Error::Unsupported(reason) => Failure::Unprocessable(reason),
            other => other.into(),
        })?;

    // Read back rather than echoed: the accent is lowercased and the strings are trimmed, and a client shown
    // what it sent would not learn that `#2563EB` became `#2563eb`.
    let stored = dam_db::branding::read(conn.executor()).await?;
    let logo_url = logo_link(&state, &caller, conn.executor(), stored.logo_asset_id).await?;
    conn.commit().await?;

    Ok(Json(view(&stored, &display_name, logo_url)))
}

fn view(
    branding: &dam_db::branding::Branding,
    display_name: &str,
    logo_url: Option<String>,
) -> BrandingView {
    BrandingView {
        site_name: branding.name_or(display_name),
        site_name_is_default: branding.site_name.trim().is_empty(),
        logo_asset_id: branding.logo_asset_id,
        logo_url,
        accent: branding.accent.clone(),
        support_email: branding.support_email.clone(),
    }
}

/// A thumbnail link for the logo, when there is one and it has been rendered.
///
/// Through `assets::thumbnail_url`, so a logo link is signed by the one thing that signs delivery tokens. A
/// second path would be a second key to rotate.
async fn logo_link(
    state: &BrandingState,
    caller: &caller::Caller,
    conn: &mut sqlx::PgConnection,
    logo: Option<Uuid>,
) -> Result<Option<String>, Failure> {
    let Some(logo) = logo else {
        return Ok(None);
    };
    let rendered =
        dam_db::derivatives::which_have(conn, &[logo], &crate::assets::thumb_op_hash()).await?;
    if !rendered.contains(&logo) {
        return Ok(None);
    }
    Ok(crate::assets::thumbnail_url(
        state.delivery.as_deref(),
        caller,
        logo,
    ))
}

/// The tenant's display name, from the control plane.
///
/// Read per request rather than cached on the state: it is one indexed lookup by primary key, and a cache
/// would mean a rename not appearing until a restart — for a value that appears in the header of every page.
async fn tenant_display_name(global: &PgPool, tenant_id: Uuid) -> Result<String, Failure> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT display_name FROM dam_global.tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(global)
            .await
            .map_err(dam_db::Error::from)?;
    Ok(name.unwrap_or_default())
}
