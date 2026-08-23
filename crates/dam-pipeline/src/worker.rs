//! The queue consumer: claim, dispatch, complete or fail.
//!
//! 0.9's remainder. `dam_db::jobs` had the queue — leases, round-robin fairness across tenants, dedupe keys,
//! attempt counting — and nothing consumed it, so every stage that is "a job" was unreachable. That is why an
//! upload stopped in staging and why no asset had a thumbnail.
//!
//! ## A lease, not a delete
//!
//! `claim` leases with an expiry, so a worker that is SIGKILLed has its jobs come back when the lease lapses
//! rather than losing them. The consequence is at-least-once delivery, and every stage below is written to be
//! safe to run twice — which is a property of the *stages*, not something this loop can provide.
//!
//! ## A permanent failure does not get five attempts
//!
//! A malformed file will never parse, however patiently the queue retries. [`crate::Error::is_transient`]
//! decides, and a permanent failure is pushed straight to its final attempt so the job lands in `failed`
//! now rather than after four pointless retries and twenty minutes of backoff.
//!
//! ## The loop is polling, and that is a considered choice
//!
//! `LISTEN`/`NOTIFY` would be lower latency and needs a dedicated connection per worker plus a fallback poll
//! anyway — because a notification delivered while nobody was listening is simply lost, and a job that
//! becomes runnable through `run_after` generates no notification at all. One poll with a short idle sleep is
//! the whole mechanism, and the sleep is what keeps an empty queue from being a busy loop.

use crate::{Error, Result};
use dam_core::TenantSlug;
use dam_db::jobs::{self, ClaimOptions, Job};
use dam_store::{BlobStore, ResumableStore};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// The job kinds this worker handles.
pub mod kind {
    /// A completed upload becomes an asset.
    pub const FINALISE_UPLOAD: &str = "finalise_upload";
    /// An asset gets its thumbnail, preview and proxy.
    pub const DERIVE: &str = "derive";
    /// An asset is written into the tenant's search index.
    pub const INDEX: &str = "index";
    /// A bulk operation is driven to a terminal state.
    pub const BULK: &str = "bulk";
    /// One asset is rendered into one tenant-defined download format (Q.11).
    pub const RENDER_CONVERSION: &str = "render_conversion";
    /// One asset is described by a hosted model (M5b).
    pub const ENRICH: &str = "enrich";
    /// The next slice of an undescribed library is submitted as one batch (M5c).
    pub const BACKFILL_SUBMIT: &str = "backfill_submit";
    /// A submitted batch is polled, and applied once it has ended (M5c).
    pub const BACKFILL_COLLECT: &str = "backfill_collect";
    /// Every lifecycle policy is planned and, where it says so, executed (§6.4).
    pub const TIER_SWEEP: &str = "tier_sweep";
    /// Restore requests are issued, checked, and expired (§6.5).
    pub const RESTORE_POLL: &str = "restore_poll";
    /// One batch of the webhook outbox is signed and sent (§11).
    pub const WEBHOOK_DISPATCH: &str = "webhook_dispatch";
    /// One asset is perceptually hashed and coloured, and its near-duplicates queued (§8.1).
    pub const SIMILARITY: &str = "similarity";
}

/// How long to wait when the queue is empty.
///
/// Short enough that an upload feels immediate — a user watching the grid should not wait seconds for a
/// thumbnail — and long enough that an idle deployment is not one query per millisecond.
pub const IDLE_SLEEP: Duration = Duration::from_millis(500);

/// How long a claim holds before another worker may take the job.
///
/// A derive of a large TIFF can take tens of seconds, so the lease is generous; a longer job renews with
/// `jobs::heartbeat` rather than taking a longer lease up front, because a long lease is also how long a
/// crashed worker's job stays stuck.
pub const LEASE: Duration = Duration::from_secs(120);

/// Everything a handler needs.
pub struct Context {
    pub global: sqlx::PgPool,
    pub store: Arc<dyn ResumableStore>,
    pub indexes: Arc<dam_search::IndexPool>,
    /// The virus scanner, when one is configured.
    ///
    /// `None` scans nothing, which is the default and is why `docker/DEPLOY.md` lists `clamd` as required for
    /// a deployment rather than optional. Held on the context rather than read from configuration inside
    /// `finalise`, for the same reason the store is: the pipeline has no business knowing how the deployment
    /// is configured.
    pub scanner: Option<dam_media::antivirus::Scanner>,
    /// The C2PA signing identity, when one is configured.
    ///
    /// `None` renders derivatives without credentials — the default, and what `provenance_gaps` reports. An
    /// ephemeral identity is refused outside development by `SigningIdentity` itself, so a deployment either
    /// has a real certificate or signs nothing.
    pub signing_identity: Option<dam_media::provenance::SigningIdentity>,
    /// What a hosted-model call needs: the sealing keyring, the price list, and the transport.
    ///
    /// `None` disables the enrichment kind outright, which is what a deployment without a model configuration
    /// gets — and it is a *skip*, not a failure: a queue full of dead letters is a worse way to say "not
    /// configured" than a run row that says so.
    pub ai: Option<crate::enrich::AiContext>,
    /// This worker's id, for the lease. Distinct per process, or two workers share a lease and both run the
    /// same job while each believes it holds it.
    pub worker: String,
    /// The HTTP client the webhook dispatcher sends with.
    ///
    /// One per process, held here rather than built per delivery, because a client is a connection pool: a
    /// dispatcher constructing one per event pays a fresh TLS handshake per event, which for a busy tenant is
    /// most of the cost of the whole operation.
    pub http: reqwest::Client,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

/// Runs until `shutdown` resolves.
///
/// Drains the batch it is holding before returning: a worker that dropped claimed jobs on SIGTERM would leave
/// them locked until the lease lapsed, which turns every deploy into a two-minute stall for whatever was in
/// flight.
pub async fn run(context: &Context, shutdown: impl std::future::Future<Output = ()>) {
    let shutdown = std::pin::pin!(shutdown);
    let mut shutdown = shutdown;

    loop {
        // Before claiming, so a crashed worker's jobs re-enter the queue rather than waiting for whichever
        // worker happens to call this next.
        match jobs::reclaim_expired(&context.global).await {
            Ok(n) if n > 0 => tracing::info!(reclaimed = n, "expired leases returned to the queue"),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "reclaiming expired leases"),
        }

        let claimed = match jobs::claim(
            &context.global,
            &context.worker,
            ClaimOptions {
                limit: 4,
                per_tenant: None,
                lease: LEASE,
            },
        )
        .await
        {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::error!(%error, "claiming jobs");
                Vec::new()
            }
        };

        if claimed.is_empty() {
            // `select!` rather than a bare sleep, so shutdown is not delayed by up to `IDLE_SLEEP`.
            tokio::select! {
                () = tokio::time::sleep(IDLE_SLEEP) => continue,
                () = &mut shutdown => return,
            }
        }

        for job in claimed {
            let id = job.id;
            let kind = job.kind.clone();
            match handle(context, &job).await {
                Ok(()) => {
                    if let Err(error) = jobs::complete(&context.global, id).await {
                        // The work is done and the row says otherwise, so the job will be retried. Every stage
                        // is idempotent precisely so this is survivable rather than a duplicate.
                        tracing::error!(%error, %id, kind, "completing a finished job");
                    }
                }
                Err(error) => {
                    let transient = error.is_transient();
                    tracing::error!(%id, kind, transient, %error, "job failed");
                    if let Err(inner) = fail(context, &job, &error).await {
                        tracing::error!(error = %inner, %id, "recording a job failure");
                    }
                }
            }
        }
    }
}

/// Records a failure, forcing a permanent one to its final attempt.
///
/// The queue counts attempts and backs off; it has no way to know that a malformed file will still be
/// malformed in four minutes. Collapsing the remaining attempts is what turns "retry five times over twenty
/// minutes" into "this will never work, say so now".
///
/// Public so a test can assert what a failure *does* to the row rather than only which variant was returned —
/// the distinction that mattered when an unknown job kind was permanent: the variant looked deliberate and the
/// effect was losing work.
pub async fn record_failure(
    context: &Context,
    job: &Job,
    error: &Error,
) -> std::result::Result<(), dam_db::Error> {
    fail(context, job, error).await
}

async fn fail(
    context: &Context,
    job: &Job,
    error: &Error,
) -> std::result::Result<(), dam_db::Error> {
    if !error.is_transient() {
        sqlx::query("UPDATE dam_global.jobs SET attempts = max_attempts WHERE id = $1")
            .bind(job.id)
            .execute(&context.global)
            .await?;
    }
    jobs::fail(&context.global, job.id, &error.to_string()).await
}

/// Dispatches one job.
pub async fn handle(context: &Context, job: &Job) -> Result<()> {
    let slug = tenant_slug(&context.global, job.tenant_id).await?;

    match job.kind.as_str() {
        kind::FINALISE_UPLOAD => {
            let upload_id = string_field(job, "upload_id")?;
            let finalised = crate::finalise::upload(
                &context.global,
                context.store.as_ref(),
                &slug,
                job.tenant_id,
                &upload_id,
                context.scanner.as_ref(),
            )
            .await?;

            tracing::info!(
                upload_id = %upload_id,
                asset_id = %finalised.asset_id,
                created = finalised.created,
                mime = %finalised.mime,
                "upload finalised"
            );

            // Chained rather than done inline. Deriving inside finalisation would make an upload's completion
            // wait on a render, and a large TIFF's thumbnail takes long enough that the two want separate
            // leases and separate retry budgets.
            enqueue_derive(&context.global, job.tenant_id, finalised.asset_id).await?;
            Ok(())
        }

        kind::DERIVE => {
            let asset_id = uuid_field(job, "asset_id")?;
            let derived = crate::derive::asset(
                &context.global,
                // The resumable store is a blob store; the upcast is because the derive stage needs no
                // resumable operations and saying so in the signature keeps it honest.
                context.store.as_ref() as &dyn BlobStore,
                &slug,
                job.tenant_id,
                asset_id,
                context.signing_identity.as_ref(),
            )
            .await?;

            tracing::info!(
                %asset_id,
                rendered = ?derived.rendered,
                already = ?derived.already,
                refused = ?derived.refused,
                "derivatives rendered"
            );

            // Indexed after the derivatives exist, so an asset appearing in search already has a thumbnail to
            // draw. The other order makes a result render as a placeholder for however long the derive takes.
            enqueue_index(&context.global, job.tenant_id, asset_id).await?;

            // Hashed and coloured, unconditionally. Unlike enrichment this needs no credential, no budget and
            // no tenant opt-in — it is arithmetic over a proxy the library already has, so there is nothing to
            // switch on and nothing to bill. Queued here because this is where the proxy starts existing.
            enqueue_similarity(&context.global, job.tenant_id, asset_id).await?;

            // And described, if the tenant has switched that on. Checked *before* enqueueing rather than only
            // inside the stage: this is the one queue where a job costs money, and a tenant with enrichment off
            // should not accumulate a million rows that exist to say "off". The stage checks again anyway,
            // because a setting can change between the two.
            if context.ai.is_some() && enrichment_enabled(&context.global, &slug).await? {
                enqueue_enrich(&context.global, job.tenant_id, asset_id).await?;
            }
            Ok(())
        }

        kind::SIMILARITY => {
            let asset_id = uuid_field(job, "asset_id")?;
            match crate::similarity::analyse(
                &context.global,
                context.store.as_ref() as &dyn BlobStore,
                &slug,
                asset_id,
            )
            .await?
            {
                Some(analysed) => tracing::info!(
                    %asset_id,
                    colours = analysed.colours,
                    candidates = analysed.candidates,
                    "hashed and coloured",
                ),
                // Not an image. A library is full of files nothing can look at, and that is not a failure.
                None => tracing::debug!(%asset_id, "nothing to hash"),
            }
            Ok(())
        }

        kind::RENDER_CONVERSION => {
            let asset_id = uuid_field(job, "asset_id")?;
            let key = string_field(job, "conversion")?;
            let rendered = crate::derive::conversion(
                &context.global,
                context.store.as_ref() as &dyn BlobStore,
                &slug,
                job.tenant_id,
                asset_id,
                &key,
                context.signing_identity.as_ref(),
            )
            .await?;

            tracing::info!(
                %asset_id,
                conversion = %rendered.key,
                rendered = rendered.rendered,
                "conversion rendered"
            );
            // No index, no chained job. A conversion is a delivery format: it changes nothing searchable, and
            // enqueueing an index here would make every download somebody chooses rewrite a search document.
            Ok(())
        }

        kind::ENRICH => {
            let asset_id = uuid_field(job, "asset_id")?;
            let Some(ai) = context.ai.as_ref() else {
                // Deliberately not a failure. A deployment with no sealing key configured has not asked for
                // enrichment, and failing every job would fill the dead-letter queue with its own absence.
                tracing::debug!(%asset_id, "no ai context; enrichment not configured");
                return Ok(());
            };
            let enriched = crate::enrich::asset(
                &context.global,
                context.store.as_ref() as &dyn BlobStore,
                ai,
                &slug,
                job.tenant_id,
                asset_id,
            )
            .await?;

            tracing::info!(%asset_id, run_id = %enriched.run_id, outcome = ?enriched.outcome, "asset enriched");
            // Reindexed, because a description and its tags are searchable and an asset whose metadata changed
            // without an index write is one that cannot be found by what the model just wrote.
            if matches!(enriched.outcome, crate::enrich::EnrichOutcome::Wrote { .. }) {
                enqueue_index(&context.global, job.tenant_id, asset_id).await?;
            }
            Ok(())
        }

        kind::WEBHOOK_DISPATCH => {
            let sent = crate::webhooks::dispatch(
                &context.global,
                &context.http,
                &slug,
                chrono::Utc::now(),
            )
            .await?;
            if sent != crate::webhooks::Dispatched::default() {
                tracing::info!(
                    accepted = sent.accepted,
                    retrying = sent.retrying,
                    dead = sent.dead,
                    disabled = sent.subscriptions_disabled,
                    reclaimed = sent.reclaimed,
                    "webhook dispatch",
                );
            }
            // Immediately when the batch came back full, because a bulk publication of ten thousand assets
            // should not take ten thousand poll intervals to go out. Otherwise after the poll interval.
            let after = if sent.batch_was_full {
                chrono::Duration::zero()
            } else {
                crate::webhooks::POLL_EVERY
            };
            requeue_webhook_dispatch(&context.global, job.tenant_id, after).await?;
            Ok(())
        }

        kind::TIER_SWEEP => {
            let swept = crate::tiering::sweep(
                &context.global,
                context.store.as_ref() as &dyn BlobStore,
                &slug,
                chrono::Utc::now(),
            )
            .await?;
            tracing::info!(
                moved = swept.moved,
                planned = swept.planned,
                skipped = swept.skipped,
                failed = swept.failed,
                halted = ?swept.halted,
                "lifecycle sweep",
            );
            // Tomorrow's sweep, queued from inside today's. Not deduped, for the reason
            // `requeue_backfill_collect` documents at length: this job is still `running`, so a shared key
            // would resolve to itself and the chain would end here.
            requeue_tier_sweep(&context.global, job.tenant_id, crate::tiering::SWEEP_EVERY).await?;
            Ok(())
        }

        kind::RESTORE_POLL => {
            let polled = crate::tiering::poll(
                &context.global,
                context.store.as_ref() as &dyn BlobStore,
                &slug,
                chrono::Utc::now(),
            )
            .await?;
            if polled != crate::tiering::Polled::default() {
                tracing::info!(
                    issued = polled.issued,
                    reissued = polled.reissued,
                    available = polled.available,
                    expired = polled.expired,
                    failed = polled.failed,
                    "restore poll",
                );
            }
            requeue_restore_poll(&context.global, job.tenant_id, crate::tiering::POLL_EVERY)
                .await?;
            Ok(())
        }

        kind::BACKFILL_SUBMIT => {
            let Some(ai) = context.ai.as_ref() else {
                tracing::debug!("no ai context; backfill not configured");
                return Ok(());
            };
            let slice = job
                .payload
                .get("slice")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(crate::backfill::DEFAULT_SLICE);
            let submitted = crate::backfill::submit(
                &context.global,
                context.store.as_ref() as &dyn BlobStore,
                ai,
                &slug,
                job.tenant_id,
                slice,
            )
            .await?;

            match &submitted {
                crate::backfill::Submitted::Batch { batch_id, count } => {
                    tracing::info!(%batch_id, count, "backfill batch submitted");
                    // The collector, and then the next slice — but only after this batch is applied, which is
                    // what chaining from `collect` rather than here achieves. Submitting every slice at once
                    // would put a whole library in flight before the first description was checked.
                    enqueue_backfill_collect(&context.global, job.tenant_id, batch_id, slice)
                        .await?;
                }
                crate::backfill::Submitted::Nothing(why) => {
                    tracing::info!(reason = %why, "backfill submitted nothing");
                }
            }
            Ok(())
        }

        kind::BACKFILL_COLLECT => {
            let Some(ai) = context.ai.as_ref() else {
                tracing::debug!("no ai context; backfill not configured");
                return Ok(());
            };
            let batch_id = string_field(job, "batch_id")?;
            let slice = job
                .payload
                .get("slice")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(crate::backfill::DEFAULT_SLICE);
            let collected =
                crate::backfill::collect(&context.global, ai, &slug, job.tenant_id, &batch_id)
                    .await?;

            match collected {
                crate::backfill::Collected::Waiting { finished, total } => {
                    tracing::info!(%batch_id, finished, total, "backfill batch still working");
                    // Re-queued rather than looped in place: a batch takes up to twenty-four hours, and a job
                    // holding a lease for that long is a job nobody can take over.
                    requeue_backfill_collect(
                        &context.global,
                        job.tenant_id,
                        &batch_id,
                        slice,
                        POLL_INTERVAL,
                    )
                    .await?;
                }
                crate::backfill::Collected::Applied {
                    wrote,
                    declined,
                    errored,
                    expired,
                    micro_cents,
                } => {
                    tracing::info!(
                        %batch_id,
                        wrote,
                        declined,
                        errored,
                        expired,
                        micro_cents,
                        "backfill batch applied"
                    );
                    // Reindexed per asset, and the next slice queued: a backfill is a chain of batches, each
                    // starting only once the last has landed.
                    for asset_id in assets_in_batch(&context.global, &slug, &batch_id).await? {
                        enqueue_index(&context.global, job.tenant_id, asset_id).await?;
                    }
                    if wrote + declined + errored + expired > 0 {
                        enqueue_backfill(&context.global, job.tenant_id, slice).await?;
                    }
                }
            }
            Ok(())
        }

        kind::INDEX => {
            let asset_id = uuid_field(job, "asset_id")?;
            index_one(context, &slug, job.tenant_id, asset_id).await
        }

        kind::BULK => {
            let operation_id = uuid_field(job, "operation_id")?;
            let job_id = job.id;
            let worker = context.worker.clone();
            let global = context.global.clone();

            let executed = crate::bulk_exec::run(
                &context.global,
                &slug,
                operation_id,
                chrono::Utc::now(),
                // The lease renews per batch, because a 40,000-item operation legitimately outlives one
                // lease — and a worker whose lease was reclaimed must *stop*, not fight the worker that took
                // over. `LeaseLost` is its own error variant for exactly that; surfacing it as transient lets
                // the new holder's run be the one that finishes.
                async move || {
                    jobs::heartbeat(&global, job_id, &worker, LEASE)
                        .await
                        .map_err(|error| match error {
                            dam_db::Error::LeaseLost { .. } => Error::Transient(format!(
                                "lease lost mid-operation: {error}; another worker owns this job now"
                            )),
                            other => other.into(),
                        })
                },
            )
            .await?;

            tracing::info!(
                %operation_id,
                state = %executed.state,
                done = executed.done,
                failed = executed.failed,
                touched = executed.touched.len(),
                "bulk operation finished"
            );

            // Every changed asset gets re-indexed, or a bulk metadata edit is invisible to search and a bulk
            // delete leaves ghosts in the results until the next full reindex.
            for asset_id in executed.touched {
                enqueue_index(&context.global, job.tenant_id, asset_id).await?;
            }
            Ok(())
        }

        // **Transient**, and this was wrong until Q.11 made it visible. An unknown kind means a worker older
        // than whatever enqueued it — which is precisely a rolling deploy, the moment a new kind first appears.
        // Marking it permanent collapsed the remaining attempts and dead-lettered the job, so every job of a
        // newly deployed kind that an old worker happened to claim was *lost*, silently, until somebody noticed
        // work missing. Found by running the real thing: an old dev worker was still up when the conversion
        // render shipped, and it killed the first job.
        //
        // Retrying will not teach *this* binary the kind. It does not need to: the retry is claimable by any
        // worker in the fleet, and the new ones understand it. A genuinely unknown kind — a typo, a kind since
        // removed — still dead-letters once its attempts are spent, which is where an operator sees it.
        other => Err(Error::Transient(format!(
            "this worker does not handle jobs of kind {other:?}; another worker in the fleet may"
        ))),
    }
}

/// Queues the derive stage for an asset.
pub async fn enqueue_derive(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    asset_id: Uuid,
) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::DERIVE)
            .payload(serde_json::json!({ "asset_id": asset_id }))
            // Below the default: a user is watching the grid for a thumbnail, and §6.4's background work is
            // not. 50 is the boundary `JobSpec::priority` documents for interactive work.
            .priority(40)
            .dedupe_key(format!("derive:{asset_id}")),
    )
    .await?)
}

/// Queues the index stage for an asset.
pub async fn enqueue_index(global: &sqlx::PgPool, tenant_id: Uuid, asset_id: Uuid) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::INDEX)
            .payload(serde_json::json!({ "asset_id": asset_id }))
            .priority(50)
            .dedupe_key(format!("index:{asset_id}")),
    )
    .await?)
}

/// How long to wait between polls of a batch.
///
/// Batches take minutes to hours, so a tight poll is a query per second that answers "not yet" for a day. Two
/// minutes is short enough that a fast batch is not left sitting and long enough to be free.
pub const POLL_INTERVAL: Duration = Duration::from_secs(120);

/// Queues the next slice of a library backfill.
///
/// The dedupe key is per tenant, so asking twice does not put two slices in flight — the whole design of the
/// chain is one batch at a time, and this is where that is enforced.
pub async fn enqueue_backfill(global: &sqlx::PgPool, tenant_id: Uuid, slice: i64) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::BACKFILL_SUBMIT)
            .payload(serde_json::json!({ "slice": slice }))
            .priority(80)
            .dedupe_key(format!("backfill:{tenant_id}")),
    )
    .await?)
}

/// Queues the collector for one batch, once.
///
/// Deduped per batch — not per tenant, because two batches may legitimately be open (somebody submitted one by
/// hand) and a shared key would leave the second unpolled.
pub async fn enqueue_backfill_collect(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    batch_id: &str,
    slice: i64,
) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        collect_spec(tenant_id, batch_id, slice, Duration::from_secs(0))
            .dedupe_key(format!("backfill_collect:{batch_id}")),
    )
    .await?)
}

/// Queues the *next* poll of a batch, from inside the poll that just ran.
///
/// **Deliberately not deduped, and this is the subtle part.** The dedupe index covers jobs that are `queued` or
/// `running`, and a handler re-queueing itself is still `running` — so an identical key conflicts with the job
/// doing the enqueueing, `jobs::enqueue` returns that job's own id, and the chain ends silently the moment it
/// completes. A live backfill did exactly that: one poll, "still working", and a batch nobody ever came back
/// for. See the note on `JobSpec::dedupe_key`.
///
/// There is no duplicate to fear instead: the only caller is the collector itself, once per claimed job.
pub async fn requeue_backfill_collect(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    batch_id: &str,
    slice: i64,
    after: Duration,
) -> Result<Uuid> {
    Ok(jobs::enqueue(global, collect_spec(tenant_id, batch_id, slice, after)).await?)
}

/// Starts a tenant's lifecycle sweep chain.
///
/// Deduped per tenant, because a second chain would double every transition attempt — harmless in itself,
/// since `transition` on an object already in the class is a no-op, but it doubles the S3 request bill and
/// makes the log unreadable.
pub async fn enqueue_tier_sweep(global: &sqlx::PgPool, tenant_id: Uuid) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        sweep_spec(tenant_id, chrono::Duration::zero())
            .dedupe_key(format!("tier_sweep:{tenant_id}")),
    )
    .await?)
}

/// Queues tomorrow's sweep, from inside today's. Not deduped — see [`requeue_backfill_collect`].
pub async fn requeue_tier_sweep(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    after: chrono::Duration,
) -> Result<Uuid> {
    Ok(jobs::enqueue(global, sweep_spec(tenant_id, after)).await?)
}

/// Queues the similarity pass for one asset.
///
/// Deduped per asset, so a re-derive does not queue a second pass for work that is idempotent anyway — the
/// hashes upsert and the candidate insert ignores conflicts, so a duplicate job would be harmless and wasted.
pub async fn enqueue_similarity(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    asset_id: Uuid,
) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::SIMILARITY)
            .payload(serde_json::json!({ "asset_id": asset_id }))
            // Below the interactive kinds and below indexing: a duplicate candidate is useful within the hour,
            // and a thumbnail is somebody waiting.
            .priority(70)
            .dedupe_key(format!("similarity:{asset_id}")),
    )
    .await?)
}

/// Starts a tenant's webhook dispatch chain.
///
/// Deduped per tenant, and called when a subscription is created rather than at boot: a deployment where
/// nobody has ever registered a webhook runs no dispatch at all, which is most deployments.
pub async fn enqueue_webhook_dispatch(global: &sqlx::PgPool, tenant_id: Uuid) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        dispatch_spec(tenant_id, chrono::Duration::zero())
            .dedupe_key(format!("webhook_dispatch:{tenant_id}")),
    )
    .await?)
}

/// Queues the next pass. Not deduped, for the reason `requeue_tier_sweep` documents: this job is still
/// `running`, so a shared key would resolve to itself and the chain would stop here.
pub async fn requeue_webhook_dispatch(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    after: chrono::Duration,
) -> Result<Uuid> {
    Ok(jobs::enqueue(global, dispatch_spec(tenant_id, after)).await?)
}

fn dispatch_spec(tenant_id: Uuid, after: chrono::Duration) -> jobs::JobSpec {
    jobs::JobSpec::new(tenant_id, kind::WEBHOOK_DISPATCH)
        // Above housekeeping, below anything interactive. A webhook is somebody's integration waiting, which
        // matters more than a lifecycle sweep and less than a thumbnail somebody is looking at.
        .priority(60)
        .run_after(chrono::Utc::now() + after)
}

fn sweep_spec(tenant_id: Uuid, after: chrono::Duration) -> jobs::JobSpec {
    jobs::JobSpec::new(tenant_id, kind::TIER_SWEEP)
        // Below every interactive kind. A sweep is housekeeping and a thumbnail is somebody waiting.
        .priority(90)
        .run_after(chrono::Utc::now() + after)
}

/// Starts a tenant's restore-poll chain.
///
/// Deduped per tenant. Called when a restore is requested rather than at boot, so a deployment where nobody
/// has ever archived anything runs no polling at all.
pub async fn enqueue_restore_poll(global: &sqlx::PgPool, tenant_id: Uuid) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        poll_spec(tenant_id, chrono::Duration::zero())
            .dedupe_key(format!("restore_poll:{tenant_id}")),
    )
    .await?)
}

/// Queues the next poll, from inside this one. Not deduped — see [`requeue_backfill_collect`].
pub async fn requeue_restore_poll(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    after: chrono::Duration,
) -> Result<Uuid> {
    Ok(jobs::enqueue(global, poll_spec(tenant_id, after)).await?)
}

fn poll_spec(tenant_id: Uuid, after: chrono::Duration) -> jobs::JobSpec {
    jobs::JobSpec::new(tenant_id, kind::RESTORE_POLL)
        // Higher than the sweep and lower than a derive: somebody is waiting for a restore, but not watching
        // it appear the way they watch a thumbnail.
        .priority(85)
        .run_after(chrono::Utc::now() + after)
}

/// The shape both collector enqueues share, minus the dedupe decision.
fn collect_spec(tenant_id: Uuid, batch_id: &str, slice: i64, after: Duration) -> jobs::JobSpec {
    jobs::JobSpec::new(tenant_id, kind::BACKFILL_COLLECT)
        .payload(serde_json::json!({ "batch_id": batch_id, "slice": slice }))
        .priority(80)
        // A wall-clock time, because the queue's own `run_after` is a timestamp: the delay is expressed here so
        // both callers say it the same way.
        .run_after(
            chrono::Utc::now()
                + chrono::Duration::from_std(after).unwrap_or_else(|_| chrono::Duration::zero()),
        )
}

/// The assets a batch touched, for reindexing.
async fn assets_in_batch(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    batch_id: &str,
) -> Result<Vec<Uuid>> {
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT asset_id FROM enrichment_runs \
          WHERE llm_batch_id = $1 AND state IN ('succeeded', 'partial')",
    )
    .bind(batch_id)
    .fetch_all(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;
    Ok(ids)
}

/// Whether this tenant wants its assets described.
///
/// One small query on the derive path. The alternative — enqueueing unconditionally — is a queue that grows by
/// one row per upload for every tenant that has never turned the feature on.
async fn enrichment_enabled(global: &sqlx::PgPool, slug: &TenantSlug) -> Result<bool> {
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let settings = dam_db::enrichment::settings(conn.executor()).await?;
    conn.commit().await?;
    Ok(settings.is_enabled)
}

/// Queues one asset for description by a hosted model.
///
/// Priority 70 — behind derivatives and indexing, both of which a person is waiting on. Enrichment is worth
/// having and nobody is watching the grid for it.
///
/// The dedupe key is per asset, so a hundred edits in a minute produce one call rather than a hundred: this is
/// the one queue in damrs where a duplicate job costs money.
pub async fn enqueue_enrich(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    asset_id: Uuid,
) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::ENRICH)
            .payload(serde_json::json!({ "asset_id": asset_id }))
            .priority(70)
            .dedupe_key(format!("enrich:{asset_id}")),
    )
    .await?)
}

/// Queues the execution of a bulk operation.
pub async fn enqueue_bulk(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    operation_id: Uuid,
) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::BULK)
            .payload(serde_json::json!({ "operation_id": operation_id }))
            // A person clicked "apply to N assets" and is watching a progress bar, so it sits with the other
            // interactive work rather than behind background maintenance.
            .priority(45)
            .dedupe_key(format!("bulk:{operation_id}")),
    )
    .await?)
}

/// Queues finalisation for a completed upload.
pub async fn enqueue_finalise(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    upload_id: &str,
) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::FINALISE_UPLOAD)
            .payload(serde_json::json!({ "upload_id": upload_id }))
            // The most interactive job there is: a user has just finished uploading and is waiting for their
            // asset to appear.
            .priority(30)
            .dedupe_key(format!("finalise:{upload_id}")),
    )
    .await?)
}

/// Queues one asset's render into one named download format.
///
/// Deduplicated on `(asset, conversion)`, which is what makes two people choosing the same format at the same
/// moment one render rather than two. The key rather than the id, because the key is what the request carried
/// and what the delivery token will carry — a dedupe key built from something the caller never named would not
/// collapse the case it exists for.
///
/// Priority sits above a background derive and below finalisation: somebody is waiting, but not for the thing
/// that makes their upload appear at all.
pub async fn enqueue_conversion(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    asset_id: Uuid,
    conversion: &str,
) -> Result<Uuid> {
    Ok(jobs::enqueue(
        global,
        jobs::JobSpec::new(tenant_id, kind::RENDER_CONVERSION)
            .payload(serde_json::json!({ "asset_id": asset_id, "conversion": conversion }))
            .priority(40)
            .dedupe_key(format!("conversion:{asset_id}:{conversion}")),
    )
    .await?)
}

/// Writes one asset into the tenant's search index.
///
/// Incremental: this deletes the asset's own document and adds it back, rather than rebuilding the tenant's
/// index. A full reindex per upload would make ingest cost time proportional to the library — which is
/// `damctl reindex`'s job, run deliberately.
async fn index_one(
    context: &Context,
    slug: &TenantSlug,
    tenant_id: Uuid,
    asset_id: Uuid,
) -> Result<()> {
    let mut conn = dam_db::TenantConn::begin(&context.global, slug).await?;
    let defs = dam_db::fields::load(conn.executor()).await?;
    let row = sqlx::query_as::<_, (String, bool, serde_json::Value, Vec<Uuid>)>(
        "SELECT assets.filename, assets.deleted_at IS NOT NULL AS deleted, \
                coalesce(m.values, '{}'::jsonb) AS values, \
                coalesce(array_agg(gm.group_id) FILTER (WHERE gm.group_id IS NOT NULL), '{}') AS groups \
         FROM assets \
         LEFT JOIN asset_metadata m ON m.asset_id = assets.id \
         LEFT JOIN asset_group_members gm ON gm.asset_id = assets.id \
         WHERE assets.id = $1 \
         GROUP BY assets.filename, assets.deleted_at, m.values",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    let Some((filename, deleted, values, groups)) = row else {
        return Err(Error::Permanent(format!(
            "asset {asset_id} does not exist, so there is nothing to index"
        )));
    };
    let _ = tenant_id;

    let schema = dam_search::IndexSchema::new(defs);
    let writer = context
        .indexes
        .writer(slug, &schema)
        .await
        .map_err(|e| Error::Transient(format!("opening the index writer: {e}")))?;
    let mut guard = writer.lock().await;

    // Deleted by term first, so a re-index replaces rather than duplicates: Tantivy has no update, and adding
    // a document whose id is already present leaves two. The return is an opstamp, not a count of what
    // matched — there is nothing to check here, which is why it is discarded explicitly rather than by
    // accident.
    let _ = guard.delete_term(tantivy::Term::from_field_text(
        schema.asset_id(),
        &asset_id.to_string(),
    ));

    let document = dam_search::AssetDocument {
        asset_id,
        filename,
        deleted,
        group_ids: groups,
        values: values.as_object().cloned().unwrap_or_default(),
    };
    guard
        .add_document(document.to_tantivy(&schema))
        .map_err(|e| Error::Transient(format!("adding the document: {e}")))?;
    guard
        .commit()
        .map_err(|e| Error::Transient(format!("committing the index: {e}")))?;

    tracing::info!(%asset_id, "indexed");
    Ok(())
}

/// The tenant's slug, which is what `TenantConn` needs.
async fn tenant_slug(global: &sqlx::PgPool, tenant_id: Uuid) -> Result<TenantSlug> {
    let slug: Option<String> =
        sqlx::query_scalar("SELECT slug FROM dam_global.tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(global)
            .await
            .map_err(dam_db::Error::from)?;

    let slug = slug.ok_or_else(|| {
        // The job's tenant is gone. Permanent: the cascade should have taken the job with it, and retrying
        // cannot resurrect a tenant.
        Error::Permanent(format!(
            "job names tenant {tenant_id}, which does not exist"
        ))
    })?;
    TenantSlug::new(&slug).map_err(|e| Error::Permanent(format!("stored slug {slug:?}: {e}")))
}

fn string_field(job: &Job, field: &str) -> Result<String> {
    job.payload
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        // Permanent: a payload missing a field it needs was enqueued wrong, and it will still be missing on
        // the fifth attempt.
        .ok_or_else(|| {
            Error::Permanent(format!(
                "job {} of kind {} has no string {field:?} in its payload",
                job.id, job.kind
            ))
        })
}

fn uuid_field(job: &Job, field: &str) -> Result<Uuid> {
    let raw = string_field(job, field)?;
    Uuid::parse_str(&raw).map_err(|e| {
        Error::Permanent(format!(
            "job {} has {field}={raw:?}, which is not a uuid: {e}",
            job.id
        ))
    })
}
