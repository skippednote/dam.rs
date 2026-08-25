//! Collections over HTTP (Q.14b).
//!
//! `dam_db::collections` has had membership, dense ordering and `pin_hot` since 2.3, and until now there was
//! no way to make or fill a collection outside a test. That mattered beyond the missing feature: a portal
//! publishes a collection, so the person who wanted a public page could not create the thing it publishes.
//!
//! ## Manage throughout, including to read
//!
//! Not `Read`. A collection's *membership* is a curatorial statement — "these are the twelve we are showing
//! the client" — and the list of collections is a map of work in progress. Both are administration rather
//! than library browsing.
//!
//! ## The predicate applies in both directions
//!
//! Nothing here widens what a caller can see. `add` filters the ids through the caller's own scope, so a
//! collection cannot be used to put an unseeable asset onto a page a portal publishes; `items` filters the
//! same way, so it cannot be used to learn that such an asset exists. The second half is the one that is
//! easy to forget — the leak arrives from the other side, when a narrowly scoped curator opens a collection
//! somebody with a wider scope curated.
//!
//! ## The key never changes
//!
//! A portal references a collection by key. `PATCH` moves the label, the description, the visibility and the
//! pinning; it cannot move the key, because doing so would silently break or repoint every portal built on it.
//! The label is what anybody actually wanted to change.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

pub struct CollectionState {
    pub global: PgPool,
    /// Held only to mint thumbnail links for the members, and optional for the same reason it is optional on
    /// [`crate::assets::AssetState`]: a build without delivery configured returns no links rather than
    /// refusing to list a collection. Shared rather than rebuilt, because a preview URL is a delivery token
    /// and there must be exactly one place that signs one.
    pub delivery: Option<Arc<crate::delivery::DeliveryState>>,
}

impl std::fmt::Debug for CollectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectionState").finish_non_exhaustive()
    }
}

pub fn router(state: CollectionState) -> Router {
    Router::new()
        .route("/collections", get(list).post(create))
        .route(
            "/collections/{id}",
            axum::routing::patch(amend).delete(remove),
        )
        .route("/collections/{id}/items", get(items).post(add))
        .route("/collections/{id}/items/{asset_id}", delete(remove_item))
        .route("/collections/{id}/items/{asset_id}/position", post(reorder))
        .with_state(Arc::new(state))
}

/// One collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CollectionView {
    pub id: Uuid,
    /// Stable, and not changeable. A portal references this.
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    /// `private`, `shared` or `public`.
    pub visibility: String,
    /// Whether membership blocks tiering (§6.4). A pinned collection's assets stay instantly fetchable.
    pub pin_hot: bool,
    pub item_count: i64,
}

impl From<dam_db::collections::Collection> for CollectionView {
    fn from(row: dam_db::collections::Collection) -> Self {
        Self {
            id: row.id,
            key: row.key,
            label: row.label,
            description: row.description,
            visibility: row.visibility,
            pin_hot: row.pin_hot,
            item_count: row.item_count,
        }
    }
}

#[utoipa::path(
    get,
    path = "/collections",
    responses((status = 200, description = "Every collection, with its size", body = [CollectionView])),
    tag = "collections",
)]
pub async fn list(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<CollectionView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = dam_db::collections::all(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(rows.into_iter().map(CollectionView::from).collect()))
}

/// What a new collection needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct NewCollectionBody {
    /// The stable name a portal will reference. Lowercase, and it cannot be changed later.
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Defaults to `private`, which is the safe direction: a collection that became public because somebody
    /// omitted a field is a disclosure caused by a default.
    #[serde(default = "private")]
    pub visibility: String,
    #[serde(default)]
    pub pin_hot: bool,
}

fn private() -> String {
    "private".to_owned()
}

#[utoipa::path(
    post,
    path = "/collections",
    request_body = NewCollectionBody,
    responses(
        (status = 201, description = "Created", body = CollectionView),
        (status = 409, description = "The key is taken", body = String),
        (status = 422, description = "Not a valid visibility", body = String),
    ),
    tag = "collections",
)]
pub async fn create(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
    Json(body): Json<NewCollectionBody>,
) -> Result<(StatusCode, Json<CollectionView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let key = body.key.trim();
    if key.is_empty() {
        return Err(Failure::Unprocessable(
            "a collection needs a key; it is what a portal references".to_owned(),
        ));
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let made = dam_db::collections::create(
        conn.executor(),
        &dam_db::collections::NewCollection {
            key,
            label: body.label.trim(),
            description: body.description.as_deref(),
            visibility: &body.visibility,
            pin_hot: body.pin_hot,
            owner_id: Some(caller.identity_id),
        },
    )
    .await;

    let id = match made {
        Ok(id) => id,
        // A taken key is the caller's to fix, and `Conflict` says which — see `Failure`'s own docs on the
        // difference between "correct the form" and "deal with what is in the way".
        Err(dam_db::Error::Unsupported(reason)) if reason.contains("already exists") => {
            return Err(Failure::Conflict(reason));
        }
        Err(dam_db::Error::Unsupported(reason)) => return Err(Failure::Unprocessable(reason)),
        Err(other) => return Err(other.into()),
    };
    let created = dam_db::collections::all(conn.executor())
        .await?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or(Failure::Internal)?;
    conn.commit().await?;

    Ok((StatusCode::CREATED, Json(CollectionView::from(created))))
}

/// What may be changed. Not the key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct AmendBody {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    pub visibility: String,
    #[serde(default)]
    pub pin_hot: bool,
}

#[utoipa::path(
    patch,
    path = "/collections/{id}",
    request_body = AmendBody,
    responses(
        (status = 200, description = "Amended", body = CollectionView),
        (status = 404, description = "No such collection"),
    ),
    tag = "collections",
)]
pub async fn amend(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<AmendBody>,
) -> Result<Json<CollectionView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let changed = dam_db::collections::rename(
        conn.executor(),
        id,
        body.label.trim(),
        body.description.as_deref(),
        &body.visibility,
        body.pin_hot,
    )
    .await
    .map_err(|error| match error {
        dam_db::Error::Unsupported(reason) => Failure::Unprocessable(reason),
        other => other.into(),
    })?;
    if !changed {
        return Err(Failure::NotFound);
    }
    let amended = dam_db::collections::all(conn.executor())
        .await?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or(Failure::NotFound)?;
    conn.commit().await?;
    Ok(Json(CollectionView::from(amended)))
}

#[utoipa::path(
    delete,
    path = "/collections/{id}",
    responses(
        (status = 204, description = "Deleted"),
        (status = 409, description = "A portal publishes it", body = String),
        (status = 404, description = "No such collection"),
    ),
    tag = "collections",
)]
pub async fn remove(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let gone = dam_db::collections::delete(conn.executor(), id)
        .await
        .map_err(|error| match error {
            // A portal publishing it is something in the way that the caller can clear, which is what 409 is
            // for. The message names how many.
            dam_db::Error::Unsupported(reason) => Failure::Conflict(reason),
            other => Failure::from(other),
        })?;
    conn.commit().await?;
    if gone {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Failure::NotFound)
    }
}

/// A collection's members, in their curated order.
///
/// Only the ones this caller can see. Reading membership is the mirror of [`add`]: if a scoped curator
/// listed a collection curated by somebody with a wider scope, the ids and the count would tell them about
/// assets they cannot read — the same existence oracle, arrived at from the other side. The positions are
/// the real ones, so a gap in the numbering is the honest signal that the collection holds more than this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ItemView {
    pub asset_id: Uuid,
    pub position: i32,
    pub filename: String,
    pub mime: String,
    /// A short-lived preview link, present only when the asset has a thumbnail derivative *and* delivery is
    /// configured. Curation is visual: reordering a set of photographs by filename is a different and much
    /// worse job than reordering it by looking at them.
    pub thumbnail_url: Option<String>,
}

#[utoipa::path(
    get,
    path = "/collections/{id}/items",
    responses((status = 200, description = "Members in order", body = [ItemView])),
    tag = "collections",
)]
pub async fn items(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ItemView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = dam_db::collections::items(conn.executor(), id).await?;
    let ids: Vec<Uuid> = rows.iter().map(|item| item.asset_id).collect();
    let visible: std::collections::HashSet<Uuid> =
        dam_db::assets::visible_among(conn.executor(), &caller.predicate, &ids)
            .await?
            .into_iter()
            .collect();
    // One query for the whole collection, as on the grid: asking "do you have a thumbnail" once per member
    // would be a round trip per row for something one `= ANY` answers.
    let with_thumbnails =
        dam_db::derivatives::which_have(conn.executor(), &ids, &crate::assets::thumb_op_hash())
            .await?;
    conn.commit().await?;
    Ok(Json(
        rows.into_iter()
            .filter(|item| visible.contains(&item.asset_id))
            .map(|item| ItemView {
                thumbnail_url: with_thumbnails
                    .contains(&item.asset_id)
                    .then(|| {
                        crate::assets::thumbnail_url(
                            state.delivery.as_deref(),
                            &caller,
                            item.asset_id,
                        )
                    })
                    .flatten(),
                asset_id: item.asset_id,
                position: item.position,
                filename: item.filename,
                mime: item.mime,
            })
            .collect(),
    ))
}

/// Assets to add.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct AddBody {
    pub asset_ids: Vec<Uuid>,
}

/// Adds assets, in the order given, ignoring any already present.
///
/// Filtered through the caller's own predicate first. Without that, a scoped curator could add an asset they
/// cannot see to a collection a portal publishes — which would turn a collection into a way to read around
/// ABAC rather than a way to arrange what you can already read.
#[utoipa::path(
    post,
    path = "/collections/{id}/items",
    request_body = AddBody,
    responses(
        (status = 200, description = "How many were added and how many the caller could not see", body = AddedView),
        (status = 404, description = "No such collection"),
    ),
    tag = "collections",
)]
pub async fn add(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<AddBody>,
) -> Result<Json<AddedView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    let visible =
        dam_db::assets::visible_among(conn.executor(), &caller.predicate, &body.asset_ids).await?;
    let out_of_scope = body.asset_ids.len().saturating_sub(visible.len());

    for asset_id in &visible {
        dam_db::collections::add(conn.executor(), id, *asset_id, Some(caller.identity_id)).await?;
    }
    conn.commit().await?;

    Ok(Json(AddedView {
        added: visible.len(),
        out_of_scope,
    }))
}

/// What an add did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AddedView {
    pub added: usize,
    /// Ids the caller cannot see, counted rather than named.
    ///
    /// Counted because naming them would confirm they exist, which is the existence oracle the asset rule
    /// exists to close. Reported at all because "I selected forty and got thirty-eight" needs an answer.
    pub out_of_scope: usize,
}

#[utoipa::path(
    delete,
    path = "/collections/{id}/items/{asset_id}",
    responses(
        (status = 204, description = "Removed, and the positions closed up"),
        (status = 404, description = "Not a member"),
    ),
    tag = "collections",
)]
pub async fn remove_item(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
    Path((id, asset_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let removed = dam_db::collections::remove(conn.executor(), id, asset_id).await?;
    conn.commit().await?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Failure::NotFound)
    }
}

/// Where to move an asset.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct PositionBody {
    /// Clamped rather than refused when out of range — a drag-and-drop that reports "position 47 of 12" is a
    /// UI bug the user cannot act on, and the intent was clearly "the end".
    pub position: i32,
}

#[utoipa::path(
    post,
    path = "/collections/{id}/items/{asset_id}/position",
    request_body = PositionBody,
    responses(
        (status = 200, description = "Moved, with the collection's new order", body = [ItemView]),
        (status = 404, description = "Not a member"),
    ),
    tag = "collections",
)]
pub async fn reorder(
    State(state): State<Arc<CollectionState>>,
    headers: HeaderMap,
    Path((id, asset_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<PositionBody>,
) -> Result<Json<Vec<ItemView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // `move_item` is a no-op for a non-member rather than an error, so membership is checked here — a
    // reorder of something that is not in the collection is a client bug worth reporting rather than a
    // silently successful call.
    let members = dam_db::collections::items(conn.executor(), id).await?;
    if !members.iter().any(|item| item.asset_id == asset_id) {
        return Err(Failure::NotFound);
    }
    dam_db::collections::move_item(conn.executor(), id, asset_id, body.position).await?;
    // The whole order back, because a drag-and-drop needs to reconcile against the truth rather than assume
    // its optimistic update matched what the dense renumber decided.
    let rows = dam_db::collections::items(conn.executor(), id).await?;
    let ids: Vec<Uuid> = rows.iter().map(|item| item.asset_id).collect();
    let visible: std::collections::HashSet<Uuid> =
        dam_db::assets::visible_among(conn.executor(), &caller.predicate, &ids)
            .await?
            .into_iter()
            .collect();
    // One query for the whole collection, as on the grid: asking "do you have a thumbnail" once per member
    // would be a round trip per row for something one `= ANY` answers.
    let with_thumbnails =
        dam_db::derivatives::which_have(conn.executor(), &ids, &crate::assets::thumb_op_hash())
            .await?;
    conn.commit().await?;
    Ok(Json(
        rows.into_iter()
            .filter(|item| visible.contains(&item.asset_id))
            .map(|item| ItemView {
                thumbnail_url: with_thumbnails
                    .contains(&item.asset_id)
                    .then(|| {
                        crate::assets::thumbnail_url(
                            state.delivery.as_deref(),
                            &caller,
                            item.asset_id,
                        )
                    })
                    .flatten(),
                asset_id: item.asset_id,
                position: item.position,
                filename: item.filename,
                mime: item.mime,
            })
            .collect(),
    ))
}
