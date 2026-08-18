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
    /// This worker's id, for the lease. Distinct per process, or two workers share a lease and both run the
    /// same job while each believes it holds it.
    pub worker: String,
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
            Ok(())
        }

        kind::INDEX => {
            let asset_id = uuid_field(job, "asset_id")?;
            index_one(context, &slug, job.tenant_id, asset_id).await
        }

        // Not an error to be retried: an unknown kind means a worker older than whatever enqueued it, and
        // retrying will not make this binary understand it. Permanent, so it lands in `failed` where an
        // operator can see the version skew rather than in a backoff loop.
        other => Err(Error::Permanent(format!(
            "this worker does not handle jobs of kind {other:?}"
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
