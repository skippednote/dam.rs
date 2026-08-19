//! One asset's history over HTTP (Q.10).
//!
//! ## The same line shape as the dashboard, deliberately
//!
//! A history line and a feed line say the same kind of thing — somebody did something to something, at a time — so
//! this returns [`crate::dashboard::ActivityEntry`] rather than a parallel type. One shape means one renderer, and
//! a renderer that already handles an unrecognised kind, a missing actor and a private comment handles them here
//! too. A second type would be a second place to forget each of those.
//!
//! ## Read, and 404 for an asset the caller cannot see
//!
//! Reading a history is part of understanding an asset, so Read is enough. An asset outside the caller's scope is
//! **404, not an empty history**: an empty list would say "this exists and nothing has happened to it", which is a
//! different and untrue statement, and the difference between the two answers is an existence oracle.

use crate::assets::Failure;
use crate::caller;
use crate::dashboard::ActivityEntry;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::IntoParams;
use uuid::Uuid;

/// What the history endpoint needs.
pub struct HistoryState {
    pub global: PgPool,
}

impl std::fmt::Debug for HistoryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryState").finish_non_exhaustive()
    }
}

/// The history route.
pub fn router(state: HistoryState) -> Router {
    Router::new()
        .route("/assets/{asset_id}/history", get(history))
        .with_state(Arc::new(state))
}

/// How much history to return.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct HistoryParams {
    /// Lines to return, newest first. Clamped rather than refused — a caller asking for a thousand wants "lots",
    /// and a 422 would be pedantry about a request whose intent is clear.
    pub limit: Option<i64>,
}

/// Everything that has happened to one asset, newest first.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/history",
    params(("asset_id" = Uuid, Path,), HistoryParams),
    responses(
        (status = 200, body = Vec<ActivityEntry>),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "history",
)]
pub async fn history(
    State(state): State<Arc<HistoryState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Vec<ActivityEntry>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // Visibility first, so an asset the caller cannot see is absent rather than eventless. See the module docs.
    let visible =
        dam_db::assets::visible_among(conn.executor(), &caller.predicate, &[asset_id]).await?;
    if visible.is_empty() {
        return Err(Failure::NotFound);
    }

    let entries = dam_db::events::for_asset(
        conn.executor(),
        asset_id,
        &caller.predicate,
        params.limit.unwrap_or(DEFAULT_LENGTH),
    )
    .await?;
    conn.commit().await?;

    // Names in one lookup, as the dashboard does.
    let ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = entries.iter().filter_map(|entry| entry.actor_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;

    Ok(Json(
        entries
            .into_iter()
            .map(|entry| ActivityEntry {
                id: entry.id,
                occurred_at: entry.occurred_at,
                kind: entry.kind,
                asset_id: entry.asset_id,
                filename: entry.filename,
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
    ))
}

/// How much history a panel shows without being asked.
///
/// Longer than the dashboard's twenty: this is one asset's whole story rather than a summary of everything, and the
/// interesting entry in a mature asset's history is often not the most recent one.
const DEFAULT_LENGTH: i64 = 50;
