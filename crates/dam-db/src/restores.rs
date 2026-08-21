//! The restore request lifecycle (3.4, §6.5).
//!
//! `dam_core::restore` decides what a restore will cost and when it will land. This is the bookkeeping, and
//! two parts of it are the reason the module exists rather than the caller writing an INSERT.
//!
//! ## Duplicate requests coalesce, they do not queue
//!
//! `restore_requests_inflight_idx` is `UNIQUE (object_key, pool_id) WHERE state IN (...)` — the in-flight
//! states only. Two people asking for the same archived asset must share one request and one S3 call: paying
//! twice for the same retrieval is a real charge, and the second `RestoreObject` on an ongoing restore is
//! billed. [`request`] returns the existing row rather than failing, because from the caller's side "somebody
//! already asked" and "you asked" have the same answer — it will be ready at the same time.
//!
//! ## Siblings batch
//!
//! §6.5: one collection restore becomes one bulk job, not 400 expedited ones. The batch id groups them and
//! the first request owns the S3 call, so the other 399 cost nothing and complete together.
//!
//! ## The expiry sweep is not optional
//!
//! A restored copy is temporary and the object's storage class never changed. When the copy lapses, a delivery
//! URL pointing at it starts failing at S3 with nothing explaining why — so the sweep moves the request to
//! `expired` and the placement's `restore_state` with it, which is what lets delivery say "this needs
//! restoring again" instead of surfacing a 403 from someone else's API.

use crate::Error;
use chrono::{DateTime, Utc};
use dam_core::restore::Plan;
use dam_core::storage::RestoreTier;
use uuid::Uuid;

/// A restore request as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRequest {
    pub id: Uuid,
    pub object_key: String,
    pub pool_id: Uuid,
    pub asset_id: Option<Uuid>,
    pub tier: String,
    pub state: String,
    pub batch_id: Option<Uuid>,
    pub eta_at: Option<DateTime<Utc>>,
    pub available_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub est_cost_cents: i64,
    pub bytes: i64,
    /// How long the temporary copy stays available once restored.
    ///
    /// On the request rather than read from configuration at issue time, because it is what was *promised*:
    /// the plan the caller approved said seven days, and a default that changed next week must not silently
    /// shorten a copy somebody is already relying on.
    pub keep_warm_days: i32,
}

impl RestoreRequest {
    /// Whether this request is still working towards a copy.
    ///
    /// The same set as the partial unique index. Kept in one place so a state added to the CHECK constraint
    /// cannot be forgotten here — a new in-flight state missing from this list would silently allow a
    /// duplicate S3 call.
    pub fn is_in_flight(&self) -> bool {
        matches!(
            self.state.as_str(),
            "queued" | "awaiting_approval" | "requested" | "ongoing"
        )
    }
}

/// What to ask for.
#[derive(Debug, Clone)]
pub struct RestoreSpec<'a> {
    pub object_key: &'a str,
    pub pool_id: Uuid,
    pub asset_id: Option<Uuid>,
    pub tier: RestoreTier,
    pub keep_warm_days: i32,
    pub requested_by: Option<Uuid>,
    /// Set when this is one of several siblings, so they share an S3 call.
    pub batch_id: Option<Uuid>,
    pub notify: serde_json::Value,
}

/// Whether a request was created or an existing one adopted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Created(RestoreRequest),
    /// Somebody had already asked. From the caller's side this is the same answer: it will be ready then.
    AlreadyInFlight(RestoreRequest),
}

impl Outcome {
    pub fn request(&self) -> &RestoreRequest {
        match self {
            Self::Created(r) | Self::AlreadyInFlight(r) => r,
        }
    }
}

/// Records a restore request, coalescing with any in-flight one for the same object.
///
/// The state comes from the plan: `awaiting_approval` when it needs a human, `queued` otherwise. Deciding
/// that here rather than in the caller means a plan that needs approval cannot be enqueued by a caller that
/// forgot to look.
pub async fn request(
    conn: &mut sqlx::PgConnection,
    spec: &RestoreSpec<'_>,
    plan: &Plan,
) -> Result<Outcome, Error> {
    let state = if plan.needs_approval {
        "awaiting_approval"
    } else {
        "queued"
    };

    let id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO restore_requests \
         (id, object_key, pool_id, asset_id, tier, keep_warm_days, state, requested_by, \
          eta_at, expires_at, est_cost_cents, bytes, batch_id, notify) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(spec.object_key)
    .bind(spec.pool_id)
    .bind(spec.asset_id)
    .bind(tier_str(spec.tier))
    .bind(spec.keep_warm_days)
    .bind(state)
    .bind(spec.requested_by)
    .bind(plan.eta_at)
    .bind(plan.expires_at)
    .bind(i64::try_from(plan.est_cost_cents).unwrap_or(i64::MAX))
    .bind(i64::try_from(plan.bytes).unwrap_or(i64::MAX))
    .bind(spec.batch_id)
    .bind(&spec.notify)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    // `ON CONFLICT DO NOTHING` covers the partial unique index without naming it. Naming the constraint would
    // couple this to the index's exact predicate, and the predicate is the list of in-flight states — which is
    // the thing most likely to gain a member.
    let existing = in_flight_for(&mut *conn, spec.object_key, spec.pool_id)
        .await?
        .ok_or_else(|| {
            Error::Inconsistent(format!(
                "restore request for {} vanished immediately after being recorded",
                spec.object_key
            ))
        })?;

    Ok(if inserted > 0 {
        Outcome::Created(existing)
    } else {
        Outcome::AlreadyInFlight(existing)
    })
}

/// The in-flight request for an object, if there is one.
pub async fn in_flight_for(
    conn: &mut sqlx::PgConnection,
    object_key: &str,
    pool_id: Uuid,
) -> Result<Option<RestoreRequest>, Error> {
    let row = sqlx::query_as::<_, RestoreRow>(
        "SELECT id, object_key, pool_id, asset_id, tier, state, batch_id, eta_at, available_at, \
                expires_at, est_cost_cents, bytes, keep_warm_days \
         FROM restore_requests \
         WHERE object_key = $1 AND pool_id = $2 \
           AND state IN ('queued', 'awaiting_approval', 'requested', 'ongoing')",
    )
    .bind(object_key)
    .bind(pool_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(into_request))
}

/// Approves a request that was held for a human.
///
/// Returns whether this call did the approving, so an audit entry is written once. A request that was never
/// held is left alone rather than "re-approved" — approving something that did not need it would put an
/// administrator's name against a decision they were never asked to make.
pub async fn approve(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    approver: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE restore_requests \
         SET state = 'queued', approved_by = $2, approved_at = $3, updated_at = $3 \
         WHERE id = $1 AND state = 'awaiting_approval'",
    )
    .bind(id)
    .bind(approver)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Claims queued requests for a worker, oldest first.
///
/// `FOR UPDATE SKIP LOCKED`, so several workers can drain the queue without one blocking behind another's
/// row — the same pattern the job queue uses.
///
/// The caller's transaction is the claim's boundary. This took a connection from a pool and opened its own
/// transaction in the first version, which read as self-contained and made the function unusable: a tenant
/// table needs the tenant's `search_path`, that lives on a `TenantConn`'s connection, and a pool handed the
/// same query the `dam_global` search path. The lock and the state change still share one transaction — it is
/// the caller's, which is where the rest of a poll's writes are anyway.
pub async fn claim_queued(
    conn: &mut sqlx::PgConnection,
    limit: i64,
    now: DateTime<Utc>,
) -> Result<Vec<RestoreRequest>, Error> {
    let rows = sqlx::query_as::<_, RestoreRow>(
        "SELECT id, object_key, pool_id, asset_id, tier, state, batch_id, eta_at, available_at, \
                expires_at, est_cost_cents, bytes, keep_warm_days \
         FROM restore_requests WHERE state = 'queued' \
         ORDER BY requested_at LIMIT $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;

    if !rows.is_empty() {
        let ids: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
        sqlx::query(
            "UPDATE restore_requests SET state = 'requested', updated_at = $2 WHERE id = ANY($1)",
        )
        .bind(&ids)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }

    Ok(rows
        .into_iter()
        .map(|row| {
            let mut request = into_request(row);
            request.state = "requested".to_owned();
            request
        })
        .collect())
}

/// Marks a request's copy available.
///
/// `expires_at` is recomputed from the actual availability rather than kept from the plan: the plan's figure
/// was derived from an *estimated* ETA, and a Bulk restore that landed early would otherwise expire early too,
/// giving the user less warm time than they were promised.
pub async fn mark_available(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE restore_requests \
         SET state = 'available', available_at = $2, \
             expires_at = $2 + make_interval(days => keep_warm_days), updated_at = $2 \
         WHERE id = $1 AND state IN ('requested', 'ongoing')",
    )
    .bind(id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Records a failure, keeping the reason.
pub async fn mark_failed(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE restore_requests SET state = 'failed', last_error = $2, updated_at = $3 \
         WHERE id = $1 AND state <> 'available'",
    )
    .bind(id)
    .bind(reason)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Expires every available copy whose window has passed.
///
/// Returns the requests that lapsed, so the caller can invalidate whatever pointed at them. Without this the
/// copy disappears at S3 and a delivery URL starts failing with a 403 that nothing in our system explains.
pub async fn sweep_expired(
    conn: &mut sqlx::PgConnection,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<RestoreRequest>, Error> {
    let rows = sqlx::query_as::<_, RestoreRow>(
        "UPDATE restore_requests SET state = 'expired', updated_at = $1 \
         WHERE id IN (SELECT id FROM restore_requests \
                      WHERE state = 'available' AND expires_at IS NOT NULL AND expires_at <= $1 \
                      ORDER BY expires_at LIMIT $2) \
         RETURNING id, object_key, pool_id, asset_id, tier, state, batch_id, eta_at, available_at, \
                   expires_at, est_cost_cents, bytes, keep_warm_days",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(into_request).collect())
}

/// The in-flight requests worth asking S3 about.
///
/// Ordered by `eta_at`, which is what `restore_requests_poll_idx` indexes — the index has existed since the
/// first migration with no query behind it, because nothing ever polled.
///
/// Requests with no ETA are included rather than skipped, and come first. A null there means the estimate
/// could not be made, not that the restore is not happening, and excluding them would strand a request
/// forever in `requested` with nothing ever looking at it again.
///
/// No clock argument, deliberately: polling only the ones whose ETA has passed would be trusting an estimate
/// to decide when to check reality. Glacier frequently lands early, and a request that was going to be ready
/// in twelve hours and arrived in twenty minutes should not wait out the other eleven.
pub async fn due_for_poll(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<RestoreRequest>, Error> {
    let rows = sqlx::query_as::<_, RestoreRow>(
        "SELECT id, object_key, pool_id, asset_id, tier, state, batch_id, eta_at, available_at, \
                expires_at, est_cost_cents, bytes, keep_warm_days \
         FROM restore_requests \
         WHERE state IN ('requested', 'ongoing') \
         ORDER BY eta_at NULLS FIRST LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(into_request).collect())
}

/// One request by id.
///
/// So a handler that just changed a request's state can answer with the row rather than with the boolean the
/// update returned — a caller that approved something needs to see what they approved.
pub async fn by_id(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
) -> Result<Option<RestoreRequest>, Error> {
    let row = sqlx::query_as::<_, RestoreRow>(
        "SELECT id, object_key, pool_id, asset_id, tier, state, batch_id, eta_at, available_at, \
                expires_at, est_cost_cents, bytes, keep_warm_days \
         FROM restore_requests WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(into_request))
}

/// Every request in a batch.
///
/// So a worker can issue **one** S3 call and then mark all of them, which is what makes a 400-asset collection
/// restore one bulk job rather than 400.
pub async fn in_batch(
    conn: &mut sqlx::PgConnection,
    batch_id: Uuid,
) -> Result<Vec<RestoreRequest>, Error> {
    let rows = sqlx::query_as::<_, RestoreRow>(
        "SELECT id, object_key, pool_id, asset_id, tier, state, batch_id, eta_at, available_at, \
                expires_at, est_cost_cents, bytes, keep_warm_days \
         FROM restore_requests WHERE batch_id = $1 ORDER BY requested_at",
    )
    .bind(batch_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(into_request).collect())
}

/// What a tenant has spent on restores in the current calendar month.
///
/// Feeds `dam_core::restore::Budget::spent_this_month_cents`. Counts everything that reached S3 — including
/// failures, because a failed retrieval is often still billed, and a budget that ignored them would let a
/// retry loop spend without limit.
pub async fn spent_this_month(
    conn: &mut sqlx::PgConnection,
    now: DateTime<Utc>,
) -> Result<u64, Error> {
    let spent: Option<i64> = sqlx::query_scalar(
        "SELECT coalesce(sum(est_cost_cents), 0)::bigint FROM restore_requests \
         WHERE state IN ('requested', 'ongoing', 'available', 'expired', 'failed') \
           AND requested_at >= date_trunc('month', $1::timestamptz)",
    )
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    Ok(u64::try_from(spent.unwrap_or(0)).unwrap_or(0))
}

type RestoreRow = (
    Uuid,
    String,
    Uuid,
    Option<Uuid>,
    String,
    String,
    Option<Uuid>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    i64,
    i64,
    i32,
);

fn into_request(row: RestoreRow) -> RestoreRequest {
    let (
        id,
        object_key,
        pool_id,
        asset_id,
        tier,
        state,
        batch_id,
        eta_at,
        available_at,
        expires_at,
        est_cost_cents,
        bytes,
        keep_warm_days,
    ) = row;
    RestoreRequest {
        id,
        object_key,
        pool_id,
        asset_id,
        tier,
        state,
        batch_id,
        eta_at,
        available_at,
        expires_at,
        est_cost_cents,
        bytes,
        keep_warm_days,
    }
}

fn tier_str(tier: RestoreTier) -> &'static str {
    match tier {
        RestoreTier::Expedited => "expedited",
        RestoreTier::Standard => "standard",
        RestoreTier::Bulk => "bulk",
    }
}
