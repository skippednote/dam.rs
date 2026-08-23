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
            let outcome = action
                .apply(global, slug, conn.executor(), asset_id)
                .await?;
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
    /// Publish or unpublish, which is one action with a direction (Q.14).
    ///
    /// Routed through the bulk machinery rather than a synchronous endpoint because publication is what a
    /// public page rests on: the actor, the selection and the per-item outcome are the audit trail a public
    /// appearance deserves, and `bulk_operations` already records all three.
    Publish {
        published: bool,
    },
    /// Archive or restore-to-active, which is the same shape as publication: one action with a direction.
    ///
    /// This is the *curation* status, not the storage tier, and conflating the two would be a bad mistake in
    /// both directions. `status = 'archived'` means "out of circulation" — off the default grid, out of the
    /// facets, still instantly fetchable. `storage_class = 'GLACIER'` means "cheap and slow". A library
    /// archives things it has finished with and tiers things nobody reads, and those are frequently different
    /// sets: last season's campaign is archived and still opened weekly; a master nobody has touched in two
    /// years is live and cold.
    ///
    /// The two do compose, and that is the point of keeping them apart: a lifecycle policy can perfectly well
    /// be scoped to archived assets, which is the obvious first rule anybody writes.
    SetStatus {
        archived: bool,
    },
    /// Bring a selection back from cold storage (§6.5).
    ///
    /// The reason this is a bulk kind rather than a loop over the single-asset endpoint is the batch id:
    /// `restores::in_batch` exists so a collection restore is one decision with one cost and one ETA, and it
    /// had no caller. Somebody restoring last year's shoot approves a figure for the shoot, not four hundred
    /// figures for four hundred files.
    ///
    /// What the batch does **not** do is collapse the S3 calls. `RestoreObject` is per object, so four hundred
    /// assets are four hundred calls whatever the grouping — the batch is about the decision, the estimate and
    /// the approval, and claiming otherwise would be claiming a saving that does not exist.
    Restore {
        tier: dam_core::storage::RestoreTier,
        keep_warm_days: i64,
        batch_id: Uuid,
    },
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
            "publish" => Ok(Self::Publish { published: true }),
            "unpublish" => Ok(Self::Publish { published: false }),
            "archive" => Ok(Self::SetStatus { archived: true }),
            "unarchive" => Ok(Self::SetStatus { archived: false }),
            "restore" => {
                let tier = match params.get("tier").and_then(Value::as_str) {
                    None => dam_core::storage::RestoreTier::Standard,
                    Some(raw) => raw
                        .parse()
                        .map_err(|_| Error::Permanent(format!("{raw:?} is not a restore tier")))?,
                };
                Ok(Self::Restore {
                    tier,
                    keep_warm_days: params
                        .get("keep_warm_days")
                        .and_then(Value::as_i64)
                        .unwrap_or(dam_core::restore::DEFAULT_KEEP_WARM_DAYS),
                    // One id for the whole operation, generated here rather than per item — which is the
                    // entire point of the kind.
                    batch_id: Uuid::now_v7(),
                })
            }
            // The schema's vocabulary is wider than what is executable, and the honest response to the gap
            // is a named refusal. "Completing" while doing nothing would be worse: the history would say the
            // work happened.
            other => Err(Error::Permanent(format!(
                "bulk operations of kind {other:?} are not executable yet; only metadata_set, delete, \
                 publish, unpublish, archive and unarchive are"
            ))),
        }
    }

    /// Applies one item.
    ///
    /// `global` and `slug` ride along for the one action that needs to reach outside the tenant transaction:
    /// a restore's estimate is priced from `dam_global.storage_pools`, which the tenant connection cannot
    /// see. Threaded rather than made a field, because a `PgPool` on an enum variant would be a pool held for
    /// the life of a forty-thousand-item operation.
    async fn apply(
        &self,
        global: &sqlx::PgPool,
        slug: &TenantSlug,
        conn: &mut sqlx::PgConnection,
        asset_id: Uuid,
    ) -> Result<Applied> {
        match self {
            Self::Delete => apply_delete(conn, asset_id).await,
            Self::MetadataSet { values } => apply_metadata(conn, asset_id, values).await,
            Self::Publish { published } => apply_publication(conn, asset_id, *published).await,
            Self::SetStatus { archived } => apply_status(conn, asset_id, *archived).await,
            Self::Restore {
                tier,
                keep_warm_days,
                batch_id,
            } => {
                apply_restore(
                    global,
                    slug,
                    conn,
                    asset_id,
                    *tier,
                    *keep_warm_days,
                    *batch_id,
                )
                .await
            }
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
        dam_db::webhooks::enqueue_asset_event(
            conn,
            dam_db::webhooks::kind::DELETED,
            asset_id,
            // The kind of deletion, because the two mean different things downstream: a soft delete is
            // recoverable and a consumer may want to hide rather than forget.
            serde_json::json!({ "soft": true }),
        )
        .await?;
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

/// Publishes or unpublishes one asset (Q.14).
///
/// The instant comes from `now()` in the database rather than from the worker's clock, for the same reason
/// every other timestamp here does: two workers on two machines with a second of drift between them would
/// order a publication before the decision that caused it.
///
/// An asset that is already published is a **skip**, not a success: re-stamping it would lose the instant
/// somebody actually decided, and that instant is the audit answer to "since when has this been public".
/// Asks for one asset's original to be brought back, as part of a batch.
///
/// A skip rather than a failure for everything that is not archived. A selection of five hundred assets will
/// contain hot ones, and refusing the operation over them would mean a user has to filter their own selection
/// by storage class before they are allowed to ask — which is knowledge the system has and they do not.
async fn apply_restore(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    tier: dam_core::storage::RestoreTier,
    keep_warm_days: i64,
    batch_id: Uuid,
) -> Result<Applied> {
    let planned = crate::tiering::plan_for(
        global,
        slug,
        asset_id,
        tier,
        keep_warm_days,
        chrono::Utc::now(),
    )
    .await?;
    let (plan, placement) = match planned {
        Ok(planned) => planned,
        Err(refusal) => {
            return Ok(match refusal {
                // The overwhelmingly common case in a mixed selection, and not a problem.
                dam_core::restore::RestoreRefusal::NotArchived { .. } => {
                    Applied::Skipped("not archived")
                }
                dam_core::restore::RestoreRefusal::Empty => Applied::Skipped("nothing stored"),
                // A tier the class does not offer, or a budget refusal, is the *operation's* problem rather
                // than this item's — every item will hit it identically. Recorded per item all the same,
                // because a run that failed for one reason should say that reason on every row rather than
                // leaving somebody to infer it.
                other => Applied::Failed(other.to_string()),
            });
        }
    };

    let outcome = dam_db::restores::request(
        &mut *conn,
        &dam_db::restores::RestoreSpec {
            object_key: &placement.object_key,
            pool_id: placement.pool_id,
            asset_id: Some(asset_id),
            tier,
            keep_warm_days: i32::try_from(keep_warm_days).unwrap_or(7),
            requested_by: None,
            batch_id: Some(batch_id),
            notify: serde_json::json!({}),
        },
        &plan,
    )
    .await?;

    Ok(match outcome {
        dam_db::restores::Outcome::Created(_) => Applied::Done,
        // Somebody already asked for this one. Coalescing is the correct behaviour and a skip is the honest
        // report: this run did not start a retrieval, and counting it as done would overstate what it did.
        dam_db::restores::Outcome::AlreadyInFlight(_) => Applied::Skipped("already being restored"),
    })
}

/// Moves one asset in or out of circulation.
///
/// Guarded in the `WHERE` like the delete above, for the same reason: the check and the change are one
/// statement, so nothing can land between them. Only `active` and `archived` are touched — an asset that is
/// `uploading` or `processing` is mid-pipeline and archiving it would strand the job that is working on it,
/// while `deleted` is already out of circulation and saying "archived" over the top of it would be a status
/// nobody asked for.
async fn apply_status(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    archived: bool,
) -> Result<Applied> {
    let (to, from) = if archived {
        ("archived", "active")
    } else {
        ("active", "archived")
    };
    let changed = sqlx::query(
        "UPDATE assets SET status = $2, updated_at = now() \
         WHERE id = $1 AND deleted_at IS NULL AND status = $3",
    )
    .bind(asset_id)
    .bind(to)
    .bind(from)
    .execute(&mut *conn)
    .await
    .map_err(dam_db::Error::from)?
    .rows_affected();

    if changed > 0 {
        dam_db::webhooks::enqueue_asset_event(
            conn,
            dam_db::webhooks::kind::STATUS_CHANGED,
            asset_id,
            // Both ends, so a consumer can act on the transition rather than having to remember the previous
            // state itself. An archived asset still resolves through the API but its original may need a
            // restore first, which is exactly what a CMS wants to know before rendering a download link.
            serde_json::json!({ "from": from, "to": to }),
        )
        .await?;
        return Ok(Applied::Done);
    }
    // Why it did not change, in the asset's own words. A bulk run over a mixed selection is *expected* to
    // skip, and a per-item reason is the difference between "148 done, 2 skipped: already archived" and a
    // number nobody can account for.
    let existing: Option<(String, bool)> =
        sqlx::query_as("SELECT status, deleted_at IS NOT NULL FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?;
    Ok(match existing {
        None => Applied::Failed("no such asset".to_owned()),
        Some((_, true)) => Applied::Skipped("deleted"),
        Some((status, _)) if status == to => {
            if archived {
                Applied::Skipped("already archived")
            } else {
                Applied::Skipped("already active")
            }
        }
        // `uploading` or `processing`: mid-pipeline, and a skip rather than a failure because the answer is
        // "not yet", not "never".
        Some(_) => Applied::Skipped("still processing"),
    })
}

async fn apply_publication(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    published: bool,
) -> Result<Applied> {
    let at = published.then(chrono::Utc::now);
    let changed = dam_db::assets::set_published(conn, asset_id, at).await?;
    if changed {
        // In the same transaction as the change, which is the whole point of an outbox: announcing after the
        // commit loses the event on a crash, and announcing before it announces something that may roll back.
        // Only when it *changed*, so a re-publication that was already published emits nothing — a consumer
        // invalidating a cache on every no-op is a consumer doing our idempotence for us.
        dam_db::webhooks::enqueue_asset_event(
            conn,
            if published {
                dam_db::webhooks::kind::PUBLISHED
            } else {
                dam_db::webhooks::kind::UNPUBLISHED
            },
            asset_id,
            serde_json::json!({ "published": published }),
        )
        .await?;
        return Ok(Applied::Done);
    }
    let exists: Option<(bool,)> =
        sqlx::query_as("SELECT deleted_at IS NOT NULL FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(dam_db::Error::from)?;
    Ok(match exists {
        Some((true,)) => Applied::Skipped("deleted"),
        Some((false,)) if published => Applied::Skipped("already published"),
        Some((false,)) => {
            Applied::Failed("the asset exists but could not be unpublished".to_owned())
        }
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

    // Per item, not once for the operation (Q.1). The pre-flight `validate_patch` catches what is wrong for
    // everyone — a typo, a bad kind, an out-of-range value — but with metadata types "applicable" stops being
    // a property of the patch alone: a field on the image form is not on the archive form, and a selection
    // spanning both is a legitimately partial operation. So this is a *skip with a reason* rather than a
    // failure, which is exactly what the `partial` end state exists to report.
    let applicable = dam_db::metadata_types::fields_for_on(&mut *conn, asset_id)
        .await
        .map_err(|refusal| dam_db::Error::Migrate(refusal.to_string()))?;
    if let Some(outside) = values
        .keys()
        .find(|key| !applicable.iter().any(|def| &&def.key == key))
    {
        return Ok(Applied::Failed(format!(
            "{outside} is not part of this asset's metadata type"
        )));
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

    // The *keys* that changed, not the values. A consumer needs to know whether the field it renders was
    // touched, which the keys answer; the values would put a tenant's metadata in a delivery log and in
    // whatever the receiver writes its request bodies to — and a receiver that wants them can read the asset
    // with its own credential and get what it is allowed to see.
    dam_db::webhooks::enqueue_asset_event(
        conn,
        dam_db::webhooks::kind::METADATA_UPDATED,
        asset_id,
        serde_json::json!({ "fields": values.keys().collect::<Vec<_>>() }),
    )
    .await?;

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
