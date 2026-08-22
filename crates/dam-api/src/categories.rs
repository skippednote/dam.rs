//! Categories over HTTP: the browse tree, and filing assets into it (Q.2b).
//!
//! ## Reading is Read, filing is Manage
//!
//! A browse tree is not secret — nobody can navigate a library without it — so listing trees and reading one
//! with its counts needs only `Read`. Everything that changes where an asset lives, or what the tree looks
//! like, needs `Manage`: re-filing somebody else's library is a content change, not a view preference.
//!
//! ## Every number here is the caller's own
//!
//! §7 says counts disclose. A rollup that reported the true total would tell a group-scoped caller exactly how
//! large the part of the library they cannot reach is — so both the tree counts and the uncategorised worklist
//! run through the caller's own predicate. The consequence is that two people can legitimately see different
//! numbers on the same branch, which is correct and worth knowing when reading a bug report.
//!
//! ## A tree that is not a tree answers 404
//!
//! `taxonomies` holds vocabularies and product attributes as well as category trees. Asking for a vocabulary
//! by id gets a 404 rather than a 422: from the caller's side there is no such tree, and saying "that is a
//! vocabulary" would confirm the id exists for some other purpose.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_core::query::{Planned, Query};
use dam_db::TenantConn;
use dam_db::categories::{self, CategoryRefusal, NewCategory};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// How many uncategorised assets to name in the worklist response.
///
/// A sample, not the list: the count is the number an administrator acts on, and returning forty thousand ids
/// to render a "61 uncategorised" link would make the cheap query expensive.
const WORKLIST_SAMPLE: i64 = 50;

/// What the category endpoints need.
pub struct CategoryState {
    pub global: PgPool,
}

impl std::fmt::Debug for CategoryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CategoryState").finish_non_exhaustive()
    }
}

/// The category routes.
pub fn router(state: CategoryState) -> Router {
    Router::new()
        .route("/categories", get(list_trees).post(create_tree))
        // Registered before `/categories/{tree_id}` so the literal wins; axum prefers literals anyway, but
        // adjacency makes the reliance visible.
        .route("/categories/{tree_id}/uncategorised", get(uncategorised))
        .route("/categories/{tree_id}/nodes", post(create_node))
        .route("/categories/{tree_id}", get(read_tree))
        .route("/assets/{asset_id}/categories", get(of_asset))
        .route(
            "/assets/{asset_id}/categories/{category_id}",
            put(file).delete(unfile),
        )
        .with_state(Arc::new(state))
}

/// A category tree.
#[derive(Debug, Serialize, ToSchema)]
pub struct TreeRow {
    pub id: Uuid,
    pub key: String,
    pub label: String,
}

/// One node, with the count of assets the caller can see at or beneath it.
#[derive(Debug, Serialize, ToSchema)]
pub struct NodeRow {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    /// The ltree path. Exposed because the descendant filter uses it, and a client linking to a subtree
    /// should not need another round trip to learn it.
    pub path: String,
    pub slug: String,
    pub label: String,
    /// Depth, so a client indents without parsing `path`.
    pub depth: usize,
    pub retired: bool,
    /// Distinct assets **this caller** can see in this category or any beneath it.
    pub assets: i64,
}

/// A tree to create.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateTreeRequest {
    pub key: String,
    pub label: String,
}

/// A category to create within a tree.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateNodeRequest {
    pub slug: String,
    pub label: String,
    /// `None` makes it a root.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

/// How many assets are in no category, and some of them.
#[derive(Debug, Serialize, ToSchema)]
pub struct Worklist {
    /// Every uncategorised asset this caller can see.
    pub total: i64,
    /// The first few, so a client can link straight into them.
    pub sample: Vec<Uuid>,
}

/// Every category tree.
#[utoipa::path(
    get,
    path = "/categories",
    responses((status = 200, body = Vec<TreeRow>)),
    tag = "categories",
)]
pub async fn list_trees(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<TreeRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let trees = categories::trees(conn.executor()).await.map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(
        trees
            .into_iter()
            .map(|tree| TreeRow {
                id: tree.id,
                key: tree.key,
                label: tree.label,
            })
            .collect(),
    ))
}

/// Creates a category tree.
#[utoipa::path(
    post,
    path = "/categories",
    request_body = CreateTreeRequest,
    responses(
        (status = 201, body = TreeRow),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "categories",
)]
pub async fn create_tree(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
    Json(request): Json<CreateTreeRequest>,
) -> Result<(StatusCode, Json<TreeRow>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let id = categories::create_tree(conn.executor(), &request.key, &request.label)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(TreeRow {
            id,
            key: request.key,
            label: request.label,
        }),
    ))
}

/// One tree, with the caller's own counts.
#[utoipa::path(
    get,
    path = "/categories/{tree_id}",
    responses(
        (status = 200, body = Vec<NodeRow>),
        (status = 404, description = "No such category tree"),
    ),
    tag = "categories",
)]
pub async fn read_tree(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
    Path(tree_id): Path<Uuid>,
) -> Result<Json<Vec<NodeRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // `Query::All` under the caller's predicate: the tree is browse chrome, so it counts everything the
    // caller may see rather than everything matching some query they have not typed yet. A rail that narrowed
    // itself to the active search belongs to the search endpoint, which already returns facets.
    let planned =
        Planned::new(Query::All, caller.predicate.clone(), &[]).map_err(|_| Failure::Internal)?;
    let nodes = categories::tree_with_counts(conn.executor(), tree_id, &planned)
        .await
        .map_err(Refused)?;
    conn.commit().await?;

    Ok(Json(nodes.into_iter().map(present).collect()))
}

/// Creates a category within a tree.
#[utoipa::path(
    post,
    path = "/categories/{tree_id}/nodes",
    request_body = CreateNodeRequest,
    responses(
        (status = 201, body = NodeRow),
        (status = 404, description = "No such category tree"),
        (status = 409, description = "A sibling already uses that slug"),
    ),
    tag = "categories",
)]
pub async fn create_node(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
    Path(tree_id): Path<Uuid>,
    Json(request): Json<CreateNodeRequest>,
) -> Result<(StatusCode, Json<NodeRow>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let id = categories::create(
        conn.executor(),
        NewCategory {
            taxonomy_id: tree_id,
            parent_id: request.parent_id,
            slug: request.slug,
            label: request.label,
        },
    )
    .await
    .map_err(Refused)?;

    // Read back rather than echoed: the path and depth are derived from the parent, and a client that had to
    // compute them would be the second place that logic lives.
    let created = categories::by_id(conn.executor(), id)
        .await
        .map_err(Refused)?
        .ok_or(Failure::Internal)?;
    conn.commit().await?;
    Ok((StatusCode::CREATED, Json(present_uncounted(created))))
}

/// The categories an asset is filed in.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/categories",
    responses((status = 200, body = Vec<NodeRow>)),
    tag = "categories",
)]
pub async fn of_asset(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Vec<NodeRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let nodes = categories::of_asset(conn.executor(), asset_id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(nodes.into_iter().map(present_uncounted).collect()))
}

/// Files an asset in a category.
#[utoipa::path(
    put,
    path = "/assets/{asset_id}/categories/{category_id}",
    responses(
        (status = 200, description = "The asset's categories after filing", body = Vec<NodeRow>),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 422, description = "That category does not exist, or is retired"),
    ),
    tag = "categories",
)]
pub async fn file(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
    Path((asset_id, category_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<NodeRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    categories::file(conn.executor(), asset_id, category_id, caller.identity_id)
        .await
        // 422 rather than the shared 404: the path addresses an asset that exists, and the category segment
        // names something unreal or retired — the request is wrong rather than the target missing.
        .map_err(|refusal| match refusal {
            CategoryRefusal::UnknownCategory(_) | CategoryRefusal::Retired(_) => {
                Failure::Unprocessable(refusal.to_string())
            }
            other => Refused(other).into(),
        })?;
    let nodes = categories::of_asset(conn.executor(), asset_id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(nodes.into_iter().map(present_uncounted).collect()))
}

/// Takes an asset out of a category.
#[utoipa::path(
    delete,
    path = "/assets/{asset_id}/categories/{category_id}",
    responses(
        (status = 200, description = "The asset's categories after unfiling", body = Vec<NodeRow>),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "categories",
)]
pub async fn unfile(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
    Path((asset_id, category_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<NodeRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    categories::unfile(conn.executor(), asset_id, category_id)
        .await
        .map_err(Refused)?;
    let nodes = categories::of_asset(conn.executor(), asset_id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(nodes.into_iter().map(present_uncounted).collect()))
}

/// How many of the caller's assets are in no category of this tree.
#[utoipa::path(
    get,
    path = "/categories/{tree_id}/uncategorised",
    responses(
        (status = 200, body = Worklist),
        (status = 404, description = "No such category tree"),
    ),
    tag = "categories",
)]
pub async fn uncategorised(
    State(state): State<Arc<CategoryState>>,
    headers: HeaderMap,
    Path(tree_id): Path<Uuid>,
) -> Result<Json<Worklist>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let planned =
        Planned::new(Query::All, caller.predicate.clone(), &[]).map_err(|_| Failure::Internal)?;
    let (total, sample) =
        categories::uncategorised(conn.executor(), tree_id, &planned, WORKLIST_SAMPLE)
            .await
            .map_err(Refused)?;
    conn.commit().await?;
    Ok(Json(Worklist { total, sample }))
}

fn present(node: categories::CountedCategory) -> NodeRow {
    NodeRow {
        id: node.id,
        parent_id: node.parent_id,
        path: node.path,
        slug: node.slug,
        label: node.label,
        depth: node.depth,
        retired: node.retired,
        assets: node.assets,
    }
}

/// A node in a context where a count would be meaningless — a freshly created category, or the list on one
/// asset. Reported as 0 rather than omitted, so the shape is one type rather than two.
fn present_uncounted(node: categories::CategoryNode) -> NodeRow {
    NodeRow {
        id: node.id,
        parent_id: node.parent_id,
        path: node.path,
        slug: node.slug,
        label: node.label,
        depth: node.depth,
        retired: node.retired,
        assets: 0,
    }
}

/// Maps a [`CategoryRefusal`] onto a status.
struct Refused(CategoryRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            // Both answer 404: from the caller's side there is no such tree, and distinguishing "that is a
            // vocabulary" would confirm the id exists for another purpose.
            CategoryRefusal::NotACategoryTree(_) => Self::NotFound,
            CategoryRefusal::UnknownCategory(_) => Self::NotFound,
            CategoryRefusal::Retired(_) => Self::Unprocessable(refusal.to_string()),
            CategoryRefusal::DuplicatePath(_) => Self::Conflict(refusal.to_string()),
            CategoryRefusal::Database(error) => error.into(),
        }
    }
}
