//! Resumable uploads: the engine behind TUS.
//!
//! ## The constraint
//!
//! A TUS client PATCHes chunks of whatever size it likes — 64 KB is a common default — and S3
//! refuses a multipart part below 5 MiB except the last. Chunks therefore cannot map onto
//! parts. They accumulate into a **tail**, and the tail becomes a part once it is large
//! enough.
//!
//! ## The tail lives in object storage
//!
//! Not in the process. That is the difference between a resumable upload and a sticky-session
//! one: a client reconnecting to a different node has to be able to continue, and a node that
//! dies must not take the upload with it. The cost is one small read and one small write per
//! sub-minimum chunk, which is the right trade for an upload measured in minutes.
//!
//! ## State is the caller's
//!
//! [`ResumableSession`] is a value the caller persists (a row) and hands back. Nothing here
//! keeps a map of live uploads, so any node can serve any PATCH and a restart loses nothing.
//! Each call mutates the session; the caller writes it back.
//!
//! ## What is deliberately not here
//!
//! No HTTP, no header parsing, no expiry policy. The TUS surface maps onto these four
//! functions, and keeping the rules out of the handler is what makes them testable against a
//! real server.

use crate::{BlobStore, Error, Key, MIN_PART_SIZE, Result};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use dam_core::StorageClass;
use uuid::Uuid;

/// Multipart primitives driven across requests.
///
/// Separate from [`BlobStore`], which is deliberately narrow and documents multipart as
/// sitting *above* it. [`crate::MultipartUpload`] owns its part list and borrows the store,
/// which is right for a single-pass upload and wrong here: a TUS upload spans many HTTP
/// requests and many processes, so the part list lives in the session row and every call is
/// stateless.
#[async_trait]
pub trait ResumableStore: BlobStore {
    /// Opens a multipart upload, returning the backend's id.
    async fn begin_resumable(&self, key: &Key, class: StorageClass) -> Result<String>;

    /// Uploads one part, returning its ETag for the completion list.
    async fn upload_resumable_part(
        &self,
        key: &Key,
        upload_id: &str,
        part_number: i32,
        body: Bytes,
    ) -> Result<String>;

    /// Assembles the parts, in the order given.
    async fn finish_resumable(
        &self,
        key: &Key,
        upload_id: &str,
        parts: &[PartRecord],
    ) -> Result<()>;

    /// Discards the upload and every part. Idempotent.
    async fn abort_resumable(&self, key: &Key, upload_id: &str) -> Result<()>;
}

/// One recorded part of the underlying multipart upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRecord {
    pub number: i32,
    pub etag: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Completed,
    Terminated,
}

/// Everything needed to resume an upload, and nothing else.
///
/// Every field is what a database row would hold. There is no handle to an open connection or
/// an in-memory buffer, which is what makes a hand-off between nodes work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumableSession {
    /// Ours, not the client's — it becomes part of an object key.
    pub id: String,
    pub tenant: Uuid,
    /// `None` for TUS `Upload-Defer-Length`, where the client does not know the size yet.
    pub declared_length: Option<u64>,
    /// Bytes accepted so far. The authoritative answer to a TUS `HEAD`.
    pub offset: u64,
    /// The backend's multipart upload id, created lazily — a small upload never opens one.
    pub s3_upload_id: Option<String>,
    pub parts: Vec<PartRecord>,
    /// Bytes held in the tail object, below the part minimum.
    pub tail_len: u64,
    pub status: SessionStatus,
}

impl ResumableSession {
    pub fn new(id: String, tenant: Uuid, declared_length: Option<u64>) -> Self {
        Self {
            id,
            tenant,
            declared_length,
            offset: 0,
            s3_upload_id: None,
            parts: Vec::new(),
            tail_len: 0,
            status: SessionStatus::Active,
        }
    }

    /// Where the assembled object lands.
    pub fn staging_key(&self) -> Result<Key> {
        Key::staging(self.tenant, &self.id)
    }

    /// Where the sub-minimum remainder is parked between chunks.
    pub fn tail_key(&self) -> Result<Key> {
        Key::staging(self.tenant, &format!("{}-tail", self.id))
    }

    /// Remaining bytes, when the client declared a length.
    pub fn remaining(&self) -> Option<u64> {
        self.declared_length.map(|l| l.saturating_sub(self.offset))
    }

    fn require_active(&self) -> Result<()> {
        match self.status {
            SessionStatus::Active => Ok(()),
            SessionStatus::Completed => Err(Error::Backend(format!(
                "upload {} is already complete; appending would modify an object already \
                 handed to the caller under a content-addressed key",
                self.id
            ))),
            SessionStatus::Terminated => {
                Err(Error::Backend(format!("upload {} was terminated", self.id)))
            }
        }
    }
}

/// What a PATCH did.
///
/// An offset conflict is an outcome rather than an error: it is the normal way a client whose
/// connection dropped mid-chunk discovers where to resume, and TUS answers it with a `409`
/// carrying the authoritative offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    Accepted { new_offset: u64 },
    OffsetConflict { expected: u64, got: u64 },
}

/// Appends a chunk at `at_offset`.
pub async fn patch<S: ResumableStore + ?Sized>(
    store: &S,
    session: &mut ResumableSession,
    at_offset: u64,
    chunk: Bytes,
    class: StorageClass,
) -> Result<PatchOutcome> {
    session.require_active()?;

    if at_offset != session.offset {
        // Includes the replay case — a client retrying a chunk whose response was lost sends
        // it at the *previous* offset. Accepting that would silently duplicate the bytes and
        // produce an object whose digest matches nothing the client can compute.
        return Ok(PatchOutcome::OffsetConflict {
            expected: session.offset,
            got: at_offset,
        });
    }
    if chunk.is_empty() {
        return Ok(PatchOutcome::Accepted {
            new_offset: session.offset,
        });
    }
    if let Some(declared) = session.declared_length {
        let would_be = session.offset.saturating_add(chunk.len() as u64);
        if would_be > declared {
            // Refused before anything is written: a partially-applied over-long chunk would
            // leave the offset ahead of what the client believes it sent.
            return Err(Error::Backend(format!(
                "upload {} declared {declared} bytes; this chunk would take it to {would_be}",
                session.id
            )));
        }
    }

    // Tail first, then the new chunk — order is the whole correctness question here.
    let tail_key = session.tail_key()?;
    let mut buffer = if session.tail_len > 0 {
        let existing = store.get(&tail_key, None).await?.into_bytes(&tail_key)?;
        if existing.len() as u64 != session.tail_len {
            // The session and the store disagree. Continuing would assemble the wrong bytes,
            // and a wrong object under a content-addressed key is worse than a failed upload.
            return Err(Error::Backend(format!(
                "upload {}: tail object is {} bytes but the session records {} — refusing to \
                 assemble from inconsistent state",
                session.id,
                existing.len(),
                session.tail_len
            )));
        }
        let mut buf = BytesMut::with_capacity(existing.len() + chunk.len());
        buf.extend_from_slice(&existing);
        buf
    } else {
        BytesMut::with_capacity(chunk.len())
    };
    buffer.extend_from_slice(&chunk);
    let buffer = buffer.freeze();

    if buffer.len() >= MIN_PART_SIZE {
        // Large enough to be a legal part. Uploaded whole rather than split at exactly 5 MiB:
        // any size between the minimum and 5 GiB is legal, and fewer parts means fewer
        // requests and a shorter completion list.
        upload_parts(store, session, buffer, class).await?;
        if session.tail_len > 0 {
            store.delete(&tail_key).await?;
            session.tail_len = 0;
        }
    } else {
        store.put(&tail_key, buffer.clone(), class).await?;
        session.tail_len = buffer.len() as u64;
    }

    session.offset += chunk.len() as u64;
    Ok(PatchOutcome::Accepted {
        new_offset: session.offset,
    })
}

/// Uploads `buffer` as one or more parts, opening the multipart upload if needed.
async fn upload_parts<S: ResumableStore + ?Sized>(
    store: &S,
    session: &mut ResumableSession,
    buffer: Bytes,
    class: StorageClass,
) -> Result<()> {
    let key = session.staging_key()?;
    let upload_id = match &session.s3_upload_id {
        Some(id) => id.clone(),
        None => {
            let id = store.begin_resumable(&key, class).await?;
            session.s3_upload_id = Some(id.clone());
            id
        }
    };

    // A single PATCH above 5 GiB is not realistic, but the split costs nothing and the
    // alternative is a request the backend rejects.
    let mut offset = 0usize;
    while offset < buffer.len() {
        let end = (offset + MAX_PART).min(buffer.len());
        let slice = buffer.slice(offset..end);
        let number = i32::try_from(session.parts.len() + 1)
            .map_err(|_| Error::Backend("part count overflowed".into()))?;
        let etag = store
            .upload_resumable_part(&key, &upload_id, number, slice)
            .await?;
        session.parts.push(PartRecord { number, etag });
        offset = end;
    }
    Ok(())
}

/// S3's maximum part size.
const MAX_PART: usize = 5 * 1024 * 1024 * 1024;

/// Assembles the upload and returns the staging key it landed at.
///
/// The caller then hashes and promotes it to a content-addressed key (`content::promote`).
pub async fn complete<S: ResumableStore + ?Sized>(
    store: &S,
    session: &mut ResumableSession,
    class: StorageClass,
) -> Result<Key> {
    session.require_active()?;
    if let Some(declared) = session.declared_length
        && session.offset != declared
    {
        // The session stays Active: the client may still send the rest. Marking it failed
        // here would turn a slow upload into a lost one.
        return Err(Error::Backend(format!(
            "upload {} has {} of {declared} declared bytes",
            session.id, session.offset
        )));
    }
    if session.offset == 0 {
        return Err(Error::Backend(format!(
            "upload {} has no bytes",
            session.id
        )));
    }

    let key = session.staging_key()?;
    let tail_key = session.tail_key()?;

    match &session.s3_upload_id {
        // Everything still fits in the tail: one copy, no multipart upload at all. A 12-byte
        // asset should not cost a create/upload/complete round trip.
        None => {
            store.copy(&tail_key, &key, session.tail_len, class).await?;
            store.delete(&tail_key).await?;
            session.tail_len = 0;
        }
        Some(upload_id) => {
            let mut parts = session.parts.clone();
            if session.tail_len > 0 {
                // The final part may be any size, which is the only reason a sub-minimum tail
                // can be flushed at all.
                let tail = store.get(&tail_key, None).await?.into_bytes(&tail_key)?;
                let number = i32::try_from(parts.len() + 1)
                    .map_err(|_| Error::Backend("part count overflowed".into()))?;
                let etag = store
                    .upload_resumable_part(&key, upload_id, number, tail)
                    .await?;
                parts.push(PartRecord { number, etag });
            }
            store.finish_resumable(&key, upload_id, &parts).await?;
            if session.tail_len > 0 {
                store.delete(&tail_key).await?;
                session.tail_len = 0;
            }
            session.parts = parts;
        }
    }

    session.status = SessionStatus::Completed;
    Ok(key)
}

/// Abandons the upload, leaving nothing billable behind.
///
/// Idempotent: terminating an already-terminated session succeeds, so a retried cleanup does
/// not fail.
pub async fn terminate<S: ResumableStore + ?Sized>(
    store: &S,
    session: &mut ResumableSession,
) -> Result<()> {
    if matches!(session.status, SessionStatus::Terminated) {
        return Ok(());
    }
    let key = session.staging_key()?;
    if let Some(upload_id) = &session.s3_upload_id {
        // Parts already uploaded are billed until the upload is aborted or a lifecycle rule
        // expires it, so this is the call that actually stops the meter.
        store.abort_resumable(&key, upload_id).await?;
    }
    store.delete(&session.tail_key()?).await?;
    store.delete(&key).await?;
    session.tail_len = 0;
    session.parts.clear();
    session.s3_upload_id = None;
    session.status = SessionStatus::Terminated;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_session_starts_at_zero_and_active() {
        let s = ResumableSession::new("abc".into(), Uuid::nil(), Some(10));
        assert_eq!(s.offset, 0);
        assert_eq!(s.status, SessionStatus::Active);
        assert_eq!(s.remaining(), Some(10));
    }

    #[test]
    fn a_deferred_length_has_no_remaining_count() {
        let s = ResumableSession::new("abc".into(), Uuid::nil(), None);
        assert_eq!(
            s.remaining(),
            None,
            "TUS Upload-Defer-Length means the total is genuinely unknown, not zero"
        );
    }

    #[test]
    fn the_tail_key_is_distinct_from_the_staging_key() {
        let s = ResumableSession::new("abc".into(), Uuid::nil(), None);
        assert_ne!(
            s.staging_key().expect("key").as_str(),
            s.tail_key().expect("key").as_str(),
            "assembling into the tail's own key would overwrite the remainder mid-upload"
        );
    }
}
