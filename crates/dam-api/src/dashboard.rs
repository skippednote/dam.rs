//! The landing page's data: activity, counts, and the searches worth keeping (Q.7).
//!
//! ## Everything here is scoped, including the numbers
//!
//! §7 says a count is a disclosure. A dashboard is almost entirely counts, so a scoped reader sees their own
//! totals — not the library's with their own results beneath it, which would tell them exactly how much they
//! cannot reach.
//!
//! ## The feed names people, and that is the point
//!
//! "Ada uploaded harbour.jpg" is the whole value of an activity feed, so actor names are resolved like a comment
//! thread's. Within a tenant that is not a new disclosure: the same person's name is already on every comment they
//! wrote and every share they made.
//!
//! ## One request, because the page is one screen
//!
//! The feed, the counts and the saved searches arrive together. Three endpoints would be three round trips for a
//! page that cannot render usefully without all three, and would let the numbers disagree with the list beneath
//! them.

use crate::assets::Failure;
use crate::caller;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_core::query::{Planned, Query as AssetQuery};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the dashboard endpoint needs.
pub struct DashboardState {
    pub global: PgPool,
}

impl std::fmt::Debug for DashboardState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DashboardState").finish_non_exhaustive()
    }
}

/// The dashboard route.
pub fn router(state: DashboardState) -> Router {
    Router::new()
        .route("/dashboard", get(dashboard))
        .with_state(Arc::new(state))
}

/// One line of activity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ActivityEntry {
    pub id: Uuid,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    /// `upload`, `edit`, `share`, `comment`, `download`, `delete`, `restore` — or anything a future subsystem
    /// records. Reported as stored rather than mapped onto a known set, so activity this build does not recognise
    /// is still visible.
    pub kind: String,
    pub asset_id: Option<Uuid>,
    /// The asset's name now, so a line reads as a sentence.
    pub filename: Option<String>,
    /// Who did it. Absent for something the system did, and for somebody since deleted.
    pub actor: Option<crate::comments::PersonView>,
    pub context: serde_json::Value,
}

/// The counts a landing page leads with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Counts {
    /// Assets this caller can see.
    pub assets: i64,
    pub uploads_this_week: i64,
    pub downloads_this_week: i64,
    pub comments_this_week: i64,
    /// Assets carrying no metadata at all — the work a landing page exists to surface.
    pub without_metadata: i64,
}

/// A saved search, as a dashboard tile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Spotlight {
    pub id: Uuid,
    pub name: String,
    /// The stored count, and named as stored: it is a cached number a worker refreshes, never an access decision
    /// and never this caller's count. A tile that presented it as "your results" would be lying to a scoped
    /// reader.
    pub cached_count: Option<i64>,
    /// Whether this caller owns it, so a tile can say "yours" rather than implying everyone's is.
    pub mine: bool,
}

/// Everything the landing page draws.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Dashboard {
    pub counts: Counts,
    pub activity: Vec<ActivityEntry>,
    /// Saved searches this caller may open, theirs first.
    pub spotlights: Vec<Spotlight>,
}

/// The landing page.
#[utoipa::path(
    get,
    path = "/dashboard",
    responses(
        (status = 200, body = Dashboard),
        (status = 403, description = "The credential holds no read scope"),
    ),
    tag = "dashboard",
)]
pub async fn dashboard(
    State(state): State<Arc<DashboardState>>,
    headers: HeaderMap,
) -> Result<Json<Dashboard>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    // A dashboard is somebody's: the spotlights are their saved searches and the activity is scoped to what they
    // can see. `authorize` already refuses a key with no identity — no identity means no membership, so no grants —
    // and this states the requirement rather than pretending to have a branch for it.
    let identity = caller.identity_id;
    let planned = Planned::new(AssetQuery::All, caller.predicate.clone(), &[])
        .map_err(|_| Failure::Internal)?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // One transaction for all three, so the numbers describe the same instant as the list beneath them.
    let counts = dam_db::events::summary(conn.executor(), &planned).await?;
    let feed = dam_db::events::feed(conn.executor(), &planned, FEED_LENGTH, &[]).await?;
    // `visible_to_on`, inside the tenant transaction: `saved_searches` is a tenant table and the pool-taking form
    // would resolve it against `dam_global`.
    let saved = dam_db::saved_searches::visible_to_on(
        conn.executor(),
        Some(identity),
        &caller.role_names,
        SPOTLIGHTS,
    )
    .await?;
    conn.commit().await?;

    // Actor names in one lookup, as a comment thread does. A feed of twenty lines between four people needs four
    // names, not twenty queries.
    let ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = feed.iter().filter_map(|entry| entry.actor_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;

    Ok(Json(Dashboard {
        counts: Counts {
            assets: counts.assets,
            uploads_this_week: counts.uploads_this_week,
            downloads_this_week: counts.downloads_this_week,
            comments_this_week: counts.comments_this_week,
            without_metadata: counts.without_metadata,
        },
        activity: feed
            .into_iter()
            .map(|entry| ActivityEntry {
                id: entry.id,
                occurred_at: entry.occurred_at,
                kind: entry.kind,
                asset_id: entry.asset_id,
                filename: entry.filename,
                // `None` for the system, and for a person since deleted. A feed line still reads without a name —
                // "harbour.jpg was uploaded" — which is why this is optional rather than a placeholder.
                actor: entry.actor_id.and_then(|id| {
                    people.iter().find(|person| person.id == id).map(|person| {
                        crate::comments::PersonView {
                            id: person.id,
                            name: person.display_name.clone(),
                            email: person.email.clone(),
                        }
                    })
                }),
                context: entry.context,
            })
            .collect(),
        spotlights: saved
            .into_iter()
            .map(|search| Spotlight {
                id: search.id,
                name: search.name,
                cached_count: search.result_count,
                mine: search.owner_id == Some(identity),
            })
            .collect(),
    }))
}

/// How many saved searches a dashboard offers.
///
/// A handful of tiles. The full list is the searches screen's job, and a landing page that showed forty of them
/// would be a list of links rather than a summary.
const SPOTLIGHTS: i64 = 8;

/// How much activity a landing page shows.
///
/// A screenful. More is a report, and a report is a different page with its own paging — putting two hundred lines
/// on a dashboard makes the counts above them scroll away, which is the one thing the page is for.
const FEED_LENGTH: i64 = 20;
