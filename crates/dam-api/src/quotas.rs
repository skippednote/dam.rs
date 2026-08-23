//! Where a tenant stands against its caps (G19).
//!
//! ## Read by the tenant, set by the operator
//!
//! Reading needs `Manage`: a cap is a commercial fact about the account, and somebody who can upload should not
//! learn how close the library is to a limit they cannot change. Setting one is not here at all — it is a
//! `damctl` command, because a tenant raising its own cap is not a feature.
//!
//! ## A level and a flow are the same number meaning different things
//!
//! "1.2 TB" is a measurement of what exists if the quota is `storage_bytes` and a total of what happened if it
//! is `egress_bytes_month`. So `is_level` travels with every row and the screen says which — a bar labelled
//! only with a percentage would let somebody read a month's egress as the size of their library.
//!
//! ## The two timestamps are the answer to "we were not told"
//!
//! `warned_at` and `exceeded_at` are stamped the first time each line is crossed and never moved. Surfaced here
//! rather than kept for support, because the tenant is the person who most needs to know that they have been
//! over for three weeks.
//!
//! ## An absent quota is absent, not zero
//!
//! Only configured caps appear. A key with no row is not a cap of nothing — `dam_db::quotas::check` says so —
//! and listing it with a limit of zero would read as exhausted.

use crate::assets::Failure;
use crate::caller;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::quotas;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;

pub struct QuotaState {
    pub global: PgPool,
}

impl std::fmt::Debug for QuotaState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaState").finish_non_exhaustive()
    }
}

pub fn router(state: QuotaState) -> Router {
    Router::new()
        .route("/quotas", get(list))
        .with_state(Arc::new(state))
}

/// One cap and where the tenant stands against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct QuotaView {
    /// `storage_bytes`, `asset_count`, `egress_bytes_month`, `ai_spend_cents_month`,
    /// `restore_spend_cents_month`, `api_requests_minute` or `seats`.
    pub quota_key: String,
    pub limit_value: i64,
    pub used: i64,
    /// The fraction at which a warning fires. Sent so a screen can draw the line rather than inventing 80%.
    pub warn_at_fraction: f32,
    /// `soft` or `hard`. A soft cap warns and keeps serving; that difference is the whole reason enforcement is
    /// per quota rather than per tenant — a hard cap on ingest loses a customer's work.
    pub enforcement: String,
    /// `allowed`, `warned` or `refused`, computed from the numbers rather than stored.
    pub standing: String,
    /// Whether `used` measures what exists or totals what happened this month. The same number means very
    /// different things, and a screen that did not say which would be misleading about the more alarming one.
    pub is_level: bool,
    pub warned_at: Option<chrono::DateTime<chrono::Utc>>,
    pub exceeded_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Every configured cap, and the month they are counted in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct QuotasView {
    /// The first day of the period the flow quotas are counted in. A calendar month in UTC, because an invoice
    /// is a calendar month — a cap that did not line up with the bill it protects would be explaining itself
    /// forever.
    pub period_start: chrono::NaiveDate,
    pub quotas: Vec<QuotaView>,
}

#[utoipa::path(
    get,
    path = "/quotas",
    responses(
        (status = 200, body = QuotasView),
        (status = 403, description = "A cap is a commercial fact about the account; reading it needs Manage"),
    ),
    tag = "quotas",
)]
pub async fn list(
    State(state): State<Arc<QuotaState>>,
    headers: HeaderMap,
) -> Result<Json<QuotasView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let period_start = quotas::month_start(chrono::Utc::now());
    // `tenant_quotas` and `tenant_spend` are control-plane tables, so this is the global pool rather than a
    // tenant transaction — the same reason `auth::grants_for` reads membership there.
    let mut conn = state.global.acquire().await.map_err(dam_db::Error::from)?;
    let standing = quotas::standing(&mut conn, caller.tenant_id, period_start).await?;

    Ok(Json(QuotasView {
        period_start,
        quotas: standing
            .into_iter()
            .map(|row| QuotaView {
                standing: match row.verdict {
                    quotas::Verdict::Allowed => "allowed",
                    quotas::Verdict::Warned { .. } => "warned",
                    quotas::Verdict::Refused { .. } => "refused",
                }
                .to_owned(),
                enforcement: match row.quota.enforcement {
                    quotas::Enforcement::Hard => "hard",
                    quotas::Enforcement::Soft => "soft",
                }
                .to_owned(),
                quota_key: row.quota_key,
                limit_value: row.quota.limit_value,
                used: row.used,
                warn_at_fraction: row.quota.warn_at_fraction,
                is_level: row.is_level,
                warned_at: row.warned_at,
                exceeded_at: row.exceeded_at,
            })
            .collect(),
    }))
}
