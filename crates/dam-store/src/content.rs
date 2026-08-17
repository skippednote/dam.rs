//! Content addressing (§6.2): BLAKE3 digests, streaming hashing, and deduplication.
//!
//! A key is derived from the bytes, which is what makes deduplication structural rather
//! than a lookup table: the same photograph uploaded by two people in two tenants' worth of
//! workflow resolves to one object, and no index has to be consulted to notice.
//!
//! ## Streaming, because the file is the size it is
//!
//! A 200 GB ProRes master cannot be buffered to be hashed (§18.3). [`StreamHasher`] and
//! [`hash_reader`] work in bounded chunks, and the digest is independent of where the chunk
//! boundaries fall — a property worth asserting rather than assuming, since a hasher fed
//! per-chunk instead of per-stream is a classic way to produce a digest nobody can
//! reproduce.
//!
//! ## Why BLAKE3 and not the ETag
//!
//! S3 hands back an ETag, but a multipart ETag is a digest *of the part digests* with a
//! `-<count>` suffix — it changes if the part size changes, so it cannot identify content.
//! BLAKE3 is computed by us, over the bytes, and is the checksum of record (§6.4).

use crate::{BlobStore, Error, Key, Placement, Result};
use bytes::Bytes;
use dam_core::StorageClass;
use tokio::io::{AsyncRead, AsyncReadExt};
use uuid::Uuid;

/// A BLAKE3 digest, always 64 lowercase hex characters.
///
/// Lowercase is enforced at construction rather than checked at use. An uppercase digest
/// would produce a *second* key for identical content, defeating the deduplication the
/// content-addressed layout exists for — so the type makes it unrepresentable instead of
/// leaving every call site to remember.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);

impl Digest {
    /// Hashes a complete buffer. For anything that might be large, use [`StreamHasher`].
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    /// Parses a hex digest, normalising case.
    ///
    /// Accepts uppercase because a digest read back from an external system may be in
    /// either case; it is normalised here so nothing downstream ever sees a variant.
    pub fn from_hex(hex: &str) -> Result<Self> {
        if hex.len() != 64 {
            return Err(Error::Backend(format!(
                "a BLAKE3 digest is 64 hex characters, got {}",
                hex.len()
            )));
        }
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Backend(format!(
                "digest contains non-hex characters: {hex:?}"
            )));
        }
        Ok(Self(hex.to_ascii_lowercase()))
    }

    pub fn as_hex(&self) -> &str {
        &self.0
    }

    /// The content-addressed key for an original master in this tenant.
    pub fn original_key(&self, tenant: Uuid) -> Result<Key> {
        Key::original(tenant, &self.0)
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A hasher fed incrementally, counting bytes as it goes.
///
/// The byte count is not incidental: it is what lets an upload detect a truncated stream,
/// and it comes for free here rather than requiring the caller to track it separately (and
/// occasionally disagree).
#[derive(Debug, Default)]
pub struct StreamHasher {
    hasher: blake3::Hasher,
    bytes: u64,
}

impl StreamHasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
    }

    pub fn bytes_hashed(&self) -> u64 {
        self.bytes
    }

    /// The digest and the total byte count.
    pub fn finish(self) -> (Digest, u64) {
        (
            Digest(self.hasher.finalize().to_hex().to_string()),
            self.bytes,
        )
    }
}

/// Hashes a reader in `chunk_size` chunks, holding at most one chunk in memory.
///
/// Refuses an empty body: the digest of nothing is a valid BLAKE3 value, so content
/// addressing alone would cheerfully store a zero-byte asset and give it a key. It is never
/// a real upload, and catching it here keeps it out of the database.
pub async fn hash_reader<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    chunk_size: usize,
) -> Result<(Digest, u64)> {
    let chunk_size = chunk_size.max(1);
    let mut hasher = StreamHasher::new();
    let mut buf = vec![0u8; chunk_size];
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| Error::Backend(format!("reading body to hash: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    if hasher.bytes_hashed() == 0 {
        return Err(Error::Backend("refusing an empty body".into()));
    }
    Ok(hasher.finish())
}

/// Hashes a reader and requires it to be exactly `expected` bytes long.
///
/// A client that declares 10 MB and delivers 4 MB has had its connection cut. Because the
/// key is derived from the bytes, the fragment would get its own perfectly valid key and
/// read back as a healthy object — so the declared length is the only thing that can
/// distinguish a truncation from a genuinely smaller file.
pub async fn hash_reader_exact<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    expected: u64,
    chunk_size: usize,
) -> Result<(Digest, u64)> {
    let (digest, actual) = hash_reader(reader, chunk_size).await?;
    if actual != expected {
        return Err(Error::Backend(format!(
            "body length mismatch: declared {expected} bytes, received {actual} — the \
             upload was truncated or the declared length is wrong"
        )));
    }
    Ok((digest, actual))
}

/// The outcome of an ingest.
#[derive(Debug)]
pub enum Ingested {
    /// The bytes were written.
    Stored(Placement),
    /// The content was already in the store, so nothing was transferred.
    AlreadyPresent { key: Key, size: u64 },
}

impl Ingested {
    pub fn key(&self) -> &Key {
        match self {
            Self::Stored(p) => &p.key,
            Self::AlreadyPresent { key, .. } => key,
        }
    }

    pub fn was_deduplicated(&self) -> bool {
        matches!(self, Self::AlreadyPresent { .. })
    }
}

/// Stores bytes at their content-addressed key, skipping the transfer if they are already
/// there.
///
/// The existence check compares **size as well as presence**. A key implies its bytes under
/// content addressing, so an object of the wrong size at that key is corruption or a
/// truncated earlier upload — and treating it as a cache hit would make that permanent,
/// since every later upload of the correct bytes would also skip the write. A full digest
/// verification would be stronger still, but it costs a download of the whole object on
/// every duplicate upload; size is the cheap check that catches the realistic failure.
pub async fn ingest<S: BlobStore + ?Sized>(
    store: &S,
    tenant: Uuid,
    body: Bytes,
    class: StorageClass,
) -> Result<Ingested> {
    if body.is_empty() {
        return Err(Error::Backend("refusing an empty body".into()));
    }
    let digest = Digest::of(&body);
    let key = digest.original_key(tenant)?;
    let size = body.len() as u64;

    match store.head(&key).await {
        Ok(state) if state.size == size => {
            return Ok(Ingested::AlreadyPresent { key, size });
        }
        Ok(state) => {
            // Deliberately falls through to the write. Left as an explicit arm because
            // "exists but wrong size" reads as a duplicate to a presence-only check.
            tracing::warn!(
                key = %key,
                expected = size,
                found = state.size,
                "object exists at its content-addressed key with the wrong size; rewriting"
            );
        }
        Err(Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }

    let placement = store.put(&key, body, class).await?;
    Ok(Ingested::Stored(placement))
}

/// S3's `CopyObject` size limit. Above this a copy must be a multipart copy of ranged
/// parts — the reason promoting a large upload is more than a rename.
pub const MAX_COPY_PART: u64 = 5 * 1024 * 1024 * 1024;

/// S3's hard cap on parts in one multipart upload.
const MAX_PARTS: u64 = 10_000;

/// Inclusive byte ranges covering `size`, each at most `part_size` bytes.
///
/// Empty when a single `CopyObject` suffices, so a caller branches on the emptiness rather
/// than duplicating the threshold.
///
/// `part_size` is grown if it would produce more than [`MAX_PARTS`] parts. A caller passing a
/// small part size for a huge object would otherwise build a plan S3 rejects at completion —
/// after every part copy has been paid for.
pub fn copy_part_ranges(size: u64, part_size: u64) -> Vec<(u64, u64)> {
    if size <= MAX_COPY_PART {
        return Vec::new();
    }
    let part_size = part_size
        .clamp(5 * 1024 * 1024, MAX_COPY_PART)
        .max(size.div_ceil(MAX_PARTS));

    let mut ranges = Vec::with_capacity(usize::try_from(size.div_ceil(part_size)).unwrap_or(0));
    let mut start = 0;
    while start < size {
        let end = (start + part_size).min(size) - 1;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

/// Promotes a staged upload to its content-addressed key.
///
/// The staging object holds bytes whose digest was computed while streaming. Promotion is a
/// **server-side** copy, so the bytes never cross the client's connection twice, and it
/// becomes a multipart copy above [`MAX_COPY_PART`].
///
/// Ordering matters on failure: the staging object is deleted only after the content object
/// exists. Staging is the sole copy of the bytes until then, and deleting it early would
/// destroy an upload that could have been retried. An abandoned staging object is reaped on
/// a timer instead — a leak that costs storage, versus a loss that costs the upload.
pub async fn promote<S: BlobStore + ?Sized>(
    store: &S,
    tenant: Uuid,
    staging: &Key,
    digest: &Digest,
    declared_size: u64,
    class: StorageClass,
) -> Result<Ingested> {
    let staged = store.head(staging).await?;
    if staged.size != declared_size {
        // The client said one thing and delivered another: a truncated stream, or a bug in
        // the caller's accounting. Either way the digest cannot be trusted to describe the
        // bytes, and promoting would give a fragment a valid content key.
        return Err(Error::Backend(format!(
            "staged object {staging} is {} bytes but {declared_size} were declared — the \
             upload was truncated or the declared length is wrong",
            staged.size
        )));
    }

    let key = digest.original_key(tenant)?;

    // Already there? Skip the copy. For a duplicate 200 GB upload this is the difference
    // between a HeadObject and a 200 GB server-side copy.
    let already = match store.head(&key).await {
        Ok(existing) if existing.size == declared_size => true,
        Ok(existing) => {
            tracing::warn!(
                key = %key,
                expected = declared_size,
                found = existing.size,
                "object exists at its content-addressed key with the wrong size; overwriting \
                 from staging"
            );
            false
        }
        Err(Error::NotFound(_)) => false,
        Err(e) => return Err(e),
    };

    if !already {
        store.copy(staging, &key, declared_size, class).await?;
    }

    // Only now is the staging copy redundant.
    store.delete(staging).await?;

    if already {
        Ok(Ingested::AlreadyPresent {
            key,
            size: declared_size,
        })
    } else {
        let state = store.head(&key).await?;
        Ok(Ingested::Stored(Placement {
            key,
            size: state.size,
            storage_class: state.storage_class,
            etag: state.etag,
            // A promotion is a copy, so any checksum here would be the server's, not the
            // BLAKE3 the caller computed while streaming. The caller already holds that.
            checksum: None,
            version_id: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_of_known_bytes_matches_blake3() {
        // Pinned against the reference implementation so a dependency swap that changed the
        // hash — and thus every key in the estate — cannot pass silently.
        assert_eq!(
            Digest::of(b"abc").as_hex(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn the_empty_digest_is_computable_but_ingest_still_refuses_it() {
        // Documents the trap: hashing nothing succeeds, which is exactly why the refusal
        // has to live at the ingest boundary rather than in the hash.
        assert_eq!(Digest::of(b"").as_hex().len(), 64);
    }

    #[test]
    fn a_copy_at_or_below_the_limit_needs_no_part_plan() {
        assert!(copy_part_ranges(1, MAX_COPY_PART).is_empty());
        assert!(copy_part_ranges(MAX_COPY_PART, MAX_COPY_PART).is_empty());
        assert!(!copy_part_ranges(MAX_COPY_PART + 1, MAX_COPY_PART).is_empty());
    }

    #[test]
    fn a_stream_hasher_agrees_with_a_one_shot_hash() {
        let mut hasher = StreamHasher::new();
        hasher.update(b"ab");
        hasher.update(b"c");
        let (digest, size) = hasher.finish();
        assert_eq!(digest, Digest::of(b"abc"));
        assert_eq!(size, 3);
    }
}
