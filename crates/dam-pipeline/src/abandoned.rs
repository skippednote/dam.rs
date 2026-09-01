//! Uploads the client never came back for (3.x, item 1).
//!
//! ## The gap
//!
//! `POST /uploads/presign` records a session and hands out a presigned PUT. The bytes then go straight from
//! the client to the bucket, **which this process never observes**. Nothing marks the session complete but the
//! client calling back, so a client that uploads successfully and then crashes, loses its network, or is a
//! browser tab somebody closed leaves a session `active` and an object in staging.
//!
//! What happened next was worse than nothing happening. The session expires, `uploads::reap` terminates it,
//! and terminating deletes the staging object — so the system takes a file the user successfully uploaded and
//! throws it away, having reported success to them at every step. That is the failure this closes.
//!
//! ## Why a HEAD rather than bucket notifications
//!
//! The AWS-native answer is S3 Event Notifications: the bucket tells a queue, a worker finalises regardless of
//! the client. It is the better mechanism where it exists, and D1 says S3-compatible — MinIO supports bucket
//! notifications and SeaweedFS only partially, so an events-only implementation would close the gap on AWS and
//! leave it open everywhere else, which is the worst of both.
//!
//! `head` is on the `BlobStore` trait, so this works on every driver today with no configuration. An event
//! path can be added later as the faster trigger for the same finalisation; this stays as the floor beneath
//! it, which is what a correctness guarantee needs.
//!
//! ## Finalise, or reclaim — never both, and never neither
//!
//! For each expired session the object either exists with bytes or it does not:
//!
//! - **It exists.** The client finished and did not tell us. Finalisation is enqueued and the session is left
//!   `active` — the finalise handler is what completes it, and it is the same path a well-behaved client
//!   takes. Nothing here promotes or adopts bytes itself, because a second ingest path is exactly what
//!   `finalise` exists to prevent.
//! - **It does not.** Nothing was uploaded, or a multipart upload was abandoned mid-way. Reclaimed as before.
//!
//! A store that cannot answer is neither: the session is left alone for the next pass. Guessing "absent" would
//! delete a real upload because of a transient error, which is the failure this module exists to stop.

use dam_store::{BlobStore, Key, ResumableStore};
use uuid::Uuid;

/// What one pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    /// Sessions whose object was there after all, now queued for finalisation.
    pub rescued: u64,
    /// Sessions with nothing behind them, reclaimed.
    pub reclaimed: u64,
    /// Sessions the store could not be asked about. Left for the next pass.
    pub deferred: u64,
}

/// How many expired sessions one pass considers, per tenant.
///
/// Bounded so a backlog cannot make a single pass run for minutes while holding nothing but making the next
/// sweep late. A backlog drains over several passes, which is fine — these are already expired.
const BATCH: i64 = 200;

/// Looks at every expired upload session for one tenant and either rescues or reclaims it.
///
/// # Errors
/// A database failure. A *store* failure is per-session and counted in [`Swept::deferred`] rather than
/// failing the pass, because one unreachable object must not stop the rest from being reclaimed.
pub async fn sweep_tenant(
    global: &sqlx::PgPool,
    conn: &mut sqlx::PgConnection,
    store: &dyn ResumableStore,
    tenant_id: Uuid,
) -> Result<Swept, dam_db::Error> {
    let due = dam_db::uploads::reapable(&mut *conn, BATCH).await?;
    let mut swept = Swept::default();

    for session in due {
        let key = match Key::staging(tenant_id, &session.id) {
            Ok(key) => key,
            Err(error) => {
                // A row whose id cannot form a key is not something a sweep can act on either way.
                tracing::warn!(upload_id = %session.id, %error, "cannot rebuild a staging key");
                swept.deferred += 1;
                continue;
            }
        };

        match (store as &dyn BlobStore).head(&key).await {
            Ok(state) if state.size > 0 => {
                // The client finished and never said so. Hand it to the ordinary finalisation path.
                match crate::worker::enqueue_finalise(global, tenant_id, &session.id).await {
                    Ok(_) => {
                        tracing::info!(
                            upload_id = %session.id, %tenant_id, bytes = state.size,
                            "an expired upload had its object after all; finalising rather than discarding it",
                        );
                        swept.rescued += 1;
                    }
                    Err(error) => {
                        // Left `active` and unqueued: the next pass tries again. Reclaiming here would
                        // delete the object because we could not queue a job, which is the wrong direction.
                        tracing::warn!(upload_id = %session.id, %error, "could not queue finalisation");
                        swept.deferred += 1;
                    }
                }
            }
            // Present but empty is the same as absent: a zero-byte staging object is a PUT that was
            // started and produced nothing, and finalising it would create an empty asset.
            Ok(_) => {
                swept.reclaimed += reclaim(&mut *conn, store, session).await?;
            }
            Err(dam_store::Error::NotFound { .. }) => {
                swept.reclaimed += reclaim(&mut *conn, store, session).await?;
            }
            Err(error) => {
                // Cannot tell. Leaving it is the only safe answer — see the module docs.
                tracing::warn!(
                    upload_id = %session.id, %error,
                    "could not check an expired upload's object; leaving it for the next pass",
                );
                swept.deferred += 1;
            }
        }
    }

    Ok(swept)
}

/// Reclaims the session this pass just checked, by name.
///
/// `reap_one` rather than `reap`, and that is the whole point: this loop checked *this* session's object, so
/// it must act on *this* session. Reaping by position after checking by name would delete an upload whose
/// bytes are present because a different one's were not.
async fn reclaim(
    conn: &mut sqlx::PgConnection,
    store: &dyn ResumableStore,
    session: dam_store::resumable::ResumableSession,
) -> Result<u64, dam_db::Error> {
    Ok(u64::from(
        dam_db::uploads::reap_one(&mut *conn, store, session).await?,
    ))
}
