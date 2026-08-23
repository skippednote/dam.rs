//! The similarity pass: hash an asset, colour it, and queue anything it looks like (M4, §8.1).
//!
//! ## It reads the proxy, never the original
//!
//! §8 is explicit: "everything reads the master proxy, so a fully archived library needs zero restores to be
//! tagged, embedded, searched, or re-processed". That matters more here than for enrichment, because this is
//! the pass most likely to be run over a whole library at once — a backfill that touched originals would issue
//! a restore per asset and bill for it.
//!
//! ## Both writes and the candidate search are one transaction
//!
//! The hashes, the colours and the candidate rows go in together. Half-applied would leave an asset with a
//! hash and no colours, and the next pass would see the hash, skip it as done, and never colour it.
//!
//! ## No model, and therefore no configuration
//!
//! Unlike `enrich`, this needs no credential, no budget check and no tenant opt-in. It is arithmetic over
//! bytes the library already has, so it runs for every asset — which is why it belongs on the derive path
//! rather than behind a settings flag.

use crate::{Error, Result};
use dam_core::TenantSlug;
use dam_db::TenantConn;
use dam_media::similarity as media;
use dam_store::{BlobStore, Key};
use uuid::Uuid;

/// What one pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Analysed {
    pub colours: usize,
    /// Candidate pairs newly queued. Excludes any a human already resolved.
    pub candidates: u64,
}

/// Hashes and colours one asset, then queues near-duplicates.
///
/// Skips rather than fails when there is no proxy yet or the proxy is not an image. A library is full of files
/// nothing can look at, and that is not a broken asset — the same reasoning `enrich` uses.
pub async fn analyse(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &TenantSlug,
    asset_id: Uuid,
) -> Result<Option<Analysed>> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let proxy = dam_db::derivatives::current_proxy(conn.executor(), asset_id).await?;
    conn.commit().await?;

    let Some(proxy) = proxy else {
        // Transient: a proxy appears when the derive job finishes, and this job is normally queued behind it.
        return Err(Error::Transient(format!(
            "asset {asset_id} has no proxy to hash yet"
        )));
    };
    if !proxy.mime.starts_with("image/") {
        return Ok(None);
    }

    // The stored key, not one recomputed from the profile: a redefined profile changes the op hash, and
    // recomputing would fetch an object that may not exist while the row points at one that does.
    let key = Key::new(proxy.object_key.clone())?;
    let bytes = store.get(&key, None).await?.into_bytes(&key)?;

    // One decode for both passes, inside `dam-media` — decoding is the expensive part, and doing it twice
    // would double the cost of the job that runs over every asset in the library. Permanent on failure: a
    // proxy that will not decode will not decode on a retry either, and the derive pass wrote it.
    let media::Analysis { hashes, colours } = media::analyse(&bytes)
        .map_err(|error| Error::Permanent(format!("analysing the proxy of {asset_id}: {error}")))?;

    let mut conn = TenantConn::begin(global, slug).await?;
    let stored = dam_db::similarity::Hashes {
        phash: hashes.phash,
        dhash: hashes.dhash,
    };
    dam_db::similarity::record_hashes(conn.executor(), asset_id, stored).await?;
    dam_db::similarity::record_colours(
        conn.executor(),
        asset_id,
        &colours
            .iter()
            .map(|colour| dam_db::similarity::Colour {
                hex: colour.hex.clone(),
                lab: colour.lab,
                coverage: colour.coverage,
                palette_bucket: colour.palette_bucket.clone(),
            })
            .collect::<Vec<_>>(),
    )
    .await?;

    // Searched *after* this asset's own hash is written, so a pair found here is symmetric: whichever of the
    // two is processed second finds the first, and the ordered insert makes it one row either way.
    let found = dam_db::similarity::near(
        conn.executor(),
        asset_id,
        stored,
        media::NEAR_DUPLICATE_DISTANCE,
    )
    .await?;
    let candidates =
        dam_db::similarity::record_candidates(conn.executor(), asset_id, &found).await?;
    conn.commit().await?;

    Ok(Some(Analysed {
        colours: colours.len(),
        candidates,
    }))
}
