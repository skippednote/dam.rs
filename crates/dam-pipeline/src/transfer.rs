//! Moving one source asset into the library.
//!
//! **This has no ingest of its own, and that is the load-bearing decision.** A browser upload becomes an
//! asset through `uploads::create` → `resumable::patch` → `finalise::upload`, and that path is where content
//! addressing, deduplication, virus scanning, derivation and indexing happen. A transfer that wrote assets
//! directly would be a second ingest, and the two would drift in exactly the ways that matter: a migrated
//! asset with no derivatives, or one that skipped the scanner, discovered a year later by somebody wondering
//! why half the library has no thumbnails. So a transfer opens a session, streams the source bytes into it,
//! and lets the existing path do the rest. Everything below is plumbing around that sequence.
//!
//! **Per record, not per run.** The loop lives with the caller. `damctl` already reads the JSON lines a
//! record at a time — a 400k-record extraction is a large file and slurping it to count it would cap the
//! thing this exists to size — so this exposes one record's worth of work and lets the reader drive. It also
//! means the unit under test is one transfer rather than a harness that has to fake stdin.
//!
//! **`source_id` is the idempotency key.** A migration that died half way is resumed by running it again:
//! every record already `migrated` is skipped rather than moved twice. Without that a retry doubles the
//! library, which is the one failure a migration must not have — and the check has to be a read of the
//! record's state rather than a guess from the content hash, because two source assets are allowed to hold
//! identical bytes.

use bytes::{Bytes, BytesMut};
use dam_core::{StorageClass, TenantSlug};
use dam_store::ResumableStore;
use serde_json::{Map, Value};
use sqlx::PgPool;
use tokio::io::AsyncReadExt as _;
use uuid::Uuid;

use crate::source::Source;
use crate::{Error, Result};

/// How much is read from the source before it is handed to the resumable engine.
///
/// The engine does its own part accounting — it buffers to S3's 5 MiB minimum and splits at the maximum — so
/// this number is about how much of somebody's video sits in memory at once, not about part sizes. Eight
/// mebibytes is two parts' worth: large enough that a big file is not thousands of round trips, small enough
/// that a migration is not measured in gigabytes of RSS.
const CHUNK: usize = 8 * 1024 * 1024;

/// What happened to one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It arrived.
    ///
    /// `created` is `finalise`'s own answer, passed through: false when that upload session had already
    /// become an asset. A transfer makes a fresh session per record, so in practice it is always true — it
    /// is reported rather than assumed because the day it is false, that is worth seeing rather than
    /// silently treating as a new asset.
    ///
    /// It is *not* a deduplication flag. Identical bytes are stored once and still become two assets, which
    /// is the right answer for a library where two records can share a file and differ in everything else.
    Migrated { asset_id: Uuid, created: bool },
    /// A previous run already moved it.
    Skipped,
    /// It did not arrive, and the reason is on the record.
    Failed(String),
}

/// Moves one source asset in, and records what became of it.
///
/// `metadata` is the crosswalked payload — already mapped and already validated by the dry run, which used
/// the real validator for exactly this reason. `record` is the raw source document, passed through because
/// which field names the file belongs to the export rather than to us.
///
/// Errors are per record. A source that cannot produce this file, or an ingest that refuses it, marks the
/// record `failed` and returns [`Outcome::Failed`] — a 400k-asset migration does not stop because one file
/// is unreadable, and the operator gets the list at the end. What comes back as `Err` is the other kind:
/// the database is gone, and continuing would only produce more of the same.
#[allow(clippy::too_many_arguments)]
pub async fn one(
    global: &PgPool,
    store: &dyn ResumableStore,
    source: &dyn Source,
    slug: &TenantSlug,
    tenant_id: Uuid,
    job: Uuid,
    source_id: &str,
    record: &Map<String, Value>,
    metadata: &Map<String, Value>,
    scanner: Option<&dam_media::antivirus::Scanner>,
) -> Result<Outcome> {
    // The record is ensured before anything else. `migrated` and `failed` both UPDATE by `source_id`, so a
    // transfer run without a dry run first would write the asset and record nothing — the counters would
    // read zero over a library that had just been filled. `note` is an upsert that refuses to un-migrate,
    // so this is safe on a resumed run as well.
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let state = dam_db::imports::state_of(conn.executor(), job, source_id).await?;
    if state.as_deref() == Some("migrated") {
        conn.commit().await?;
        return Ok(Outcome::Skipped);
    }
    if state.is_none() {
        dam_db::imports::note(
            conn.executor(),
            job,
            source_id,
            None,
            &serde_json::json!([]),
            None,
        )
        .await?;
    }
    conn.commit().await?;

    let fetched = match source.fetch(record).await {
        Ok(fetched) => fetched,
        Err(error) => return refuse(global, slug, job, source_id, error).await,
    };

    match ingest(global, store, slug, tenant_id, fetched, scanner).await {
        Ok(finalised) => {
            let mut conn = dam_db::TenantConn::begin(global, slug).await?;
            if !metadata.is_empty() {
                // No outbox row here, and the reason is worth stating because both *edit* paths do emit
                // one. A transfer only ever writes metadata onto an asset it has just created, and that
                // asset already announces itself through the event `finalise` emits. A `metadata.updated`
                // behind it would be a second notification about a single arrival — the duplicate-event
                // problem, arrived at from the other side.
                dam_db::metadata::merge(conn.executor(), finalised.asset_id, metadata.clone())
                    .await?;
            }
            dam_db::imports::migrated(conn.executor(), job, source_id, finalised.asset_id).await?;
            conn.commit().await?;

            // **The chain, which is the other half of having no ingest of its own.** `finalise::upload`
            // does not queue the follow-on work; its one production caller — the worker's finalise handler —
            // does, and a transfer that called finalise and stopped would produce exactly the drift this
            // module's header warns about. It did, on the first real run: five assets arrived with a
            // placement each and nothing queued, so no proxy, no thumbnail, nothing in the index. The
            // library looked full and searched empty.
            //
            // One enqueue is enough because the DERIVE handler chains the rest — index, then similarity,
            // then enrichment if the tenant has it on — and duplicating that order here is how the two
            // would come apart.
            //
            // At the default band rather than the interactive one: see `enqueue_derive_at`.
            crate::worker::enqueue_derive_at(global, tenant_id, finalised.asset_id, 100).await?;

            tracing::info!(
                %source_id,
                asset = %finalised.asset_id,
                created = finalised.created,
                source = source.name(),
                "transferred",
            );
            Ok(Outcome::Migrated {
                asset_id: finalised.asset_id,
                created: finalised.created,
            })
        }
        Err(error) => refuse(global, slug, job, source_id, error).await,
    }
}

/// Decides whether one record's failure is the record's fault, and records it if so.
///
/// **The distinction is the whole function, and it was got wrong first.** Every failure used to mark the
/// record `failed`. Then the object store was unreachable during a real run and all seven records were
/// branded `failed` — permanently, in the database — for a connection refused on a socket. On a real
/// migration that is four hundred thousand records an operator has to reset by hand, over an outage that
/// lasted a minute, and the report afterwards says the export was bad when the export was fine.
///
/// So a transient error is not this record's news. It goes back to the caller, which stops the run: the
/// record stays `pending`, the operator fixes the store, and re-running picks up exactly where it left off —
/// which the `source_id` skip already makes safe. Only a `Permanent` error, meaning this file is what it is,
/// is written against the record.
///
/// The same weather-versus-news line `integrity` draws, for the same reason: a status that cries wolf is a
/// status nobody reads.
async fn refuse(
    global: &PgPool,
    slug: &TenantSlug,
    job: Uuid,
    source_id: &str,
    error: Error,
) -> Result<Outcome> {
    if error.is_transient() {
        tracing::error!(
            %source_id,
            %error,
            "stopping the transfer: this is not the record's fault, and it stays pending",
        );
        return Err(error);
    }

    let reason = error.to_string();
    tracing::warn!(%source_id, %reason, "a record did not transfer");
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    dam_db::imports::failed(conn.executor(), job, source_id, &reason).await?;
    conn.commit().await?;
    Ok(Outcome::Failed(reason))
}

/// The upload a browser would have made, made from a file instead.
///
/// Deliberately the same three calls in the same order as the TUS handler: a session, the bytes through the
/// resumable engine, and `finalise`. Nothing here knows what an asset is.
async fn ingest(
    global: &PgPool,
    store: &dyn ResumableStore,
    slug: &TenantSlug,
    tenant_id: Uuid,
    mut fetched: crate::source::Fetched,
    scanner: Option<&dam_media::antivirus::Scanner>,
) -> Result<crate::finalise::Finalised> {
    let upload_id = Uuid::new_v4().simple().to_string();

    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let mut session = dam_db::uploads::create(
        conn.executor(),
        tenant_id,
        &upload_id,
        fetched.len,
        Some(&fetched.filename),
        // No declared MIME: the source rarely has one worth trusting, and the ingest path sniffs the bytes
        // anyway. Passing a guess would only give it something wrong to reconcile.
        None,
        // No `created_by`: a migration is not a person, and attributing four hundred thousand assets to
        // whoever held the terminal would make the field a lie in the one place it is used to answer "who
        // put this here".
        None,
        // No upload profile, so `finalise` falls back to the tenant's — the same resolution an ordinary
        // upload gets when the client names none.
        None,
    )
    .await?;
    conn.commit().await?;

    let mut offset = 0u64;
    let mut buffer = BytesMut::with_capacity(CHUNK);
    loop {
        buffer.clear();
        // `read_buf` fills up to the capacity but may stop short, so this reads until the buffer is full or
        // the file ends rather than treating one short read as the end of the stream.
        while buffer.len() < CHUNK {
            let read = fetched
                .reader
                .read_buf(&mut buffer)
                .await
                .map_err(|error| Error::Permanent(format!("reading the source: {error}")))?;
            if read == 0 {
                break;
            }
        }
        if buffer.is_empty() {
            break;
        }

        let chunk = Bytes::copy_from_slice(&buffer);
        let sent = chunk.len() as u64;
        match dam_store::resumable::patch(
            store,
            &mut session,
            offset,
            chunk,
            StorageClass::Standard,
        )
        .await?
        {
            dam_store::resumable::PatchOutcome::Accepted { .. } => {}
            // Neither can happen here — the offset is this loop's own counter and nothing else writes to
            // this session — so if one does, something is wrong enough that guessing would be worse than
            // refusing the record.
            other => {
                return Err(Error::Permanent(format!(
                    "the upload session refused a chunk at {offset}: {other:?}"
                )));
            }
        }
        offset += sent;
    }

    // Once, after the last chunk rather than after each. The parts have to be in the database before
    // `finalise` reads the session back, and a save per chunk would be a transaction per 8 MiB bought
    // nothing: the session id is not recorded anywhere, so a run that dies mid-file cannot resume that file
    // regardless — the *record* stays pending and the retry starts it again.
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    dam_db::uploads::save(conn.executor(), &session).await?;
    conn.commit().await?;

    crate::finalise::upload(global, store, slug, tenant_id, &upload_id, scanner).await
}
