//! Multipart upload.
//!
//! The path a large original takes (§18.3). Kept off [`crate::BlobStore`] because the
//! trait's `put` takes the whole body: a 200 GB ProRes master cannot be a `Bytes`, and a
//! trait method that could only be called with the entire object in memory would be a
//! trap rather than an abstraction.
//!
//! Two things here are about failing early rather than expensively:
//!
//! - **The 5 MiB minimum is enforced at `upload_part`.** S3 only reports `EntityTooSmall`
//!   at `CompleteMultipartUpload`, by which point every byte has already crossed the wire
//!   and been paid for. For a multi-gigabyte upload that is an expensive way to learn the
//!   part sizing was wrong.
//! - **A completed multipart upload reports no whole-object checksum.** The ETag of a
//!   multipart object is a digest *of the part digests* with a `-<count>` suffix, and is
//!   not the digest of the bytes. Reporting it as a checksum would make the integrity
//!   scrub compare two things that were never meant to match.

use crate::s3::Encrypted;
use crate::{BlobStore, Error, Key, Placement, Result, S3Store};
use aws_sdk_s3::{
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use bytes::Bytes;
use dam_core::StorageClass;

/// S3's minimum part size for every part except the last.
pub const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// An upload in progress.
///
/// Not `Drop`-based: aborting is a network call that can fail, and a `Drop` that silently
/// swallows a failed abort leaves parts accruing storage charges with nothing recording
/// it. So [`MultipartUpload::abort`] is explicit, and an abandoned upload is cleaned up by
/// a bucket lifecycle rule — belt and braces, since a process can always be killed.
#[derive(Debug)]
pub struct MultipartUpload<'a> {
    store: &'a S3Store,
    key: Key,
    upload_id: String,
    parts: Vec<CompletedPart>,
    /// Set when the part just uploaded was under the minimum, which is legal only if no
    /// further part follows. Checked on the *next* call, because that is the moment the
    /// undersized part stops being the last one.
    trailing_part_undersized: bool,
    bytes: u64,
    /// The class asked for at create time. Completion does not echo it back, and the
    /// placement row has to record what was requested.
    class: StorageClass,
}

impl S3Store {
    /// Starts a multipart upload.
    pub async fn begin_multipart(
        &self,
        key: &Key,
        class: StorageClass,
    ) -> Result<MultipartUpload<'_>> {
        let mut req = self
            .client()
            .create_multipart_upload()
            .bucket(self.bucket_name())
            .key(key.as_str())
            .encrypted_with(self.sse_kms_key_id());
        if self.capabilities().storage_classes {
            req = req.storage_class(aws_sdk_s3::types::StorageClass::from(class.as_s3()));
        }
        let out = req.send().await.map_err(|e| self.map_err(key, &e))?;
        let upload_id = out
            .upload_id()
            .ok_or_else(|| Error::Backend("create_multipart_upload returned no upload id".into()))?
            .to_owned();

        Ok(MultipartUpload {
            store: self,
            key: key.clone(),
            upload_id,
            parts: Vec::new(),
            trailing_part_undersized: false,
            bytes: 0,
            class,
        })
    }
}

impl MultipartUpload<'_> {
    /// The upload id, for a resumable upload whose parts arrive across requests (TUS).
    pub fn upload_id(&self) -> &str {
        &self.upload_id
    }

    pub fn parts_uploaded(&self) -> usize {
        self.parts.len()
    }

    pub fn bytes_uploaded(&self) -> u64 {
        self.bytes
    }

    /// Uploads the next part, in order.
    ///
    /// Refuses if the *previous* part was under [`MIN_PART_SIZE`]: that part was only legal
    /// as the final one, and this call is what makes it not final.
    pub async fn upload_part(&mut self, body: Bytes) -> Result<()> {
        if self.trailing_part_undersized {
            return Err(Error::Backend(format!(
                "part {} of {} was under the 5 MiB ({MIN_PART_SIZE} byte) minimum, so it \
                 could only have been the final part — S3 would reject this upload at \
                 completion, after every byte had been sent",
                self.parts.len(),
                self.key
            )));
        }
        if body.is_empty() {
            return Err(Error::Backend(format!(
                "refusing an empty part {} of {}",
                self.parts.len() + 1,
                self.key
            )));
        }

        // Part numbers are 1-based and S3 caps an upload at 10,000 parts.
        let part_number = i32::try_from(self.parts.len() + 1)
            .map_err(|_| Error::Backend("part count overflowed".into()))?;
        if part_number > 10_000 {
            return Err(Error::Backend(format!(
                "{} would exceed S3's 10,000-part limit; increase the part size",
                self.key
            )));
        }

        let len = body.len();
        let out = self
            .store
            .client()
            .upload_part()
            .bucket(self.store.bucket_name())
            .key(self.key.as_str())
            .upload_id(&self.upload_id)
            .part_number(part_number)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| self.store.map_err(&self.key, &e))?;

        let e_tag = out.e_tag().ok_or_else(|| {
            // Without the part ETag the upload cannot be completed, and retrying the part
            // is cheaper than discovering it at completion.
            Error::Backend(format!(
                "part {part_number} of {} returned no ETag",
                self.key
            ))
        })?;

        self.parts.push(
            CompletedPart::builder()
                .part_number(part_number)
                .e_tag(e_tag)
                .build(),
        );
        self.trailing_part_undersized = len < MIN_PART_SIZE;
        self.bytes += len as u64;
        Ok(())
    }

    /// Completes the upload, assembling the parts in order.
    pub async fn finish(self) -> Result<Placement> {
        if self.parts.is_empty() {
            // S3 rejects this too, but completing with no parts is a caller bug worth
            // naming: it means the source stream was empty and nobody noticed. Left
            // uncompleted, so `abort` is still the caller's to make.
            return Err(Error::Backend(format!(
                "refusing to complete {} with no parts",
                self.key
            )));
        }

        let out = self
            .store
            .client()
            .complete_multipart_upload()
            .bucket(self.store.bucket_name())
            .key(self.key.as_str())
            .upload_id(&self.upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(self.parts.clone()))
                    .build(),
            )
            .send()
            .await
            .map_err(|e| self.store.map_err(&self.key, &e))?;

        Ok(Placement {
            key: self.key.clone(),
            size: self.bytes,
            // The class was set at create time and is not echoed back on completion, so
            // report what was asked for — or Standard where the backend ignores the
            // header, matching `put`.
            storage_class: if self.store.capabilities().storage_classes {
                self.class
            } else {
                StorageClass::Standard
            },
            etag: out.e_tag().map(str::to_owned),
            // Deliberately absent — see the module docs.
            checksum: None,
            version_id: out.version_id().map(str::to_owned),
        })
    }

    /// Discards the upload and every part uploaded so far.
    ///
    /// Idempotent in the way that matters: aborting an upload the server has already
    /// forgotten succeeds, so a retried cleanup does not fail.
    pub async fn abort(self) -> Result<()> {
        match self
            .store
            .client()
            .abort_multipart_upload()
            .bucket(self.store.bucket_name())
            .key(self.key.as_str())
            .upload_id(&self.upload_id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let rendered = format!("{e:?}");
                if rendered.contains("NoSuchUpload") {
                    Ok(())
                } else {
                    Err(self.store.map_err(&self.key, &e))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_minimum_part_size_is_s3s_five_mebibytes() {
        // Pinned as a test because a "helpful" rounding to 5,000,000 bytes would make
        // every multi-part upload fail at completion, and only for large files.
        assert_eq!(MIN_PART_SIZE, 5_242_880);
    }
}
