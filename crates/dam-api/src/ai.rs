//! A tenant's own model credentials, and the cap on what they may spend (M5a·4).
//!
//! Two surfaces that only make sense together: a place to put a provider key, and a limit on what enrichment
//! may do with it. §8.3's cost table is the reason the second is not a later slice — a mis-triggered
//! re-enrichment of a large library is a five-figure event, and a cap added after the first bulk run is a cap
//! added after the invoice.
//!
//! ## A key goes in and never comes out
//!
//! `POST /ai/credentials` is the only route that accepts plaintext. It seals immediately, stores the ciphertext,
//! and answers with a view that carries a four-character hint and nothing else. There is deliberately no route
//! that reads a key back: an admin surface that could show one turns every session cookie into a credential
//! exfiltration path, and nobody needs it — the key is already in whatever password manager it came from.
//! Rotation is `PUT /ai/credentials/{id}/key`, which is a write.
//!
//! ## Verification is a real call, and the only one in this codebase
//!
//! `POST /ai/credentials/{id}/verify` asks the provider for one short answer. It exists because everything else
//! about the hosted-model integration is tested against a recorded transport — see `dam_ai::testing` on why —
//! and a pinned request shape is not a working integration. Ten tokens is the cheapest way for whoever pasted a
//! key to learn that it works, that the endpoint is reachable, and that the model name is one the provider
//! knows.
//!
//! ## Manage, and a note about that
//!
//! Every route here requires `Action::Manage`, the same gate as the other tenant-configuration surfaces. A
//! money-bearing secret arguably deserves something narrower than "may change metadata" — `tenant:ai`, say —
//! and the fine-grained permission strings exist for exactly that kind of narrowing. It is not used here
//! because the built-in administrator role's permissions are wildcards that nothing expands, so a new
//! permission string would be a gate no existing role could pass. That is written up in NEEDS-REVIEW.md and is
//! a decision for a human; this surface follows the existing convention until it is made.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use dam_ai::model::{Ask, Effort, ModelError, Part, Transport};
use dam_ai::pricing::Prices;
use dam_core::Secret;
use dam_core::policy::Action;
use dam_core::sealed::SealingKeyring;
use dam_db::ai_credentials::{
    self, Credential, CredentialRefusal, NewCredential, Provider, associated_data,
};
use dam_db::quotas::{self, Enforcement, Quota, Verdict};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the AI configuration endpoints need.
pub struct AiState {
    pub global: PgPool,
    /// Seals what goes in, opens what comes out. Built from configuration once, at startup.
    pub keyring: SealingKeyring,
    /// What a call costs, for the budget view. Configuration merged over the built-in table.
    pub prices: Prices,
    /// How a verification call reaches the provider. Injected so a test can drive the whole route without a key
    /// or a network — the same seam the clients use.
    pub transport: Arc<dyn Transport>,
}

impl std::fmt::Debug for AiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiState").finish_non_exhaustive()
    }
}

/// The AI configuration routes.
pub fn router(state: AiState) -> Router {
    Router::new()
        .route("/ai/credentials", get(list).post(add))
        .route("/ai/credentials/{id}/key", put(replace_key))
        .route("/ai/credentials/{id}/default", patch(make_default))
        .route("/ai/credentials/{id}/active", patch(set_active))
        .route("/ai/credentials/{id}/verify", post(verify))
        .route("/ai/budget", get(read_budget).put(set_budget))
        .route("/ai/enrichment", get(read_enrichment).put(set_enrichment))
        .route("/ai/review", get(review))
        .route("/ai/backfill", get(read_backfill).post(start_backfill))
        // Pathed under `/search` because that is what it is for — the answer goes in the search box — and
        // defined here because this is where the credential, the price list and the transport live.
        .route("/search/ask", post(ask_query))
        .route("/assets/{id}/enrich", post(enrich))
        .route("/assets/{id}/ai", get(asset_disclosure))
        .route("/assets/{asset_id}/tags/{term_id}", patch(decide_tag))
        .with_state(Arc::new(state))
}

/// One credential, as an administrator sees it. No key, sealed or otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CredentialView {
    pub id: Uuid,
    /// `anthropic` or `openai_compatible` — the wire format, not the vendor. See
    /// `dam_db::ai_credentials::Provider`.
    pub provider: String,
    pub label: String,
    /// `null` for Anthropic's own endpoint. Required for everything else.
    pub base_url: Option<String>,
    /// The last four characters of the key. Enough to recognise which of two keys a row holds, useless to
    /// anybody who steals the response.
    pub hint: String,
    pub default_model: String,
    pub is_active: bool,
    pub is_default: bool,
    /// Whether this row is still sealed under a retired key.
    ///
    /// The rotation worklist, computed without opening anything. An operator who has rotated the sealing key
    /// needs to know which credentials still need re-sealing, and the answer must not require decrypting them
    /// to find out.
    pub needs_resealing: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl CredentialView {
    fn of(row: Credential, current_key_id: &str) -> Self {
        Self {
            needs_resealing: row.sealing_key_id != current_key_id,
            id: row.id,
            provider: row.provider,
            label: row.label,
            base_url: row.base_url,
            hint: row.hint,
            default_model: row.default_model,
            is_active: row.is_active,
            is_default: row.is_default,
            created_at: row.created_at,
        }
    }
}

/// A credential to store. The only shape in this API that carries a plaintext key.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NewCredentialRequest {
    /// `anthropic` or `openai_compatible`.
    pub provider: String,
    /// What to call it in a list. A person's name for the key, not the vendor's.
    pub label: String,
    /// Required for `openai_compatible`, including the version segment —
    /// `https://api.moonshot.ai/v1`, `https://api.groq.com/openai/v1`. Optional for Anthropic, where it
    /// overrides the vendor endpoint with a gateway.
    pub base_url: Option<String>,
    /// The model to use when a pipeline stage does not name one.
    pub default_model: String,
    /// The provider's key. Sealed before the response is written and never readable again.
    pub api_key: String,
    /// Whether enrichment should use this one.
    #[serde(default)]
    pub make_default: bool,
}

/// A replacement key for an existing credential.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReplaceKeyRequest {
    pub api_key: String,
}

/// Whether a credential is withdrawn or in use.
///
/// Named for the credential rather than reusing `conversions::ActiveRequest`: utoipa keys components by type
/// name, and two `ActiveRequest`s in one document would silently be one schema.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct CredentialActiveRequest {
    pub is_active: bool,
}

/// What a verification call found out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VerifyResult {
    pub ok: bool,
    /// The model that actually answered, which is not always the one asked for.
    pub model: Option<String>,
    /// What the provider said, or what went wrong, in one sentence somebody can act on.
    pub detail: String,
    /// Whether trying again might help. A rejected key never will; a throttle will.
    pub worth_retrying: bool,
}

/// The cap, and what has been spent against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BudgetView {
    /// `null` when no cap is configured, which is not a cap of zero — enrichment runs unmetered.
    pub limit_cents: Option<i64>,
    /// `soft` warns and keeps working; `hard` refuses new enrichment.
    pub enforcement: String,
    pub warn_at_fraction: f32,
    /// Whole cents charged this period.
    pub used_cents: i64,
    /// The first day of the calendar month the spend is counted in, in UTC.
    pub period_start: chrono::NaiveDate,
    /// What state the tenant is in right now: `allowed`, `warned` or `refused`.
    pub state: String,
}

/// A cap to set.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct BudgetRequest {
    pub limit_cents: i64,
    /// `true` to refuse enrichment past the limit rather than only warning.
    #[serde(default)]
    pub hard: bool,
    /// Where the warning fires. 0.8 gives a customer time to react rather than discovering the cap by hitting it.
    #[serde(default = "default_warn")]
    pub warn_at_fraction: f32,
}

fn default_warn() -> f32 {
    0.8
}

/// Every credential, withdrawn ones included.
#[utoipa::path(
    get,
    path = "/ai/credentials",
    responses(
        (status = 200, body = Vec<CredentialView>),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "ai",
)]
pub async fn list(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<CredentialView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = ai_credentials::all(conn.executor()).await?;
    conn.commit().await?;
    let current = state.keyring.current_key_id();
    Ok(Json(
        rows.into_iter()
            .map(|row| CredentialView::of(row, current))
            .collect(),
    ))
}

/// Stores a credential. The one route that accepts a plaintext key.
#[utoipa::path(
    post,
    path = "/ai/credentials",
    request_body = NewCredentialRequest,
    responses(
        (status = 201, body = CredentialView),
        (status = 422, description = "An unusable provider, endpoint or model; the body says which"),
    ),
    tag = "ai",
)]
pub async fn add(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Json(request): Json<NewCredentialRequest>,
) -> Result<(StatusCode, Json<CredentialView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let Some(provider) = Provider::parse(&request.provider) else {
        return Err(Failure::Unprocessable(format!(
            "`{}` is not a provider this build speaks; use `anthropic` or `openai_compatible`",
            request.provider
        )));
    };
    if matches!(provider, Provider::OpenAiCompatible) && request.base_url.is_none() {
        // Refused here rather than at first use: a credential stored without an endpoint is one that fails at
        // enrichment time, hours later, with nobody watching.
        return Err(Failure::Unprocessable(
            "an openai-compatible credential needs a base url including the version segment, \
             for example https://api.moonshot.ai/v1"
                .to_owned(),
        ));
    }
    if request.api_key.trim().is_empty() {
        return Err(Failure::Unprocessable(
            "the api key is empty; nothing was stored".to_owned(),
        ));
    }

    // The id first, because it is part of the associated data the key is sealed under — a database-generated id
    // would force a seal-then-update, and a failure between the two leaves a ciphertext bound to an id nothing
    // has.
    let id = Uuid::now_v7();
    let plaintext = Secret::new(request.api_key.trim().to_owned());
    let aad = associated_data(caller.tenant_slug.as_str(), provider.as_str(), id);
    let sealed = state
        .keyring
        .seal(&plaintext, &aad)
        .map_err(|_| Failure::Internal)?;
    let hint = dam_core::sealed::hint(&plaintext);

    let new = NewCredential {
        id,
        provider,
        label: request.label,
        base_url: request.base_url,
        sealed_key: sealed,
        sealing_key_id: state.keyring.current_key_id().to_owned(),
        hint,
        default_model: request.default_model,
        make_default: request.make_default,
    };
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let created = ai_credentials::add(conn.executor(), &new)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    let current = state.keyring.current_key_id();
    Ok((
        StatusCode::CREATED,
        Json(CredentialView::of(created, current)),
    ))
}

/// Replaces a credential's key.
#[utoipa::path(
    put,
    path = "/ai/credentials/{id}/key",
    request_body = ReplaceKeyRequest,
    responses(
        (status = 200, body = CredentialView),
        (status = 404, description = "No such credential"),
    ),
    tag = "ai",
)]
pub async fn replace_key(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<ReplaceKeyRequest>,
) -> Result<Json<CredentialView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    if request.api_key.trim().is_empty() {
        return Err(Failure::Unprocessable(
            "the api key is empty; the stored one is unchanged".to_owned(),
        ));
    }
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let Some(existing) = ai_credentials::read(conn.executor(), id).await? else {
        conn.commit().await?;
        return Err(Failure::NotFound);
    };
    let plaintext = Secret::new(request.api_key.trim().to_owned());
    // The row's own associated data, not a freshly composed one: the seal has to be bound to the provider this
    // row already claims, or a re-seal would quietly re-bind a key to a different context.
    let sealed = state
        .keyring
        .seal(
            &plaintext,
            &existing.associated_data(caller.tenant_slug.as_str()),
        )
        .map_err(|_| Failure::Internal)?;
    let updated = ai_credentials::replace_key(
        conn.executor(),
        id,
        &sealed,
        state.keyring.current_key_id(),
        &dam_core::sealed::hint(&plaintext),
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;
    let current = state.keyring.current_key_id();
    Ok(Json(CredentialView::of(updated, current)))
}

/// Makes one credential the one enrichment uses.
#[utoipa::path(
    patch,
    path = "/ai/credentials/{id}/default",
    responses(
        (status = 200, body = CredentialView),
        (status = 404, description = "No such credential"),
        (status = 409, description = "A withdrawn credential cannot be the default"),
    ),
    tag = "ai",
)]
pub async fn make_default(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<CredentialView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let updated = ai_credentials::make_default(conn.executor(), id)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    let current = state.keyring.current_key_id();
    Ok(Json(CredentialView::of(updated, current)))
}

/// Withdraws a credential, or restores one.
#[utoipa::path(
    patch,
    path = "/ai/credentials/{id}/active",
    request_body = CredentialActiveRequest,
    responses(
        (status = 200, body = CredentialView),
        (status = 404, description = "No such credential"),
    ),
    tag = "ai",
)]
pub async fn set_active(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<CredentialActiveRequest>,
) -> Result<Json<CredentialView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let updated = ai_credentials::set_active(conn.executor(), id, request.is_active)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    let current = state.keyring.current_key_id();
    Ok(Json(CredentialView::of(updated, current)))
}

/// Asks the provider one short question, to find out whether the credential works.
#[utoipa::path(
    post,
    path = "/ai/credentials/{id}/verify",
    responses(
        (status = 200, body = VerifyResult, description = "The attempt was made; `ok` says how it went"),
        (status = 404, description = "No such credential"),
        (status = 422, description = "The credential cannot be used at all — see the body"),
    ),
    tag = "ai",
)]
pub async fn verify(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<VerifyResult>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let credential = ai_credentials::read(conn.executor(), id).await?;
    conn.commit().await?;
    let Some(credential) = credential else {
        return Err(Failure::NotFound);
    };

    let model = dam_ai::credential::open(
        &credential,
        caller.tenant_slug.as_str(),
        &state.keyring,
        Arc::clone(&state.transport),
        None,
    )
    .map_err(|error| Failure::Unprocessable(error.to_string()))?;

    // Deliberately tiny, and deliberately not a schema: this is asking "does the key work", and a structured
    // answer would add a way for the check to fail that has nothing to do with the credential.
    let ask = Ask {
        instructions: "Answer with one word.".to_owned(),
        parts: vec![Part::Text("Reply with the word ready.".to_owned())],
        schema: None,
        max_tokens: 16,
        effort: Effort::Low,
    };
    Ok(Json(match model.ask(&ask).await {
        Ok(completion) => VerifyResult {
            ok: true,
            model: Some(completion.model),
            detail: completion.text.trim().to_owned(),
            worth_retrying: false,
        },
        Err(error) => VerifyResult {
            ok: false,
            model: None,
            // A refusal counts as a *working* credential answering, but not as `ok` — the operator asked whether
            // the model would answer, and it did not. The sentence says which of the two happened.
            detail: match &error {
                ModelError::Declined(_) => {
                    format!("the credential works; the model declined this request ({error})")
                }
                other => other.to_string(),
            },
            worth_retrying: error.is_transient(),
        },
    }))
}

/// The AI spend cap and what has been used against it.
#[utoipa::path(
    get,
    path = "/ai/budget",
    responses(
        (status = 200, body = BudgetView),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "ai",
)]
pub async fn read_budget(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
) -> Result<Json<BudgetView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    // The global schema, not the tenant's: quotas are a control-plane fact, and a tenant that could write its
    // own cap would be a tenant that could remove it.
    let mut conn = state.global.acquire().await.map_err(dam_db::Error::from)?;
    let period = quotas::month_start(chrono::Utc::now());
    let quota = quotas::quota(&mut conn, caller.tenant_id, quotas::AI_SPEND).await?;
    let used = quotas::used(&mut conn, caller.tenant_id, quotas::AI_SPEND, period).await?;
    Ok(Json(budget_view(quota.as_ref(), used, period)))
}

/// Sets the AI spend cap.
#[utoipa::path(
    put,
    path = "/ai/budget",
    request_body = BudgetRequest,
    responses(
        (status = 200, body = BudgetView),
        (status = 422, description = "A limit or fraction the cap cannot use"),
    ),
    tag = "ai",
)]
pub async fn set_budget(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Json(request): Json<BudgetRequest>,
) -> Result<Json<BudgetView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    if request.limit_cents < 0 {
        return Err(Failure::Unprocessable(
            "a spend limit cannot be negative".to_owned(),
        ));
    }
    if !(0.0..=1.0).contains(&request.warn_at_fraction) {
        return Err(Failure::Unprocessable(
            "warn_at_fraction is a fraction of the limit, between 0 and 1".to_owned(),
        ));
    }
    let quota = Quota {
        limit_value: request.limit_cents,
        warn_at_fraction: request.warn_at_fraction,
        enforcement: if request.hard {
            Enforcement::Hard
        } else {
            Enforcement::Soft
        },
    };
    let mut conn = state.global.acquire().await.map_err(dam_db::Error::from)?;
    quotas::set(&mut conn, caller.tenant_id, quotas::AI_SPEND, &quota).await?;
    let period = quotas::month_start(chrono::Utc::now());
    let used = quotas::used(&mut conn, caller.tenant_id, quotas::AI_SPEND, period).await?;
    Ok(Json(budget_view(Some(&quota), used, period)))
}

/// The view for a cap and a spend. Separate so the state naming is in one place.
fn budget_view(quota: Option<&Quota>, used: i64, period: chrono::NaiveDate) -> BudgetView {
    let (limit_cents, enforcement, warn_at_fraction, state) = match quota {
        Some(quota) => (
            Some(quota.limit_value),
            match quota.enforcement {
                Enforcement::Hard => "hard",
                Enforcement::Soft => "soft",
            },
            quota.warn_at_fraction,
            match quotas::verdict(quota, used) {
                Verdict::Allowed => "allowed",
                Verdict::Warned { .. } => "warned",
                Verdict::Refused { .. } => "refused",
            },
        ),
        // No cap is not a cap of zero. Saying `allowed` with a null limit is the honest reading: enrichment runs
        // and nothing is metering it.
        None => (None, "soft", 0.8, "allowed"),
    };
    BudgetView {
        limit_cents,
        enforcement: enforcement.to_owned(),
        warn_at_fraction,
        used_cents: used,
        period_start: period,
        state: state.to_owned(),
    }
}

/// Turns a store refusal into an HTTP one.
struct Refused(CredentialRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            CredentialRefusal::Unknown(_) => Self::NotFound,
            // A constraint name is not a sentence for a person, but it names the field, which is more than
            // "invalid" does — and the CHECKs are the specification, so restating them here would be a second
            // copy to drift.
            CredentialRefusal::Invalid(what) => Self::Conflict(what),
            CredentialRefusal::Database(error) => Self::from(error),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────────────────────────
// Enrichment: the settings, the queue, and what a model wrote
// ───────────────────────────────────────────────────────────────────────────────────────────────

/// What a model should do for this tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnrichmentSettings {
    /// False until somebody turns it on. This is the pipeline that bills per asset.
    pub is_enabled: bool,
    /// The tenant's own instructions, and the cacheable half of every request.
    pub guidance: String,
    pub language: String,
    /// Overrides the credential's default model for this pipeline.
    pub model: Option<String>,
    /// Where the alt text lands. `null` writes none.
    pub alt_text_field: Option<String>,
    pub description_field: Option<String>,
    pub suggest_tags: bool,
    /// Whether a question in the search box may be turned into a query by a model.
    ///
    /// Its own switch, not part of `is_enabled`: describing the library costs per asset and answering questions
    /// costs per question, and a tenant may want either alone. Adding a key to describe a library should not
    /// silently make every reader's search box a paid endpoint.
    #[serde(default)]
    pub natural_language_search: bool,
}

impl From<dam_db::enrichment::Settings> for EnrichmentSettings {
    fn from(row: dam_db::enrichment::Settings) -> Self {
        Self {
            is_enabled: row.is_enabled,
            guidance: row.guidance,
            language: row.language,
            model: row.model,
            alt_text_field: row.alt_text_field,
            description_field: row.description_field,
            suggest_tags: row.suggest_tags,
            natural_language_search: row.natural_language_search,
        }
    }
}

impl From<EnrichmentSettings> for dam_db::enrichment::Settings {
    fn from(request: EnrichmentSettings) -> Self {
        Self {
            is_enabled: request.is_enabled,
            guidance: request.guidance,
            language: request.language,
            model: request.model,
            alt_text_field: request.alt_text_field,
            description_field: request.description_field,
            suggest_tags: request.suggest_tags,
            natural_language_search: request.natural_language_search,
        }
    }
}

/// One asset in the review queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReviewRow {
    pub asset_id: Uuid,
    pub filename: String,
    pub mime: String,
    pub suggested: Vec<SuggestedTagView>,
    pub fields: Vec<MachineFieldView>,
}

/// A tag waiting for a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SuggestedTagView {
    pub term_id: Uuid,
    pub slug: String,
    pub label: String,
    /// As the model claimed. Shown so a reviewer can sort, never as a reason to skip reviewing.
    pub confidence: Option<f32>,
    /// How many generators proposed it independently.
    pub votes: i16,
    pub source: String,
}

/// A machine-written value, as the disclosure surface shows it (G2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MachineFieldView {
    pub key: String,
    pub value: serde_json::Value,
    /// The model that produced it, as it answered.
    pub model: String,
    pub confidence: Option<f64>,
    pub reviewed: bool,
}

/// A decision about a suggested tag.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct TagDecision {
    /// `true` confirms the tag, `false` rejects it. Both are recorded: the rejections are the training signal.
    pub accept: bool,
}

/// What enqueueing an enrichment produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnrichQueued {
    pub asset_id: Uuid,
    pub job_id: Uuid,
}

/// The enrichment settings.
#[utoipa::path(
    get,
    path = "/ai/enrichment",
    responses(
        (status = 200, body = EnrichmentSettings),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "ai",
)]
pub async fn read_enrichment(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
) -> Result<Json<EnrichmentSettings>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let settings = dam_db::enrichment::settings(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(settings.into()))
}

/// Replaces the enrichment settings.
#[utoipa::path(
    put,
    path = "/ai/enrichment",
    request_body = EnrichmentSettings,
    responses(
        (status = 200, body = EnrichmentSettings),
        (status = 422, description = "A language, model or field name the column will not hold"),
    ),
    tag = "ai",
)]
pub async fn set_enrichment(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Json(request): Json<EnrichmentSettings>,
) -> Result<Json<EnrichmentSettings>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    // Turning it on with no credential would be a setting that cannot do anything, and the failure would arrive
    // later as a queue of skipped runs. Refused here, where the person who can fix it is looking.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if request.is_enabled
        && dam_db::ai_credentials::current(conn.executor())
            .await?
            .is_none()
    {
        conn.commit().await?;
        return Err(Failure::Unprocessable(
            "add a model credential before switching enrichment on; there is nothing for it to call yet"
                .to_owned(),
        ));
    }
    let saved = dam_db::enrichment::save_settings(conn.executor(), &request.into()).await?;
    conn.commit().await?;
    Ok(Json(saved.into()))
}

/// Queues one asset for description.
#[utoipa::path(
    post,
    path = "/assets/{id}/enrich",
    responses(
        (status = 202, body = EnrichQueued),
        (status = 404, description = "No such asset, or not one this caller may see"),
        (status = 422, description = "Enrichment is switched off for this tenant"),
    ),
    tag = "ai",
)]
pub async fn enrich(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<(StatusCode, Json<EnrichQueued>), Failure> {
    // Manage, not Read: this spends money and writes metadata.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Under the caller's predicate, so an asset outside their scope is 404 rather than a job they could not
    // otherwise have caused — the same rule as everywhere else.
    let visible = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id)
        .await?
        .is_some();
    let settings = dam_db::enrichment::settings(conn.executor()).await?;
    conn.commit().await?;
    if !visible {
        return Err(Failure::NotFound);
    }
    if !settings.is_enabled {
        return Err(Failure::Unprocessable(
            "enrichment is switched off for this tenant".to_owned(),
        ));
    }

    let job_id = dam_pipeline::worker::enqueue_enrich(&state.global, caller.tenant_id, asset_id)
        .await
        .map_err(|_| Failure::Internal)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(EnrichQueued { asset_id, job_id }),
    ))
}

/// The review queue.
#[utoipa::path(
    get,
    path = "/ai/review",
    params(("limit" = Option<i64>, Query, description = "How many assets to return; 50 by default")),
    responses(
        (status = 200, body = Vec<ReviewRow>),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "ai",
)]
pub async fn review(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Query(params): Query<ReviewParams>,
) -> Result<Json<Vec<ReviewRow>>, Failure> {
    // Manage: deciding what a tag means for the whole library is an editorial act, not a reader's.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let items = dam_db::enrichment::review_queue(conn.executor(), &caller.predicate, limit).await?;
    conn.commit().await?;
    Ok(Json(items.into_iter().map(into_review_row).collect()))
}

/// How much of the queue to return.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct ReviewParams {
    pub limit: Option<i64>,
}

fn into_review_row(item: dam_db::enrichment::ReviewItem) -> ReviewRow {
    ReviewRow {
        asset_id: item.asset_id,
        filename: item.filename,
        mime: item.mime,
        suggested: item
            .suggested
            .into_iter()
            .map(|tag| SuggestedTagView {
                term_id: tag.term_id,
                slug: tag.slug,
                label: tag.label,
                confidence: tag.confidence,
                votes: tag.votes,
                source: tag.source,
            })
            .collect(),
        fields: item
            .fields
            .into_iter()
            .map(|field| MachineFieldView {
                key: field.key,
                value: field.value,
                model: field.model,
                confidence: field.confidence,
                reviewed: field.reviewed,
            })
            .collect(),
    }
}

/// What a model wrote on one asset (G2, Article 50).
#[utoipa::path(
    get,
    path = "/assets/{id}/ai",
    responses(
        (status = 200, body = Vec<MachineFieldView>),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "ai",
)]
pub async fn asset_disclosure(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Vec<MachineFieldView>>, Failure> {
    // Read, deliberately: this is a disclosure. Somebody who may see the asset may see that a model wrote its
    // description — that is the whole point of marking it, and gating it behind Manage would make the marking
    // invisible to the people it exists for.
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id)
        .await?
        .is_none()
    {
        conn.commit().await?;
        return Err(Failure::NotFound);
    }
    let disclosed = dam_db::enrichment::machine_written(conn.executor(), asset_id).await?;
    let values: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_optional(conn.executor())
            .await
            .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    let values = values.unwrap_or_else(|| serde_json::json!({}));
    Ok(Json(
        disclosed
            .into_iter()
            .map(|(key, marking)| MachineFieldView {
                value: values.get(&key).cloned().unwrap_or(serde_json::Value::Null),
                key,
                model: marking
                    .get("model")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                confidence: marking
                    .get("confidence")
                    .and_then(serde_json::Value::as_f64),
                reviewed: marking
                    .get("reviewed_by")
                    .is_some_and(|value| !value.is_null()),
            })
            .collect(),
    ))
}

/// Confirms or rejects a suggested tag.
#[utoipa::path(
    patch,
    path = "/assets/{asset_id}/tags/{term_id}",
    request_body = TagDecision,
    responses(
        (status = 204, description = "The decision was recorded"),
        (status = 404, description = "No such asset, or nothing suggested to decide"),
    ),
    tag = "ai",
)]
pub async fn decide_tag(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Path((asset_id, term_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<TagDecision>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id)
        .await?
        .is_none()
    {
        conn.commit().await?;
        return Err(Failure::NotFound);
    }
    let decided = dam_db::enrichment::decide_tag(
        conn.executor(),
        asset_id,
        term_id,
        if request.accept {
            dam_db::enrichment::Verdict::Accept
        } else {
            dam_db::enrichment::Verdict::Reject
        },
        Some(caller.identity_id),
    )
    .await?;
    conn.commit().await?;
    if decided {
        // No content, not an empty 200: there is nothing to return, and a 200 with no body is a shape every
        // client has to special-case — which one of them did not, and the browser suite is where that surfaced.
        Ok(StatusCode::NO_CONTENT)
    } else {
        // Two reviewers with the same queue open is an ordinary race, and the second click is not an error —
        // but it also did not do anything, and saying 200 would claim it had.
        Err(Failure::NotFound)
    }
}

/// How a library-wide description is getting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BackfillView {
    /// Assets with a proxy that no model has described under the current prompt.
    pub outstanding: i64,
    /// Assets that have one.
    pub described: i64,
    /// Requests sitting in a batch that has not ended yet.
    pub in_flight: i64,
    /// Whether a slice is queued or in flight right now.
    pub running: bool,
}

/// Starts, or nudges, a library-wide description.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct BackfillRequest {
    /// How many assets per batch. Smaller slices land their first descriptions sooner; the default is what the
    /// pipeline picks.
    pub slice: Option<i64>,
}

/// What starting one produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BackfillQueued {
    pub job_id: Uuid,
    /// How many assets are waiting to be described, at the moment it was queued.
    pub outstanding: i64,
}

/// How far a library-wide description has got.
#[utoipa::path(
    get,
    path = "/ai/backfill",
    responses(
        (status = 200, body = BackfillView),
        (status = 403, description = "The caller holds no manage scope"),
    ),
    tag = "ai",
)]
pub async fn read_backfill(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
) -> Result<Json<BackfillView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let progress = dam_db::enrichment::backfill_progress(
        conn.executor(),
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
    )
    .await?;
    conn.commit().await?;

    // A queued slice counts as running even before a batch exists, or the button offers itself again while the
    // first slice is still being packed.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs \
          WHERE tenant_id = $1 AND kind IN ('backfill_submit', 'backfill_collect') \
            AND state IN ('queued', 'running')",
    )
    .bind(caller.tenant_id)
    .fetch_one(&state.global)
    .await
    .map_err(dam_db::Error::from)?;

    Ok(Json(BackfillView {
        outstanding: progress.outstanding,
        described: progress.described,
        in_flight: progress.in_flight,
        running: queued > 0 || progress.in_flight > 0,
    }))
}

/// Describes the whole library, a batch at a time.
#[utoipa::path(
    post,
    path = "/ai/backfill",
    request_body = BackfillRequest,
    responses(
        (status = 202, body = BackfillQueued),
        (status = 409, description = "A backfill is already running"),
        (status = 422, description = "Enrichment is switched off, or there is nothing to describe"),
    ),
    tag = "ai",
)]
pub async fn start_backfill(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Json(request): Json<BackfillRequest>,
) -> Result<(StatusCode, Json<BackfillQueued>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let settings = dam_db::enrichment::settings(conn.executor()).await?;
    let progress = dam_db::enrichment::backfill_progress(
        conn.executor(),
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
    )
    .await?;
    conn.commit().await?;

    if !settings.is_enabled {
        return Err(Failure::Unprocessable(
            "enrichment is switched off for this tenant".to_owned(),
        ));
    }
    if progress.outstanding == 0 {
        // Not an error worth a 500 and not a silent success: a tenant clicking "describe everything" on a
        // library that is already described should be told so.
        return Err(Failure::Unprocessable(
            "every asset with a proxy already has a description under the current prompt"
                .to_owned(),
        ));
    }
    if progress.in_flight > 0 {
        return Err(Failure::Conflict(format!(
            "a batch of {} requests is still in flight; the next slice starts when it lands",
            progress.in_flight
        )));
    }

    let slice = request
        .slice
        .unwrap_or(dam_pipeline::backfill::DEFAULT_SLICE)
        .clamp(1, 10_000);
    let job_id = dam_pipeline::worker::enqueue_backfill(&state.global, caller.tenant_id, slice)
        .await
        .map_err(|_| Failure::Internal)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(BackfillQueued {
            job_id,
            outstanding: progress.outstanding,
        }),
    ))
}

// ───────────────────────────────────────────────────────────────────────────────────────────────
// A question, turned into the library's own search syntax (M5d)
// ───────────────────────────────────────────────────────────────────────────────────────────────

/// A question to translate.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AskRequest {
    /// What somebody typed. "Harbour photos from last week", not a query.
    pub question: String,
}

/// What the model made of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AskResult {
    /// The query, in the same syntax a person types. Put it in the search box and search.
    pub shorthand: String,
    /// One sentence for the person who asked, so a wrong query is correctable rather than mysterious.
    pub explanation: String,
    /// As claimed by the model. Shown, never used as a gate.
    pub confidence: f32,
    /// Whether the query parses. **A caller must check this**: a query that does not parse is not a query, and
    /// the honest fallback is to search the question as plain text — which is what the box would have done for
    /// free.
    pub parses: bool,
    /// Why it does not parse, when it does not: the parser's own message and column.
    pub problem: Option<crate::search::QueryProblem>,
    pub model: String,
}

/// Turns a question into a query.
///
/// ## Why this returns a query rather than results
///
/// Because the answer belongs in the search box. Somebody can see what was understood, correct it, and keep it —
/// and the results then come from the ordinary search path, under the ordinary predicate, with no second code
/// path that could filter differently. A model that answered with *assets* would be a second retrieval route
/// into a governed library, which is exactly what §12 argues against.
///
/// ## Read, and paid
///
/// Searching is what a reader does, so gating this behind Manage would put it where it is not needed. It costs a
/// call per question, which is why it has its own switch (`enrichment_settings.natural_language_search`, off by
/// default) and why the AI spend cap applies. The cap is the control.
#[utoipa::path(
    post,
    path = "/search/ask",
    request_body = AskRequest,
    responses(
        (status = 200, body = AskResult, description = "The translation; check `parses` before using it"),
        (status = 422, description = "Natural-language search is off, there is no credential, or the question is empty"),
        (status = 429, description = "The tenant is over its hard AI spend cap for this month"),
    ),
    tag = "ai",
)]
pub async fn ask_query(
    State(state): State<Arc<AiState>>,
    headers: HeaderMap,
    Json(request): Json<AskRequest>,
) -> Result<Json<AskResult>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let question = request.question.trim().to_owned();
    if question.is_empty() {
        return Err(Failure::Unprocessable(
            "there is no question to translate".to_owned(),
        ));
    }
    // Bounded before it reaches a paid endpoint: a question is a sentence, and a megabyte of prose in the box
    // is either a mistake or an attempt to spend somebody else's money.
    if question.chars().count() > MAX_QUESTION_CHARS {
        return Err(Failure::Unprocessable(format!(
            "a question is a sentence; this one is {} characters and the limit is {MAX_QUESTION_CHARS}",
            question.chars().count()
        )));
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let settings = dam_db::enrichment::settings(conn.executor()).await?;
    let credential = dam_db::ai_credentials::current(conn.executor()).await?;
    // One loader, so the vocabulary the model is given and the parser the answer goes through are the same
    // object. Loading them separately is how a model gets told about a field the parser has never heard of.
    let parse_schema = dam_db::fields::search_schema_on(conn.executor()).await?;
    conn.commit().await?;

    if !settings.natural_language_search {
        return Err(Failure::Unprocessable(
            "natural-language search is switched off for this tenant".to_owned(),
        ));
    }
    let Some(credential) = credential else {
        return Err(Failure::Unprocessable(
            "no model credential is configured, so there is nothing to ask".to_owned(),
        ));
    };

    // Before the call. A reader can spend here, so the cap is the only thing standing between a paid endpoint
    // and a script in a loop.
    let period = dam_db::quotas::month_start(chrono::Utc::now());
    let verdict = {
        let mut global = state.global.acquire().await.map_err(dam_db::Error::from)?;
        dam_db::quotas::check(
            &mut global,
            caller.tenant_id,
            dam_db::quotas::AI_SPEND,
            period,
        )
        .await?
    };
    if !verdict.allowed() {
        return Err(Failure::Throttled(
            "the tenant is over its hard AI spend cap for this month".to_owned(),
        ));
    }

    // The vocabulary the model may use: the tenant's own fields with their kinds and aliases, and the category
    // paths. Sorted, because the instructions are the cached prefix and a different order is a different prefix.
    let by_key = parse_schema.aliases_by_key();
    let mut fields: Vec<dam_ai::ask::FieldHint> = parse_schema
        .fields()
        .iter()
        .map(|def| dam_ai::ask::FieldHint {
            key: def.key.clone(),
            kind: def.kind.as_str().to_owned(),
            alias: by_key.get(&def.key).cloned(),
        })
        .collect();
    fields.sort_by(|left, right| left.key.cmp(&right.key));
    let paths = parse_schema.category_paths();

    let model = dam_ai::credential::open(
        &credential,
        caller.tenant_slug.as_str(),
        &state.keyring,
        Arc::clone(&state.transport),
        settings.model.as_deref(),
    )
    .map_err(|error| Failure::Unprocessable(error.to_string()))?;

    let asked = dam_ai::ask::translate(
        model.as_ref(),
        &dam_ai::ask::Vocabulary {
            fields,
            categories: paths,
        },
        &question,
        &chrono::Utc::now().date_naive().to_string(),
    )
    .await
    .map_err(|error| match error {
        ModelError::Declined(_) => {
            Failure::Unprocessable("the model declined to answer that question".to_owned())
        }
        ModelError::RateLimited(_) => {
            Failure::Throttled("the provider is rate limiting; try again shortly".to_owned())
        }
        other => Failure::Unprocessable(other.to_string()),
    })?;

    // Charged whatever the answer was: the call was made. Failing to charge for an unusable answer would make
    // the cap easy to evade with bad questions.
    {
        let mut global = state.global.acquire().await.map_err(dam_db::Error::from)?;
        let micro = state.prices.estimate(&asked.model, &asked.usage);
        dam_db::quotas::charge(
            &mut global,
            caller.tenant_id,
            dam_db::quotas::AI_SPEND,
            period,
            micro,
        )
        .await?;
    }

    // The real parser, the same one a typed query goes through. This is what makes the feature safe: whatever
    // the model produced is either a query this library understands or an error with a column.
    let problem = match dam_core::shorthand::parse(&asked.shorthand, &parse_schema) {
        Ok(_) => None,
        Err(error) => Some(crate::search::QueryProblem {
            message: error.detail.clone(),
            code: Some(error.code.to_owned()),
            at: Some(error.column),
            // The model produced this query, so the suggestion is for whoever reads the failure — the same
            // one-click fix a hand-typed query gets.
            suggestion: error.suggestion.clone(),
        }),
    };

    Ok(Json(AskResult {
        shorthand: asked.shorthand,
        explanation: asked.explanation,
        confidence: asked.confidence,
        parses: problem.is_none(),
        problem,
        model: asked.model,
    }))
}

/// Longest question accepted, in characters.
///
/// A question is a sentence. The shorthand parser has its own, larger bound for a *query*; this one exists
/// because everything past it is paid for by the tenant.
pub const MAX_QUESTION_CHARS: usize = 500;
