//! Asking for cold storage, and asking to get out of it (§6.4, §6.5).
//!
//! Three audiences, and the shape of each route follows from which one it serves.
//!
//! **Somebody who wants an archived asset** asks for a restore and is told what it will cost and when it will
//! land, *before* confirming. That is §6.5's requirement and the reason `POST /assets/{id}/restore` answers
//! with a plan rather than an acknowledgement: Expedited against Bulk is roughly 10× on price and 100× on
//! latency, and a user picking without seeing either is guessing with their employer's money.
//!
//! **An administrator** reads what the tiering policies would do before they do it. `POST
//! /lifecycle/policies/{id}/plan` runs the planner and returns every candidate paired with a verdict —
//! including the skips, with their reasons, which are the answer to the only question anybody asks about a
//! lifecycle run ("why did nothing happen?").
//!
//! **A release approver** clears the restores that came in over the threshold.
//!
//! ## The plan is a GET-shaped thing behind a POST
//!
//! It has no side effects, so `GET` would be defensible. It is a `POST` because it is expensive — a full scan
//! of a tenant's placements — and because caching it would be actively wrong: the answer changes as objects
//! age into eligibility, and a proxy serving yesterday's plan would be showing an operator a set of
//! transitions that no longer describes what would happen.
//!
//! ## Nothing here executes a transition
//!
//! `POST /lifecycle/runs` enqueues the sweep; it does not perform it. A synchronous endpoint that moved
//! terabytes would hold a request open for hours and lose its work to the first timeout, and §8.3's rule is
//! that all library-scale work runs in the queue. The response says what was queued, and the sweep's own
//! `dry_run` decides whether anything moves — which means an operator cannot accidentally execute a plan by
//! pressing the button that shows it to them.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use dam_core::policy::Action;
use dam_core::storage::RestoreTier;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the archival endpoints need.
pub struct ArchivalState {
    pub global: PgPool,
}

impl std::fmt::Debug for ArchivalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchivalState").finish_non_exhaustive()
    }
}

/// The archival routes.
pub fn router(state: ArchivalState) -> Router {
    Router::new()
        .route("/lifecycle/policies", get(policies))
        .route("/lifecycle/policies/{id}/plan", post(plan))
        .route("/lifecycle/runs", post(run))
        .route(
            "/assets/{asset_id}/restore",
            get(restore_state).post(request_restore),
        )
        .route("/assets/{asset_id}/restore/quote", get(quote))
        .route("/restores/{id}/approve", post(approve))
        .with_state(Arc::new(state))
}

// ─── policies ───────────────────────────────────────────────────────────────

/// One tiering rule, as an administrator reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PolicyView {
    pub id: Uuid,
    pub name: String,
    pub enabled: bool,
    /// `original`, `derivative` or `both`.
    pub applies_to: String,
    /// Where eligible objects go.
    pub target_class: String,
    /// Days of idleness before an object is eligible.
    pub after_days: u32,
    /// **True by default.** A policy that has never been taken off dry run has never moved anything, which is
    /// the state every policy starts in and the most important thing on this row.
    pub dry_run: bool,
    pub max_objects_per_run: Option<u32>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_run_moved: Option<i32>,
}

/// Every enabled policy, in the order the engine applies them.
#[utoipa::path(
    get,
    path = "/lifecycle/policies",
    responses((status = 200, description = "The tiering rules", body = [PolicyView])),
    tag = "archival",
)]
pub async fn policies(
    State(state): State<Arc<ArchivalState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<PolicyView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = dam_db::tiering::policies(conn.executor()).await?;
    let runs = last_runs(conn.executor()).await?;
    conn.commit().await?;

    Ok(Json(
        rows.into_iter()
            .map(|policy| {
                let (at, moved) = runs
                    .iter()
                    .find(|(id, _, _)| *id == policy.id)
                    .map_or((None, None), |(_, at, moved)| (*at, *moved));
                PolicyView {
                    id: policy.id,
                    name: policy.engine.name,
                    enabled: policy.engine.enabled,
                    applies_to: policy.applies_to,
                    target_class: policy.engine.target_class.to_string(),
                    after_days: policy.engine.after_days,
                    dry_run: policy.engine.dry_run,
                    max_objects_per_run: policy.engine.max_objects_per_run,
                    last_run_at: at,
                    last_run_moved: moved,
                }
            })
            .collect(),
    ))
}

type LastRun = (Uuid, Option<DateTime<Utc>>, Option<i32>);

async fn last_runs(conn: &mut sqlx::PgConnection) -> Result<Vec<LastRun>, Failure> {
    Ok(
        sqlx::query_as("SELECT id, last_run_at, last_run_moved FROM lifecycle_policies")
            .fetch_all(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?,
    )
}

/// What one policy would do, without doing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PlanView {
    pub policy_name: String,
    /// Whether executing this policy would move anything at all.
    pub dry_run: bool,
    /// Present when the run stopped early, with what stopped it. A truncated plan that did not say so reads
    /// exactly like a policy that is working.
    pub halted: Option<String>,
    pub transitions: Vec<TransitionView>,
    /// Every candidate that was examined and left alone, with why. The answer to "why did nothing happen?".
    pub skipped: Vec<SkipView>,
}

/// One object a run would move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TransitionView {
    pub object_key: String,
    pub from: String,
    pub to: String,
    pub size_bytes: u64,
    /// When this object could next move, after this one. The minimum billable duration the new class starts.
    pub min_duration_until: Option<DateTime<Utc>>,
}

/// One object a run would leave alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SkipView {
    pub object_key: String,
    /// A stable machine-readable reason, for grouping a plan of ten thousand into a handful of lines.
    pub reason: String,
    /// The human sentence, where there is more to say — a pin's reason, or the date something becomes
    /// eligible.
    pub detail: Option<String>,
}

/// Plans one policy against the current library.
#[utoipa::path(
    post,
    path = "/lifecycle/policies/{id}/plan",
    responses(
        (status = 200, description = "What the policy would do", body = PlanView),
        (status = 404, description = "No such policy"),
    ),
    tag = "archival",
)]
pub async fn plan(
    State(state): State<Arc<ArchivalState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<PlanView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Disabled policies plan too, and deliberately: reading what a rule *would* do before enabling it is the
    // whole reason this endpoint exists, and refusing until it is on would mean the only way to find out is to
    // turn it on.
    let Some(policy) = dam_db::tiering::policy(conn.executor(), id).await? else {
        return Err(Failure::NotFound);
    };
    let now = Utc::now();
    let candidates = dam_db::tiering::candidates(conn.executor(), &policy, now).await?;
    conn.commit().await?;

    let plan = dam_store::lifecycle::plan(&policy.engine, &candidates, now);
    Ok(Json(PlanView {
        policy_name: plan.policy_name.clone(),
        dry_run: plan.dry_run,
        halted: plan.halted.as_ref().map(|halt| match halt {
            dam_store::lifecycle::HaltReason::PolicyDisabled => "policy_disabled".to_owned(),
            dam_store::lifecycle::HaltReason::ObjectLimit { limit, remaining } => {
                format!("stopped at {limit} objects, {remaining} not examined")
            }
            dam_store::lifecycle::HaltReason::Unsupported { what } => {
                format!("not executable yet: {what}")
            }
        }),
        transitions: plan
            .transitions()
            .map(|one| TransitionView {
                object_key: one.object_key.as_str().to_owned(),
                from: one.from.to_string(),
                to: one.to.to_string(),
                size_bytes: one.size_bytes,
                min_duration_until: one.min_duration_until,
            })
            .collect(),
        skipped: plan
            .skipped()
            .map(|(key, reason)| SkipView {
                object_key: key.as_str().to_owned(),
                reason: skip_code(reason).to_owned(),
                detail: skip_detail(reason),
            })
            .collect(),
    }))
}

/// The stable name for a skip, for a client that groups them.
fn skip_code(reason: &dam_store::lifecycle::SkipReason) -> &'static str {
    use dam_store::lifecycle::SkipReason as R;
    match reason {
        R::Pinned { .. } => "pinned",
        R::TierExempt => "tier_exempt",
        R::NotYetEligible { .. } => "not_yet_eligible",
        R::MinDurationNotElapsed { .. } => "min_duration_not_elapsed",
        R::AlreadyInClass => "already_in_class",
        R::WouldWarm { .. } => "would_warm",
        R::BelowMinimumSize { .. } => "below_minimum_size",
        R::NotPresent { .. } => "not_present",
        R::RestoreInFlight { .. } => "restore_in_flight",
    }
}

fn skip_detail(reason: &dam_store::lifecycle::SkipReason) -> Option<String> {
    use dam_store::lifecycle::SkipReason as R;
    match reason {
        R::Pinned { reason } => reason.clone(),
        R::NotYetEligible { eligible_at } => Some(format!("eligible from {eligible_at}")),
        R::MinDurationNotElapsed { until } => Some(format!("its class is billed until {until}")),
        R::WouldWarm { from, to } => Some(format!(
            "{from} to {to} is a restore, not a transition; a policy cannot warm an object"
        )),
        R::BelowMinimumSize { size, minimum } => Some(format!(
            "{size} bytes is below the {minimum}-byte floor, where the colder class bills more than it saves"
        )),
        R::NotPresent { state } => Some(format!("the placement is {state}")),
        // A restore in flight is its own explanation: somebody may be downloading the temporary copy right
        // now, and the retrieval fee is already spent.
        R::RestoreInFlight { state } => Some(format!("a restore is {state}")),
        R::TierExempt | R::AlreadyInClass => None,
    }
}

/// What a queued run reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RunQueued {
    pub job_id: Uuid,
    /// Restated on the response, because "I pressed run and nothing moved" is otherwise a support ticket
    /// rather than a policy that is still in dry run.
    pub policies_in_dry_run: usize,
    pub policies_enabled: usize,
}

/// Queues a lifecycle sweep.
#[utoipa::path(
    post,
    path = "/lifecycle/runs",
    responses((status = 202, description = "The sweep is queued", body = RunQueued)),
    tag = "archival",
)]
pub async fn run(
    State(state): State<Arc<ArchivalState>>,
    headers: HeaderMap,
) -> Result<(axum::http::StatusCode, Json<RunQueued>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let policies = dam_db::tiering::policies(conn.executor()).await?;
    conn.commit().await?;

    let job_id = dam_pipeline::worker::enqueue_tier_sweep(&state.global, caller.tenant_id)
        .await
        .map_err(|_| Failure::Internal)?;

    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(RunQueued {
            job_id,
            policies_in_dry_run: policies.iter().filter(|p| p.engine.dry_run).count(),
            policies_enabled: policies.len(),
        }),
    ))
}

// ─── restores ───────────────────────────────────────────────────────────────

/// What a caller asks for.
///
/// Query parameters rather than a body, and not only for tidiness. A restore has no entity to send — the
/// asset is in the path and these two are modifiers — and an empty body under a JSON content-type is an
/// extractor rejection, which axum returns *before* the handler runs. That turned `POST .../restore` with no
/// body from a reader into a `400` about the body instead of the `403` the permission check owes them: the
/// gate never ran. A request with nothing to send should not have to send `{}` to find out it was refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema, utoipa::IntoParams)]
pub struct RestoreOptions {
    /// `expedited`, `standard` or `bulk`. Defaults to Standard — the middle, because a default of Expedited
    /// spends ten times more than most callers meant to and a default of Bulk makes the feature feel broken.
    #[serde(default)]
    pub tier: Option<String>,
    /// How long the copy stays warm. Defaults to seven days (§6.5).
    #[serde(default)]
    pub keep_warm_days: Option<i64>,
}

/// A restore, as the caller sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RestoreView {
    pub id: Uuid,
    pub tier: String,
    /// `queued`, `awaiting_approval`, `requested`, `ongoing`, `available`, `expired`, `failed`, `cancelled`.
    pub state: String,
    /// What this is expected to cost, in whole cents, rounded up so an approved estimate is not exceeded.
    pub est_cost_cents: i64,
    pub bytes: i64,
    /// When the copy is expected. The slow end of the documented window, so an ETA met early beats one missed.
    pub eta_at: Option<DateTime<Utc>>,
    pub available_at: Option<DateTime<Utc>>,
    /// When the temporary copy lapses. The storage class never changed, so after this the asset needs
    /// restoring again.
    pub expires_at: Option<DateTime<Utc>>,
    /// True when this request was already in flight and the caller has joined it rather than starting a
    /// second one. Two people asking for the same archived asset share one retrieval and one charge.
    pub joined_existing: bool,
}

impl RestoreView {
    fn of(request: dam_db::restores::RestoreRequest, joined_existing: bool) -> Self {
        Self {
            id: request.id,
            tier: request.tier,
            state: request.state,
            est_cost_cents: request.est_cost_cents,
            bytes: request.bytes,
            eta_at: request.eta_at,
            available_at: request.available_at,
            expires_at: request.expires_at,
            joined_existing,
        }
    }
}

/// Asks for an archived asset to be brought back.
///
/// `Download`, not `Manage`: a restore is the first half of taking a copy, and somebody who may download an
/// asset may ask for its bytes to be fetchable. It is also where the cost sits, which is why the threshold
/// exists — the answer to an expensive request is "somebody senior confirms", not "no".
#[utoipa::path(
    post,
    path = "/assets/{asset_id}/restore",
    params(RestoreOptions),
    responses(
        (status = 200, description = "The restore, planned and recorded", body = RestoreView),
        (status = 422, description = "The tier is not offered for this asset's class, or nothing is archived", body = String),
        (status = 404, description = "No such asset, or nothing archived to restore"),
    ),
    tag = "archival",
)]
pub async fn request_restore(
    State(state): State<Arc<ArchivalState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    axum::extract::Query(options): axum::extract::Query<RestoreOptions>,
) -> Result<Json<RestoreView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Download).await?;
    let tier = match options.tier.as_deref() {
        None => RestoreTier::Standard,
        Some(raw) => raw
            .parse()
            .map_err(|_| Failure::Unprocessable(format!("{raw:?} is not a restore tier")))?,
    };
    let keep_warm_days = options
        .keep_warm_days
        .unwrap_or(dam_core::restore::DEFAULT_KEEP_WARM_DAYS);

    // The caller's predicate first. Planning a restore for an asset this caller cannot see would disclose that
    // it exists, and the cost estimate would disclose roughly how large it is.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let visible = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id)
        .await?
        .is_some();
    conn.commit().await?;
    if !visible {
        return Err(Failure::NotFound);
    }

    let planned = dam_pipeline::tiering::plan_for(
        &state.global,
        &caller.tenant_slug,
        asset_id,
        tier,
        keep_warm_days,
        Utc::now(),
    )
    .await
    .map_err(|_| Failure::Internal)?;

    let (plan, placement) = match planned {
        Ok(planned) => planned,
        // The refusals are the caller's to read: "this is not archived" and "Deep Archive has no Expedited
        // tier" are both answers somebody can act on, and both are `Display` on the refusal itself so the
        // sentence the domain wrote is the sentence the caller gets.
        Err(refusal) => return Err(Failure::Unprocessable(refusal.to_string())),
    };

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let outcome = dam_db::restores::request(
        conn.executor(),
        &dam_db::restores::RestoreSpec {
            object_key: &placement.object_key,
            pool_id: placement.pool_id,
            asset_id: Some(asset_id),
            tier,
            keep_warm_days: i32::try_from(keep_warm_days).unwrap_or(7),
            requested_by: caller.identity_id,
            batch_id: None,
            notify: serde_json::json!({}),
        },
        &plan,
    )
    .await?;
    conn.commit().await?;

    // Only once there is something to poll for. A deployment where nobody has ever archived anything runs no
    // polling at all, which is the difference between a queue with a heartbeat and a queue with a job in it.
    dam_pipeline::worker::enqueue_restore_poll(&state.global, caller.tenant_id)
        .await
        .map_err(|_| Failure::Internal)?;

    Ok(Json(match outcome {
        dam_db::restores::Outcome::Created(request) => RestoreView::of(request, false),
        dam_db::restores::Outcome::AlreadyInFlight(request) => RestoreView::of(request, true),
    }))
}

/// The restore in flight for an asset, if there is one.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/restore",
    responses(
        (status = 200, description = "The current restore", body = Option<RestoreView>),
        (status = 404, description = "No such asset"),
    ),
    tag = "archival",
)]
pub async fn restore_state(
    State(state): State<Arc<ArchivalState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Option<RestoreView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id)
        .await?
        .is_none()
    {
        return Err(Failure::NotFound);
    }
    // The most recent request for this asset in any state, not only the in-flight ones: a caller looking at an
    // asset needs to see `available` (there is a copy, use it) and `failed` (it did not work, ask again) as
    // much as `ongoing`.
    let row: Option<LatestRestore> = sqlx::query_as(
        "SELECT id, tier, state, est_cost_cents, bytes, eta_at, available_at, expires_at \
             FROM restore_requests WHERE asset_id = $1 ORDER BY requested_at DESC LIMIT 1",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    Ok(Json(row.map(
        |(id, tier, state, est_cost_cents, bytes, eta_at, available_at, expires_at)| RestoreView {
            id,
            tier,
            state,
            est_cost_cents,
            bytes,
            eta_at,
            available_at,
            expires_at,
            joined_existing: false,
        },
    )))
}

/// The columns the latest-restore read selects, named so the row is not eight anonymous fields.
type LatestRestore = (
    Uuid,
    String,
    String,
    i64,
    i64,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

/// What each tier would cost and when it would land, without asking for anything.
///
/// §6.5 requires the estimate *before* the user confirms, and without this endpoint there was no way to
/// honour that: the only thing that produced a plan was the POST, which also records the request. So a screen
/// could either show a price or ask for a restore, and showing the price meant having already asked.
///
/// All three tiers in one response rather than one call per tier. The whole reason to show a number is the
/// comparison — 10× on price against 100× on latency — and three round trips to assemble one table would
/// leave a screen rendering half a decision.
///
/// A tier the class cannot offer is present with `available: false` and the reason, rather than absent. Deep
/// Archive has no Expedited, and a chooser that silently showed two options where another asset shows three
/// invites "why is this one different" — which is a question the response can simply answer.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/restore/quote",
    responses(
        (status = 200, description = "What each tier would cost", body = QuoteView),
        (status = 404, description = "No such asset"),
        (status = 422, description = "Nothing archived to restore", body = String),
    ),
    tag = "archival",
)]
pub async fn quote(
    State(state): State<Arc<ArchivalState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<QuoteView>, Failure> {
    // Read, not Download. A quote is a question about the library's shape and its costs, and a person who may
    // see an asset may see why it is slow. What it does *not* let them do is spend anything.
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id).await?;
    conn.commit().await?;
    if found.is_none() {
        return Err(Failure::NotFound);
    }

    let now = Utc::now();
    let mut options = Vec::new();
    let mut refusal = None;
    for tier in [
        RestoreTier::Expedited,
        RestoreTier::Standard,
        RestoreTier::Bulk,
    ] {
        let planned = dam_pipeline::tiering::plan_for(
            &state.global,
            &caller.tenant_slug,
            asset_id,
            tier,
            dam_core::restore::DEFAULT_KEEP_WARM_DAYS,
            now,
        )
        .await
        .map_err(|_| Failure::Internal)?;

        match planned {
            Ok((plan, _)) => options.push(QuoteOption {
                tier: tier.as_str().to_owned(),
                available: true,
                est_cost_cents: plan.est_cost_cents,
                eta_at: Some(plan.eta_at),
                needs_approval: plan.needs_approval,
                unavailable_because: None,
            }),
            Err(dam_core::restore::RestoreRefusal::TierUnavailable { .. }) => {
                options.push(QuoteOption {
                    tier: tier.as_str().to_owned(),
                    available: false,
                    est_cost_cents: 0,
                    eta_at: None,
                    needs_approval: false,
                    unavailable_because: Some(format!("this storage class has no {tier} tier",)),
                });
            }
            // Not-archived and nothing-stored are facts about the *asset*, not about a tier, so they refuse
            // the whole quote rather than appearing three times.
            Err(other) => refusal = Some(other.to_string()),
        }
    }

    if let Some(refusal) = refusal {
        return Err(Failure::Unprocessable(refusal));
    }
    Ok(Json(QuoteView { options }))
}

/// What restoring one asset would cost, per tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuoteView {
    pub options: Vec<QuoteOption>,
}

/// One tier's price and wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuoteOption {
    pub tier: String,
    /// False when the storage class does not offer this tier.
    pub available: bool,
    /// Whole cents, rounded up. Zero when the pool has no prices recorded, which means "this deployment does
    /// not know" rather than "free" — the screen should say so rather than promising nothing.
    pub est_cost_cents: u64,
    pub eta_at: Option<DateTime<Utc>>,
    /// Whether choosing this would wait for an administrator instead of starting.
    pub needs_approval: bool,
    pub unavailable_because: Option<String>,
}

/// Releases a restore that was held for approval.
#[utoipa::path(
    post,
    path = "/restores/{id}/approve",
    responses(
        (status = 200, description = "Released; the worker will issue it", body = RestoreView),
        (status = 404, description = "No such request, or it was not awaiting approval"),
    ),
    tag = "archival",
)]
pub async fn approve(
    State(state): State<Arc<ArchivalState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<RestoreView>, Failure> {
    // `Manage`, not `Download`. The threshold exists so that a large spend needs somebody other than the
    // person who asked, and letting the requester approve their own request would make it a confirmation
    // dialog rather than a control.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    // An approver without an identity cannot approve: the row records *who* released the spend, and a null
    // approver on an approved request is an audit trail saying the money was authorised by nobody.
    //
    // **Unobservable, and worth saying so rather than letting it read as tested.** `authorize` already
    // refuses a key with no identity — "no identity, no membership, no grants" — so nothing can reach this
    // branch, and mutating it changes no test outcome. It stays because it is the only correct handling of
    // the case and because `Caller::identity_id` is an `Option` that three other call sites also re-check;
    // the cleanup that would make all four unnecessary is its own item in TASKS.md. Documented because
    // undocumented unobservable code reads as covered when it is not.
    let Some(approver) = caller.identity_id else {
        return Err(Failure::Forbidden(
            "an approval records who authorised the spend, and a machine key has nobody behind it"
                .to_owned(),
        ));
    };
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let released = dam_db::restores::approve(conn.executor(), id, approver, Utc::now()).await?;
    if !released {
        return Err(Failure::NotFound);
    }
    let request = dam_db::restores::by_id(conn.executor(), id)
        .await?
        .ok_or(Failure::NotFound)?;
    conn.commit().await?;

    dam_pipeline::worker::enqueue_restore_poll(&state.global, caller.tenant_id)
        .await
        .map_err(|_| Failure::Internal)?;
    Ok(Json(RestoreView::of(request, false)))
}
