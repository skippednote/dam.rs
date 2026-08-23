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
//! Each step is safe to repeat and none of them destroys the previous one's output until the next is durable.
//! The staging object is deleted last, after the asset row exists.
//!
//! That ordering is **defensive rather than load-bearing**, and saying so is the point. What actually makes a
//! crash between the promotion and the insert recoverable is `upload_sessions.content_hash`, written the moment
//! the promotion succeeds: a re-run reads it, skips the promotion and records the asset, whether or not staging
//! survived. A mutation that unstages *before* the commit survives the test suite for exactly that reason — and
//! an earlier version of this comment claimed the ordering was what prevented an orphan. It is not; the digest
//! column is. The ordering costs nothing and still helps for a failure that lands before the digest is
//! recorded, so it stays.
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
    scanner: Option<&dam_media::antivirus::Scanner>,
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
            // Scanned before promotion, so infected bytes never reach a content-addressed key and never
            // become an asset. The alternative — promote, then quarantine the asset row — leaves the object
            // in the library's own namespace and makes "is this asset safe" a question about a column.
            //
            // Skipped for the already-promoted branch above: those bytes were scanned by the attempt that
            // promoted them, and re-reading a 200 GB master to re-scan it on every retry would make a
            // transient failure expensive.
            scan(store, &staging, upload_id, scanner).await?;

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
    //
    // One read, two readers: the probe wants dimensions and `embedded` wants EXIF/XMP, and both live in the
    // same leading bytes. Fetching the prefix twice would double the egress on every upload for nothing.
    let header = read_header(store, &original, size).await;
    let probe = header
        .as_deref()
        .and_then(|bytes| dam_media::probe::image(bytes).ok());

    let declared_filename = declared
        .filename
        .clone()
        .unwrap_or_else(|| upload_id.to_owned());

    let mut conn = TenantConn::begin(global, slug).await?;
    let asset_id = Uuid::new_v4();

    // The profile this upload was made under, if the tenant has any (Q.3). It carries the answers the intake
    // already knows: which form, what metadata is true of everything from this source, and whether machine
    // tagging is permitted at all.
    let profile =
        dam_db::upload_profiles::for_upload_on(conn.executor(), declared.upload_profile_id)
            .await
            .map_err(|refusal| dam_db::Error::Migrate(refusal.to_string()))?;

    // Resolved on this connection, inside the same transaction as the insert: a type created or
    // re-pointed between the two would otherwise put the asset on a form that no longer matches its class.
    //
    // The profile's choice wins over the mime's class, because a profile is an explicit statement about an
    // intake and the class is a guess. A profile that names no type falls through to the guess.
    let metadata_type_id = match profile.as_ref().and_then(|p| p.metadata_type_id) {
        Some(from_profile) => Some(from_profile),
        None => dam_db::metadata_types::for_mime_on(conn.executor(), &promoted.mime)
            .await
            .map_err(|refusal| dam_db::Error::Migrate(refusal.to_string()))?
            .map(|chosen| chosen.id),
    };

    sqlx::query(
        // `metadata_type_id` comes from the mime's media class (Q.1), chosen here rather than left null so
        // an asset arrives with the form it should have. Null is still valid and still resolves — see
        // `metadata_types::fields_for` — but leaving every asset to the fallback would make types something
        // an administrator has to apply by hand to a library that already exists.
        "INSERT INTO assets \
         (id, content_hash, filename, ext, mime, bytes, width, height, orientation, color_space, \
          has_alpha, status, version_group_id, source, metadata_type_id, upload_profile_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'active', $1, 'ui', $12, $13)",
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
    .bind(metadata_type_id)
    // On the asset as well as the session, because enrichment runs long after the reaper takes the session
    // row and "was this allowed to be machine-tagged" has to still be answerable then.
    .bind(profile.as_ref().map(|p| p.id))
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    // Content credentials, read before anything transforms the bytes (D13, G1).
    //
    // On the *original*, and on the whole object rather than the header window: a C2PA manifest lives in a
    // JUMBF box whose position depends on the format, and reading a prefix would report `absent` for a
    // perfectly good credential sitting past it — which is the worst available answer, because absence is
    // indistinguishable from "we did not look".
    //
    // Recorded whatever the verdict, including `invalid`. D13 prohibits stripping and a broken chain is the
    // customer's evidence of what broke; discarding it would destroy the only artefact that says so.
    if let Err(error) = record_provenance(
        global,
        store,
        slug,
        tenant_id,
        asset_id,
        &promoted.mime,
        &original,
    )
    .await
    {
        // Logged, not fatal. A credential we could not read is a fact about the file, and refusing the upload
        // over it would make every malformed manifest in the world a reason a photograph cannot be filed.
        // `provenance_gaps` is the view that finds these afterwards.
        tracing::warn!(%error, %asset_id, "could not record content credentials");
    }

    // Everything the file says about itself, kept whether or not this tenant maps any of it.
    //
    // Auto-import is a *projection*: a tag reaches `values` only where a mapping names a field for it (Q.4).
    // That is the right rule for the tenant's own schema — a library with no `lens` field should not grow one
    // because a camera wrote a tag — but reading twenty-two tags and keeping four was lossy in a way nothing
    // could recover from. The bytes are in cold storage and a mapping added next month had nothing to apply
    // itself to; the answer to "does this photo know where it was taken" was "re-download the original and
    // find out".
    //
    // So the whole extracted set is kept here, next to the other read-only facts, and mapping stays a
    // projection over something durable. This is also what makes a mapping added later backfillable without
    // touching object storage.
    let embedded = header
        .as_deref()
        .map(dam_media::embedded::read)
        .unwrap_or_default();

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
        // Nested rather than flattened, so a tag named `sniffed_mime` by some future camera cannot shadow the
        // fact above it, and so a reader can tell "the file claims this" from "we measured this".
        "embedded": embedded,
    });
    // The profile's defaults become the asset's starting metadata (Q.3), validated on the way in — the same
    // validator a human's edit goes through, so a default cannot write a read-only field or a value of the
    // wrong kind. Re-validated here rather than trusted from save time because a field definition can have
    // changed since, and a default that has quietly become invalid must fail visibly.
    //
    // `Permanent`, not transient: the profile is misconfigured, and retrying the upload will fail identically
    // until somebody fixes it. A transient error would retry forever and say nothing.
    // Auto-import runs *before* the defaults, and the order is the whole design (Q.4).
    //
    // A profile default is a blanket statement about an intake — "everything from the press pickup is credited
    // to the agency" — while an embedded value is what this particular file says about itself. Applying the
    // defaults first would make every one of them a held value, and a mapping's `overwrite: false` would then
    // read it as "a person put this here" and decline: one blanket default would silently defeat the import on
    // every asset from that source. Running the import first keeps the profile doing exactly what it promises,
    // which is to fill what is *not* otherwise known.
    let imported = if embedded.is_empty() {
        serde_json::Map::new()
    } else {
        let plan =
            dam_db::auto_import::plan_on(conn.executor(), &embedded, &serde_json::Map::new())
                .await
                .map_err(|refusal| dam_db::Error::Migrate(refusal.to_string()))?;
        // Logged, not fatal: a file whose tag will not fit its field is the tenant's configuration to fix,
        // and failing the upload over it would strand bytes somebody is waiting on. Silence is the thing to
        // avoid — a mapping that never produces anything should be findable.
        for rejection in &plan.rejected {
            tracing::warn!(
                %asset_id,
                field = %rejection.key,
                code = %rejection.code,
                detail = %rejection.detail,
                "an auto-imported value did not fit its field",
            );
        }
        plan.values
    };

    let defaults = match profile.as_ref() {
        Some(profile) => {
            dam_db::upload_profiles::apply_defaults_on(conn.executor(), profile, &imported)
                .await
                .map_err(|refusal| {
                    Error::Permanent(format!(
                        "upload profile {} cannot be applied: {refusal}",
                        profile.key
                    ))
                })?
        }
        None => imported,
    };

    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values, technical) VALUES ($1, $3, $2) \
         ON CONFLICT (asset_id) DO UPDATE SET technical = excluded.technical",
    )
    .bind(asset_id)
    .bind(&technical)
    .bind(serde_json::Value::Object(defaults))
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    // The feed entry (Q.7). Recorded inside the same transaction as the asset, so a feed cannot show an upload
    // that was rolled back — and a failure here fails the finalisation, which is the right trade *only* because it
    // is the same transaction: outside one, an upload that succeeded and then failed to log would be reported as a
    // failure despite having worked.
    dam_db::events::record(
        conn.executor(),
        dam_db::events::NewEvent {
            kind: dam_db::events::Kind::Uploaded,
            asset_id: Some(asset_id),
            // From the session rather than invented: an upload made through a service credential has no person,
            // and `system` is the honest actor for it.
            actor_id: declared.created_by,
            actor_kind: declared
                .created_by
                .map_or(dam_db::events::ActorKind::System, |_| {
                    dam_db::events::ActorKind::User
                }),
            context: serde_json::json!({
                "filename": declared_filename,
                "mime": promoted.mime,
                "deduplicated": promoted.deduplicated,
            }),
            bytes: Some(i64::try_from(size).unwrap_or(i64::MAX)),
        },
    )
    .await?;

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

    // Content credentials (D13, G1), after the commit that creates the asset row.
    //
    // **After**, deliberately: `provenance_manifests.asset_id` is a foreign key and the asset is inserted
    // inside the transaction above, so recording before the commit meant a separate connection could not see
    // the row it referenced. That surfaced as an FK violation on the very first credentialed upload — visible
    // only because this is logged rather than fatal.
    //
    // Read from the *original*, and from the whole object rather than the header window: a C2PA manifest
    // lives in a JUMBF box whose position depends on the format, so reading a prefix would report `absent`
    // for a perfectly good credential sitting past it. Absence is indistinguishable from "we did not look",
    // which makes it the worst available wrong answer.
    //
    // Recorded whatever the verdict, `invalid` included. D13 prohibits stripping, and a broken chain is the
    // customer's evidence of what broke.
    if let Err(error) = record_provenance(
        global,
        store,
        slug,
        tenant_id,
        asset_id,
        &promoted.mime,
        &original,
    )
    .await
    {
        // Logged, not fatal. A credential that cannot be read is a fact about the file; refusing the upload
        // over it would make every malformed manifest in the world a reason a photograph cannot be filed.
        // `provenance_gaps` is the view that finds these afterwards.
        tracing::warn!(%error, %asset_id, "could not record content credentials");
    }

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

/// Verifies the original's content credentials and records what it found.
///
/// Three things land: the manifest as its own object under a tier-exempt key, a `provenance_manifests` row
/// with the validation state and codes, and `assets.provenance_state` plus `had_inbound_manifest`.
///
/// The manifest is stored separately from the asset deliberately. §2 keeps metadata hot while masters tier to
/// Deep Archive, so a credential that lived only inside the original's bytes would become unverifiable the
/// moment the original was archived — and "unverifiable because we filed it somewhere slow" is not a
/// provenance story anybody can use.
async fn record_provenance(
    global: &sqlx::PgPool,
    store: &dyn ResumableStore,
    slug: &dam_core::TenantSlug,
    tenant_id: Uuid,
    asset_id: Uuid,
    mime: &str,
    original: &dam_store::Key,
) -> Result<()> {
    let bytes = match store.get(original, None).await? {
        dam_store::GetOutcome::Bytes(bytes) => bytes,
        // Freshly promoted, so this is unreachable rather than a case to handle — but reading it as "no
        // credential" would record an absence that was never checked.
        dam_store::GetOutcome::NotAvailable(ticket) => {
            return Err(Error::Transient(format!(
                "cannot read the original to verify credentials: it is {}",
                ticket.class
            )));
        }
    };

    let verified = dam_media::provenance::verify(mime, &bytes)
        .map_err(|error| Error::Permanent(format!("verifying content credentials: {error}")))?;

    // Nothing to record and nothing to store. `provenance_state` already defaults to `none`, and writing a
    // row saying "there was no manifest" for every ordinary photograph would bury the interesting rows.
    if verified.manifest.is_none() && verified.state == dam_core::rights::ProvenanceState::None {
        return Ok(());
    }

    let object_key = match &verified.manifest {
        Some(manifest) => {
            let key = dam_store::Key::manifest(tenant_id, &content_hash_of(original))
                .map_err(|error| Error::Permanent(format!("manifest key: {error}")))?;
            store
                .put(
                    &key,
                    bytes::Bytes::from(manifest.clone()),
                    StorageClass::Standard,
                )
                .await?;
            key.as_str().to_owned()
        }
        // A state without a manifest is `invalid` with nothing extractable. The row still matters — it is the
        // tamper signal — so it is recorded with an empty key rather than dropped.
        None => String::new(),
    };

    let mut conn = TenantConn::begin(global, slug).await?;
    dam_db::provenance::record_inbound(
        conn.executor(),
        asset_id,
        &dam_db::provenance::NewManifest {
            object_key: &object_key,
            bytes: verified.manifest.as_ref().map_or(0, |m| m.len() as i64),
            validation_state: verified.state.as_validation_state(),
            validation_detail: serde_json::json!({
                "codes": verified.detail,
                "source_types": verified.source_types,
                "ingredients": verified.ingredient_count,
            }),
            signer_cn: verified.signer_cn.as_deref(),
            claim_generator: verified.claim_generator.as_deref(),
            spec_version: verified.spec_version.as_deref(),
            captured_at: None,
            actions: verified.actions.clone(),
        },
    )
    .await?;
    conn.commit().await?;

    tracing::info!(
        %asset_id,
        state = verified.state.as_validation_state(),
        ingredients = verified.ingredient_count,
        "content credentials recorded",
    );
    Ok(())
}

/// The content hash out of an original's key.
///
/// The key *is* `<tenant>/o/<aa>/<bb>/<hash>`, so the hash is its last segment. Taken from the key rather
/// than threaded through as an argument because the key is the thing that cannot be wrong: it was derived
/// from the digest at promotion.
fn content_hash_of(original: &dam_store::Key) -> String {
    original
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Scans a staged upload, refusing anything the scanner objects to.
///
/// Three outcomes, three different kinds of failure:
///
/// - **Infected** is permanent. The signature does not change on a retry, and the bytes must not be promoted.
/// - **Unreachable** is transient. The upload stays in staging and finalises when `clamd` returns; failing
///   open here would be a security control that switches itself off during an outage.
/// - **Unintelligible** is permanent too, and deliberately not treated as clean: a reply this code cannot
///   read means a version or configuration mismatch, and guessing "clean" would turn a protocol change into a
///   silent bypass.
///
/// No scanner configured is a no-op. That is the default, because requiring `clamd` on every developer
/// machine to accept an upload would make the dev stack a three-container affair — but it means a deployment
/// that never sets it never scans anything, which is why `docker/DEPLOY.md` lists it as required rather than
/// optional.
async fn scan(
    store: &dyn ResumableStore,
    staging: &dam_store::Key,
    upload_id: &str,
    scanner: Option<&dam_media::antivirus::Scanner>,
) -> Result<()> {
    let Some(scanner) = scanner else {
        return Ok(());
    };
    let bytes = match store.get(staging, None).await? {
        dam_store::GetOutcome::Bytes(bytes) => bytes,
        // A staged object is minutes old and in the hot pool by construction, so this is unreachable rather
        // than a case to handle — but reading it as "nothing to scan" would be a bypass.
        dam_store::GetOutcome::NotAvailable(ticket) => {
            return Err(Error::Transient(format!(
                "upload {upload_id} cannot be scanned: staging object is {}",
                ticket.class
            )));
        }
    };

    match scanner.scan(&bytes).await {
        Ok(dam_media::antivirus::Verdict::Clean) => Ok(()),
        Ok(dam_media::antivirus::Verdict::Infected(signature)) => {
            tracing::warn!(%upload_id, %signature, "upload refused: infected");
            Err(Error::Permanent(format!(
                "upload {upload_id} refused: {signature}"
            )))
        }
        Ok(dam_media::antivirus::Verdict::TooLarge { bytes, limit }) => {
            // Accepted, and said out loud. A DAM cannot refuse every video master, and a silent skip is how
            // an operator comes to believe everything is scanned.
            tracing::warn!(
                %upload_id,
                bytes,
                limit,
                "upload accepted WITHOUT a virus scan: larger than the scanner accepts",
            );
            Ok(())
        }
        Err(error @ dam_media::antivirus::Error::Unreachable { .. }) => {
            Err(Error::Transient(error.to_string()))
        }
        Err(error @ dam_media::antivirus::Error::Unintelligible(_)) => {
            Err(Error::Permanent(error.to_string()))
        }
    }
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

/// The object's leading bytes, or `None` when they cannot be read.
///
/// A read failure is not a finalisation failure: a DAM stores formats the pure-Rust probe cannot measure and
/// files that carry no embedded metadata at all, and §18.2 routes the former through libvips. Recording an asset
/// with unknown dimensions is right; refusing an upload because we could not inspect it is not.
async fn read_header(store: &dyn ResumableStore, key: &Key, size: u64) -> Option<Vec<u8>> {
    let end = PROBE_PREFIX.min(size).saturating_sub(1);
    store
        .get(key, Some(ByteRange::new(0, Some(end))))
        .await
        .ok()?
        .into_bytes(key)
        .ok()
        .map(Into::into)
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
