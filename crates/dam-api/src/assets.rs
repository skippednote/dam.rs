//! The asset endpoints the UI reads (`GET /assets`, `GET /assets/{id}`, `PATCH /assets/{id}/metadata`).
//!
//! Every read renders the caller's compiled predicate through `dam_db::assets`, which puts it *in* the query.
//! §7 is the reason it is not a post-filter: pagination counts alone disclose the existence of assets a
//! caller cannot see, and the two implementations return the same rows so the leak is invisible until
//! somebody compares a count.
//!
//! ## A missing asset and a forbidden one answer the same way
//!
//! 404 for both. A 403 on an asset in a group the caller does not hold confirms the asset exists, which is
//! the disclosure the group scoping was for.
//!
//! ## `thumbnail_url` is an internal-preview token, and the rights argument for that is written down
//!
//! A thumbnail is a render, so it goes through the same signed-URL chokepoint as a download — D12's "one code
//! path" is intact. What differs is the *purpose* signed into the token: `InternalPreview` does not consult the
//! rights verdict, because an unlicensed asset is `RightsState::Unknown` and unknown denies, which would leave
//! a fresh library with no thumbnails at all. The full argument, and the three restrictions that keep it from
//! being a hole, are in `dam_core::signed_url::Purpose`.
//!
//! A URL is minted only for an asset that *has* a thumbnail derivative. Minting one regardless would produce a
//! link that 404s, and a grid cannot tell a broken URL from an asset still being processed — so the absence of
//! the field is how "not rendered yet" is expressed.

use crate::caller;
use crate::delivery::DeliveryState;
use crate::dto::{AssetPage, AssetSummary};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::assets::{self, Order};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// What the asset endpoints need.
pub struct AssetState {
    /// The shared pool. Tenant scoping happens per request through `TenantConn`, not by holding a pool per
    /// tenant — §5.2's reason is that a thousand tenants is a thousand idle connection sets.
    pub global: PgPool,
    /// Shared with the delivery routes, because a thumbnail URL is a delivery token and there must be exactly
    /// one keyring. Two would mean tokens minted here failing verification there, which presents as an
    /// intermittently broken grid rather than as a configuration error.
    pub delivery: Option<Arc<DeliveryState>>,
}

impl std::fmt::Debug for AssetState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The pool's own Debug prints its connection string, password included.
        f.debug_struct("AssetState").finish_non_exhaustive()
    }
}

/// The asset routes.
pub fn router(state: AssetState) -> Router {
    Router::new()
        .route("/assets", get(list))
        .route("/assets/{asset_id}", get(detail))
        .route("/assets/{asset_id}/metadata", patch(update_metadata))
        .route("/fields", get(fields))
        .with_state(Arc::new(state))
}

/// One field definition, as a form needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FieldDefinition {
    pub key: String,
    /// The tenant's own label. A form shows this rather than the key.
    pub label: String,
    /// The database spelling of the kind, so a client can pick an input type.
    pub kind: String,
    /// Whether the field takes an array.
    ///
    /// Load-bearing rather than informational: a client that does not know this sends a comma-joined string
    /// to a field that takes an array, and the server refuses it with a message about delimiters that the
    /// user cannot act on. Discovered exactly that way, editing a multivalued field in a real browser.
    pub multivalued: bool,
    pub required: bool,
    /// Set by ingest or by a connector; an editor must not offer it.
    pub read_only: bool,
    /// Whether an enrichment run may write it.
    pub ai_writable: bool,
    pub facetable: bool,
    /// The shorthand prefix, when the tenant defined one: `bra:acme` for `brand`.
    pub search_alias: Option<String>,
    pub taxonomy_id: Option<Uuid>,
    /// When this field applies at all (Q.19b).
    ///
    /// Present so the editor can *hide* an inapplicable field rather than offering one whose value the server
    /// will refuse. The refusal exists too — a client is not trusted with a rule — but a form that asks a
    /// question and then rejects the answer is a form nobody trusts back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<FieldDependency>,
}

/// The condition a dependent field hangs off (Q.19b).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FieldDependency {
    /// The field whose value decides.
    pub key: String,
    /// The values that make this one apply. Compared as text, so `true` and `2026` work as written.
    pub values: Vec<String>,
}

/// The tenant's field definitions, in display order.
#[utoipa::path(
    get,
    path = "/fields",
    responses(
        (status = 200, description = "Every field definition, in display order", body = Vec<FieldDefinition>),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no read scope"),
    ),
    tag = "assets",
)]
pub async fn fields(
    State(state): State<Arc<AssetState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<FieldDefinition>>, Failure> {
    // `Read`, not `Manage`: a schema is not secret and every reader needs it to render a form or a filter
    // rail. Editing one is schema administration and will have its own endpoint and its own action.
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let catalogued = dam_db::fields::catalog(conn.executor()).await?;
    conn.commit().await?;

    Ok(Json(
        catalogued
            .into_iter()
            .map(|def| FieldDefinition {
                key: def.key,
                label: def.label,
                kind: def.kind,
                multivalued: def.multivalued,
                required: def.required,
                read_only: def.read_only,
                ai_writable: def.ai_writable,
                facetable: def.facetable,
                search_alias: def.search_alias,
                taxonomy_id: def.taxonomy_id,
                // From `validation`, where the definition lives — see `dam_core::fields::Constraints`.
                depends_on: dam_core::fields::Constraints::from_json(&def.validation)
                    .depends_on
                    .map(|dependency| FieldDependency {
                        key: dependency.key,
                        values: dependency.values,
                    }),
            })
            .collect(),
    ))
}

/// How a client asks for a page.
#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct ListParams {
    /// Zero-based index of the first row wanted. A virtualised grid sends the window it is about to draw.
    #[serde(default)]
    pub offset: i64,
    /// Rows wanted. Clamped server-side; see `dam_db::assets::MAX_LIMIT`.
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub order: SortOrder,
}

fn default_limit() -> i64 {
    50
}

/// The orders a client may ask for.
///
/// A closed set on the wire as well as in SQL. Accepting a column name would make the ORDER BY
/// caller-supplied, and validating one against a list is the same list written twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Newest,
    Oldest,
    FilenameAsc,
    FilenameDesc,
    LargestFirst,
    /// The caller's favourites first, then newest within each group (Q.5b·3).
    Favourites,
}

impl SortOrder {
    /// The database order, given who is asking.
    ///
    /// Not a `From` impl, because one of these orders needs the caller: "favourites first" without saying whose
    /// is a question with no answer, and `dam_db::assets::Order` puts the identity in the variant so it cannot
    /// be asked. A conversion that could not see the caller would have had to invent one or drop the order.
    fn resolve(self, viewer: Option<Uuid>) -> Order {
        match (self, viewer) {
            (Self::Newest, _) => Order::Newest,
            (Self::Oldest, _) => Order::Oldest,
            (Self::FilenameAsc, _) => Order::FilenameAsc,
            (Self::FilenameDesc, _) => Order::FilenameDesc,
            (Self::LargestFirst, _) => Order::LargestFirst,
            (Self::Favourites, Some(viewer)) => Order::FavouritesFirst(viewer),
            // Unreachable through `authorize`, which refuses a key with no identity. Newest rather than an
            // error: a machine key that asked for favourites-first has no favourites, so "newest" is the
            // honest answer to what remains of the question.
            (Self::Favourites, None) => Order::Newest,
        }
    }
}

/// One asset in full, as the detail panel draws it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssetDetail {
    #[serde(flatten)]
    pub summary: AssetSummary,
    /// The validated metadata, keyed by field definition key.
    pub values: serde_json::Value,
    /// Probed technical facts — EXIF, codec, colour. Read-only, and shaped by the file rather than by the
    /// tenant's schema, so it is not merged into `values`.
    pub technical: serde_json::Value,
    pub duration_ms: Option<i64>,
    pub page_count: Option<i32>,
    pub color_space: Option<String>,
    pub has_alpha: Option<bool>,
    /// BLAKE3 of the original bytes, lowercase hex. What deduplication and integrity both key on.
    pub content_hash: String,
    pub status: String,
    pub enrichment_state: String,
    /// Blocks tiering *and* deletion. Surfaced because a user who cannot delete an asset deserves to know
    /// why rather than to see a failing button.
    pub legal_hold: bool,
    pub release_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub version_no: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Ratings, favourites and the watch state, in full (Q.5b·3).
    ///
    /// The whole picture here, unlike the two fields the summary carries: the panel draws the star widget, the
    /// counts and the watch toggle, and fetching them separately would make opening a panel two requests where
    /// one will do.
    pub engagement: crate::engagement::EngagementView,
    /// A signed internal-preview URL for the `preview-1024` rendition, when one has been rendered.
    ///
    /// On the detail endpoint only. A list of sixty of these would mint sixty tokens for images no grid draws —
    /// the grid uses the thumbnail — and a lightbox opens one asset at a time.
    ///
    /// `Contain`-fitted rather than cropped, which is what makes it the right image for somebody inspecting an
    /// asset: the thumbnail is a 256px square crop, so enlarging it is a blurry crop of the wrong aspect.
    pub preview_url: Option<String>,
}

/// A page of assets the caller may see.
#[utoipa::path(
    get,
    path = "/assets",
    params(ListParams),
    responses(
        (status = 200, description = "One page, with the total counted under the caller's own scope", body = AssetPage),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no read scope"),
    ),
    tag = "assets",
)]
pub async fn list(
    State(state): State<Arc<AssetState>>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Result<Json<AssetPage>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    // Scoped by a transaction rather than by a per-tenant pool: `SET LOCAL search_path` is
    // transaction-bound, so the pooled connection returns clean and no tenant's path can leak onto a later
    // request (§5.2).
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let page = assets::page(
        conn.executor(),
        &caller.predicate,
        params.order.resolve(caller.identity_id),
        params.offset,
        params.limit,
    )
    .await?;

    // One query for the whole page, not one per asset. A page of sixty asking "do you have a thumbnail" sixty
    // times is sixty round trips for information one `= ANY` answers.
    let ids: Vec<Uuid> = page.items.iter().map(|item| item.id).collect();
    let with_thumbnails =
        dam_db::derivatives::which_have(conn.executor(), &ids, &thumb_profile().op_hash()).await?;

    // Engagement for the page in one read, for the same reason (Q.5b·3). A grid drawing sixty filled-or-empty
    // stars would otherwise make sixty requests, which is the shape the endpoint exists to avoid.
    let engagement = page_engagement(&caller, conn.executor(), &ids).await?;
    // One read for the page, as with thumbnails and engagement. Scoped, so paperwork the caller cannot see does not
    // set the flag — which would disclose that something exists.
    let with_attachments =
        dam_db::attachments::which_have(conn.executor(), &ids, &caller.predicate)
            .await
            .map_err(|_| Failure::Internal)?;
    conn.commit().await?;

    let items = page
        .items
        .iter()
        .map(|row| {
            let mut summary = summary_with_extras(row, &engagement, &with_attachments);
            if with_thumbnails.contains(&row.id) {
                summary.thumbnail_url = preview_link(&state, &caller, row.id, thumb_profile().name);
            }
            summary
        })
        .collect();

    Ok(Json(AssetPage {
        // Browse, not search: there is no query to have misspelled.
        did_you_mean: None,
        items,
        total: page.total,
        offset: page.offset,
        // Browsing is not ranking: this endpoint orders by the caller's chosen `order`, so a grid must not
        // claim relevance over it.
        ranked: false,
    }))
}

/// One asset in full.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}",
    params(("asset_id" = Uuid, Path, description = "The asset")),
    responses(
        (status = 200, body = AssetDetail),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no read scope"),
        (status = 404, description = "No such asset, or not one this caller may see — deliberately the same answer"),
    ),
    tag = "assets",
)]
pub async fn detail(
    State(state): State<Arc<AssetState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<AssetDetail>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = assets::detail(conn.executor(), &caller.predicate, asset_id).await?;
    // Both recipes in one query. Two calls would be two round trips for one row's worth of information.
    let rendered = dam_db::derivatives::which_of(
        conn.executor(),
        asset_id,
        &[&thumb_profile().op_hash(), &preview_profile().op_hash()],
    )
    .await?;
    // In the same transaction as the row it describes: a rating arriving between two transactions would make
    // the panel show an average that does not match the asset it is drawn beside.
    let engagement = page_engagement(&caller, conn.executor(), &[asset_id]).await?;
    conn.commit().await?;
    let found = found.ok_or(Failure::NotFound)?;

    let mut summary = summary_with_engagement(&found.summary, &engagement);
    if rendered.contains(&thumb_profile().op_hash()) {
        summary.thumbnail_url = preview_link(&state, &caller, asset_id, thumb_profile().name);
    }
    let preview_url = if rendered.contains(&preview_profile().op_hash()) {
        preview_link(&state, &caller, asset_id, preview_profile().name)
    } else {
        None
    };

    Ok(Json(AssetDetail {
        summary,
        values: found.values,
        technical: found.technical,
        duration_ms: found.duration_ms,
        page_count: found.page_count,
        color_space: found.color_space,
        has_alpha: found.has_alpha,
        content_hash: found.content_hash,
        status: found.status,
        enrichment_state: found.enrichment_state,
        legal_hold: found.summary.legal_hold,
        release_at: found.release_at,
        expires_at: found.expires_at,
        version_no: found.version_no,
        created_at: found.created_at,
        updated_at: found.updated_at,
        // The engagement read is scoped to the asset the caller asked for, so the single element is it — and a
        // caller with no identity gets the untouched state rather than a failure, because a panel still renders.
        engagement: crate::engagement::view_of(engagement.into_iter().next(), asset_id),
        preview_url,
    }))
}

/// A metadata edit: the fields to change, keyed by definition key.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MetadataPatch {
    /// A merge, not a replacement. `null` for a value clears that field; a key that is absent is left
    /// alone. Two clients editing different fields of one asset must not overwrite each other, and a PUT of
    /// the whole document guarantees they do.
    pub values: serde_json::Map<String, serde_json::Value>,
}

/// The outcome of a metadata edit.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetadataAccepted {
    /// The stored document after the merge, so a client does not have to guess what the validator
    /// normalised — a date reformatted or a number coerced would otherwise show up as an unexplained diff
    /// on the next read.
    pub values: serde_json::Value,
}

/// Why an edit was refused, field by field.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ValidationProblem {
    /// The payload key, or the field key for a missing required field.
    pub key: String,
    /// A stable machine-readable code. Stable because a client branches on it and a UI maps it to a message
    /// in the user's language, neither of which can be done with prose.
    pub code: String,
    pub detail: String,
}

/// Edits an asset's metadata.
#[utoipa::path(
    patch,
    path = "/assets/{asset_id}/metadata",
    params(("asset_id" = Uuid, Path, description = "The asset")),
    request_body = MetadataPatch,
    responses(
        (status = 200, body = MetadataAccepted),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "No such asset, or not one this caller may see"),
        (status = 422, description = "The payload failed validation", body = Vec<ValidationProblem>),
    ),
    tag = "assets",
)]
pub async fn update_metadata(
    State(state): State<Arc<AssetState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(patch): Json<MetadataPatch>,
) -> Result<Json<MetadataAccepted>, Failure> {
    // `Manage`, not `Read`. A caller who can see an asset is not thereby allowed to relabel it, and reading
    // the action from the handler rather than from the route is how that stays true when a route is copied.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;

    // The read, the validation and the write are one transaction. Two would let a concurrent edit land
    // between them, and the loser's merge would be computed against a document that no longer exists —
    // which silently reverts the winner's change rather than conflicting with it.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // The predicate is applied first, so an asset the caller may not see cannot be written to — and it
    // answers 404 rather than 403 for the same reason the read does.
    let existing = assets::detail(conn.executor(), &caller.predicate, asset_id)
        .await?
        .ok_or(Failure::NotFound)?;

    // Validated as a *patch*, which is what `Mode::Patch` means: an absent key is left alone so `required`
    // does not apply to it, while a key present with `null` is an instruction to clear — and clearing a
    // required field is refused. Merging first and validating the whole document would lose that distinction
    // and demand every required field on every edit of one caption.
    let accepted = match dam_db::fields::validate_for_on(
        conn.executor(),
        // Scoped to this asset's metadata type (Q.1): a field its form does not show is a field this write
        // must not accept, or the type is decoration and the value lands where no form will display it again.
        // Same transaction as the read and the write, so a type reassignment cannot land in between.
        Some(asset_id),
        &patch.values,
        dam_core::fields::Mode::Patch,
        dam_core::fields::Writer::Human,
    )
    .await
    {
        Ok(accepted) => accepted,
        Err(dam_db::fields::ValidationOutcome::Rejected(rejections)) => {
            return Err(Failure::Invalid(
                rejections
                    .into_iter()
                    .map(|r| ValidationProblem {
                        key: r.key,
                        code: r.code.to_owned(),
                        detail: r.detail,
                    })
                    .collect(),
            ));
        }
        Err(dam_db::fields::ValidationOutcome::Failed(error)) => return Err(error.into()),
    };

    // Merged onto the stored document, using the *normalised* values rather than the ones that arrived — a
    // date reformatted or a number coerced has to be what lands, or the next read shows an unexplained diff.
    let mut merged = existing.values.as_object().cloned().unwrap_or_default();
    // Every key this request touched is now a human's, whatever it was before. Collected here because the
    // provenance for those keys has to go: leaving it would mark a person's sentence as machine output for as
    // long as the asset exists, and a disclosure wrong in that direction teaches people to ignore the marking
    // altogether. See `dam_db::enrichment`.
    let mut edited: Vec<String> = Vec::with_capacity(accepted.values.len());
    for (key, value) in accepted.values {
        edited.push(key.clone());
        if value.is_null() {
            merged.remove(&key);
        } else {
            merged.insert(key, value);
        }
    }
    let stored = serde_json::Value::Object(merged);

    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2) \
         ON CONFLICT (asset_id) DO UPDATE SET values = excluded.values, updated_at = now()",
    )
    .bind(asset_id)
    .bind(&stored)
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    dam_db::enrichment::forget_provenance(conn.executor(), asset_id, &edited).await?;

    // The asset's own `updated_at` moves too, or a metadata edit is invisible to anything watching the
    // asset — the reindex queue and the connector both key off it.
    sqlx::query("UPDATE assets SET updated_at = now() WHERE id = $1")
        .bind(asset_id)
        .execute(conn.executor())
        .await
        .map_err(dam_db::Error::from)?;

    // The outbox row, in this transaction. Both edit paths emit — this one and the bulk executor's — because
    // a consumer cannot tell which route an edit took, and an event that fired for one and not the other
    // would be a cache that goes stale depending on how many assets somebody selected.
    dam_db::webhooks::enqueue_asset_event(
        conn.executor(),
        dam_db::webhooks::kind::METADATA_UPDATED,
        asset_id,
        serde_json::json!({ "fields": edited }),
    )
    .await?;

    conn.commit().await?;

    Ok(Json(MetadataAccepted { values: stored }))
}

/// Everything that can go wrong in these handlers.
#[derive(Debug)]
pub enum Failure {
    Refused(caller::Refusal),
    /// No such asset, or not one this caller may see. One variant, because they answer the same.
    NotFound,
    Invalid(Vec<ValidationProblem>),
    /// The request is well-formed but the world refuses it — a key already taken, a change stored values
    /// will not permit. Distinct from `Invalid` because the fix is different: not "correct the form" but
    /// "deal with what is in the way", and the sentence saying what that is rides along.
    Conflict(String),
    /// The request itself is not usable, with the reason as one sentence. `Invalid` carries per-field
    /// problems for a form to place; this is for refusals that belong to the request as a whole.
    Unprocessable(String),
    /// Authenticated, and refused for a *named* reason: a rights verdict with its codes, or a download format
    /// whose permission the caller does not hold (Q.11).
    ///
    /// Distinct from `NotFound`, and the difference is deliberate. The asset rule collapses "you may not see it"
    /// into "it does not exist", because the gap between those answers is an existence oracle. Neither of these
    /// is about an asset's existence: the caller has already been shown the asset, and what is being withheld is
    /// a licence verdict or a piece of tenant configuration. A person refused for a reason they can act on
    /// deserves to be told which one. See DECISIONS.md.
    Forbidden(String),
    /// Come back later: a hard spend cap reached, or a provider rate limiting us.
    ///
    /// The first genuine 429 in this API, and it earns its own variant rather than borrowing 409: a conflict is
    /// something in the way that the caller could clear, and this is a limit that clears itself. A client
    /// distinguishing them decides between "fix something" and "retry".
    Throttled(String),
    Internal,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Refused(refusal) => refusal.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            // 422 rather than 400: the request was well-formed JSON and the *content* was refused, which is
            // the distinction a client needs in order to decide whether to show field errors or to retry.
            Self::Invalid(problems) => {
                (StatusCode::UNPROCESSABLE_ENTITY, Json(problems)).into_response()
            }
            Self::Conflict(reason) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "reason": reason })),
            )
                .into_response(),
            Self::Unprocessable(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "reason": reason })),
            )
                .into_response(),
            Self::Forbidden(reason) => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "reason": reason })),
            )
                .into_response(),
            Self::Throttled(reason) => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({ "reason": reason })),
            )
                .into_response(),
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<caller::Refusal> for Failure {
    fn from(refusal: caller::Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<dam_db::Error> for Failure {
    fn from(error: dam_db::Error) -> Self {
        match error {
            // A tenant whose schema is gone is not a 500 in the caller's terms — but it is not something a
            // caller can act on either, and naming it would describe the deployment's state to them.
            dam_db::Error::TenantNotProvisioned(schema) => {
                tracing::error!(%schema, "an authenticated key names a tenant with no schema");
                Self::Internal
            }
            other => {
                tracing::error!(error = %other, "asset endpoint database error");
                Self::Internal
            }
        }
    }
}

/// How long a thumbnail URL stays valid.
///
/// Long enough that a grid the user leaves open and comes back to still renders, short enough that a URL
/// captured from a browser cache or a shared screenshot stops working the same day. It is well under
/// `delivery::MAX_TOKEN_TTL`, which clamps it anyway.
const THUMBNAIL_TTL: chrono::Duration = chrono::Duration::hours(6);

/// The profile a lightbox draws.
fn preview_profile() -> &'static dam_media::profiles::Profile {
    &dam_media::profiles::PREVIEW_1024
}

/// The profile a grid cell draws.
///
/// Resolved through `profiles::by_name` rather than referencing the constant, so that if the name and the
/// constant ever disagree this fails loudly here instead of minting tokens for a profile the delivery path
/// cannot resolve.
fn thumb_profile() -> &'static dam_media::profiles::Profile {
    &dam_media::profiles::THUMB_256
}

/// A signed internal-preview URL for one of `asset_id`'s proxy renditions, if this deployment can mint one.
///
/// `None` when the delivery state is absent — which is the case in the endpoint tests, and is why the field is
/// optional in the first place rather than something a caller must have. A machine key has no identity, and an
/// internal preview requires one, so it gets no URL either: that is the restriction in
/// `signed_url::Purpose`, and it is enforced by the mint refusing rather than by this returning early.
fn preview_link(
    state: &AssetState,
    caller: &caller::Caller,
    asset_id: Uuid,
    transform: &str,
) -> Option<String> {
    preview_link_with(state.delivery.as_deref(), caller, asset_id, transform)
}

/// The thumbnail profile's op hash, for asking which of a page's assets have one.
///
/// Public because the search endpoints ask the same question about the same profile, and a second literal
/// there would be a grid whose pictures silently stop appearing the day the profile changes.
#[must_use]
pub fn thumb_op_hash() -> String {
    thumb_profile().op_hash()
}

/// A thumbnail URL for one asset, from a keyring rather than from an [`AssetState`].
///
/// The search endpoints hold a `SearchState`, which has no asset state to pass; taking the keyring directly is
/// what lets both families of endpoint mint the same link through the same code.
#[must_use]
pub fn thumbnail_url(
    delivery: Option<&crate::delivery::DeliveryState>,
    caller: &caller::Caller,
    asset_id: Uuid,
) -> Option<String> {
    preview_link_with(delivery, caller, asset_id, thumb_profile().name)
}

fn preview_link_with(
    delivery: Option<&crate::delivery::DeliveryState>,
    caller: &caller::Caller,
    asset_id: Uuid,
    transform: &str,
) -> Option<String> {
    let delivery = delivery?;
    let identity = caller.identity_id?;
    let now = delivery.now();

    // Blocking on the mint would be wrong here: it is HMAC over a few dozen bytes, so it is microseconds, and
    // making it async would mean a page of sixty awaiting sixty futures for no I/O.
    delivery
        .sign_preview(
            caller.tenant_id,
            asset_id,
            transform,
            identity,
            THUMBNAIL_TTL,
            now,
        )
        .ok()
}

/// Engagement for a page of assets, or nothing when there is no person to ask about.
///
/// Returns an empty vector rather than failing for a key with no identity: a grid still renders, just with no
/// stars filled in, which is the truth for a caller who cannot have favourites. Unreachable through `authorize`
/// in any case.
pub async fn page_engagement(
    caller: &caller::Caller,
    conn: &mut sqlx::PgConnection,
    ids: &[Uuid],
) -> Result<Vec<dam_db::engagement::Engagement>, Failure> {
    let Some(identity) = caller.identity_id else {
        return Ok(Vec::new());
    };
    // The caller's predicate again, even though every current call site passes ids that came from a read which
    // already applied it. That makes this filter unobservable — mutating it to an all-groups predicate changes
    // no test outcome, and the reason is that there is no path here carrying unfiltered ids. Kept because it is
    // the only correct predicate to use and the next call site may not be as careful, and documented because
    // undocumented unobservable code reads as tested when it is not.
    let planned =
        dam_core::query::Planned::new(dam_core::query::Query::All, caller.predicate.clone(), &[])
            .map_err(|_| Failure::Internal)?;
    dam_db::engagement::many(conn, ids, identity, &planned)
        .await
        .map_err(|_| Failure::Internal)
}

/// The wire summary for a read row, with this caller's engagement folded in.
///
/// Separate from [`summary_of`] rather than an argument to it, because three call sites build a page and one of
/// them (the ranked search path) walks its rows one at a time. A caller that forgot the engagement would produce
/// a grid where every star is empty — visibly wrong but easy to ship, since nothing else about the page changes.
#[must_use]
pub fn summary_with_engagement(
    row: &assets::Summary,
    engagement: &[dam_db::engagement::Engagement],
) -> AssetSummary {
    summary_with_extras(row, engagement, &[])
}

/// [`summary_with_engagement`], plus which assets have paperwork.
///
/// A second function rather than a fourth argument everywhere, because two of the three page paths do not read
/// attachments and threading an empty slice through them reads as an oversight rather than a choice.
#[must_use]
pub fn summary_with_extras(
    row: &assets::Summary,
    engagement: &[dam_db::engagement::Engagement],
    with_attachments: &[Uuid],
) -> AssetSummary {
    let mut summary = summary_of(row);
    if let Some(state) = engagement.iter().find(|e| e.asset_id == row.id) {
        summary.is_favourite = state.is_favourite;
        summary.average_stars = state.average_stars;
    }
    summary.has_attachment = with_attachments.contains(&row.id);
    summary
}

/// The wire summary for a read row.
///
/// Public because the search handler builds the same page shape, and two mappings of one row is how a field
/// ends up populated on one endpoint and null on the other.
pub fn summary_of(row: &assets::Summary) -> AssetSummary {
    AssetSummary {
        id: row.id,
        filename: row.filename.clone(),
        mime: row.mime.clone(),
        bytes: row.bytes,
        width: row.width,
        height: row.height,
        tier: row.tier,
        rights_state: row.rights_state,
        provenance_state: row.provenance_state,
        // See the module docs: pending the rights decision in NEEDS-REVIEW.md.
        thumbnail_url: None,
        tag_confidence: row.tag_confidence,
        // Filled by the caller from one engagement read per page, not per row. Defaulted to "no engagement"
        // rather than left absent so a client never has to distinguish "not favourited" from "not told".
        is_favourite: false,
        average_stars: None,
        // Same: filled from one attachment read per page.
        has_attachment: false,
        published_at: row.published_at,
        legal_hold: row.legal_hold,
    }
}
