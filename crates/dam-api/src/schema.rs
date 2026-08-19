//! Schema administration: defining, amending, removing and ordering the tenant's metadata fields.
//!
//! ## Reading is Read, editing is Manage
//!
//! `GET /fields` (in `assets`) is deliberately available to any reader — a schema is not secret and a form
//! cannot be drawn without it. Editing is a different act with a different blast radius: a field definition
//! is what the validator refuses payloads against, what the search renderer decides textual-ness from, what
//! the facet counter enumerates, and what every metadata form is built from. An integration key handed to a
//! website build should be able to read all of that and change none of it.
//!
//! ## The interesting part of every response is the consequence, not the row
//!
//! `dam_db::fields` computes three things the caller cannot: how many assets carry a value under a key, how
//! many would fail their next write because a field just became required, and whether the change makes the
//! search index stale. All three ride on the response. An administrator who is not told that facets are now
//! wrong finds out from a support ticket; one who is not told that forty thousand assets just became
//! unsaveable finds out one 422 at a time.
//!
//! ## Statuses carry the difference between "bad request" and "not in this state"
//!
//! A malformed key or an unknown kind is 422 — the request is wrong. A duplicate key, a taken alias, or a
//! kind locked by stored values is 409 — the request is fine and the *world* refuses it. That distinction is
//! what tells a client whether to fix the form or to show the administrator what is in the way.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, put};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::TenantConn;
use dam_db::fields::{self, Amendment, Catalogued, NewField, SchemaRefusal};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the schema endpoints need.
pub struct SchemaState {
    pub global: PgPool,
}

impl std::fmt::Debug for SchemaState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaState").finish_non_exhaustive()
    }
}

/// The schema-administration routes.
pub fn router(state: SchemaState) -> Router {
    Router::new()
        .route("/schema/fields", get(list).post(define))
        // The order route is registered before the `{key}` one it would otherwise be captured by: axum
        // matches literals ahead of parameters, but keeping them adjacent makes that reliance visible.
        .route("/schema/fields/order", put(reorder))
        .route(
            "/schema/fields/{key}",
            axum::routing::patch(amend).delete(remove),
        )
        .with_state(Arc::new(state))
}

/// A field definition with the numbers an administrator needs before touching it.
#[derive(Debug, Serialize, ToSchema)]
pub struct SchemaField {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub multivalued: bool,
    pub required: bool,
    pub read_only: bool,
    pub ai_writable: bool,
    pub facetable: bool,
    pub searchable: bool,
    pub search_alias: Option<String>,
    pub taxonomy_id: Option<Uuid>,
    /// How many live assets carry a value under this key.
    ///
    /// The number that decides whether an edit is safe, so it is on the row rather than behind another
    /// request: an administrator deciding whether to remove a field should not have to go and ask.
    pub assets_with_values: i64,
}

/// A field to define. Flags default to the conservative reading when omitted.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DefineRequest {
    pub key: String,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub taxonomy_id: Option<Uuid>,
    #[serde(default)]
    pub multivalued: bool,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub read_only: bool,
    /// Defaults to true: a field nobody can search for is the surprising choice, not the safe one.
    #[serde(default = "yes")]
    pub searchable: bool,
    #[serde(default)]
    pub facetable: bool,
    #[serde(default)]
    pub ai_writable: bool,
    #[serde(default)]
    pub search_alias: Option<String>,
    #[serde(default)]
    pub validation: Option<serde_json::Value>,
}

fn yes() -> bool {
    true
}

/// What to change. An omitted member is left alone; `search_alias: null` clears it.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AmendRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default, with = "double_option")]
    pub taxonomy_id: Option<Option<Uuid>>,
    #[serde(default)]
    pub multivalued: Option<bool>,
    #[serde(default)]
    pub required: Option<bool>,
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub searchable: Option<bool>,
    #[serde(default)]
    pub facetable: Option<bool>,
    #[serde(default)]
    pub ai_writable: Option<bool>,
    /// Doubly optional: absent leaves the alias alone, `null` removes it. Those are different intents and
    /// a single `Option` cannot express both.
    #[serde(default, with = "double_option")]
    pub search_alias: Option<Option<String>>,
    #[serde(default)]
    pub validation: Option<serde_json::Value>,
}

/// Distinguishes "absent" from "present and null" in a JSON body.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}

/// An amended field, with the consequences of the amendment.
#[derive(Debug, Serialize, ToSchema)]
pub struct AmendedField {
    #[serde(flatten)]
    pub field: SchemaField,
    /// Whether the search index is now stale and needs rebuilding.
    pub reindex_required: bool,
    /// How many live assets would now fail a metadata write for want of a newly-required value.
    pub assets_now_incomplete: i64,
}

/// What a removal did.
#[derive(Debug, Serialize, ToSchema)]
pub struct RemovedField {
    pub key: String,
    /// How many assets still carry a value under this key. The values are **kept** — see `dam_db`'s
    /// `fields::remove` — so this is what has gone invisible, not what was destroyed.
    pub assets_with_values: i64,
    pub reindex_required: bool,
}

/// The complete field order, in the order fields should appear.
#[derive(Debug, Deserialize, ToSchema)]
pub struct OrderRequest {
    pub keys: Vec<String>,
}

/// Every field definition with its usage counts.
#[utoipa::path(
    get,
    path = "/schema/fields",
    responses(
        (status = 200, body = Vec<SchemaField>),
        (status = 403, description = "Authenticated, and holds no read scope"),
    ),
    tag = "schema",
)]
pub async fn list(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<SchemaField>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let catalogued = fields::catalog(conn.executor()).await?;

    // One counting query per field rather than one per request: the schema is tens of rows, not thousands,
    // and a single grouped query would need the key list in it anyway.
    let mut out = Vec::with_capacity(catalogued.len());
    for def in catalogued {
        let count = fields::usage(conn.executor(), &def.key)
            .await
            .map_err(Refusal)?;
        out.push(present(def, count));
    }
    conn.commit().await?;
    Ok(Json(out))
}

/// Defines a new field.
#[utoipa::path(
    post,
    path = "/schema/fields",
    request_body = DefineRequest,
    responses(
        (status = 201, body = SchemaField),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 409, description = "The key or alias is already taken; `reason` says which"),
        (status = 422, description = "The key or kind is not usable; `reason` says why"),
    ),
    tag = "schema",
)]
pub async fn define(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Json(request): Json<DefineRequest>,
) -> Result<(StatusCode, Json<SchemaField>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let defined = fields::define_on(
        conn.executor(),
        NewField {
            key: request.key,
            label: request.label,
            kind: request.kind,
            taxonomy_id: request.taxonomy_id,
            multivalued: request.multivalued,
            required: request.required,
            read_only: request.read_only,
            searchable: request.searchable,
            facetable: request.facetable,
            ai_writable: request.ai_writable,
            search_alias: request.search_alias,
            validation: request.validation.unwrap_or_else(|| serde_json::json!({})),
        },
    )
    .await
    .map_err(Refusal)?;
    // The count for a brand-new key is not always zero: a field removed earlier leaves its values behind,
    // and re-defining the key adopts them. Reporting the real number is what makes that recoverability
    // visible instead of surprising.
    let count = fields::usage(conn.executor(), &defined.key)
        .await
        .map_err(Refusal)?;
    conn.commit().await?;
    Ok((StatusCode::CREATED, Json(present(defined, count))))
}

/// Amends a field.
#[utoipa::path(
    patch,
    path = "/schema/fields/{key}",
    request_body = AmendRequest,
    responses(
        (status = 200, body = AmendedField),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "No field with that key"),
        (status = 409, description = "Stored values or a taken alias refuse the change"),
        (status = 422, description = "The kind or taxonomy is not usable"),
    ),
    tag = "schema",
)]
pub async fn amend(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(request): Json<AmendRequest>,
) -> Result<Json<AmendedField>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let amended = fields::amend_on(
        conn.executor(),
        &key,
        Amendment {
            label: request.label,
            kind: request.kind,
            taxonomy_id: request.taxonomy_id,
            multivalued: request.multivalued,
            required: request.required,
            read_only: request.read_only,
            searchable: request.searchable,
            facetable: request.facetable,
            ai_writable: request.ai_writable,
            search_alias: request.search_alias,
            validation: request.validation,
        },
    )
    .await
    .map_err(Refusal)?;
    let count = fields::usage(conn.executor(), &key)
        .await
        .map_err(Refusal)?;
    conn.commit().await?;

    Ok(Json(AmendedField {
        field: present(amended.field, count),
        reindex_required: amended.reindex_required,
        assets_now_incomplete: amended.assets_now_incomplete,
    }))
}

/// Removes a field definition, keeping its stored values.
#[utoipa::path(
    delete,
    path = "/schema/fields/{key}",
    responses(
        (status = 200, body = RemovedField),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "No field with that key"),
    ),
    tag = "schema",
)]
pub async fn remove(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Json<RemovedField>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let removed = fields::remove_on(conn.executor(), &key)
        .await
        .map_err(Refusal)?;
    conn.commit().await?;

    Ok(Json(RemovedField {
        key: removed.key,
        assets_with_values: removed.assets_with_values,
        reindex_required: removed.reindex_required,
    }))
}

/// Sets the complete field order.
#[utoipa::path(
    put,
    path = "/schema/fields/order",
    request_body = OrderRequest,
    responses(
        (status = 204, description = "Reordered"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 422, description = "The list does not name every field exactly once"),
    ),
    tag = "schema",
)]
pub async fn reorder(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Json(request): Json<OrderRequest>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    fields::reorder_on(conn.executor(), &request.keys)
        .await
        .map_err(Refusal)?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn present(def: Catalogued, assets_with_values: i64) -> SchemaField {
    SchemaField {
        key: def.key,
        label: def.label,
        kind: def.kind,
        multivalued: def.multivalued,
        required: def.required,
        read_only: def.read_only,
        ai_writable: def.ai_writable,
        facetable: def.facetable,
        searchable: def.searchable,
        search_alias: def.search_alias,
        taxonomy_id: def.taxonomy_id,
        assets_with_values,
    }
}

/// Wraps a [`SchemaRefusal`] so the status mapping lives in one place.
struct Refusal(SchemaRefusal);

impl From<Refusal> for Failure {
    fn from(Refusal(refusal): Refusal) -> Self {
        // The split that matters to a client: 422 means "fix the request", 409 means "the request is fine
        // and something in the world is in the way". Both carry the refusal's own sentence, because these
        // reach an administrator in a form and each one names its own fix.
        match refusal {
            SchemaRefusal::UnknownField(_) => Self::NotFound,
            SchemaRefusal::DuplicateKey(_)
            | SchemaRefusal::DuplicateAlias(_)
            | SchemaRefusal::KindLockedByValues { .. } => Self::Conflict(refusal.to_string()),
            SchemaRefusal::BadKey { .. }
            | SchemaRefusal::ReservedKey(_)
            | SchemaRefusal::UnknownKind(_)
            | SchemaRefusal::TaxonomyRequired
            | SchemaRefusal::UnknownTaxonomy(_)
            | SchemaRefusal::IncompleteOrder { .. } => Self::Unprocessable(refusal.to_string()),
            SchemaRefusal::Database(error) => error.into(),
        }
    }
}
