//! `BlobStore` over `aws-sdk-s3`.
//!
//! `aws-sdk-s3` rather than `object_store` (D3): `object_store` hides storage class,
//! `RestoreObject`, and the `x-amz-restore` header — the three things cold tiering is
//! built on.
//!
//! ## Capabilities belong to the backend, not to this code
//!
//! The same driver code talks to AWS, SeaweedFS, Ceph RGW, and Wasabi, and those differ
//! in what they implement. So [`Capabilities`] is set at construction and the caller
//! declares which backend they pointed at. [`S3Store::seaweedfs`] is not a different
//! implementation — it is the same one telling the truth about a narrower server.
//!
//! In particular SeaweedFS **echoes the storage-class header back** without changing
//! behaviour, which is worse than rejecting it for testing purposes: a driver that
//! inferred support from the echo would pass a conformance case while proving nothing.
//! Hence the explicit declaration rather than a probe.

use crate::{
    BlobStore, ByteRange, Capabilities, Error, GetOutcome, Key, ObjectState, Placement,
    RestoreTicket, Result,
};
use async_trait::async_trait;
use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    error::SdkError,
    presigning::PresigningConfig,
    primitives::ByteStream,
    types::{GlacierJobParameters, RestoreRequest, Tier},
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use dam_core::{LatencyClass, RestoreState, RestoreTier, StorageClass};
use std::time::Duration;

/// S3-compatible blob store.
#[derive(Debug, Clone)]
pub struct S3Store {
    client: Client,
    bucket: String,
    capabilities: Capabilities,
    latency_class: LatencyClass,
    driver: &'static str,
}

impl S3Store {
    /// A store against real AWS S3, using the ambient credential chain.
    pub async fn aws(bucket: &str, region: &str) -> Self {
        let conf = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region.to_owned()))
            .load()
            .await;
        Self {
            client: Client::new(&conf),
            bucket: bucket.to_owned(),
            capabilities: Capabilities::full(),
            latency_class: LatencyClass::Instant,
            driver: "s3",
        }
    }

    /// A store against a non-AWS S3-compatible endpoint with static credentials.
    ///
    /// `force_path_style` is always on: every non-AWS endpoint needs it, and AWS accepts
    /// it, so making it conditional would only create a way to get it wrong.
    pub fn compatible(
        endpoint: &str,
        bucket: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
        capabilities: Capabilities,
        driver: &'static str,
    ) -> Self {
        let creds = Credentials::new(access_key, secret_key, None, None, "damrs-static");
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.to_owned()))
            .endpoint_url(endpoint)
            .credentials_provider(creds)
            .force_path_style(true)
            .build();
        Self {
            client: Client::from_conf(conf),
            bucket: bucket.to_owned(),
            capabilities,
            latency_class: LatencyClass::Instant,
            driver,
        }
    }

    /// A store against SeaweedFS, declaring what SeaweedFS actually implements.
    ///
    /// Versioning and object lock are real here — SeaweedFS refuses a version-scoped
    /// delete under a legal hold, which is the one thing `FakeS3Store` cannot prove,
    /// because the point of object lock is that the *server* says no.
    ///
    /// Storage classes and restore are **not** claimed: the header round-trips but
    /// behaviour is unchanged, so those cases run against the fake and the AWS nightly.
    pub fn seaweedfs(endpoint: &str, bucket: &str, access_key: &str, secret_key: &str) -> Self {
        Self::compatible(
            endpoint,
            bucket,
            "us-east-1",
            access_key,
            secret_key,
            Capabilities {
                storage_classes: false,
                restore: false,
                versioning: true,
                object_lock: true,
                presigned_urls: true,
                ranged_get: true,
                // SeaweedFS does not return a stored checksum on HEAD, so the integrity
                // scrub cannot verify without downloading. Declared honestly rather than
                // assumed — see DECISIONS.md.
                server_checksums: false,
            },
            "seaweedfs",
        )
    }

    /// Creates the bucket if absent. Used by the test harness; a deployed bucket is
    /// created by infrastructure, not by the application.
    pub async fn create_bucket(&self) -> Result<()> {
        match self
            .client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let msg = format!("{e:?}");
                // Already existing is success — this is called on every harness start.
                if msg.contains("BucketAlreadyOwnedByYou") || msg.contains("BucketAlreadyExists") {
                    Ok(())
                } else {
                    Err(Error::Backend(format!("create_bucket: {e}")))
                }
            }
        }
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn bucket_name(&self) -> &str {
        &self.bucket
    }

    /// Refuses a call the backend cannot honour, before it reaches the wire.
    ///
    /// A declared capability is the contract (§20.2); a caller that gets `Unsupported`
    /// can degrade, while one that gets a backend-specific 400 has to pattern-match on
    /// error text per backend.
    pub(crate) fn require(&self, have: bool, capability: &'static str) -> Result<()> {
        if have {
            Ok(())
        } else {
            Err(Error::Unsupported {
                driver: self.driver,
                capability,
            })
        }
    }

    /// Renders a bucket-level SDK error with its cause.
    ///
    /// `Display` on `SdkError` is just "service error" — the code and message live in the
    /// `Debug` rendering, and an operator reading a log needs them.
    pub(crate) fn op_err<E: std::fmt::Debug>(
        &self,
        op: &str,
        e: &SdkError<E, impl std::fmt::Debug>,
    ) -> Error {
        Error::Backend(format!("{}: {op}: {e:?}", self.driver))
    }

    /// Maps an SDK error to ours, distinguishing the three cases that need different
    /// handling: absent, archived, and everything else.
    pub(crate) fn map_err<E: std::fmt::Debug>(
        &self,
        key: &Key,
        e: &SdkError<E, impl std::fmt::Debug>,
    ) -> Error {
        let rendered = format!("{e:?}");
        if rendered.contains("NoSuchKey")
            || rendered.contains("NotFound")
            || rendered.contains("status: 404")
        {
            return Error::NotFound(key.as_str().to_owned());
        }
        if rendered.contains("InvalidObjectState") {
            // The archived case. Kept distinct because the caller's next move is to
            // request a restore and wait, not to give up.
            return Error::NotRestored {
                key: key.as_str().to_owned(),
                class: StorageClass::Glacier,
            };
        }
        Error::Backend(format!("{}: {rendered}", self.driver))
    }

    /// Parses the `x-amz-restore` header.
    ///
    /// Two shapes, and the difference is the whole point:
    ///   `ongoing-request="true"`                                    -> in progress
    ///   `ongoing-request="false", expiry-date="Fri, 1 Aug 2026 ..."` -> available until
    ///
    /// A missing header means no restore was ever requested, which is distinct from one
    /// that has expired — an expired restore can be re-requested at a known cost, while
    /// an absent one may mean the object was never archived at all.
    fn parse_restore(header: Option<&str>) -> (RestoreState, Option<DateTime<Utc>>) {
        let Some(h) = header else {
            return (RestoreState::None, None);
        };
        if h.contains("ongoing-request=\"true\"") {
            return (RestoreState::Ongoing, None);
        }
        let expiry = h
            .split("expiry-date=")
            .nth(1)
            .map(|s| s.trim().trim_matches('"'))
            .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
            .map(|d| d.with_timezone(&Utc));
        match expiry {
            Some(e) if e > Utc::now() => (RestoreState::Available, Some(e)),
            Some(_) => (RestoreState::Expired, None),
            // `ongoing-request="false"` with an unparseable expiry: the restore finished
            // but we cannot tell for how long. Reporting Available without an expiry
            // would violate the invariant the database enforces, so it is Expired —
            // pessimistic, and a re-request is cheap relative to a wrong answer.
            None => (RestoreState::Expired, None),
        }
    }

    fn to_sdk_class(class: StorageClass) -> aws_sdk_s3::types::StorageClass {
        aws_sdk_s3::types::StorageClass::from(class.as_s3())
    }

    fn from_sdk_class(class: Option<&aws_sdk_s3::types::StorageClass>) -> StorageClass {
        // S3 omits the header for STANDARD, so absent means Standard rather than unknown.
        class
            .map(|c| c.as_str())
            .unwrap_or("STANDARD")
            .parse()
            .unwrap_or(StorageClass::Standard)
    }

    fn to_sdk_tier(tier: RestoreTier) -> Tier {
        match tier {
            RestoreTier::Expedited => Tier::Expedited,
            RestoreTier::Standard => Tier::Standard,
            RestoreTier::Bulk => Tier::Bulk,
        }
    }
}

#[async_trait]
impl BlobStore for S3Store {
    fn driver(&self) -> &'static str {
        self.driver
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn latency_class(&self) -> LatencyClass {
        self.latency_class
    }

    async fn put(&self, key: &Key, body: Bytes, class: StorageClass) -> Result<Placement> {
        let size = body.len() as u64;
        let checksum = blake3::hash(&body).to_hex().to_string();

        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key.as_str())
            .body(ByteStream::from(body));

        // Only send the header when the backend honours it. Sending it to a backend that
        // merely echoes it back would make `head` report a class the object is not
        // actually in, which is exactly the confusion the capability flag exists to
        // prevent.
        if self.capabilities.storage_classes {
            req = req.storage_class(Self::to_sdk_class(class));
        }

        let out = req.send().await.map_err(|e| self.map_err(key, &e))?;

        Ok(Placement {
            key: key.clone(),
            size,
            storage_class: if self.capabilities.storage_classes {
                class
            } else {
                StorageClass::Standard
            },
            etag: out.e_tag().map(str::to_owned),
            checksum: Some(checksum),
            version_id: out.version_id().map(str::to_owned),
        })
    }

    async fn get(&self, key: &Key, range: Option<ByteRange>) -> Result<GetOutcome> {
        let mut req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key.as_str());
        if let Some(r) = range {
            req = req.range(r.as_header());
        }

        match req.send().await {
            Ok(out) => {
                let data = out
                    .body
                    .collect()
                    .await
                    .map_err(|e| Error::Backend(format!("reading body: {e}")))?;
                Ok(GetOutcome::Bytes(data.into_bytes()))
            }
            Err(e) => {
                // An archived object is a normal outcome, not an error — turn
                // InvalidObjectState back into a ticket so the caller sees the same
                // shape it does from every other driver.
                match self.map_err(key, &e) {
                    Error::NotRestored { .. } => {
                        let state = self.head(key).await?;
                        Ok(GetOutcome::NotAvailable(RestoreTicket {
                            class: state.storage_class,
                            state: state.restore_state,
                            tier: None,
                            eta: None,
                            expires_at: state.restore_expires_at,
                        }))
                    }
                    other => Err(other),
                }
            }
        }
    }

    async fn head(&self, key: &Key) -> Result<ObjectState> {
        let out = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key.as_str())
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))?;

        let (restore_state, restore_expires_at) = Self::parse_restore(out.restore());

        Ok(ObjectState {
            size: out.content_length().unwrap_or(0).max(0) as u64,
            storage_class: Self::from_sdk_class(out.storage_class()),
            restore_state,
            restore_expires_at,
            etag: out.e_tag().map(str::to_owned),
            checksum: if self.capabilities.server_checksums {
                out.checksum_sha256()
                    .or_else(|| out.checksum_crc32_c())
                    .map(str::to_owned)
            } else {
                None
            },
            last_modified: out
                .last_modified()
                .and_then(|t| DateTime::from_timestamp(t.secs(), 0)),
        })
    }

    async fn delete(&self, key: &Key) -> Result<()> {
        // S3 returns 204 for a missing key, so this is idempotent without extra work —
        // which the purge worker relies on when retrying.
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key.as_str())
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))?;
        Ok(())
    }

    async fn list(&self, prefix: &str, limit: usize) -> Result<Vec<Key>> {
        let out = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .max_keys(i32::try_from(limit).unwrap_or(i32::MAX))
            .send()
            .await
            .map_err(|e| Error::Backend(format!("list {prefix}: {e:?}")))?;

        // S3 returns keys lexicographically, which the import reconciler depends on.
        out.contents()
            .iter()
            .filter_map(|o| o.key())
            .map(|k| Key::new(k.to_owned()))
            .collect()
    }

    async fn transition(&self, key: &Key, to: StorageClass) -> Result<()> {
        if !self.capabilities.storage_classes {
            return Err(Error::Unsupported {
                driver: self.driver,
                capability: "storage class transitions",
            });
        }
        // A self-copy with a new storage class is how S3 transitions an object; there is
        // no dedicated API. The object keeps its key and its content.
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(key.as_str())
            .copy_source(format!("{}/{}", self.bucket, key.as_str()))
            .storage_class(Self::to_sdk_class(to))
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Copy)
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))?;
        Ok(())
    }

    async fn restore(
        &self,
        key: &Key,
        tier: RestoreTier,
        keep_for: Duration,
    ) -> Result<RestoreTicket> {
        if !self.capabilities.restore {
            return Err(Error::Unsupported {
                driver: self.driver,
                capability: "RestoreObject",
            });
        }

        let state = self.head(key).await?;
        if !tier.is_available_for(state.storage_class) {
            return Err(Error::Backend(format!(
                "{tier} retrieval is not available for {}",
                state.storage_class
            )));
        }

        // S3 counts keep-warm in whole days, minimum 1.
        let days = i32::try_from(keep_for.as_secs() / 86_400)
            .unwrap_or(1)
            .max(1);
        let request = RestoreRequest::builder()
            .days(days)
            .glacier_job_parameters(
                GlacierJobParameters::builder()
                    .tier(Self::to_sdk_tier(tier))
                    .build()
                    .map_err(|e| Error::Backend(format!("glacier job parameters: {e}")))?,
            )
            .build();

        match self
            .client
            .restore_object()
            .bucket(&self.bucket)
            .key(key.as_str())
            .restore_request(request)
            .send()
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let rendered = format!("{e:?}");
                // A restore already in flight returns RestoreAlreadyInProgress. That is a
                // no-op, not a failure — and treating it as one would let a UI retry turn
                // into a second charge.
                if !rendered.contains("RestoreAlreadyInProgress") {
                    return Err(self.map_err(key, &e));
                }
            }
        }

        let after = self.head(key).await?;
        Ok(RestoreTicket {
            class: after.storage_class,
            state: match after.restore_state {
                // S3 reports nothing until the request registers; the caller asked for a
                // restore, so Requested is more truthful than None.
                RestoreState::None => RestoreState::Requested,
                other => other,
            },
            tier: Some(tier),
            eta: Some(
                Utc::now()
                    + chrono::Duration::from_std(tier.expected_wait(after.storage_class))
                        .unwrap_or_else(|_| chrono::Duration::hours(12)),
            ),
            expires_at: after.restore_expires_at,
        })
    }

    async fn presign_get(&self, key: &Key, ttl: Duration) -> Result<String> {
        let cfg = PresigningConfig::expires_in(ttl)
            .map_err(|e| Error::Backend(format!("presigning config: {e}")))?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key.as_str())
            .presigned(cfg)
            .await
            .map_err(|e| Error::Backend(format!("presign get: {e:?}")))?;
        Ok(req.uri().to_owned())
    }

    async fn presign_put(&self, key: &Key, ttl: Duration) -> Result<String> {
        let cfg = PresigningConfig::expires_in(ttl)
            .map_err(|e| Error::Backend(format!("presigning config: {e}")))?;
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key.as_str())
            .presigned(cfg)
            .await
            .map_err(|e| Error::Backend(format!("presign put: {e:?}")))?;
        Ok(req.uri().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_in_progress_restore_is_parsed_as_ongoing() {
        let (state, expiry) = S3Store::parse_restore(Some("ongoing-request=\"true\""));
        assert_eq!(state, RestoreState::Ongoing);
        assert!(expiry.is_none());
    }

    #[test]
    fn a_completed_restore_with_a_future_expiry_is_available() {
        let future = (Utc::now() + chrono::Duration::days(3)).to_rfc2822();
        let header = format!("ongoing-request=\"false\", expiry-date=\"{future}\"");
        let (state, expiry) = S3Store::parse_restore(Some(&header));
        assert_eq!(state, RestoreState::Available);
        assert!(
            expiry.is_some(),
            "an Available restore must carry an expiry"
        );
    }

    #[test]
    fn a_completed_restore_with_a_past_expiry_is_expired() {
        let past = (Utc::now() - chrono::Duration::days(1)).to_rfc2822();
        let header = format!("ongoing-request=\"false\", expiry-date=\"{past}\"");
        assert_eq!(
            S3Store::parse_restore(Some(&header)).0,
            RestoreState::Expired
        );
    }

    #[test]
    fn no_header_means_no_restore_was_ever_requested() {
        assert_eq!(S3Store::parse_restore(None).0, RestoreState::None);
    }

    #[test]
    fn an_unparseable_expiry_is_treated_pessimistically() {
        // Reporting Available without an expiry would violate the invariant the database
        // enforces, so an unreadable expiry becomes Expired. A re-request is cheap
        // relative to handing out a URL that 403s.
        let header = "ongoing-request=\"false\", expiry-date=\"not a date\"";
        assert_eq!(
            S3Store::parse_restore(Some(header)).0,
            RestoreState::Expired
        );
    }

    #[test]
    fn an_absent_storage_class_header_means_standard() {
        assert_eq!(S3Store::from_sdk_class(None), StorageClass::Standard);
    }

    #[test]
    fn storage_classes_round_trip_through_the_sdk_type() {
        for c in [
            StorageClass::Standard,
            StorageClass::GlacierIr,
            StorageClass::DeepArchive,
        ] {
            let sdk = S3Store::to_sdk_class(c);
            assert_eq!(S3Store::from_sdk_class(Some(&sdk)), c);
        }
    }
}
