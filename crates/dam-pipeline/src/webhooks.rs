//! The dispatcher: drain the outbox, one pass at a time (Q.20c, §11).
//!
//! `dam_db::webhooks` decides what may go out and in what order; `dam_connect::webhooks` signs and sends it.
//! This is the loop between them, and the interesting decisions are all about *not* holding a database
//! connection while waiting on somebody else's server.
//!
//! ## Claim, release the connection, send, then reconnect to record
//!
//! A delivery waits up to ten seconds. Holding a pooled connection across that would let a handful of slow
//! endpoints exhaust the pool and take the API down with them — the classic way an integration becomes an
//! outage. So each attempt is three short transactions with the HTTP in between, and the `delivering` state in
//! the table is what stands in for the lock that is deliberately not being held.
//!
//! ## Concurrency is bounded, and per pass rather than per subscription
//!
//! A pass claims a batch and sends it concurrently. The batch is what bounds it: [`claim`] already refuses to
//! hand out two deliveries for one asset, so the concurrency is across assets and endpoints, which is exactly
//! where it is safe. No semaphore, no per-subscription accounting — the ordering rule is doing that work
//! already.
//!
//! ## The chain re-queues itself, and drains before it sleeps
//!
//! A pass that filled its batch queues the next one immediately, because a bulk publication of ten thousand
//! assets should not take ten thousand times the poll interval to go out. A pass that found nothing sleeps.
//!
//! [`claim`]: dam_db::webhooks::claim

use crate::Result;
use dam_connect::webhooks::Outcome;
use dam_core::TenantSlug;
use dam_db::TenantConn;
use dam_db::webhooks::{self, AfterFailure};

/// How many deliveries one pass sends concurrently.
///
/// Sixteen: enough that a tenant with many endpoints is not serialised behind one slow one, small enough that
/// a pass cannot hold more connections than the pool has when it comes back to record the results. Each
/// delivery takes a connection only briefly, so the peak is what matters rather than the total.
pub const BATCH: i64 = 16;

/// How long to wait after an empty pass.
pub const POLL_EVERY: chrono::Duration = chrono::Duration::seconds(30);

/// Anything left `delivering` for longer than this is presumed abandoned.
///
/// Well past the ten-second send timeout plus any plausible pause, because the cost of reclaiming too early is
/// a duplicate delivery — which a receiver can deduplicate on the delivery id, but only if it bothered to.
/// The cost of reclaiming too late is one asset's stream stalled, which is worse but visible.
pub const STALE_AFTER: chrono::Duration = chrono::Duration::minutes(5);

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dispatched {
    pub accepted: usize,
    pub retrying: usize,
    pub dead: usize,
    pub subscriptions_disabled: usize,
    pub reclaimed: u64,
    /// Whether the batch came back full, meaning there is probably more waiting.
    pub batch_was_full: bool,
}

/// Sends one batch of due deliveries.
pub async fn dispatch(
    global: &sqlx::PgPool,
    client: &reqwest::Client,
    slug: &TenantSlug,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Dispatched> {
    let mut done = Dispatched::default();

    // Reclaim first. A row stuck in `delivering` blocks every later event for its asset, so a worker that died
    // mid-delivery silently halts one asset's stream — and the halt would outlast this process without this.
    let mut conn = TenantConn::begin(global, slug).await?;
    done.reclaimed = webhooks::reclaim_stalled(conn.executor(), STALE_AFTER.num_seconds()).await?;
    let claimed = webhooks::claim(conn.executor(), BATCH).await?;
    conn.commit().await?;

    if claimed.is_empty() {
        return Ok(done);
    }
    done.batch_was_full = i64::try_from(claimed.len()).unwrap_or(i64::MAX) >= BATCH;

    // The connection is released before any HTTP happens. Ten seconds of somebody else's server is not
    // something to hold a pooled connection through.
    let stamp = now.timestamp();
    let sent = futures::future::join_all(
        claimed
            .iter()
            .map(|delivery| dam_connect::webhooks::send(client, delivery, stamp)),
    )
    .await;

    for (delivery, outcome) in claimed.iter().zip(sent) {
        // One short transaction per result rather than one for the batch. A batch-wide transaction would hold
        // a connection for as long as the slowest recording, and would roll back every result if one failed.
        let mut conn = TenantConn::begin(global, slug).await?;

        // A rejection and a retry take the same path, because a rejection still spends an attempt. The
        // argument is in `dam_connect::webhooks`: abandoning on the first 4xx loses an event to a stray 404
        // from a load balancer, while retrying a genuine rejection wastes a bounded number of tries. What
        // differs is only the diagnosis written to the log, which `classify` has already decided.
        let failure = match outcome {
            Outcome::Accepted { status } => {
                webhooks::delivered(conn.executor(), delivery.id, status).await?;
                done.accepted += 1;
                None
            }
            Outcome::Retry { status, reason } => Some((status, reason)),
            Outcome::Rejected { status, reason } => Some((Some(status), reason)),
        };

        if let Some((status, reason)) = failure {
            match webhooks::failed(conn.executor(), delivery.id, status, &reason).await? {
                AfterFailure::Retrying => done.retrying += 1,
                AfterFailure::DeadLettered => done.dead += 1,
                AfterFailure::SubscriptionDisabled => {
                    done.dead += 1;
                    done.subscriptions_disabled += 1;
                }
            }
        }
        conn.commit().await?;
    }

    Ok(done)
}

/// Returns every claimed delivery to the queue without charging an attempt.
///
/// For a shutdown between the claim and the send. Without it those rows sit `delivering` until
/// [`STALE_AFTER`] elapses, which stalls their assets' streams for five minutes over an ordinary deploy.
pub async fn release_all(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    ids: &[uuid::Uuid],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut conn = TenantConn::begin(global, slug).await?;
    for id in ids {
        webhooks::release(conn.executor(), *id).await?;
    }
    conn.commit().await?;
    Ok(())
}
