//! Bulk operations (2.10, GAPS G18).
//!
//! The schema states the difficulty: "partial failure is the hard part — an operation over 40,000 assets that
//! fails at 31,000 must be resumable and must report exactly which rows did not apply."
//!
//! ## The target set is snapshotted, not re-evaluated
//!
//! A predicate is materialised into `bulk_operation_items` at start, which the schema also calls for: "a
//! predicate is snapshotted to a materialised id list at start, so a long-running operation applies to the set
//! the user saw rather than a set that shifts under it."
//!
//! Re-running the query per batch looks equivalent and is not. An operation that *changes* what its own
//! predicate matches — tagging everything untagged, say — would walk a set that shrinks as it works, and an
//! operation on `brand = Acme` that also sets the brand would either loop forever or silently skip. Snapshotting
//! makes the target count mean something, which is what the confirmation dialog shows.
//!
//! ## Dry run is the default posture, not a flag on the side
//!
//! A bulk operation over 40,000 assets is unreviewable after the fact. [`dry_run`] materialises the same target
//! set and reports what would happen without applying anything, so the number in the confirmation is the number
//! that will be touched.
//!
//! ## Completed-with-failures is `partial`, never `completed`
//!
//! Reporting `completed` for an operation where 9,000 rows failed is the kind of thing somebody discovers a month
//! later. `partial` is a distinct state precisely so a UI cannot show a green tick over it.

use crate::Error;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// An operation as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: Uuid,
    pub kind: String,
    pub actor_id: Option<Uuid>,
    pub target_count: i64,
    pub state: String,
    pub done_count: i64,
    pub failed_count: i64,
    /// The greatest asset id whose outcome has been recorded — the progress marker an operator reads. It does
    /// **not** decide what [`next_batch`] serves; see that function for why.
    pub resume_after: Option<Uuid>,
    pub params: serde_json::Value,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Operation {
    /// Whether the operation has stopped for good.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state.as_str(),
            "completed" | "partial" | "failed" | "cancelled"
        )
    }
}

/// One asset's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub asset_id: Uuid,
    pub state: String,
    pub reason: Option<String>,
}

/// What to run.
#[derive(Debug, Clone)]
pub struct OperationSpec<'a> {
    pub kind: &'a str,
    pub actor_id: Option<Uuid>,
    /// The predicate the target set came from, recorded for the audit trail. **Not** re-evaluated — see the
    /// module docs.
    pub predicate: Option<serde_json::Value>,
    pub params: serde_json::Value,
}

/// What a dry run would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRun {
    pub kind: String,
    /// How many assets the operation would touch. The number a confirmation dialog shows, so it has to be the
    /// number that will actually be touched — which is why it comes from the same materialisation the real run
    /// uses.
    pub target_count: i64,
    /// A sample, for the UI to show rather than 40,000 rows.
    pub sample: Vec<Uuid>,
}

/// Longest sample a dry run returns.
pub const DRY_RUN_SAMPLE: usize = 20;

/// Reports what an operation would do, without recording anything.
///
/// Writes nothing at all — not even a `bulk_operations` row. A dry run that left a row behind would put
/// abandoned operations in the actor's history, and the history is where somebody looks to find what they
/// actually ran.
pub fn dry_run(kind: &str, targets: &[Uuid]) -> DryRun {
    DryRun {
        kind: kind.to_owned(),
        target_count: i64::try_from(targets.len()).unwrap_or(i64::MAX),
        sample: targets.iter().take(DRY_RUN_SAMPLE).copied().collect(),
    }
}

/// Creates an operation and materialises its target set.
///
/// One statement for the items, via `UNNEST`. Forty thousand individual inserts would take long enough that the
/// set really could shift underneath — which is the thing snapshotting exists to prevent.
pub async fn create(
    pool: &sqlx::PgPool,
    spec: &OperationSpec<'_>,
    targets: &[Uuid],
) -> Result<Operation, Error> {
    let mut tx = pool.begin().await?;
    let operation = create_on(&mut tx, spec, targets).await?;
    tx.commit().await?;
    Ok(operation)
}

/// The same creation, on a connection the caller has already scoped.
///
/// A request handler and the worker both hold a [`crate::TenantConn`], whose `search_path` is
/// transaction-scoped — so these statements have to run on *that* connection or the unqualified
/// `bulk_operations` resolves against whatever schema the pooled connection last had. The caller's
/// transaction also provides the atomicity the pool version got from its own.
pub async fn create_on(
    conn: &mut sqlx::PgConnection,
    spec: &OperationSpec<'_>,
    targets: &[Uuid],
) -> Result<Operation, Error> {
    if targets.is_empty() {
        // Refused rather than recorded as instantly complete. An operation over nothing is a mis-built query or a
        // stale selection, and a "completed" row would hide it.
        return Err(Error::Unsupported(format!(
            "a {} operation over zero assets is a mis-built selection rather than an empty job",
            spec.kind
        )));
    }

    let id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO bulk_operations (id, kind, actor_id, predicate, target_count, params, state) \
         VALUES ($1, $2, $3, $4, $5, $6, 'queued')",
    )
    .bind(id)
    .bind(spec.kind)
    .bind(spec.actor_id)
    .bind(&spec.predicate)
    .bind(i64::try_from(targets.len()).unwrap_or(i64::MAX))
    .bind(&spec.params)
    .execute(&mut *conn)
    .await?;

    // Deduplicated, because a selection assembled from several pages can repeat an id and the primary key would
    // otherwise abort the whole operation.
    let mut unique: Vec<Uuid> = targets.to_vec();
    unique.sort_unstable();
    unique.dedup();

    sqlx::query(
        "INSERT INTO bulk_operation_items (operation_id, asset_id, state) \
         SELECT $1, unnested, 'pending' FROM unnest($2::uuid[]) AS unnested \
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(&unique)
    .execute(&mut *conn)
    .await?;

    // The count is corrected to the deduplicated size, so `done + failed = target` can actually hold at the end.
    sqlx::query("UPDATE bulk_operations SET target_count = $2 WHERE id = $1")
        .bind(id)
        .bind(i64::try_from(unique.len()).unwrap_or(i64::MAX))
        .execute(&mut *conn)
        .await?;

    load_on(conn, id).await?.ok_or_else(|| {
        Error::Inconsistent(format!(
            "bulk operation {id} vanished immediately after creation"
        ))
    })
}

/// Loads an operation.
pub async fn load(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<Operation>, Error> {
    let mut conn = pool.acquire().await?;
    load_on(&mut conn, id).await
}

/// The same read, on a scoped connection.
pub async fn load_on(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Option<Operation>, Error> {
    let row = sqlx::query_as::<_, OperationRow>(
        "SELECT id, kind, actor_id, target_count, state, done_count, failed_count, resume_after, \
                params, started_at, finished_at \
         FROM bulk_operations WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(into_operation))
}

/// Claims the next batch of pending items, in id order.
///
/// Resumption is exact because selection is on **item state**: a worker that died at 31,000 finds the items it
/// recorded already marked `done`, `skipped` or `failed`, and gets the rest. The `asset_id` order makes batches
/// stable — ordering by anything mutable, a timestamp or a state, would let a row move between batches and be
/// applied twice — and `bulk_operation_items_pending_idx` supplies it, so the scan reads only the rows it returns
/// rather than walking the finished prefix.
///
/// It does **not** cursor on `resume_after`, and that was a real bug rather than a simplification. `resume_after`
/// is a high-water mark of the greatest asset id recorded; a worker that fans a batch out concurrently records in
/// completion order, not id order, so recording the highest id first stepped the cursor past every lower pending
/// item. Those items could never be served again, `done + failed = target` would never hold, and the operation
/// could not legitimately finish. State cannot skip a row it has not seen an outcome for.
pub async fn next_batch(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    limit: i64,
) -> Result<Vec<Uuid>, Error> {
    let mut conn = pool.acquire().await?;
    next_batch_on(&mut conn, operation_id, limit).await
}

/// The same claim, on a scoped connection.
pub async fn next_batch_on(
    conn: &mut sqlx::PgConnection,
    operation_id: Uuid,
    limit: i64,
) -> Result<Vec<Uuid>, Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT asset_id FROM bulk_operation_items \
         WHERE operation_id = $1 AND state = 'pending' \
         ORDER BY asset_id LIMIT $2",
    )
    .bind(operation_id)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(ids)
}

/// Marks the operation running.
pub async fn start(pool: &sqlx::PgPool, id: Uuid, now: DateTime<Utc>) -> Result<bool, Error> {
    let mut conn = pool.acquire().await?;
    start_on(&mut conn, id, now).await
}

/// The same transition, on a scoped connection.
pub async fn start_on(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE bulk_operations SET state = 'running', started_at = coalesce(started_at, $2) \
         WHERE id = $1 AND state IN ('queued', 'paused')",
    )
    .bind(id)
    .bind(now)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Records one asset's outcome and advances the progress marker.
///
/// The item state, the counters and the cursor move in **one transaction**. Splitting them lets a crash leave a
/// row marked done while the counter says otherwise, and then the operation's own progress report is the thing
/// that is wrong.
pub async fn record_outcome(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    asset_id: Uuid,
    outcome: ItemOutcome<'_>,
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;
    record_outcome_on(&mut tx, operation_id, asset_id, outcome).await?;
    tx.commit().await?;
    Ok(())
}

/// The same recording, on a connection whose transaction the caller holds.
///
/// The atomicity the pool version buys with its own transaction comes from the caller's here — a
/// [`crate::TenantConn`] is one, which is also what scopes the unqualified table names. Calling this on a bare
/// autocommit connection would let a crash land the item state without the counters, and then the operation's
/// own progress report is the thing that is wrong.
pub async fn record_outcome_on(
    conn: &mut sqlx::PgConnection,
    operation_id: Uuid,
    asset_id: Uuid,
    outcome: ItemOutcome<'_>,
) -> Result<(), Error> {
    let (state, reason) = match outcome {
        ItemOutcome::Done => ("done", None),
        ItemOutcome::Skipped(reason) => ("skipped", Some(reason)),
        ItemOutcome::Failed(reason) => ("failed", Some(reason)),
    };

    let updated = sqlx::query(
        "UPDATE bulk_operation_items SET state = $3, reason = $4 \
         WHERE operation_id = $1 AND asset_id = $2 AND state = 'pending'",
    )
    .bind(operation_id)
    .bind(asset_id)
    .bind(state)
    .bind(reason)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    // Only count a transition that actually happened. A retried worker re-recording the same asset would
    // otherwise inflate the counters past the target, and `done + failed = target` is the invariant a UI reads
    // to decide whether an operation is finished.
    if updated > 0 {
        // `skipped` counts as neither done nor failed: it is an asset the operation deliberately did not touch —
        // already tagged, not eligible — and counting it as done would claim work that never happened.
        let (done, failed) = match state {
            "done" => (1, 0),
            "failed" => (0, 1),
            _ => (0, 0),
        };
        sqlx::query(
            "UPDATE bulk_operations \
             SET done_count = done_count + $2, failed_count = failed_count + $3, \
                 resume_after = greatest(coalesce(resume_after, $4), $4), \
                 error_sample = CASE \
                     WHEN $5::text IS NULL OR jsonb_array_length(error_sample) >= 20 THEN error_sample \
                     ELSE error_sample || jsonb_build_object('asset_id', $4::text, 'reason', $5::text) \
                 END \
             WHERE id = $1",
        )
        .bind(operation_id)
        .bind(done)
        .bind(failed)
        .bind(asset_id)
        .bind(if state == "failed" { reason } else { None })
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// What happened to one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemOutcome<'a> {
    Done,
    /// Deliberately not touched — already in the target state, or not eligible. Carries a reason, because a
    /// silent skip is indistinguishable from a bug.
    Skipped(&'a str),
    Failed(&'a str),
}

/// Finishes an operation, choosing the terminal state from what actually happened.
///
/// The caller does not get to say "completed": the state is derived from the counters, so an operation with
/// failures cannot be reported green. That is the schema's `partial` state doing its job.
pub async fn finish(pool: &sqlx::PgPool, id: Uuid, now: DateTime<Utc>) -> Result<Operation, Error> {
    let mut conn = pool.acquire().await?;
    finish_on(&mut conn, id, now).await
}

/// The same finishing, on a scoped connection.
pub async fn finish_on(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<Operation, Error> {
    sqlx::query(
        "UPDATE bulk_operations SET \
             state = CASE \
                 WHEN failed_count > 0 AND done_count = 0 THEN 'failed' \
                 WHEN failed_count > 0 THEN 'partial' \
                 ELSE 'completed' \
             END, \
             finished_at = $2 \
         WHERE id = $1 AND state IN ('running', 'queued', 'paused')",
    )
    .bind(id)
    .bind(now)
    .execute(&mut *conn)
    .await?;

    load_on(conn, id)
        .await?
        .ok_or_else(|| Error::Inconsistent(format!("bulk operation {id} vanished while finishing")))
}

/// Pauses a running operation, so it can be resumed.
pub async fn pause(pool: &sqlx::PgPool, id: Uuid) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE bulk_operations SET state = 'paused' WHERE id = $1 AND state = 'running'",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Cancels an operation. Work already applied stays applied.
///
/// Nothing is rolled back, and that is not laziness: a bulk tag over 31,000 assets cannot be undone by a
/// cancellation without a second bulk operation, and pretending otherwise would be worse than saying so. The
/// remaining items stay `pending`, so what was and was not done is still readable afterwards.
pub async fn cancel(pool: &sqlx::PgPool, id: Uuid, now: DateTime<Utc>) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE bulk_operations SET state = 'cancelled', finished_at = $2 \
         WHERE id = $1 AND state IN ('queued', 'running', 'paused')",
    )
    .bind(id)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Every item that did not apply, so the operation can report exactly which rows failed.
pub async fn failures(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    limit: i64,
) -> Result<Vec<Item>, Error> {
    let mut conn = pool.acquire().await?;
    failures_on(&mut conn, operation_id, limit).await
}

/// The same read, on a scoped connection.
pub async fn failures_on(
    conn: &mut sqlx::PgConnection,
    operation_id: Uuid,
    limit: i64,
) -> Result<Vec<Item>, Error> {
    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>)>(
        "SELECT asset_id, state, reason FROM bulk_operation_items \
         WHERE operation_id = $1 AND state = 'failed' ORDER BY asset_id LIMIT $2",
    )
    .bind(operation_id)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(asset_id, state, reason)| Item {
            asset_id,
            state,
            reason,
        })
        .collect())
}

/// Items in any state, for a detail view.
pub async fn items(
    pool: &sqlx::PgPool,
    operation_id: Uuid,
    limit: i64,
) -> Result<Vec<Item>, Error> {
    let rows = sqlx::query_as::<_, (Uuid, String, Option<String>)>(
        "SELECT asset_id, state, reason FROM bulk_operation_items \
         WHERE operation_id = $1 ORDER BY asset_id LIMIT $2",
    )
    .bind(operation_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(asset_id, state, reason)| Item {
            asset_id,
            state,
            reason,
        })
        .collect())
}

type OperationRow = (
    Uuid,
    String,
    Option<Uuid>,
    i64,
    String,
    i64,
    i64,
    Option<Uuid>,
    serde_json::Value,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

fn into_operation(row: OperationRow) -> Operation {
    let (
        id,
        kind,
        actor_id,
        target_count,
        state,
        done_count,
        failed_count,
        resume_after,
        params,
        started_at,
        finished_at,
    ) = row;
    Operation {
        id,
        kind,
        actor_id,
        target_count,
        state,
        done_count,
        failed_count,
        resume_after,
        params,
        started_at,
        finished_at,
    }
}
