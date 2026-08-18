//! Turning a completed upload into an asset.
//!
//! The step that was missing. An upload's bytes land under `<tenant>/staging/<upload_id>`, which is a
//! namespace the reaper sweeps — so until this runs, a successful upload is on a timer to be deleted.
//!
//! ## The store half already existed
//!
//! `dam_media::ingest::finalize` validates and promotes: it heads the object, refuses an empty or oversized
//! one from the HEAD alone, sniffs the type from a ranged prefix, hashes the whole object in **bounded
//! windows** — so a 200 GB master never materialises in memory — and promotes it to its content-addressed
//! key. This module is the *database* half: the rows that make a promoted object an asset.
//!
//! Deliberately not reimplemented. A second hashing path would be a second answer to "what is this object's
//! digest", and the two would diverge on exactly the file nobody tested.
//!
//! ## The order is: assemble, promote, record, then unstage
//!
//! Each step is safe to repeat and none of them destroys the previous one's output until the next is
//! durable. Specifically the staging object is deleted **last**, after the asset row exists: doing it
//! earlier would mean a crash between the copy and the insert leaves bytes under a content-addressed key
//! with nothing pointing at them, which is an orphan the scrub has to reason about. Deleting last means a
//! crash leaves the staging object instead — and that has a reaper.
//!
//! ## The type comes from the bytes, never from the client
//!
//! `Upload-Metadata` is attacker-controlled. The declared filename is preserved verbatim in
//! `assets.filename` because it is what the user called their file, and the *type* is sniffed
//! (`dam_media::sniff`). A mismatch between the two is recorded rather than acted on: usually a careless
//! client, occasionally an attempt, and either way the evidence should not be discarded.
//!
//! ## Deduplication is a consequence of content addressing, not a feature bolted on
//!
//! The object key is derived from the BLAKE3 of the bytes, so uploading the same file twice writes the same
//! key. Two *assets* still exist — they have different filenames, metadata and rights — and they share one
//! object. That is D1, and it means the copy below is sometimes a no-op the store performs anyway.

use crate::{Error, Result};
use dam_core::StorageClass;
use dam_db::TenantConn;
use dam_store::resumable::SessionStatus;
use dam_store::{ByteRange, Key, ResumableStore, resumable};
use uuid::Uuid;

/// How much of the object the dimension probe sees.
///
/// The `image` crate reads dimensions from the header, so a prefix is enough — and it has to be a prefix,
/// because this runs on every upload and downloading a 2 GB master to learn it is 8000×6000 would make ingest
/// cost egress proportional to the library. 256 KiB covers JPEG, PNG, WebP, TIFF and GIF headers with room to
/// spare; a format whose header sits further in probes as unknown, which §18.2 fills in through libvips rather
/// than by reading more here.
const PROBE_PREFIX: u64 = 256 * 1024;

/// What finalisation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finalised {
    pub asset_id: Uuid,
    pub content_hash: String,
    pub mime: String,
    pub bytes: i64,
    /// False when the asset already existed — a re-run of the same job.
    pub created: bool,
}

/// Finalises `upload_id` into an asset.
///
/// Idempotent. A second run finds the session already `completed` and the asset already recorded, and returns
/// the same answer with `created: false` — which matters because the queue is at-least-once and a worker that
/// died after the insert will be asked to do this again.
pub async fn upload(
    global: &sqlx::PgPool,
    store: &dyn ResumableStore,
    slug: &dam_core::TenantSlug,
    tenant_id: Uuid,
    upload_id: &str,
) -> Result<Finalised> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let session = dam_db::uploads::load(conn.executor(), tenant_id, upload_id).await?;
    conn.commit().await?;

    let Some(mut session) = session else {
        // Permanent: the session is gone, so no amount of retrying will find it. Usually the reaper got
        // there first, which is the correct outcome for an upload nobody finished.
        return Err(Error::Permanent(format!(
            "upload {upload_id} has no session; it was reaped or never existed"
        )));
    };

    let staging = session.staging_key()?;

    // Assembled only if it has not been. `complete` refuses a session that is already `Completed`, and that
    // refusal is the idempotency check rather than something to work around.
    if matches!(session.status, SessionStatus::Active) {
        resumable::complete(store, &mut session, StorageClass::Standard)
            .await
            .map_err(|error| match error {
                // A short upload is not a failure to retry — the client may still send the rest, and the
                // session deliberately stays active. Reporting it as permanent stops the queue from burning
                // attempts on an upload that is merely unfinished.
                dam_store::Error::Backend(message) if message.contains("declared bytes") => {
                    Error::Permanent(format!("upload {upload_id} is incomplete: {message}"))
                }
                other => Error::Store(other),
            })?;

        let mut conn = TenantConn::begin(global, slug).await?;
        dam_db::uploads::save(conn.executor(), &session).await?;
        conn.commit().await?;
    } else if matches!(session.status, SessionStatus::Terminated) {
        return Err(Error::Permanent(format!(
            "upload {upload_id} was terminated; its bytes are gone"
        )));
    }

    let mut conn = TenantConn::begin(global, slug).await?;
    let declared = dam_db::uploads::declared(conn.executor(), tenant_id, upload_id)
        .await?
        .unwrap_or_default();
    conn.commit().await?;

    // Already finalised. Returned before any store work, because a re-run of this job is the normal case
    // under an at-least-once queue and re-copying an object is not free.
    if let Some(asset_id) = declared.asset_id {
        let existing = existing_asset(global, slug, asset_id).await?;
        return Ok(Finalised {
            asset_id,
            content_hash: existing.0,
            mime: existing.1,
            bytes: existing.2,
            created: false,
        });
    }

    // Already promoted by an attempt that died before recording the asset. The bytes are at their
    // content-addressed key, staging is gone, and re-promoting is impossible — so this resumes from the digest
    // instead. Without it the retry failed permanently at "staging object not found" on an upload whose bytes
    // were safely stored the whole time, which is exactly what happened the first time this ran for real.
    let promoted = match declared.content_hash.as_deref() {
        Some(hash) => Promoted::already(tenant_id, hash, store).await?,
        None => {
            let finished = dam_media::ingest::finalize(
                store,
                tenant_id,
                &staging,
                declared.mime.as_deref(),
                session.declared_length,
                StorageClass::Standard,
                dam_media::ingest::Policy::default(),
            )
            .await
            .map_err(|error| match error {
                // `Refused` is the policy saying no — empty, oversized, an executable, or a size that
                // disagrees with what the client declared. None of those change on a retry, and `finalize` has
                // already destroyed the staged bytes, so retrying would only fail differently.
                dam_media::ingest::Error::Refused(reason) => {
                    Error::Permanent(format!("upload {upload_id} refused: {reason}"))
                }
                dam_media::ingest::Error::Store(store) => Error::Store(store),
            })?;

            // Recorded immediately, before anything else can fail. This is the whole point of the column.
            let mut conn = TenantConn::begin(global, slug).await?;
            dam_db::uploads::record_promotion(
                conn.executor(),
                tenant_id,
                upload_id,
                finished.digest.as_hex(),
            )
            .await?;
            conn.commit().await?;

            Promoted::from(finished)
        }
    };

    let content_hash = promoted.content_hash.clone();
    let size = promoted.size;
    let original = promoted.key.clone();

    // From the promoted object rather than from staging: on a re-run staging may already be gone, and the
    // promoted key is the one that will still be there.
    let probe = probe_header(store, &original, size).await;

    let declared_filename = declared
        .filename
        .clone()
        .unwrap_or_else(|| upload_id.to_owned());

    let mut conn = TenantConn::begin(global, slug).await?;
    let asset_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO assets \
         (id, content_hash, filename, ext, mime, bytes, width, height, orientation, color_space, \
          has_alpha, status, version_group_id, source) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'active', $1, 'ui')",
    )
    .bind(asset_id)
    .bind(&content_hash)
    .bind(&declared_filename)
    .bind(promoted.ext.as_deref())
    .bind(&promoted.mime)
    .bind(i64::try_from(size).unwrap_or(i64::MAX))
    .bind(probe.as_ref().and_then(|p| dimension(p.display_width)))
    .bind(probe.as_ref().and_then(|p| dimension(p.display_height)))
    .bind(
        probe
            .as_ref()
            .and_then(|p| p.orientation)
            .and_then(|v| i16::try_from(v).ok()),
    )
    .bind(probe.as_ref().and_then(|p| p.color_space.clone()))
    .bind(probe.as_ref().and_then(|p| p.has_alpha))
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    // The technical facts go in `asset_metadata.technical`, which is read-only and shaped by the file rather
    // than by the tenant's schema — which is why it is not merged into `values`.
    let technical = serde_json::json!({
        "sniffed_mime": promoted.mime,
        "declared_mime": declared.mime,
        "deduplicated": promoted.deduplicated,
        "declared_mismatch": promoted.declared_mismatch,
        "has_icc_profile": probe.as_ref().map(|p| p.has_icc_profile),
        "stored_width": probe.as_ref().and_then(|p| p.stored_width),
        "stored_height": probe.as_ref().and_then(|p| p.stored_height),
    });
    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values, technical) VALUES ($1, '{}'::jsonb, $2) \
         ON CONFLICT (asset_id) DO UPDATE SET technical = excluded.technical",
    )
    .bind(asset_id)
    .bind(&technical)
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    // The placement is what makes the tier derivable. Without it the asset reads as `hot` by default, which
    // happens to be right here and would be wrong the moment the lifecycle engine moved the object.
    sqlx::query(
        "INSERT INTO object_placements \
         (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state) \
         VALUES ($1, $2, $3, $4, $5, 'STANDARD', 'present') \
         ON CONFLICT (object_key, pool_id) DO NOTHING",
    )
    .bind(original.as_str())
    .bind(default_pool(global).await?)
    .bind(asset_id)
    .bind(i64::try_from(size).unwrap_or(i64::MAX))
    .bind(&content_hash)
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    // The session records which asset it became, which is what makes a re-run return rather than insert a
    // second asset for the same bytes.
    sqlx::query(
        "UPDATE upload_sessions SET asset_id = $3, status = 'completed', completed_at = now() \
         WHERE tenant_id = $1 AND upload_id = $2",
    )
    .bind(tenant_id)
    .bind(upload_id)
    .bind(asset_id)
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    conn.commit().await?;

    // Last, and only after the row is durable — see the module docs. A failure here leaves a staging object
    // for the reaper rather than an asset with no bytes.
    if let Err(error) = store.delete(&staging).await {
        tracing::warn!(%error, upload_id, "could not unstage a finalised upload; the reaper will");
    }

    Ok(Finalised {
        asset_id,
        content_hash,
        mime: promoted.mime,
        bytes: i64::try_from(size).unwrap_or(i64::MAX),
        created: true,
    })
}

/// The pool a new object is placed in.
///
/// The tenant's default hot pool. Resolved rather than hard-coded because `object_placements` is keyed
/// `(object_key, pool_id)` and a placement recorded against the wrong pool would make the lifecycle engine
/// reason about an object that is not where it thinks.
async fn default_pool(global: &sqlx::PgPool) -> Result<Uuid> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM dam_global.storage_pools \
         WHERE latency_class = 'instant' ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(global)
    .await
    .map_err(dam_db::Error::from)?;

    id.ok_or_else(|| {
        Error::Permanent(
            "no instant storage pool is configured; a placement cannot be recorded without one"
                .to_owned(),
        )
    })
}

/// A promoted object, however it got there.
///
/// One shape for both paths — a fresh promotion and a resumed one — so the recording code below cannot
/// accidentally depend on facts only the fresh path has. The resumed path genuinely knows less: there is no
/// staged object left to sniff, so the type comes from the stored asset's own extension rather than from the
/// bytes.
struct Promoted {
    content_hash: String,
    size: u64,
    key: Key,
    mime: String,
    ext: Option<String>,
    declared_mismatch: Option<String>,
    deduplicated: bool,
}

impl From<dam_media::ingest::Finalized> for Promoted {
    fn from(finished: dam_media::ingest::Finalized) -> Self {
        Self {
            content_hash: finished.digest.as_hex().to_owned(),
            size: finished.size,
            key: finished.key().clone(),
            mime: finished.sniffed.mime.clone(),
            ext: finished.sniffed.ext.clone(),
            declared_mismatch: finished.sniffed.declared_mismatch.clone(),
            deduplicated: finished.was_deduplicated(),
        }
    }
}

impl Promoted {
    /// Rebuilds the facts about an object a previous attempt already promoted.
    ///
    /// The type is re-sniffed from the promoted object rather than remembered, because remembering it would
    /// need a second column and the object is right there. One ranged read, the same prefix the fresh path
    /// uses.
    async fn already(
        tenant_id: Uuid,
        content_hash: &str,
        store: &dyn ResumableStore,
    ) -> Result<Self> {
        let key = Key::original(tenant_id, content_hash)?;
        let state = store.head(&key).await.map_err(|error| {
            // The session says the bytes were promoted and they are not there. Permanent: retrying cannot
            // conjure them, and saying so beats looping.
            Error::Permanent(format!(
                "upload was promoted to {} and the object is gone: {error}",
                key.as_str()
            ))
        })?;

        let end = PROBE_PREFIX.min(state.size).saturating_sub(1);
        let prefix = store
            .get(&key, Some(ByteRange::new(0, Some(end))))
            .await?
            .into_bytes(&key)?;
        let sniffed = dam_media::sniff::sniff(&prefix, None, None);

        Ok(Self {
            content_hash: content_hash.to_owned(),
            size: state.size,
            key,
            mime: sniffed.mime,
            ext: sniffed.ext,
            declared_mismatch: sniffed.declared_mismatch,
            // Unknowable on a resume, and reported as false rather than as a guess: the technical block says
            // what this run observed, and it observed nothing about the first promotion.
            deduplicated: false,
        })
    }
}

/// Dimensions from the object's header, or `None`.
///
/// A probe failure is not a finalisation failure: a DAM stores formats the pure-Rust probe cannot read, and
/// §18.2 routes those through libvips. Recording an asset with unknown dimensions is right; refusing an upload
/// because we could not measure it is not.
async fn probe_header(
    store: &dyn ResumableStore,
    key: &Key,
    size: u64,
) -> Option<dam_media::probe::Probe> {
    let end = PROBE_PREFIX.min(size).saturating_sub(1);
    let prefix = store
        .get(key, Some(ByteRange::new(0, Some(end))))
        .await
        .ok()?
        .into_bytes(key)
        .ok()?;
    dam_media::probe::image(&prefix).ok()
}

fn dimension(value: Option<u32>) -> Option<i32> {
    value.and_then(|v| i32::try_from(v).ok())
}

/// The content hash, mime and size of an asset that finalisation has already produced.
///
/// Read back rather than recomputed: a re-run must return the *same* answer as the first run, and
/// recomputing from the staging object it has since deleted would fail.
async fn existing_asset(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    asset_id: Uuid,
) -> Result<(String, String, i64)> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let row = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT content_hash, mime, bytes FROM assets WHERE id = $1",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    row.ok_or_else(|| {
        // The session says it became this asset and the asset is gone. Permanent: retrying cannot
        // reconcile it, and the honest response is to say so rather than to create a second asset.
        Error::Permanent(format!(
            "upload session names asset {asset_id}, which no longer exists"
        ))
    })
}
