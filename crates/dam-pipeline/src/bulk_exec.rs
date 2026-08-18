//! Executing a bulk operation (2.10's other half).
//!
//! `dam_db::bulk` holds the bookkeeping — the snapshot, the counters, the resumable batches — and nothing
//! drove it, so a created operation sat `queued` forever. This is the driver: claim a batch, apply the
//! operation to each asset, record each outcome, repeat until the batch comes back empty, then derive the
//! terminal state from the counters.
//!
//! ## Two kinds are executable, and the rest are refused by name
//!
//! `metadata_set` and `delete`. The schema's kind vocabulary is wider — `tag_add`, `restore`,
//! `download_zip`, `tier` — and each of those needs machinery that does not exist yet (taxonomy application,
//! the restore queue, a zip assembler, the lifecycle engine). An unknown-to-us kind fails **permanently** at
//! the first batch rather than skipping every item: an operation that "completed" while doing nothing is
//! worse than one that says it cannot run.
//!
//! ## Per-item outcomes are the product, not a log
//!
//! The schema says why: "an operation over 40,000 assets that fails at 31,000 must be resumable and must
//! report exactly which rows did not apply." So a legal-held asset under a bulk delete is `Skipped("legal
//! hold")` — not an error, because the hold is doing its job, and not silently dropped, because a silent skip
//! is indistinguishable from a bug.
//!
//! ## Idempotent, like every stage
//!
//! The queue is at-least-once. Batches select on item state, so a re-run resumes where the outcomes stop;
//! the applications themselves are idempotent (a second soft delete of a deleted asset is `Skipped`, a
//! second identical metadata merge writes the same bytes); and `record_outcome` only counts a transition
//! that actually happened.

use crate::{Error, Result};
use dam_core::TenantSlug;
use dam_db::TenantConn;
use dam_db::bulk::{self, ItemOutcome};
use serde_json::Value;
use uuid::Uuid;

/// Items applied per batch.
///
/// Small enough that one batch stays well inside a lease renewal interval, large enough that a 40,000-asset
/// operation is hundreds of round trips rather than tens of thousands.
pub const BATCH: i64 = 100;

/// What one run of the executor did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    pub operation_id: Uuid,
    /// The terminal state the counters produced: `completed`, `partial` or `failed`.
    pub state: String,
    pub done: i64,
    pub failed: i64,
    /// Assets whose search document is now stale and needs re-indexing.
    ///
    /// Returned rather than enqueued here, because enqueueing is the worker's job — this module stays
    /// callable from a test or from `damctl` without dragging the queue in.
    pub touched: Vec<Uuid>,
}

/// Drives `operation_id` to a terminal state.
pub async fn run(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    operation_id: Uuid,
    now: chrono::DateTime<chrono::Utc>,
    mut heartbeat: impl AsyncFnMut() -> Result<()>,
) -> Result<Executed> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let operation = bulk::load_on(conn.executor(), operation_id).await?;
    conn.commit().await?;

    let Some(operation) = operation else {
        // Permanent: the row is gone and retrying cannot find it.
        return Err(Error::Permanent(format!(
            "bulk operation {operation_id} does not exist"
        )));
    };

    // A re-run of a finished operation is the at-least-once queue doing its thing, not an error.
    if operation.is_terminal() {
        return Ok(Executed {
            operation_id,
            state: operation.state,
            done: operation.done_count,
            failed: operation.failed_count,
            touched: Vec::new(),
        });
    }

    let action = Action::from_operation(&operation.kind, &operation.params)?;

    // Params are validated once, before any item is touched. The patch is the same for every asset, so a
    // malformed one would fail all 40,000 items identically — better said once, as a permanent failure, than
    // recorded 40,000 times.
    if let Action::MetadataSet { values } = &action {
        let mut conn = TenantConn::begin(global, slug).await?;
        validate_patch(conn.executor(), values).await?;
        conn.commit().await?;
    }

    let mut conn = TenantConn::begin(global, slug).await?;
    bulk::start_on(conn.executor(), operation_id, now).await?;
    conn.commit().await?;

    let mut touched = Vec::new();
    loop {
        let mut conn = TenantConn::begin(global, slug).await?;
        let batch = bulk::next_batch_on(conn.executor(), operation_id, BATCH).await?;
        conn.commit().await?;
        if batch.is_empty() {
            break;
        }

        for asset_id in batch {
            // One transaction per item: the application and its outcome land together, so a crash between
            // them cannot record a change without its bookkeeping or the reverse.
            let mut conn = TenantConn::begin(global, slug).await?;
            let outcome = action.apply(conn.executor(), asset_id).await?;
            let changed = matches!(outcome, Applied::Done);
            bulk::record_outcome_on(conn.executor(), operation_id, asset_id, outcome.as_item())
                .await?;
            conn.commit().await?;

            if changed {
                touched.push(asset_id);
            }
        }

        // The lease renews per batch, because a large operation legitimately outlives one lease — and a
        // worker that lost its lease mid-run must stop rather than fight the worker that took over.
        heartbeat().await?;
    }

    let mut conn = TenantConn::begin(global, slug).await?;
    let finished = bulk::finish_on(conn.executor(), operation_id, chrono::Utc::now()).await?;
    conn.commit().await?;

    Ok(Executed {
        operation_id,
        state: finished.state,
        done: finished.done_count,
        failed: finished.failed_count,
        touched,
    })
}

/// The kinds this executor can apply.
enum Action {
    MetadataSet {
        values: serde_json::Map<String, Value>,
    },
    Delete,
}

/// What applying to one asset produced. A thin wrapper so the borrow of a `&'static str` reason is simple.
enum Applied {
    Done,
    Skipped(&'static str),
    Failed(String),
}

impl Applied {
    fn as_item(&self) -> ItemOutcome<'_> {
        match self {
            Self::Done => ItemOutcome::Done,
            Self::Skipped(reason) => ItemOutcome::Skipped(reason),
            Self::Failed(reason) => ItemOutcome::Failed(reason),
        }
    }
}

impl Action {
    fn from_operation(kind: &str, params: &Value) -> Result<Self> {
        match kind {
            "metadata_set" => {
                let values = params
                    .get("values")
                    .and_then(Value::as_object)
                    .cloned()
                    .ok_or_else(|| {
                        Error::Permanent(
                            "a metadata_set operation needs params.values as an object".to_owned(),
                        )
                    })?;
                if values.is_empty() {
                    return Err(Error::Permanent(
                        "a metadata_set over no fields would mark every asset done while changing \
                         nothing"
                            .to_owned(),
                    ));
                }
                Ok(Self::MetadataSet { values })
            }
            "delete" => Ok(Self::Delete),
            // The schema's vocabulary is wider than what is executable, and the honest response to the gap
            // is a named refusal. "Completing" while doing nothing would be worse: the history would say the
            // work happened.
            other => Err(Error::Permanent(format!(
                "bulk operations of kind {other:?} are not executable yet; only metadata_set and \
                 delete are"
            ))),
        }
    }

    async fn apply(&self, conn: &mut sqlx::PgConnection, asset_id: Uuid) -> Result<Applied> {
        match self {
            Self::Delete => apply_delete(conn, asset_id).await,
            Self::MetadataSet { values } => apply_metadata(conn, asset_id, values).await,
        }
    }
}

/// Soft-deletes one asset.
///
/// The guards live in the `WHERE`, so the check and the change are one statement — two would let a hold
/// land between them. Legal hold blocks deletion (the schema says so in as many words), and an
/// already-deleted asset is a skip rather than a success, because claiming work that a previous run did is
/// how a re-run double-counts.
async fn apply_delete(conn: &mut sqlx::PgConnection, asset_id: Uuid) -> Result<Applied> {
    let deleted = sqlx::query(
        "UPDATE assets SET deleted_at = now(), status = 'deleted', updated_at = now() \
         WHERE id = $1 AND deleted_at IS NULL AND NOT legal_hold",
    )
    .bind(asset_id)
    .execute(&mut *conn)
    .await
    .map_err(dam_db::Error::from)?
    .rows_affected();

    if deleted > 0 {
        return Ok(Applied::Done);
    }

    // Nothing changed — say why, because "skipped" with no reason is indistinguishable from a bug.
    let row: Option<(bool, bool)> =
        sqlx::query_as("SELECT legal_hold, deleted_at IS NOT NULL FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?;

    Ok(match row {
        Some((true, _)) => Applied::Skipped("legal hold blocks deletion"),
        Some((_, true)) => Applied::Skipped("already deleted"),
        Some(_) => Applied::Failed("the asset exists but could not be deleted".to_owned()),
        None => Applied::Failed("no such asset".to_owned()),
    })
}

/// Merges `values` into one asset's metadata, exactly as the single-asset PATCH endpoint does.
///
/// A present `null` clears the field; an absent key is left alone. The values were validated once for the
/// whole operation, so per-asset work is the merge and nothing else.
async fn apply_metadata(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    values: &serde_json::Map<String, Value>,
) -> Result<Applied> {
    let exists: Option<bool> =
        sqlx::query_scalar("SELECT deleted_at IS NULL FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?;
    match exists {
        None => return Ok(Applied::Failed("no such asset".to_owned())),
        Some(false) => return Ok(Applied::Skipped("deleted")),
        Some(true) => {}
    }

    let stored: Option<Value> =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?;
    let mut merged = stored
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (key, value) in values {
        if value.is_null() {
            merged.remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }

    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2) \
         ON CONFLICT (asset_id) DO UPDATE SET values = excluded.values, updated_at = now()",
    )
    .bind(asset_id)
    .bind(Value::Object(merged))
    .execute(&mut *conn)
    .await
    .map_err(dam_db::Error::from)?;

    // The asset's own `updated_at` moves too, or the edit is invisible to anything watching the asset — the
    // reindex queue and the connector both key off it.
    sqlx::query("UPDATE assets SET updated_at = now() WHERE id = $1")
        .bind(asset_id)
        .execute(&mut *conn)
        .await
        .map_err(dam_db::Error::from)?;

    Ok(Applied::Done)
}

/// Validates the patch once for the whole operation.
///
/// The normalised values are *not* substituted back — the merge uses the caller's values as validated. The
/// single-asset endpoint substitutes because it echoes the document back; here there is no echo, and the
/// validator's acceptance is the contract.
async fn validate_patch(
    conn: &mut sqlx::PgConnection,
    values: &serde_json::Map<String, Value>,
) -> Result<()> {
    match dam_db::fields::validate_on(
        conn,
        values,
        dam_core::fields::Mode::Patch,
        dam_core::fields::Writer::Human,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(dam_db::fields::ValidationOutcome::Rejected(rejections)) => {
            let detail: Vec<String> = rejections
                .iter()
                .map(|r| format!("{}: {}", r.key, r.code))
                .collect();
            // Permanent, and failed *before* any item: the patch is the same for every asset, so it would
            // fail all of them identically — better said once than recorded 40,000 times.
            Err(Error::Permanent(format!(
                "the metadata patch does not validate ({}), so no asset was touched",
                detail.join(", ")
            )))
        }
        Err(dam_db::fields::ValidationOutcome::Failed(error)) => Err(error.into()),
    }
}
