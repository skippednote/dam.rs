//! The job queue (D6: one global table, not one per tenant).
//!
//! One worker polls one table. Polling N tenant schemas does not scale, and a global
//! table is what makes per-tenant fairness expressible at all.
//!
//! ## Leases, not locks
//!
//! A claimed job carries `lease_expires_at`. A worker that dies has its work reclaimed
//! when the lease lapses — reclaim happens inside [`claim`], so there is no reaper
//! process to deploy, monitor, or forget to run. Long jobs renew with [`heartbeat`].
//!
//! ## Fairness
//!
//! Claiming is round-robin across tenants, not FIFO across the table. Without that, a
//! tenant bulk-importing 100k assets stalls every other tenant's thumbnails behind
//! their backlog — the queue would be technically correct and operationally useless.
//!
//! Ranking is `row_number() OVER (PARTITION BY tenant_id ORDER BY priority, run_after,
//! id)`, and the batch is taken in **rank order**: every tenant's next job before any
//! tenant's job after that. One tenant alone still fills the batch, so fairness costs
//! no throughput. A hard per-tenant cap would — and did, until the fairness test
//! showed a worker getting 5 of a requested 10 while 200 jobs waited.
//!
//! Using a window function means
//! **`FOR UPDATE SKIP LOCKED` is unavailable**: Postgres rejects `FOR UPDATE` in any
//! query containing window functions. Correctness comes from the `UPDATE`'s own
//! `WHERE state = 'queued'` instead. Under `READ COMMITTED`, an `UPDATE` re-evaluates
//! its predicate after acquiring the row lock, so when two workers target the same
//! row the second sees `state = 'running'`, the row fails the predicate, and it is
//! excluded from `RETURNING`. No double-claim; the loser simply gets a smaller batch
//! and asks again. `concurrent_workers_never_claim_the_same_job` pins this.

use crate::Error;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// A job as handed to a worker.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub kind: String,
    pub payload: Json,
    /// Includes the current attempt: a first claim reports 1.
    pub attempts: i32,
    pub max_attempts: i32,
}

impl Job {
    /// True when this attempt is the last one before the job goes dead. Lets a worker
    /// log more aggressively, or take a cheaper fallback path, on the final try.
    pub fn is_final_attempt(&self) -> bool {
        self.attempts >= self.max_attempts
    }
}

/// What to enqueue.
#[derive(Debug, Clone)]
pub struct JobSpec {
    tenant_id: Uuid,
    kind: String,
    payload: Json,
    priority: i16,
    run_after: Option<DateTime<Utc>>,
    max_attempts: i32,
    dedupe_key: Option<String>,
}

impl JobSpec {
    pub fn new(tenant_id: Uuid, kind: impl Into<String>) -> Self {
        Self {
            tenant_id,
            kind: kind.into(),
            payload: Json::Object(serde_json::Map::new()),
            priority: 100,
            run_after: None,
            max_attempts: 5,
            dedupe_key: None,
        }
    }

    pub fn payload(mut self, payload: Json) -> Self {
        self.payload = payload;
        self
    }

    /// Lower runs first. 100 is the default; reserve below 50 for interactive work
    /// a user is waiting on.
    pub fn priority(mut self, priority: i16) -> Self {
        self.priority = priority;
        self
    }

    pub fn run_after(mut self, at: DateTime<Utc>) -> Self {
        self.run_after = Some(at);
        self
    }

    pub fn max_attempts(mut self, n: i32) -> Self {
        self.max_attempts = n;
        self
    }

    /// Idempotency key. While a job with this `(tenant, kind, key)` is queued or
    /// running, enqueueing again returns the existing id instead of creating a
    /// duplicate. Once it finishes, the key is free again — re-deriving a thumbnail
    /// after the first derive completed is legitimate work, not a duplicate.
    ///
    /// **A handler cannot re-queue itself under its own key.** The job doing the
    /// enqueueing is `running`, so the insert conflicts with it and this returns
    /// that job's own id — and when the handler then completes, nothing is left
    /// queued. A chained stage that reschedules itself (the batch collector's poll,
    /// say) has to leave the key off. Found the hard way: one poll, "still
    /// working", and a batch nobody ever came back for.
    pub fn dedupe_key(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }
}

/// The priority at and above which work is background: nothing is waiting on it.
///
/// The boundary [`JobSpec::priority`] documents, named here because [`claim`] reserves a
/// slice of every batch for this band and a magic 50 in the SQL would not survive anyone
/// moving the boundary.
const BACKGROUND_BAND: i16 = 50;

/// Tuning for one claim call.
#[derive(Debug, Clone, Copy)]
pub struct ClaimOptions {
    /// Maximum jobs to return.
    pub limit: i64,
    /// Hard cap per tenant within one batch. Normally `None`.
    ///
    /// Fairness does **not** come from this — it comes from the round-robin rank
    /// ordering in [`claim`]. An earlier draft made this a mandatory cap of 4, and the
    /// fairness test caught the consequence: with `limit = 10`, two tenants, and 200
    /// jobs queued, a worker got **5 jobs** because the cap bound before the limit
    /// did. That starves the worker, not the greedy tenant, and leaves the backlog
    /// growing while capacity idles.
    ///
    /// Rank ordering has neither problem: one tenant alone fills the batch, and a
    /// quiet tenant's single job still lands in rank 1. Keep this as a safety valve
    /// for a pathological tenant, not as the mechanism.
    pub per_tenant: Option<i64>,
    /// How long the claim holds. Long enough that a normal job finishes inside it,
    /// short enough that a crashed worker's jobs come back promptly. Long jobs renew
    /// with [`heartbeat`] rather than taking a longer lease up front.
    pub lease: Duration,
}

impl Default for ClaimOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            per_tenant: None,
            lease: Duration::from_secs(60),
        }
    }
}

/// Enqueues a job, or returns the id of an equivalent one already in flight.
pub async fn enqueue(pool: &PgPool, spec: JobSpec) -> Result<Uuid, Error> {
    let id = Uuid::now_v7();

    // The partial unique index on (tenant_id, kind, dedupe_key) WHERE state IN
    // ('queued','running') does the work. `DO NOTHING` returns no row on conflict,
    // so a follow-up select finds the incumbent.
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO dam_global.jobs \
           (id, tenant_id, kind, payload, priority, run_after, max_attempts, dedupe_key) \
         VALUES ($1, $2, $3, $4, $5, coalesce($6, now()), $7, $8) \
         ON CONFLICT DO NOTHING \
         RETURNING id",
    )
    .bind(id)
    .bind(spec.tenant_id)
    .bind(&spec.kind)
    .bind(&spec.payload)
    .bind(spec.priority)
    .bind(spec.run_after)
    .bind(spec.max_attempts)
    .bind(spec.dedupe_key.as_deref())
    .fetch_optional(pool)
    .await?;

    if let Some(id) = inserted {
        return Ok(id);
    }

    // Conflicted: return the incumbent so the caller can track it.
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM dam_global.jobs \
         WHERE tenant_id = $1 AND kind = $2 AND dedupe_key = $3 \
           AND state IN ('queued', 'running') \
         LIMIT 1",
    )
    .bind(spec.tenant_id)
    .bind(&spec.kind)
    .bind(spec.dedupe_key.as_deref())
    .fetch_optional(pool)
    .await?;

    existing.ok_or_else(|| {
        // The incumbent finished between our insert and this select. Rare, and the
        // caller should retry rather than be told a lie about which job is theirs.
        Error::Migrate("job insert conflicted but no in-flight job found; retry the enqueue".into())
    })
}

/// Reclaims expired leases, then claims a fair batch.
pub async fn claim(pool: &PgPool, worker: &str, opts: ClaimOptions) -> Result<Vec<Job>, Error> {
    reclaim_expired(pool).await?;

    let lease_secs = opts.lease.as_secs_f64();

    // Background work gets a reserved slice of every batch, because strict priority plus an
    // urgent band that keeps refilling is indefinite starvation rather than a queue.
    //
    // Measured, not theorised. A load run ingested 2,637 assets across five tenants, and the
    // `derive` jobs those uploads fan out to (priority 40) stayed permanently non-empty. Behind
    // them, 1,280 `index` jobs and 1,280 `similarity` jobs sat at `attempts = 0` with
    // `run_after` half an hour in the past — never claimed once — so half the library answered
    // a text search with nothing while sitting in `assets` the whole time.
    //
    // **A quarter of the batch, pooled and oldest-first — not a slot per band.** The worker
    // claims four at a time, so there is no batch size at which every band gets its own slot:
    // reserving per band would leave interactive work one slot in four *and* still starve the
    // lowest bands, because three slots cannot cover seven bands. Pooling them and ordering by
    // age instead bounds the wait rather than guaranteeing a slot: a band's head advances every
    // time it is picked, so a job in a small band waits for the background work older than it
    // and no longer. Bounded by the backlog ahead of it, which is what a queue means.
    //
    // Aging the background band into the interactive one would also have worked, by
    // contradicting the boundary `JobSpec::priority` documents. This keeps that contract:
    // interactive work still takes every slot the reserve does not, and the reserve is empty
    // whenever the background bands are.
    //
    // Never the last slot: at `limit = 1` there is no reserve at all, so a single-job claim is
    // decided by priority alone and stays predictable.
    let reserve = if opts.limit >= 2 {
        (opts.limit / 4).clamp(1, opts.limit - 1)
    } else {
        0
    };

    // See the module docs for why there is no SKIP LOCKED here: window functions and
    // FOR UPDATE are mutually exclusive in Postgres, and the UPDATE's own
    // `state = 'queued'` predicate provides the same guarantee under READ COMMITTED.
    let rows: Vec<(Uuid, Uuid, String, Json, i32, i32)> = sqlx::query_as(
        "WITH eligible AS ( \
             SELECT id, priority, run_after, \
                    row_number() OVER ( \
                        PARTITION BY tenant_id ORDER BY priority, run_after, id \
                    ) AS rn \
             FROM dam_global.jobs \
             WHERE state = 'queued' AND run_after <= now() \
         ), \
         fair AS ( \
             SELECT id, priority, run_after, rn FROM eligible \
             WHERE $3::bigint IS NULL OR rn <= $3 \
         ), \
         starved AS ( \
             SELECT id FROM fair \
             WHERE priority >= $6::smallint \
             ORDER BY run_after, id \
             LIMIT $5 \
         ), \
         urgent AS ( \
             SELECT f.id FROM fair f \
             WHERE NOT EXISTS (SELECT 1 FROM starved s WHERE s.id = f.id) \
             ORDER BY f.rn, f.priority, f.run_after, f.id \
             LIMIT GREATEST($4 - (SELECT count(*) FROM starved), 0) \
         ), \
         picked AS (SELECT id FROM starved UNION ALL SELECT id FROM urgent) \
         UPDATE dam_global.jobs j \
         SET state = 'running', \
             locked_by = $1, \
             lease_expires_at = now() + make_interval(secs => $2), \
             attempts = j.attempts + 1, \
             updated_at = now() \
         FROM picked \
         WHERE j.id = picked.id AND j.state = 'queued' \
         RETURNING j.id, j.tenant_id, j.kind, j.payload, j.attempts, j.max_attempts",
    )
    .bind(worker)
    .bind(lease_secs)
    .bind(opts.per_tenant)
    .bind(opts.limit)
    .bind(reserve)
    .bind(BACKGROUND_BAND)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, tenant_id, kind, payload, attempts, max_attempts)| Job {
                id,
                tenant_id,
                kind,
                payload,
                attempts,
                max_attempts,
            },
        )
        .collect())
}

/// Returns jobs whose lease has lapsed to `queued`, or to `dead` if they are out of
/// attempts.
///
/// Called at the top of [`claim`] so a crashed worker's jobs recover without a
/// separate reaper. The `dead` transition matters: a job that reliably kills its
/// worker would otherwise be reclaimed forever, taking a worker down each time.
pub async fn reclaim_expired(pool: &PgPool) -> Result<u64, Error> {
    let result = sqlx::query(
        "UPDATE dam_global.jobs \
         SET state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END, \
             locked_by = NULL, \
             lease_expires_at = NULL, \
             last_error = coalesce(last_error, 'lease expired; worker presumed dead'), \
             finished_at = CASE WHEN attempts >= max_attempts THEN now() ELSE NULL END, \
             updated_at = now() \
         WHERE state = 'running' AND lease_expires_at < now()",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Marks a job succeeded.
pub async fn complete(pool: &PgPool, id: Uuid) -> Result<(), Error> {
    sqlx::query(
        "UPDATE dam_global.jobs \
         SET state = 'succeeded', locked_by = NULL, lease_expires_at = NULL, \
             finished_at = now(), updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a failure: back off and retry, or go dead once attempts are exhausted.
///
/// Backoff is exponential on attempt count, capped at an hour. The cap matters more
/// than the curve — uncapped doubling reaches days, at which point a transient outage
/// has effectively lost the work.
pub async fn fail(pool: &PgPool, id: Uuid, error: &str) -> Result<(), Error> {
    sqlx::query(
        "UPDATE dam_global.jobs \
         SET state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END, \
             locked_by = NULL, \
             lease_expires_at = NULL, \
             last_error = $2, \
             run_after = now() + make_interval( \
                 secs => least(3600, power(2, least(attempts, 12))::double precision) \
             ), \
             finished_at = CASE WHEN attempts >= max_attempts THEN now() ELSE NULL END, \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Extends a lease. Only the holder may.
///
/// The `locked_by` check is not paranoia: without it a worker that had its lease
/// reclaimed could keep renewing a job another worker is now running, and the two
/// would race with no error anywhere.
pub async fn heartbeat(
    pool: &PgPool,
    id: Uuid,
    worker: &str,
    lease: Duration,
) -> Result<(), Error> {
    let result = sqlx::query(
        "UPDATE dam_global.jobs \
         SET lease_expires_at = now() + make_interval(secs => $3), updated_at = now() \
         WHERE id = $1 AND locked_by = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(worker)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(Error::LeaseLost {
            job_id: id,
            worker: worker.to_owned(),
        });
    }
    Ok(())
}

/// Queue depth per state, for the health endpoint and for deciding whether the
/// backlog needs more workers.
pub async fn depth(pool: &PgPool) -> Result<Vec<(String, i64)>, Error> {
    let rows = sqlx::query_as(
        "SELECT state::text, count(*) FROM dam_global.jobs GROUP BY state ORDER BY state",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
