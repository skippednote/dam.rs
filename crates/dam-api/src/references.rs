//! The usage index over HTTP (M3d·4, §11.4).
//!
//! ## Only a site may report its own usage
//!
//! `PUT /connectors/{id}/refs` requires the *connector's own key*, not `Manage`. An administrator reporting on
//! a site's behalf is meaningless — they do not know which pages render which media — and the write feeds a
//! pin-hot signal that keeps objects out of cold storage, so a caller who can write it can hold a library in
//! Standard. Narrowing it to the one credential that has first-hand knowledge is both the honest rule and the
//! tighter one.
//!
//! ## A full sync is one request, because two would leave a window
//!
//! `full_sync` makes the request mean "this is everything of that type I have", and anything absent becomes
//! orphaned in the same transaction. Split into report-then-sweep, a site that crashed between them would
//! leave every reference it had just re-reported looking abandoned — or, worse the other way, leave deleted
//! pages pinning assets hot indefinitely.
//!
//! ## The impact report is deliberately blunt about what it is
//!
//! `pages` is the *site's own* count, summed. It is the number an operator weighs before a takedown and the
//! softest of the three, so it is named separately from `sites` and `entities` rather than presented as one
//! total. And it counts only live references: telling somebody a page exists that does not is worse than
//! telling them nothing.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, put};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::connector_refs::{self, NewRef};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// How many references one request may carry.
///
/// A thousand. A site with more entities than that syncs in pages, which it has to do anyway — the request
/// body is the limit long before the database is.
const MAX_REFS: usize = 1_000;

pub struct ReferenceState {
    pub global: PgPool,
}

impl std::fmt::Debug for ReferenceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceState").finish_non_exhaustive()
    }
}

pub fn router(state: ReferenceState) -> Router {
    Router::new()
        .route("/connectors/{id}/refs", put(report).get(list))
        .route("/assets/{asset_id}/references", get(for_asset))
        .with_state(Arc::new(state))
}

/// One thing a site is telling us it uses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct RefBody {
    pub asset_id: Uuid,
    /// The remote's own id for the entity — a Drupal media id. Together with the type it is what identifies
    /// this reference, so an entity that switches asset updates rather than duplicating.
    pub remote_entity_id: String,
    #[serde(default)]
    pub remote_uuid: Option<String>,
    /// Where an operator can go and look. The most useful field in a takedown report.
    #[serde(default)]
    pub remote_url: Option<String>,
    /// How many places downstream the entity actually appears. Zero is meaningful: a media row nobody has
    /// placed on a page is not a live page, and it will not pin.
    #[serde(default)]
    pub usage_count: i32,
    /// `[{url, title}]`, a sample rather than a list.
    #[serde(default)]
    pub usage_sample: Option<serde_json::Value>,
    /// Which version the site is rendering, when it knows. Behind the current one means drift.
    #[serde(default)]
    pub synced_version_no: Option<i32>,
}

/// A sync from a connected site.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct SyncBody {
    /// `media` for Drupal. Scoped, so one integration cannot orphan another's rows.
    pub remote_entity_type: String,
    pub references: Vec<RefBody>,
    /// `true` means this is everything of that type the site has; anything absent becomes orphaned in the same
    /// transaction. `false` is an incremental report that only adds and updates.
    #[serde(default)]
    pub full_sync: bool,
}

/// What a sync did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SyncedView {
    pub written: u64,
    /// How many references the site no longer mentions. Zero on an incremental report, always.
    pub orphaned: u64,
}

#[utoipa::path(
    put,
    path = "/connectors/{id}/refs",
    request_body = SyncBody,
    responses(
        (status = 200, body = SyncedView),
        (status = 403, description = "Only the connector's own credential may report its usage"),
        (status = 404, description = "No such connector"),
        (status = 422, description = "Too many references, or an entity type that is not usable"),
    ),
    tag = "connectors",
)]
pub async fn report(
    State(state): State<Arc<ReferenceState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<SyncBody>,
) -> Result<Json<SyncedView>, Failure> {
    // `Read` is enough as the *action*; being the connector is the real gate, below.
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;

    if body.remote_entity_type.trim().is_empty() {
        return Err(Failure::Unprocessable(
            "a reference needs an entity type; it is what scopes a full sync".to_owned(),
        ));
    }
    if body.references.len() > MAX_REFS {
        return Err(Failure::Unprocessable(format!(
            "{} references in one request; the limit is {MAX_REFS}, so sync in pages",
            body.references.len()
        )));
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let connector = dam_db::connectors::by_id(conn.executor(), id)
        .await?
        .ok_or(Failure::NotFound)?;

    // The gate. Only the site itself may say what the site renders — and the write feeds a signal that keeps
    // objects out of cold storage, so a caller who can forge it can hold a library in Standard.
    if connector.api_key_id != Some(caller.api_key_id) {
        conn.rollback().await?;
        return Err(Failure::Forbidden(
            "only a connector's own credential may report its usage".to_owned(),
        ));
    }
    // A revoked or paused site reporting is a site that should not be trusted about what it renders. 404
    // rather than 403: to whoever holds a revoked credential, the registration is gone.
    if !connector.status.may_render() {
        conn.rollback().await?;
        return Err(Failure::NotFound);
    }

    let entity_type = body.remote_entity_type.trim();
    let refs: Vec<NewRef<'_>> = body
        .references
        .iter()
        .map(|one| NewRef {
            asset_id: one.asset_id,
            remote_entity_type: entity_type,
            remote_entity_id: &one.remote_entity_id,
            remote_uuid: one.remote_uuid.as_deref(),
            remote_url: one.remote_url.as_deref(),
            usage_count: one.usage_count,
            usage_sample: one
                .usage_sample
                .clone()
                .unwrap_or_else(|| serde_json::json!([])),
            synced_version_no: one.synced_version_no,
        })
        .collect();

    // Assets the connector cannot see are dropped rather than refused. A site reporting one is a site whose
    // scope was narrowed after it cached the id — an ordinary state, and failing the whole sync over it would
    // stop the other nine hundred references from being recorded.
    let ids: Vec<Uuid> = refs.iter().map(|one| one.asset_id).collect();
    let visible: std::collections::HashSet<Uuid> =
        dam_db::assets::visible_among(conn.executor(), &caller.predicate, &ids)
            .await?
            .into_iter()
            .collect();
    let refs: Vec<NewRef<'_>> = refs
        .into_iter()
        .filter(|one| visible.contains(&one.asset_id))
        .collect();

    let now = chrono::Utc::now();
    let reported = connector_refs::report(conn.executor(), id, &refs, now).await?;

    // In the same transaction as the report, deliberately. Split in two, a site that crashed between them
    // would leave what it had just re-reported looking abandoned — or leave deleted pages pinning assets hot.
    let orphaned = if body.full_sync {
        let seen: Vec<&str> = body
            .references
            .iter()
            .map(|one| one.remote_entity_id.as_str())
            .collect();
        connector_refs::sweep_absent(conn.executor(), id, entity_type, &seen).await?
    } else {
        0
    };
    dam_db::connectors::seen(conn.executor(), id, None, now).await?;
    conn.commit().await?;

    Ok(Json(SyncedView {
        written: reported.written,
        orphaned,
    }))
}

/// One reference, as a report reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReferenceView {
    pub connector_id: Uuid,
    /// The site's name, so a report reads without a second lookup.
    pub connector_label: String,
    pub asset_id: Uuid,
    pub remote_entity_type: String,
    pub remote_entity_id: String,
    pub remote_url: Option<String>,
    pub usage_count: i32,
    pub usage_sample: serde_json::Value,
    pub synced_version_no: Option<i32>,
    pub synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `linked`, `expired`, `unpublished` or `orphaned` — what somebody asserted. Never `stale`: staleness is
    /// the two derived fields below, so they cannot disagree with the timestamps under them.
    pub state: String,
    /// The site is rendering an older version. A job to run.
    pub version_drifted: bool,
    /// The site has not reported inside the freshness window. A site to go and look at — and the reason this
    /// reference no longer keeps the asset out of cold storage.
    pub refresh_overdue: bool,
}

/// What pulling an asset would affect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImpactView {
    pub sites: i64,
    pub entities: i64,
    /// Places those entities are used, summed from what each site reported. The softest of the three — it is
    /// the site's own count — which is why it is named separately rather than folded into one total.
    pub pages: i64,
    /// The references themselves, most-used first.
    pub references: Vec<ReferenceView>,
}

#[utoipa::path(
    get,
    path = "/assets/{asset_id}/references",
    responses(
        (status = 200, body = ImpactView),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "connectors",
)]
pub async fn for_asset(
    State(state): State<Arc<ReferenceState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<ImpactView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Through the caller's predicate: an asset they cannot see is absent, not described. Otherwise this
    // endpoint would report the filenames of pages using assets they were never shown.
    if dam_db::assets::visible_among(conn.executor(), &caller.predicate, &[asset_id])
        .await?
        .is_empty()
    {
        conn.rollback().await?;
        return Err(Failure::NotFound);
    }

    let now = chrono::Utc::now();
    let rows = connector_refs::for_asset(conn.executor(), asset_id, now).await?;
    let impact = connector_refs::impact(conn.executor(), &[asset_id], now).await?;
    conn.commit().await?;

    let counted = impact.get(&asset_id).copied().unwrap_or_default();
    Ok(Json(ImpactView {
        sites: counted.sites,
        entities: counted.entities,
        pages: counted.pages,
        // Every reference, including the orphaned and overdue ones — the counts above are what is live, and
        // the list is what an operator reads to understand why. Dropping the dead rows would answer "how many"
        // and hide "and one site stopped reporting three weeks ago".
        references: rows.into_iter().map(view).collect(),
    }))
}

#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
}

const fn default_limit() -> i64 {
    100
}

#[utoipa::path(
    get,
    path = "/connectors/{id}/refs",
    params(ListParams),
    responses(
        (status = 200, body = [ReferenceView]),
        (status = 404, description = "No such connector"),
    ),
    tag = "connectors",
)]
pub async fn list(
    State(state): State<Arc<ReferenceState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<ReferenceView>>, Failure> {
    // Manage: reading what a site renders is reading part of the library's shape, and it is an operator's
    // question rather than a curator's.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if dam_db::connectors::by_id(conn.executor(), id)
        .await?
        .is_none()
    {
        conn.rollback().await?;
        return Err(Failure::NotFound);
    }
    let rows = connector_refs::for_connector(conn.executor(), id, params.limit, chrono::Utc::now())
        .await?;
    conn.commit().await?;
    Ok(Json(rows.into_iter().map(view).collect()))
}

fn view(row: connector_refs::Reference) -> ReferenceView {
    ReferenceView {
        connector_id: row.connector_id,
        connector_label: row.connector_label,
        asset_id: row.asset_id,
        remote_entity_type: row.remote_entity_type,
        remote_entity_id: row.remote_entity_id,
        remote_url: row.remote_url,
        usage_count: row.usage_count,
        usage_sample: row.usage_sample,
        synced_version_no: row.synced_version_no,
        synced_at: row.synced_at,
        state: row.state,
        version_drifted: row.version_drifted,
        refresh_overdue: row.refresh_overdue,
    }
}
