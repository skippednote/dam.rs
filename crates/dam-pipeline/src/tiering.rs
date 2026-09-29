//! Moving cold objects to cold storage, and warm copies back (§6.4, §6.5).
//!
//! Everything this needs already existed and nothing connected it, which is the same shape as 0.9: the
//! planner in `dam_store::lifecycle`, the S3 `transition` and `restore` calls (conformance-tested against
//! real Glacier nightly), the restore arithmetic in `dam_core::restore`, the bookkeeping in
//! `dam_db::restores`, and the schema for all of it. What was missing was anything that *ran* — so every
//! asset stayed in the class it was uploaded to, permanently, and `restore_requests` was a table nothing
//! wrote.
//!
//! ## Two jobs, because they fail differently
//!
//! [`sweep`] is a slow, bounded, idempotent pass that can be skipped for a day with no consequence. A
//! restore is a person waiting: [`poll`] runs often, and its failure mode is somebody staring at a spinner
//! for a retrieval that was never issued. Sharing one job kind would mean the daily pass's backoff deciding
//! how long that person waits.
//!
//! ## Both re-enqueue themselves
//!
//! The pattern M5c established for backfill: a run ends by scheduling the next one with a `run_after` and a
//! dedupe key. No cron, no leader election, and a deployment with no workers running simply resumes when one
//! starts. The chain is started by the API, and `dedupe_key` is what stops two API calls from producing two
//! chains.
//!
//! ## A transition is in place, and a cross-pool move is refused
//!
//! S3 has no transition API: changing an object's class is a self-copy with a new `StorageClass`, so the
//! object keeps its key and its bucket. That makes a same-pool class change the only move this executes, and
//! it is the one the planner can express. A policy naming a *different* target pool is asking for a copy
//! between buckets — real, and not this; it halts as `Unsupported` rather than silently tiering in place,
//! because "moved, but not where you said" is worse than "did nothing, and said so".

use crate::{Error, Result};
use chrono::{DateTime, Duration, Utc};
use dam_core::TenantSlug;
use dam_core::restore::{self, Budget, RestoreRefusal};
use dam_core::storage::{RestoreTier, StorageClass};
use dam_db::restores::{self as restore_db, RestoreRequest};
use dam_store::BlobStore;
use dam_store::lifecycle::{self, HaltReason, SkipReason};
use uuid::Uuid;

/// How long between sweeps.
///
/// Daily, because every input it reads moves on the scale of days: `idle_days`, `min_age_days`, and the
/// 30-to-180-day minimum billable durations. An hourly sweep would re-read the whole placement table to
/// discover that nothing has aged into eligibility since breakfast.
pub const SWEEP_EVERY: Duration = Duration::hours(24);

/// How long between restore polls.
///
/// Expedited Glacier lands in 1–5 minutes, so a poll interval longer than that is a person waiting for a
/// copy that has been sitting there. Bulk Deep Archive takes 48 hours and is polled just as often, which
/// costs one `HEAD` — cheaper than the alternative of deriving a schedule per tier and getting it wrong.
pub const POLL_EVERY: Duration = Duration::minutes(2);

/// What one sweep did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Swept {
    /// Objects whose class actually changed.
    pub moved: usize,
    /// Objects a plan named and a dry run did not touch.
    pub planned: usize,
    /// Objects examined and left alone, with a reason.
    pub skipped: usize,
    /// Objects the store refused to move.
    pub failed: usize,
    /// Policies that halted before finishing.
    pub halted: Vec<String>,
}

/// Runs every enabled policy once.
///
/// Errors from one policy do not abandon the rest: a policy naming a pool this deployment cannot reach is a
/// configuration problem with that policy, and letting it fail the job would mean one bad row stops all
/// tiering for the tenant — the failure mode that makes an operator turn the whole engine off.
pub async fn sweep(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &TenantSlug,
    now: DateTime<Utc>,
) -> Result<Swept> {
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let policies = dam_db::tiering::policies(conn.executor()).await?;
    conn.commit().await?;

    let mut swept = Swept::default();
    for policy in policies {
        match one_policy(global, store, slug, &policy, now).await {
            Ok(result) => {
                swept.moved += result.moved;
                swept.planned += result.planned;
                swept.skipped += result.skipped;
                swept.failed += result.failed;
                swept.halted.extend(result.halted);
            }
            Err(error) => tracing::error!(
                %error,
                policy = %policy.engine.name,
                policy_id = %policy.id,
                "a lifecycle policy could not run; the others continue",
            ),
        }
    }
    Ok(swept)
}

async fn one_policy(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &TenantSlug,
    policy: &dam_db::tiering::Policy,
    now: DateTime<Utc>,
) -> Result<Swept> {
    let mut swept = Swept::default();

    // Read the candidates and let go of the connection. A sweep can take minutes of S3 calls, and holding a
    // transaction across them would pin a connection and block the vacuum on a table this reads whole.
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let candidates = dam_db::tiering::candidates(conn.executor(), policy, now).await?;
    conn.commit().await?;

    if policy.action != "transition" {
        tracing::info!(
            policy = %policy.engine.name,
            action = %policy.action,
            "only transitions execute; eviction and replication are planned but not performed",
        );
        swept.halted.push(policy.engine.name.clone());
        return Ok(swept);
    }

    let plan = lifecycle::plan(&policy.engine, &candidates, now);
    swept.skipped = plan.skipped().count();
    for (key, reason) in plan.skipped() {
        // At debug, not info: on a library of any size the skips *are* the plan, and an operator asking why
        // nothing moved reads them deliberately rather than finding them in yesterday's log.
        tracing::debug!(policy = %policy.engine.name, key = %key.as_str(), ?reason, "not tiered");
        if let SkipReason::Pinned { reason: Some(why) } = reason {
            tracing::info!(policy = %policy.engine.name, key = %key.as_str(), %why, "pinned");
        }
    }
    if let Some(halt) = plan.halted.clone() {
        tracing::warn!(policy = %policy.engine.name, ?halt, "the run halted before finishing");
        swept.halted.push(policy.engine.name.clone());
        // An object limit is a truncation, not a refusal: what was planned before the cap is still correct
        // and still worth doing, and the next run picks up where this one stopped.
        if !matches!(halt, HaltReason::ObjectLimit { .. }) {
            return Ok(swept);
        }
    }

    for transition in plan.transitions() {
        swept.planned += 1;
        if plan.dry_run {
            tracing::info!(
                policy = %policy.engine.name,
                key = %transition.object_key.as_str(),
                from = %transition.from,
                to = %transition.to,
                "dry run: would transition",
            );
            continue;
        }
        // A policy pointing somewhere else is not executed here; see the module docs. Checked per object
        // rather than per policy because the pool lives on the placement, so a policy could name the right
        // pool and still meet an object that is somewhere else.
        if policy
            .target_pool_id
            .is_some_and(|target| target != transition.pool_id)
        {
            tracing::warn!(
                policy = %policy.engine.name,
                key = %transition.object_key.as_str(),
                "the policy targets another pool; a cross-pool move is not a transition",
            );
            continue;
        }

        // Per object, and a failure here does not abandon the policy.
        //
        // The first version propagated, which cost the accounting: one refusal aborted the run and the
        // partial result went with it, so a policy that had already moved five objects reported `moved=0`.
        // A log that understates what happened is worse than no log — the next run would move five it
        // believed were still Standard, and the minimum-duration counters would be the only evidence.
        if let Err(error) = store
            .transition(&transition.object_key, transition.to)
            .await
        {
            swept.failed += 1;
            let unsupported = matches!(error, dam_store::Error::Unsupported { .. });
            tracing::error!(
                %error,
                policy = %policy.engine.name,
                key = %transition.object_key.as_str(),
                "could not transition",
            );
            // A capability the driver does not have will refuse every remaining object identically, so
            // continuing would be a hundred more copies of the same line. Anything else — a timeout, a
            // throttle, one object under a lock — is per-object, and the other hundred deserve their turn.
            if unsupported {
                swept.halted.push(policy.engine.name.clone());
                break;
            }
            continue;
        }
        let mut conn = dam_db::TenantConn::begin(global, slug).await?;
        dam_db::tiering::transitioned(
            conn.executor(),
            transition.object_key.as_str(),
            transition.pool_id,
            transition.to,
            transition.min_duration_until,
        )
        .await?;
        conn.commit().await?;
        swept.moved += 1;
        tracing::info!(
            policy = %policy.engine.name,
            key = %transition.object_key.as_str(),
            from = %transition.from,
            to = %transition.to,
            "transitioned",
        );
    }

    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    dam_db::tiering::ran(
        conn.executor(),
        policy.id,
        i32::try_from(swept.moved).unwrap_or(i32::MAX),
        now,
    )
    .await?;
    conn.commit().await?;
    Ok(swept)
}

/// What one poll did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Polled {
    /// Requests handed to S3 this pass.
    pub issued: usize,
    /// Requests whose copy is now available.
    pub available: usize,
    /// Requests whose copy has lapsed.
    pub expired: usize,
    /// Requests S3 refused.
    pub failed: usize,
    /// Requests found claimed but never actually asked for, and asked for now.
    pub reissued: usize,
}

/// Drives every restore request one step.
///
/// Three steps in one job, in the order a request travels: issue what is queued, check what is in flight,
/// expire what has lapsed. One job because they share the store and the connection, and because the middle
/// step is the only one with anything to do most of the time.
pub async fn poll(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &TenantSlug,
    now: DateTime<Utc>,
) -> Result<Polled> {
    let mut polled = Polled::default();

    // Claimed in one transaction and issued outside it. Holding the transaction across the S3 calls would
    // mean a slow vendor keeps rows locked, and a worker that died mid-batch would roll the claim back and
    // re-issue every restore in it — each one a real charge.
    //
    // A batch-capable store claims a bigger slice, because it can fold the whole slice into one S3 Batch job
    // rather than one call per object.
    let can_batch = store.capabilities().batch_operations;
    let claim_limit = if can_batch {
        BULK_CLAIM_BATCH
    } else {
        CLAIM_BATCH
    };
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let claimed = restore_db::claim_queued(conn.executor(), claim_limit, now).await?;
    conn.commit().await?;

    if can_batch && claimed.len() >= BULK_RESTORE_THRESHOLD {
        issue_in_bulk(global, store, slug, claimed, now, &mut polled).await?;
    } else {
        for request in claimed {
            match issue(store, &request).await {
                Ok(()) => polled.issued += 1,
                Err(error) => {
                    // Recorded on the row rather than only logged, because the person waiting is watching the
                    // request and not the worker's stderr.
                    tracing::error!(%error, id = %request.id, key = %request.object_key, "issuing a restore");
                    record_issue_failure(global, slug, request.id, &error, now).await?;
                    polled.failed += 1;
                }
            }
        }
    }

    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let in_flight = restore_db::due_for_poll(conn.executor(), CLAIM_BATCH).await?;
    conn.commit().await?;

    for request in in_flight {
        let key = match dam_store::Key::new(request.object_key.clone()) {
            Ok(key) => key,
            Err(error) => {
                tracing::error!(%error, key = %request.object_key, "a restore request holds an unusable key");
                let mut conn = dam_db::TenantConn::begin(global, slug).await?;
                restore_db::mark_failed(conn.executor(), request.id, &error.to_string(), now)
                    .await?;
                conn.commit().await?;
                polled.failed += 1;
                continue;
            }
        };
        let state = store.head(&key).await?;

        // A row saying `requested` over an object with no restore in progress is a call that never landed.
        //
        // That is the failure mode of claiming in one transaction and issuing outside it: a worker that dies
        // in between leaves the row claimed and nothing asked for. The alternative — holding the transaction
        // across the vendor call — trades this for re-issuing every restore in a batch on the same crash, and
        // each of those is a real charge, so at-most-once is the right way round.
        //
        // Which leaves this reconciliation as the other half of that choice: S3's own state is the truth, so
        // a request that never reached it is re-issued rather than waited on forever. Without it, a crash at
        // the wrong instant produces a request that stays `requested` until somebody notices that a person
        // has been watching a spinner since Tuesday.
        if matches!(state.restore_state, dam_core::RestoreState::None) && !state.is_readable() {
            tracing::warn!(
                id = %request.id,
                key = %request.object_key,
                "the object has no restore in progress; re-issuing",
            );
            match issue(store, &request).await {
                Ok(()) => polled.reissued += 1,
                Err(error) => {
                    record_issue_failure(global, slug, request.id, &error, now).await?;
                    polled.failed += 1;
                }
            }
            continue;
        }

        if state.is_readable() {
            let mut conn = dam_db::TenantConn::begin(global, slug).await?;
            restore_db::mark_available(conn.executor(), request.id, now).await?;
            // The placement carries the same two facts, and the *tier badge reads the placement*. Marking
            // only the request would leave a restored asset still drawing as `archive` in the grid with its
            // download disabled — the copy is there and nothing says so.
            placement_restored(
                conn.executor(),
                &request.object_key,
                request.pool_id,
                now + Duration::days(i64::from(request.keep_warm_days.max(1))),
            )
            .await?;
            conn.commit().await?;
            polled.available += 1;
            tracing::info!(id = %request.id, key = %request.object_key, "restored");
        }
    }

    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let lapsed = restore_db::sweep_expired(conn.executor(), now, CLAIM_BATCH).await?;
    for request in &lapsed {
        // Same argument in reverse: the copy is gone, so the placement must stop claiming it is there or
        // delivery keeps minting URLs that 403 at S3 with nothing explaining why.
        placement_lapsed(conn.executor(), &request.object_key, request.pool_id).await?;
    }
    conn.commit().await?;
    polled.expired = lapsed.len();
    Ok(polled)
}

/// How many requests one pass takes on the per-object path.
///
/// Bounded so a queue of ten thousand does not become one job holding a connection for an hour; the next
/// pass is two minutes away and picks up the rest.
const CLAIM_BATCH: i64 = 32;

/// How many requests one pass claims when the store can batch (S3 Batch Operations).
///
/// Larger than `CLAIM_BATCH` because the whole point of a batch job is to fold a big backlog into one
/// server-side operation: claiming 32 at a time would create a job every two minutes and never realise the
/// "hundreds of round trips become one job" win. Still bounded, so a queue of a million is drained over a
/// handful of jobs rather than one that takes a day to build a manifest for.
const BULK_CLAIM_BATCH: i64 = 1000;

/// The claim size at or above which a batch-capable store issues an S3 Batch job instead of a
/// `RestoreObject` per object.
///
/// Below this the per-object path is both cheaper and lower-latency: an S3 Batch job has a fixed setup cost
/// and its own startup delay, which is not worth paying to restore a handful of objects. A collection restore
/// — the case batch exists for — is hundreds, comfortably over the line.
const BULK_RESTORE_THRESHOLD: usize = 100;

/// Records on the placement that a temporary copy exists.
async fn placement_restored(
    conn: &mut sqlx::PgConnection,
    object_key: &str,
    pool_id: Uuid,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE object_placements \
         SET restore_state = 'available', restore_expires_at = $3 \
         WHERE object_key = $1 AND pool_id = $2",
    )
    .bind(object_key)
    .bind(pool_id)
    .bind(expires_at)
    .execute(&mut *conn)
    .await
    .map_err(dam_db::Error::from)?;
    Ok(())
}

/// Records that the copy has gone.
async fn placement_lapsed(
    conn: &mut sqlx::PgConnection,
    object_key: &str,
    pool_id: Uuid,
) -> Result<()> {
    sqlx::query(
        "UPDATE object_placements \
         SET restore_state = 'expired', restore_expires_at = NULL \
         WHERE object_key = $1 AND pool_id = $2",
    )
    .bind(object_key)
    .bind(pool_id)
    .execute(&mut *conn)
    .await
    .map_err(dam_db::Error::from)?;
    Ok(())
}

async fn issue(store: &dyn BlobStore, request: &RestoreRequest) -> Result<()> {
    let key = dam_store::Key::new(request.object_key.clone())
        .map_err(|error| Error::Permanent(error.to_string()))?;
    let tier: RestoreTier = request
        .tier
        .parse()
        .map_err(|_| Error::Permanent(format!("restore tier {:?} is not one", request.tier)))?;
    store
        .restore(&key, tier, keep_for_of(request.keep_warm_days))
        .await?;
    Ok(())
}

/// Marks one restore request failed, in its own transaction.
///
/// Recorded on the row rather than only logged, because the person waiting watches the request, not the
/// worker's stderr. Shared by every issue path (per-object, re-issue, and each fallback within the bulk
/// path) so they cannot drift on how a failure is surfaced.
async fn record_issue_failure(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    id: Uuid,
    error: &Error,
    now: DateTime<Utc>,
) -> Result<()> {
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    restore_db::mark_failed(conn.executor(), id, &error.to_string(), now).await?;
    conn.commit().await?;
    Ok(())
}

/// The keep-warm window as a `Duration`, from a whole-days count that is at least one.
fn keep_for_of(keep_warm_days: i32) -> std::time::Duration {
    std::time::Duration::from_secs(u64::try_from(keep_warm_days.max(1)).unwrap_or(7) * 24 * 60 * 60)
}

/// Issues a large batch of claimed restores as S3 Batch jobs — one per (tier, keep-warm) group, because a
/// job carries a single retrieval tier and a single expiry.
///
/// A group the backend cannot batch — an unparsable tier, or Expedited, which S3 Batch restore has no
/// equivalent of — falls back to issuing its members one at a time, so nothing is dropped for being in the
/// wrong shape. A key that will not parse is failed and left out of the manifest. On a job-creation failure
/// the whole group is marked failed, matching the per-object path: recorded on the row, not just the log.
async fn issue_in_bulk(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &TenantSlug,
    claimed: Vec<RestoreRequest>,
    now: DateTime<Utc>,
    polled: &mut Polled,
) -> Result<()> {
    let mut groups: std::collections::HashMap<(String, i32), Vec<RestoreRequest>> =
        std::collections::HashMap::new();
    for request in claimed {
        groups
            .entry((request.tier.clone(), request.keep_warm_days))
            .or_default()
            .push(request);
    }

    for ((tier_raw, keep_warm_days), requests) in groups {
        // Only Standard and Bulk batch; Expedited and an unknown tier go the per-object route.
        let batchable_tier = match tier_raw.parse::<RestoreTier>() {
            Ok(tier @ (RestoreTier::Standard | RestoreTier::Bulk)) => Some(tier),
            _ => None,
        };
        let Some(tier) = batchable_tier else {
            for request in &requests {
                match issue(store, request).await {
                    Ok(()) => polled.issued += 1,
                    Err(error) => {
                        tracing::error!(%error, id = %request.id, key = %request.object_key, "issuing a restore");
                        record_issue_failure(global, slug, request.id, &error, now).await?;
                        polled.failed += 1;
                    }
                }
            }
            continue;
        };

        // Parse keys; a request with an unusable key is failed and excluded, so one bad row cannot sink the
        // whole job.
        let mut keys = Vec::with_capacity(requests.len());
        let mut batched = Vec::with_capacity(requests.len());
        for request in requests {
            match dam_store::Key::new(request.object_key.clone()) {
                Ok(key) => {
                    keys.push(key);
                    batched.push(request);
                }
                Err(error) => {
                    let error = Error::Permanent(error.to_string());
                    tracing::error!(%error, id = %request.id, "a restore request holds an unusable key");
                    record_issue_failure(global, slug, request.id, &error, now).await?;
                    polled.failed += 1;
                }
            }
        }
        if keys.is_empty() {
            continue;
        }

        match store
            .bulk_restore(&keys, tier, keep_for_of(keep_warm_days))
            .await
        {
            Ok(handle) => {
                tracing::info!(job = %handle.job_id, count = keys.len(), %tier, "issued a bulk restore");
                polled.issued += batched.len();
            }
            Err(error) => {
                let error = Error::from(error);
                tracing::error!(%error, count = keys.len(), "issuing a bulk restore");
                for request in &batched {
                    record_issue_failure(global, slug, request.id, &error, now).await?;
                }
                polled.failed += batched.len();
            }
        }
    }
    Ok(())
}

/// Plans a restore for one asset's archived original.
///
/// In the pipeline rather than the API because the worker needs it too — a bulk restore of a collection is
/// this, per asset, batched — and because it is where the store and the pool's prices are both in hand.
pub async fn plan_for(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    asset_id: Uuid,
    tier: RestoreTier,
    keep_warm_days: i64,
    now: DateTime<Utc>,
) -> Result<std::result::Result<(restore::Plan, Placement), RestoreRefusal>> {
    let Some(placement) = coldest_placement(global, slug, asset_id).await? else {
        return Ok(Err(RestoreRefusal::Empty));
    };
    let prices = pool_prices(global, placement.pool_id).await?;
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let spent = restore_db::spent_this_month(conn.executor(), now).await?;
    conn.commit().await?;
    // The thresholds themselves are the tenant's to set; only the spend is read here. `Budget::default`
    // carries the approval threshold §6.5 asks for, which is why this fills the one field it knows rather
    // than constructing the struct from scratch — a literal here would silently drop the threshold and every
    // restore would proceed unapproved.
    let budget = Budget {
        spent_this_month_cents: spent,
        ..Budget::default()
    };
    let candidate = restore::Candidate {
        bytes: placement.size_bytes,
        class: placement.storage_class,
    };
    Ok(
        restore::plan(&[candidate], tier, prices, &budget, keep_warm_days, now)
            .map(|plan| (plan, placement)),
    )
}

/// The placement a restore acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub object_key: String,
    pub pool_id: Uuid,
    pub size_bytes: u64,
    pub storage_class: StorageClass,
}

/// The *coldest* present copy of an asset's original.
///
/// Coldest, which is the opposite of what the grid's tier badge reads. The badge answers "can this be
/// fetched now", so it takes the warmest copy. A restore answers "what needs thawing", and picking the
/// warmest there would return a Standard replica and refuse the request as `NotArchived` while the copy the
/// caller actually wants sits in Deep Archive.
async fn coldest_placement(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    asset_id: Uuid,
) -> Result<Option<Placement>> {
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let row: Option<(String, Uuid, i64, String)> = sqlx::query_as(
        "SELECT object_key, pool_id, size_bytes, storage_class \
         FROM object_placements \
         WHERE asset_id = $1 AND derivative_id IS NULL AND state = 'present' \
         ORDER BY CASE storage_class \
                      WHEN 'DEEP_ARCHIVE' THEN 0 \
                      WHEN 'GLACIER' THEN 1 \
                      ELSE 2 \
                  END, object_key \
         LIMIT 1",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    row.map(|(object_key, pool_id, size, class)| {
        Ok(Placement {
            object_key,
            pool_id,
            size_bytes: u64::try_from(size).unwrap_or(0),
            storage_class: class
                .parse()
                .map_err(|_| Error::Permanent(format!("storage_class holds {class:?}")))?,
        })
    })
    .transpose()
}

/// The pool's retrieval prices, in the 1e-12-dollar units `estimate_cents` works in.
///
/// The scaling happens in SQL rather than in Rust, because the columns are `numeric(12, 8)` and the
/// alternative is a decimal crate in the dependency tree to multiply by a constant. `numeric` already does
/// exact decimal arithmetic; asking Postgres to finish the sum it is holding is less machinery than pulling
/// the value out mid-calculation.
///
/// Expedited and Bulk fall back to the Standard column when unrecorded — see the migration for why that is a
/// fallback rather than a refusal or a derived ratio.
async fn pool_prices(global: &sqlx::PgPool, pool_id: Uuid) -> Result<restore::RetrievalPrices> {
    let row: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT (coalesce(cost_per_gb_retrieval_expedited, cost_per_gb_retrieval) * 1e12)::bigint, \
                (cost_per_gb_retrieval * 1e12)::bigint, \
                (coalesce(cost_per_gb_retrieval_bulk, cost_per_gb_retrieval) * 1e12)::bigint, \
                (cost_per_1k_requests * 1e12)::bigint \
         FROM dam_global.storage_pools WHERE id = $1",
    )
    .bind(pool_id)
    .fetch_optional(global)
    .await
    .map_err(dam_db::Error::from)?;

    // A pool with no prices recorded estimates zero, and that is deliberate: a made-up price is a number
    // somebody would approve a spend against. Zero reads as "this deployment does not know", which is true.
    let (expedited, standard, bulk, per_1k) = row.unwrap_or((0, 0, 0, 0));
    Ok(restore::RetrievalPrices {
        expedited_per_gb: u64::try_from(expedited).unwrap_or(0),
        standard_per_gb: u64::try_from(standard).unwrap_or(0),
        bulk_per_gb: u64::try_from(bulk).unwrap_or(0),
        per_1000_requests: u64::try_from(per_1k).unwrap_or(0),
    })
}
