//! Tag vocabulary administration over HTTP (Q.20b).
//!
//! A vocabulary is the label set zero-shot tagging scores against, and §8.2's claim that "a closed vocabulary
//! is what keeps AI tags governable" was not true before this: `taxonomies.ai_taggable` had existed since 0001
//! and nothing read it, `dam_db::taxonomy`'s lifecycle operations were unreachable, and there was no way to
//! create a vocabulary outside SQL. So the only governance available was the absence of a feature.
//!
//! ## Manage throughout
//!
//! Unlike categories, where reading the tree is `Read` because nobody can navigate a library without it. A
//! vocabulary's *terms* are already readable through the pickers and the tag facets; what this surface exposes
//! is the machinery — thresholds, precision, retirement, and which vocabularies a model may draw on. That is
//! administration, and `assignable` remains the read path for everybody else.
//!
//! ## Retire and merge, never delete
//!
//! There is no delete endpoint, and that is not an omission. `asset_tags` cascades, so deleting a term untags
//! every asset that carried it — years of somebody's work, gone quietly, discovered when a search comes back
//! empty. `dam_db::taxonomy`'s module docs make the argument at length; this surface simply offers no way to
//! do it.
//!
//! ## The threshold is read back, not echoed
//!
//! It is clamped to 0..=1 in the database layer, so the response carries what was stored. A screen showing
//! what it sent would not tell an operator that their 1.5 became 1.0.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::taxonomy;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

pub struct VocabularyState {
    pub global: PgPool,
}

impl std::fmt::Debug for VocabularyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VocabularyState").finish_non_exhaustive()
    }
}

pub fn router(state: VocabularyState) -> Router {
    Router::new()
        .route("/vocabularies", get(list).post(create))
        .route("/vocabularies/{id}/ai", post(set_ai))
        .route("/vocabularies/{id}/terms", get(terms).post(add_term))
        .route(
            "/vocabularies/{id}/terms/{term_id}",
            axum::routing::patch(amend_term),
        )
        .route(
            "/vocabularies/{id}/terms/{term_id}/retire",
            post(retire_term),
        )
        .route("/vocabularies/{id}/terms/{term_id}/merge", post(merge_term))
        .with_state(Arc::new(state))
}

/// One vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VocabularyView {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    /// Whether the zero-shot pass may propose these terms. False for a new one.
    pub ai_taggable: bool,
    /// Live terms — what this vocabulary costs the enrichment prompt.
    pub term_count: i64,
}

#[utoipa::path(
    get,
    path = "/vocabularies",
    responses((status = 200, description = "Every vocabulary", body = [VocabularyView])),
    tag = "vocabularies",
)]
pub async fn list(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<VocabularyView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = taxonomy::vocabularies(conn.executor())
        .await
        .map_err(refusal)?;
    conn.commit().await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| VocabularyView {
                id: row.id,
                key: row.key,
                label: row.label,
                ai_taggable: row.ai_taggable,
                term_count: row.term_count,
            })
            .collect(),
    ))
}

/// A vocabulary to create.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct NewVocabularyBody {
    pub key: String,
    pub label: String,
}

#[utoipa::path(
    post,
    path = "/vocabularies",
    request_body = NewVocabularyBody,
    responses(
        (status = 201, description = "Created, and off-limits to machine tagging", body = VocabularyView),
        (status = 409, description = "The key is taken"),
    ),
    tag = "vocabularies",
)]
pub async fn create(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
    Json(body): Json<NewVocabularyBody>,
) -> Result<(StatusCode, Json<VocabularyView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let key = body.key.trim();
    if key.is_empty() {
        return Err(Failure::Unprocessable(
            "a vocabulary needs a key; it is what an import and a model answer with".to_owned(),
        ));
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let made = taxonomy::create_vocabulary(conn.executor(), key, body.label.trim()).await;
    let id = match made {
        Ok(id) => id,
        // A unique violation on `taxonomies.key`, which is shared with the category trees — so the refusal
        // says the key is taken without saying by what: "there is already a category tree called colours"
        // would be a small existence oracle over a surface this caller may not administer.
        Err(taxonomy::Error::Database(dam_db::Error::Sqlx(error)))
            if error
                .as_database_error()
                .is_some_and(|db| db.is_unique_violation()) =>
        {
            return Err(Failure::Conflict(format!(
                "the key {key:?} is already taken by a taxonomy"
            )));
        }
        Err(other) => return Err(refusal(other)),
    };
    let made = taxonomy::vocabularies(conn.executor())
        .await
        .map_err(refusal)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or(Failure::Internal)?;
    conn.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(VocabularyView {
            id: made.id,
            key: made.key,
            label: made.label,
            ai_taggable: made.ai_taggable,
            term_count: made.term_count,
        }),
    ))
}

/// Whether a model may draw on this vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct AiBody {
    pub ai_taggable: bool,
}

/// Opens a vocabulary to machine tagging, or closes it.
///
/// Its own endpoint rather than a field on an update body: this is the setting that decides what an LLM is
/// told about a customer's library, and it should not be possible to change it while editing a label.
#[utoipa::path(
    post,
    path = "/vocabularies/{id}/ai",
    request_body = AiBody,
    responses(
        (status = 200, description = "Set", body = VocabularyView),
        (status = 404, description = "No such vocabulary"),
    ),
    tag = "vocabularies",
)]
pub async fn set_ai(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<AiBody>,
) -> Result<Json<VocabularyView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let changed = taxonomy::set_ai_taggable(conn.executor(), id, body.ai_taggable)
        .await
        .map_err(refusal)?;
    if !changed {
        return Err(Failure::NotFound);
    }
    let row = taxonomy::vocabularies(conn.executor())
        .await
        .map_err(refusal)?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or(Failure::NotFound)?;
    conn.commit().await?;
    Ok(Json(VocabularyView {
        id: row.id,
        key: row.key,
        label: row.label,
        ai_taggable: row.ai_taggable,
        term_count: row.term_count,
    }))
}

/// One term, as an administrator sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TermView {
    pub id: Uuid,
    pub path: String,
    /// What a model answers with, and what an import resolves. Not changeable.
    pub slug: String,
    pub label: String,
    pub synonyms: Vec<String>,
    pub ai_threshold: f32,
    /// Measured from confirmations and rejections, or absent before anybody has reviewed one.
    pub ai_precision: Option<f32>,
    /// Confirmed tags. Denormalised and worker-maintained, named so nobody reads it as live.
    pub asset_count: i64,
    /// Set once the term is retired from new assignment. It stays resolvable.
    pub deprecated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Where the meaning went, when a merge retired it.
    pub superseded_by: Option<Uuid>,
}

impl From<taxonomy::VocabularyTerm> for TermView {
    fn from(row: taxonomy::VocabularyTerm) -> Self {
        Self {
            id: row.id,
            path: row.path,
            slug: row.slug,
            label: row.label,
            synonyms: row.synonyms,
            ai_threshold: row.ai_threshold,
            ai_precision: row.ai_precision,
            asset_count: row.asset_count,
            deprecated_at: row.deprecated_at,
            superseded_by: row.superseded_by,
        }
    }
}

#[utoipa::path(
    get,
    path = "/vocabularies/{id}/terms",
    responses((status = 200, description = "Every term, retired ones included", body = [TermView])),
    tag = "vocabularies",
)]
pub async fn terms(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<TermView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = taxonomy::terms(conn.executor(), id)
        .await
        .map_err(refusal)?;
    conn.commit().await?;
    Ok(Json(rows.into_iter().map(TermView::from).collect()))
}

/// A term to add.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct NewTermBody {
    pub slug: String,
    pub label: String,
    #[serde(default)]
    pub synonyms: Vec<String>,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

#[utoipa::path(
    post,
    path = "/vocabularies/{id}/terms",
    request_body = NewTermBody,
    responses(
        (status = 201, description = "Added", body = TermView),
        (status = 409, description = "The slug or path is taken"),
    ),
    tag = "vocabularies",
)]
pub async fn add_term(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<NewTermBody>,
) -> Result<(StatusCode, Json<TermView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let slug = body.slug.trim();
    if slug.is_empty() {
        return Err(Failure::Unprocessable(
            "a term needs a slug; it is what a model answers with".to_owned(),
        ));
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let synonyms = clean(&body.synonyms);
    let made = taxonomy::add_term(
        conn.executor(),
        id,
        &taxonomy::NewTerm {
            slug,
            label: body.label.trim(),
            synonyms: &synonyms,
            parent_id: body.parent_id,
        },
    )
    .await
    .map_err(refusal)?;

    let row = taxonomy::terms(conn.executor(), id)
        .await
        .map_err(refusal)?
        .into_iter()
        .find(|row| row.id == made)
        .ok_or(Failure::Internal)?;
    conn.commit().await?;
    Ok((StatusCode::CREATED, Json(TermView::from(row))))
}

/// What may be changed on a term. Not the slug.
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
pub struct AmendTermBody {
    pub label: String,
    #[serde(default)]
    pub synonyms: Vec<String>,
    pub ai_threshold: f32,
}

#[utoipa::path(
    patch,
    path = "/vocabularies/{id}/terms/{term_id}",
    request_body = AmendTermBody,
    responses(
        (status = 200, description = "Amended, with the threshold as stored", body = TermView),
        (status = 404, description = "No such term"),
    ),
    tag = "vocabularies",
)]
pub async fn amend_term(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
    Path((id, term_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<AmendTermBody>,
) -> Result<Json<TermView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let synonyms = clean(&body.synonyms);
    let changed = taxonomy::amend_term(
        conn.executor(),
        term_id,
        body.label.trim(),
        &synonyms,
        body.ai_threshold,
    )
    .await
    .map_err(refusal)?;
    if !changed {
        return Err(Failure::NotFound);
    }
    let row = in_vocabulary(&mut conn, id, term_id).await?;
    conn.commit().await?;
    Ok(Json(TermView::from(row)))
}

/// Retires a term from new assignment. It stays resolvable, and its assets keep it.
#[utoipa::path(
    post,
    path = "/vocabularies/{id}/terms/{term_id}/retire",
    responses(
        (status = 200, description = "Retired", body = TermView),
        (status = 409, description = "Live children remain"),
        (status = 404, description = "No such term"),
    ),
    tag = "vocabularies",
)]
pub async fn retire_term(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
    Path((id, term_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<TermView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    taxonomy::deprecate(conn.executor(), term_id)
        .await
        .map_err(refusal)?;
    let row = in_vocabulary(&mut conn, id, term_id).await?;
    conn.commit().await?;
    Ok(Json(TermView::from(row)))
}

/// Where a merge sends the meaning.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct MergeBody {
    pub into: Uuid,
}

/// Merges this term into another: the assets move, and this one is retired pointing at it.
///
/// The retag and the retirement are one transaction — half of it is worse than none, because assets moved with
/// the source still live means two active terms for one concept, and the source retired with its assets left
/// behind means they are tagged with something no picker offers.
#[utoipa::path(
    post,
    path = "/vocabularies/{id}/terms/{term_id}/merge",
    request_body = MergeBody,
    responses(
        (status = 200, description = "Merged; the source is retired and resolves to the target", body = TermView),
        (status = 409, description = "Would cycle, or crosses vocabularies, or the target is retired"),
    ),
    tag = "vocabularies",
)]
pub async fn merge_term(
    State(state): State<Arc<VocabularyState>>,
    headers: HeaderMap,
    Path((id, term_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<MergeBody>,
) -> Result<Json<TermView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    taxonomy::merge(conn.executor(), term_id, body.into)
        .await
        .map_err(refusal)?;
    let row = in_vocabulary(&mut conn, id, term_id).await?;
    conn.commit().await?;
    Ok(Json(TermView::from(row)))
}

/// Reads one term back, and proves it belongs to the vocabulary in the path.
///
/// The check matters: without it `/vocabularies/{a}/terms/{term_of_b}` would operate on b's term and answer
/// 200, so the id in the URL would be decoration. A caller who guessed a term id would also learn it exists.
async fn in_vocabulary(
    conn: &mut dam_db::TenantConn<'_>,
    taxonomy_id: Uuid,
    term_id: Uuid,
) -> Result<taxonomy::VocabularyTerm, Failure> {
    taxonomy::terms(conn.executor(), taxonomy_id)
        .await
        .map_err(refusal)?
        .into_iter()
        .find(|row| row.id == term_id)
        .ok_or(Failure::NotFound)
}

/// Trims, drops empties, and de-duplicates case-insensitively.
///
/// Synonyms are typed by hand into a list, so blanks and repeats are the normal input. They cost prompt bytes
/// on every enrichment call, and `"Harbour"` beside `"harbour"` widens nothing — `dam_ai::enrich` already
/// matches case-insensitively.
fn clean(synonyms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for candidate in synonyms {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }
        if out.iter().any(|seen| seen.eq_ignore_ascii_case(trimmed)) {
            continue;
        }
        out.push(trimmed.to_owned());
    }
    out
}

/// Maps a taxonomy refusal onto the status a client can act on.
fn refusal(error: taxonomy::Error) -> Failure {
    match error {
        taxonomy::Error::NotFound(_) => Failure::NotFound,
        // All of these are "something is in the way", which is what 409 means — and each carries the sentence
        // saying what, because "conflict" alone is a refusal nobody can act on.
        taxonomy::Error::HasLiveChildren { .. }
        | taxonomy::Error::WouldCycle { .. }
        | taxonomy::Error::DifferentTaxonomies { .. }
        | taxonomy::Error::TargetDeprecated { .. }
        | taxonomy::Error::PathTaken { .. }
        | taxonomy::Error::Deprecated { .. } => Failure::Conflict(error.to_string()),
        taxonomy::Error::Database(inner) => inner.into(),
    }
}
