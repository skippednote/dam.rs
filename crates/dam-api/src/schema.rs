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
use dam_db::metadata_types;
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
        // The refine-search rail (Q.19): what the filter panel offers, and in what order.
        .route("/schema/facets", get(list_facets).put(set_facets))
        .route("/schema/types", get(list_types).post(define_type))
        .route(
            "/schema/types/{id}",
            axum::routing::patch(amend_type).delete(remove_type),
        )
        // On the asset rather than under `/schema`, because it is a property of the asset: which form it
        // gets, not what forms exist.
        .route(
            "/assets/{asset_id}/metadata-type",
            get(read_asset_type).put(set_asset_type),
        )
        .with_state(Arc::new(state))
}

/// One thing the refine-search rail can show, and whether it does (Q.19).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RailEntry {
    /// `field:<key>`, `taxonomy:<uuid>` or `builtin:<name>` — the kind and the name, because a vocabulary
    /// called `brand` and a field called `brand` are different entries.
    pub entry: String,
    /// What to show an administrator. A field's own label, a vocabulary's, or the built-in's name.
    pub label: String,
    /// `field`, `taxonomy` or `builtin`, so a screen can group them.
    pub kind: String,
    pub is_enabled: bool,
}

/// The ordered list of entries the rail should offer.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RailRequest {
    /// Enabled entries, in the order they should appear. Anything the rail could show and this list omits is
    /// recorded as *disabled* rather than forgotten — see `dam_db::rail::replace`.
    pub enabled: Vec<String>,
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

/// Everything the rail could show, with the tenant's configuration applied (Q.19).
///
/// The candidate list is derived rather than stored, for the same reason `rail` stores no defaults: a field
/// defined after somebody last touched this screen has to appear, and a stored list would have gone stale
/// silently. So this reads the schema, the vocabularies and the four built-ins every time, then answers with
/// each one's state.
#[utoipa::path(
    get,
    path = "/schema/facets",
    responses(
        (status = 200, description = "Every entry the rail can show, in the order it will", body = Vec<RailEntry>),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "schema",
)]
pub async fn list_facets(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<RailEntry>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let entries = rail_candidates(&mut conn).await?;
    let configured = dam_db::rail::read(conn.executor()).await?;
    conn.commit().await?;

    // Ordered as the rail will order it, so this screen is a preview rather than a list to cross-reference.
    let pairs: Vec<(String, RailEntry)> = entries
        .into_iter()
        .map(|entry| (entry.entry.clone(), entry))
        .collect();
    let mut arranged = dam_db::rail::arrange(&pairs, &configured);
    // `arrange` drops what is disabled, because that is what the rail wants. This screen needs the disabled
    // ones too — you cannot re-enable what you cannot see — so they come back at the end, marked.
    let shown: Vec<String> = arranged.iter().map(|entry| entry.entry.clone()).collect();
    for (entry, mut candidate) in pairs {
        if !shown.contains(&entry) {
            candidate.is_enabled = false;
            arranged.push(candidate);
        }
    }
    Ok(Json(arranged))
}

/// Replaces the rail's configuration.
#[utoipa::path(
    put,
    path = "/schema/facets",
    request_body = RailRequest,
    responses(
        (status = 204, description = "Stored"),
        (status = 403, description = "The caller holds no manage scope"),
        (status = 422, description = "An entry names something the rail cannot show"),
    ),
    tag = "schema",
)]
pub async fn set_facets(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Json(request): Json<RailRequest>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let candidates = rail_candidates(&mut conn).await?;
    let known: Vec<String> = candidates.iter().map(|entry| entry.entry.clone()).collect();

    // An entry the rail cannot show is refused rather than stored. A typo'd key written to the table would be
    // a row that matches nothing — invisible, and it would silently take the position an administrator meant
    // for something real.
    if let Some(unknown) = request.enabled.iter().find(|one| !known.contains(one)) {
        conn.commit().await?;
        return Err(Failure::Unprocessable(format!(
            "`{unknown}` is not something the rail can show; ask GET /schema/facets for the list"
        )));
    }

    dam_db::rail::replace(conn.executor(), &request.enabled, &known).await?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Everything the rail could show, before configuration.
async fn rail_candidates(conn: &mut TenantConn<'_>) -> Result<Vec<RailEntry>, Failure> {
    // The *catalogue* rather than the definitions, because this screen is for a person: `FieldDef` carries the
    // validation shape and no display name, so a rail built from it reads `colours` where the tenant wrote
    // "Colours". Found by looking at the screen.
    let catalogued = dam_db::fields::catalog(conn.executor()).await?;
    let mut entries: Vec<RailEntry> = catalogued
        .iter()
        .filter(|def| def.facetable)
        .map(|def| RailEntry {
            entry: format!("field:{}", def.key),
            label: def.label.clone(),
            kind: "field".to_owned(),
            is_enabled: true,
        })
        .collect();

    let taxonomies: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT id, label FROM taxonomies WHERE kind <> 'category' ORDER BY label")
            .fetch_all(conn.executor())
            .await
            .map_err(dam_db::Error::from)?;
    entries.extend(taxonomies.into_iter().map(|(id, label)| RailEntry {
        entry: format!("taxonomy:{id}"),
        label,
        kind: "taxonomy".to_owned(),
        is_enabled: true,
    }));

    entries.extend(
        dam_db::facets::Builtin::ALL
            .into_iter()
            .map(|builtin| RailEntry {
                entry: format!("builtin:{}", builtin.key()),
                label: builtin.key().to_owned(),
                kind: "builtin".to_owned(),
                is_enabled: true,
            }),
    );
    Ok(entries)
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

// ---------------------------------------------------------------------------------------------------
// Metadata types (Q.1b)
// ---------------------------------------------------------------------------------------------------

/// A metadata type, with the count that decides whether editing it is safe.
#[derive(Debug, Serialize, ToSchema)]
pub struct MetadataTypeRow {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    /// The media classes this type is the natural choice for at ingest.
    pub applies_to: Vec<String>,
    pub is_default: bool,
    /// Its fields, in its own order.
    pub field_keys: Vec<String>,
    /// How many live assets currently carry this type.
    ///
    /// On the row for the same reason a field's usage count is: removing a type re-forms every asset that
    /// referenced it, and an administrator should not have to go and ask how many that is.
    pub assets: i64,
}

/// A metadata type to create.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DefineTypeRequest {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub field_keys: Vec<String>,
}

/// What to change about a type. An omitted member is left alone.
///
/// `field_keys` replaces the list wholesale rather than merging: a type's fields are an ordered list, and
/// "add this one" computed against a client's stale copy would silently drop whatever it had not seen.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AmendTypeRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub applies_to: Option<Vec<String>>,
    #[serde(default)]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub field_keys: Option<Vec<String>>,
}

/// Which type an asset has, and the form that follows from it.
#[derive(Debug, Serialize, ToSchema)]
pub struct AssetTypeView {
    /// `None` means the asset falls back — to the tenant's default type, or to the whole vocabulary when
    /// there is no default. Either way it still has a form; see `dam_db::metadata_types`.
    pub metadata_type_id: Option<Uuid>,
    pub metadata_type_key: Option<String>,
    /// The fields that apply *now*, in order — the resolved answer rather than the stored pointer, because
    /// this is what the client has to draw.
    pub field_keys: Vec<String>,
}

/// What to set an asset's type to. `null` clears it, which is different from omitting the member.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetAssetTypeRequest {
    pub metadata_type_id: Option<Uuid>,
}

/// Every metadata type, in display order.
#[utoipa::path(
    get,
    path = "/schema/types",
    responses((status = 200, body = Vec<MetadataTypeRow>)),
    tag = "schema",
)]
pub async fn list_types(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<MetadataTypeRow>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let types = metadata_types::list_on(conn.executor())
        .await
        .map_err(TypeFailure)?;

    let mut out = Vec::with_capacity(types.len());
    for kind in types {
        let assets: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM assets WHERE metadata_type_id = $1 AND deleted_at IS NULL",
        )
        .bind(kind.id)
        .fetch_one(conn.executor())
        .await
        .map_err(dam_db::Error::from)?;
        out.push(present_type(kind, assets));
    }
    conn.commit().await?;
    Ok(Json(out))
}

/// Creates a metadata type.
#[utoipa::path(
    post,
    path = "/schema/types",
    request_body = DefineTypeRequest,
    responses(
        (status = 201, body = MetadataTypeRow),
        (status = 409, description = "The key is already taken"),
        (status = 422, description = "A named field does not exist"),
    ),
    tag = "schema",
)]
pub async fn define_type(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Json(request): Json<DefineTypeRequest>,
) -> Result<(StatusCode, Json<MetadataTypeRow>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let created = metadata_types::define_on(
        conn.executor(),
        metadata_types::NewType {
            key: request.key,
            label: request.label,
            applies_to: request.applies_to,
            is_default: request.is_default,
            field_keys: request.field_keys,
        },
    )
    .await
    .map_err(TypeFailure)?;
    conn.commit().await?;
    // Freshly created, so nothing can carry it yet.
    Ok((StatusCode::CREATED, Json(present_type(created, 0))))
}

/// Amends a metadata type.
#[utoipa::path(
    patch,
    path = "/schema/types/{id}",
    request_body = AmendTypeRequest,
    responses(
        (status = 200, body = MetadataTypeRow),
        (status = 404, description = "No such type"),
        (status = 422, description = "A named field does not exist"),
    ),
    tag = "schema",
)]
pub async fn amend_type(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<AmendTypeRequest>,
) -> Result<Json<MetadataTypeRow>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    metadata_types::amend_on(
        conn.executor(),
        id,
        metadata_types::Amendment {
            label: request.label,
            applies_to: request.applies_to,
            field_keys: request.field_keys,
        },
    )
    .await
    .map_err(TypeFailure)?;
    // Separate from the amendment because it is a different kind of change: the default is a property of the
    // *set* of types, and setting it moves the flag off whoever held it.
    if request.is_default == Some(true) {
        metadata_types::set_default_on(conn.executor(), id)
            .await
            .map_err(TypeFailure)?;
    }

    let updated = metadata_types::load_on(conn.executor(), id)
        .await
        .map_err(TypeFailure)?;
    let assets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assets WHERE metadata_type_id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;
    Ok(Json(present_type(updated, assets)))
}

/// Removes a metadata type. Assets carrying it fall back rather than being blocked or orphaned.
#[utoipa::path(
    delete,
    path = "/schema/types/{id}",
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "No such type"),
    ),
    tag = "schema",
)]
pub async fn remove_type(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    metadata_types::remove_on(conn.executor(), id)
        .await
        .map_err(TypeFailure)?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The type an asset has, and the form that follows from it.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/metadata-type",
    responses((status = 200, body = AssetTypeView)),
    tag = "schema",
)]
pub async fn read_asset_type(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<AssetTypeView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let view = resolve_asset_type(conn.executor(), asset_id).await?;
    conn.commit().await?;
    Ok(Json(view))
}

/// Sets or clears an asset's metadata type.
#[utoipa::path(
    put,
    path = "/assets/{asset_id}/metadata-type",
    request_body = SetAssetTypeRequest,
    responses(
        (status = 200, body = AssetTypeView),
        (status = 422, description = "No such type"),
    ),
    tag = "schema",
)]
pub async fn set_asset_type(
    State(state): State<Arc<SchemaState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(request): Json<SetAssetTypeRequest>,
) -> Result<Json<AssetTypeView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // Checked before the write rather than left to the foreign key, so "set this to a type I invented" is a
    // named refusal. Letting it through as a clear would hide the mistake behind a plausible outcome: the
    // asset would silently fall back and the caller would believe the type had been applied.
    if let Some(id) = request.metadata_type_id {
        // 422, not the 404 the shared mapping would give: the *path* addresses the asset and the asset
        // exists, so nothing here is missing — the body names a type that is not real, which is the request
        // being wrong. `DELETE /schema/types/{id}` on the same refusal is a 404 precisely because there the
        // id is what was addressed. Letting it through as a clear would be worse than either: the asset would
        // fall back and the caller would believe the type had been applied.
        metadata_types::load_on(conn.executor(), id)
            .await
            .map_err(|refusal| match refusal {
                metadata_types::TypeRefusal::UnknownType(id) => {
                    Failure::Unprocessable(format!("no metadata type {id} exists"))
                }
                other => TypeFailure(other).into(),
            })?;
    }
    metadata_types::assign_on(conn.executor(), asset_id, request.metadata_type_id)
        .await
        .map_err(TypeFailure)?;

    // The resolved form comes back, because that is what the client has to redraw. A 204 would make the
    // caller either guess or re-fetch, and guessing is how a form drifts from the schema.
    let view = resolve_asset_type(conn.executor(), asset_id).await?;
    conn.commit().await?;
    Ok(Json(view))
}

async fn resolve_asset_type(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
) -> Result<AssetTypeView, Failure> {
    let stored: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT metadata_type_id FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?;
    let Some(stored) = stored else {
        return Err(Failure::NotFound);
    };

    let key = match stored {
        Some(id) => Some(
            metadata_types::load_on(&mut *conn, id)
                .await
                .map_err(TypeFailure)?
                .key,
        ),
        None => None,
    };
    let field_keys = metadata_types::fields_for_on(&mut *conn, asset_id)
        .await
        .map_err(TypeFailure)?
        .into_iter()
        .map(|def| def.key)
        .collect();

    Ok(AssetTypeView {
        metadata_type_id: stored,
        metadata_type_key: key,
        field_keys,
    })
}

fn present_type(kind: metadata_types::MetadataType, assets: i64) -> MetadataTypeRow {
    MetadataTypeRow {
        id: kind.id,
        key: kind.key,
        label: kind.label,
        applies_to: kind.applies_to,
        is_default: kind.is_default,
        field_keys: kind.field_keys,
        assets,
    }
}

/// Maps a [`metadata_types::TypeRefusal`] onto a status, same split as the field refusals.
struct TypeFailure(metadata_types::TypeRefusal);

impl From<TypeFailure> for Failure {
    fn from(TypeFailure(refusal): TypeFailure) -> Self {
        use metadata_types::TypeRefusal as R;
        match refusal {
            R::UnknownType(_) => Self::NotFound,
            R::DuplicateKey(_) => Self::Conflict(refusal.to_string()),
            R::UnknownField(_) => Self::Unprocessable(refusal.to_string()),
            R::Database(error) => error.into(),
        }
    }
}
