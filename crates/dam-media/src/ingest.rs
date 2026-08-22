//! Finalising a staged upload: sniff, hash, promote.
//!
//! Every upload path — TUS, presigned, plain PUT, connector import — converges here, and the
//! presigned path is why it must. A presigned `PUT` hands the client a URL and steps out of
//! the way: S3 will not cap the size, will not constrain the type, and does not tell us what
//! arrived. Anything checked at mint time is therefore advisory. The checks that matter run
//! after the bytes have landed at a staging key and before they are promoted to a
//! content-addressed one, because that promotion is what makes an object real.
//!
//! Reading the object back to hash it is budgeted, not wasteful: §18.3 allows the original to
//! be read exactly twice — once to hash, once to derive. The read is ranged, so a 200 GB
//! master never materialises in memory.

use crate::sniff::{self, SNIFF_PREFIX, Sniffed};
use dam_core::StorageClass;
use dam_store::{
    BlobStore, ByteRange, Digest, Ingested, Key, StreamHasher,
    content::{self},
};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("upload refused: {0}")]
    Refused(String),

    #[error(transparent)]
    Store(#[from] dam_store::Error),
}

type Result<T> = std::result::Result<T, Error>;

/// What an upload is allowed to be.
///
/// A struct rather than constants because these differ by tenant and by plan, and because a
/// default that cannot be overridden becomes a support ticket. The defaults are the safe ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Refuse executables. On by default: an executable is never a creative asset, and one
    /// served from the customer's own asset domain is a malware distribution endpoint.
    pub refuse_executables: bool,
    /// Hard size cap, checked from `HeadObject` before a single byte is read.
    pub max_bytes: Option<u64>,
    /// Ranged-read window used for hashing. Bounds memory regardless of object size.
    pub read_chunk_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            refuse_executables: true,
            max_bytes: None,
            // 8 MiB: large enough that a 200 GB object is ~25k requests rather than 200k,
            // small enough to stay off the heap in any meaningful way.
            read_chunk_bytes: 8 * 1024 * 1024,
        }
    }
}

/// The result of finalising.
#[derive(Debug)]
pub struct Finalized {
    pub digest: Digest,
    pub size: u64,
    pub sniffed: Sniffed,
    pub ingested: Ingested,
}

impl Finalized {
    /// The content-addressed key the bytes now live at.
    pub fn key(&self) -> &Key {
        self.ingested.key()
    }

    pub fn was_deduplicated(&self) -> bool {
        self.ingested.was_deduplicated()
    }
}

/// Validates a staged object and promotes it to its content-addressed key.
///
/// `declared_mime` and `declared_size` are the client's claims. The MIME is compared and
/// recorded, never adopted. The size **is** enforced, because on the presigned path it is the
/// only cross-check available: the client asked for a URL for N bytes, and the object at the
/// key being a different size means either a truncated transfer or a client doing something
/// other than what it said.
pub async fn finalize<S: BlobStore + ?Sized>(
    store: &S,
    tenant: Uuid,
    staging: &Key,
    declared_mime: Option<&str>,
    declared_size: Option<u64>,
    class: StorageClass,
    policy: Policy,
) -> Result<Finalized> {
    let state = store.head(staging).await?;
    let size = state.size;

    if size == 0 {
        return Err(refuse(store, staging, "the staged object is empty".into()).await);
    }
    if let Some(max) = policy.max_bytes
        && size > max
    {
        // From HEAD alone, so an oversized upload costs no egress to reject.
        return Err(refuse(
            store,
            staging,
            format!("{size} bytes exceeds the {max}-byte limit"),
        )
        .await);
    }
    if let Some(declared) = declared_size
        && declared != size
    {
        return Err(refuse(
            store,
            staging,
            format!("the client declared {declared} bytes but {size} arrived"),
        )
        .await);
    }

    // Sniff from a ranged read of the head. One small request, whatever the object's size.
    let prefix_end = SNIFF_PREFIX.min(usize::try_from(size).unwrap_or(usize::MAX)) as u64;
    let prefix = store
        .get(staging, Some(ByteRange::new(0, Some(prefix_end - 1))))
        .await?
        .into_bytes(staging)?;
    let sniffed = sniff::sniff(&prefix, declared_mime, None);

    if policy.refuse_executables && sniffed.is_dangerous() {
        // The staged bytes are destroyed rather than left for the reaper: until it runs, a
        // refused executable stays retrievable at a key the uploader knows.
        return Err(refuse(
            store,
            staging,
            format!(
                "{} is an executable ({}); executables are not assets",
                staging, sniffed.mime
            ),
        )
        .await);
    }

    // Hash over the whole object, in bounded windows.
    let digest = hash_object(store, staging, size, policy.read_chunk_bytes).await?;

    let ingested = content::promote(store, tenant, staging, &digest, size, class).await?;
    Ok(Finalized {
        digest,
        size,
        sniffed,
        ingested,
    })
}

/// Streams the object through a hasher using ranged reads.
async fn hash_object<S: BlobStore + ?Sized>(
    store: &S,
    key: &Key,
    size: u64,
    window: u64,
) -> Result<Digest> {
    let window = window.max(64 * 1024);
    let mut hasher = StreamHasher::new();
    let mut offset = 0u64;
    while offset < size {
        let end = (offset + window).min(size) - 1;
        let chunk = store
            .get(key, Some(ByteRange::new(offset, Some(end))))
            .await?
            .into_bytes(key)?;
        if chunk.is_empty() {
            // A backend that returns nothing for a legal range would otherwise spin here
            // forever, and a hash over fewer bytes than the object would be silently wrong.
            return Err(Error::Refused(format!(
                "{key}: empty response for bytes={offset}-{end} of {size}"
            )));
        }
        hasher.update(&chunk);
        offset += chunk.len() as u64;
    }
    let (digest, hashed) = hasher.finish();
    if hashed != size {
        return Err(Error::Refused(format!(
            "{key}: hashed {hashed} bytes but the object is {size}"
        )));
    }
    Ok(digest)
}

/// Refuses an upload and destroys the staged bytes.
///
/// The delete is best-effort: a failure to clean up must not mask *why* the upload was
/// refused, which is the message the caller has to show a user.
async fn refuse<S: BlobStore + ?Sized>(store: &S, staging: &Key, reason: String) -> Error {
    if let Err(e) = store.delete(staging).await {
        tracing::warn!(key = %staging, error = %e, "could not remove refused staging object");
    }
    Error::Refused(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_refuses_executables() {
        // Asserted rather than assumed: this default is the difference between storing a file
        // and hosting malware, and a careless edit to `Default` would be invisible.
        assert!(Policy::default().refuse_executables);
        assert!(Policy::default().read_chunk_bytes >= 1024 * 1024);
    }

    #[test]
    fn a_refusal_reads_as_a_refusal_and_not_as_a_storage_fault() {
        // The API maps these to different statuses — 4xx against 5xx — so a caller must be
        // able to tell them apart without matching on message text.
        let refused = Error::Refused("nope".into());
        assert!(matches!(refused, Error::Refused(_)));
        let store: Error = dam_store::Error::NotFound("k".into()).into();
        assert!(matches!(store, Error::Store(_)));
    }
}
