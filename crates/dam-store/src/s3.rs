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

/// Retries per operation, including the first attempt.
///
/// **`aws_sdk_s3::Config::builder()` applies no retry policy at all** — only `aws_config::defaults()`
/// does, and that is not the path a non-AWS endpoint takes. So every operation against a self-hosted
/// gateway ran with retries disabled, which is exactly backwards: a MinIO or SeaweedFS deployment is
/// more likely to return a transient `500` than AWS is, and a 40-part upload that fails outright on one
/// of them loses the whole upload.
///
/// Found by a test suite failing three separate times on a SeaweedFS `InternalError` whose own response
/// was marked `retryable: true`.
const MAX_ATTEMPTS: u32 = 5;

/// Ceiling on one attempt, so a stalled connection cannot hold a worker forever.
///
/// Per *attempt*, not per operation: a multipart part upload can legitimately take minutes, and a
/// deadline on the whole operation would cancel healthy slow transfers along with dead ones.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the connection itself. Short, because a wrong endpoint should fail fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a credential *refresh* may take before the operation carrying it fails.
///
/// The SDK's own default is five seconds (`DEFAULT_LOAD_TIMEOUT` in `aws-smithy-runtime`), and five seconds is
/// not much for the things that sit behind a credential refresh: an SSO token exchange, an STS
/// `AssumeRoleWithWebIdentity`, or IMDS on a loaded instance. It is also invisible until it happens, because a
/// freshly-started process has warm credentials and only starts refreshing an hour later.
///
/// Found against real AWS. A worker that had been up twenty minutes failed every `CopyObject` with
/// `ConnectorError { source: TimedOutError(5s) }` while `PutObject` from earlier in its life had succeeded and
/// the same copy took 0.58 s from the CLI. The five in the error message was this default, not anything about
/// the request — and a restarted worker succeeded immediately, which is the shape of a credential problem
/// wearing a network problem's error.
///
/// Thirty seconds because the failure mode of waiting too long is a slow operation, and the failure mode of
/// waiting too little is a tiering run that dies an hour after it was deployed and blames the network.
const CREDENTIAL_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `endpoint` is plain HTTP, and therefore has no use for a certificate store.
///
/// Not a style question. The SDK's default HTTP client enables the platform native root store, and
/// `aws-smithy-http-client` loads it **once per process** into a `LazyLock`. Two consequences on a
/// self-hosted deployment talking to `http://minio:9000`:
///
/// - it pays a root-store load (about 300ms on macOS) for certificates that can never be consulted; and
/// - if that one load comes back empty — which concurrent macOS keychain reads can cause — then *every*
///   subsequent client construction in the process trips
///   `debug_assert!(valid > 0, "TrustStore configured to enable native roots but no valid root
///   certificates parsed!")`.
///
/// That second one is not theoretical: it took out nine S3 test cases at once, including one that never
/// opens a connection, because the poisoned `LazyLock` is process-wide. A plain-HTTP endpoint gets a
/// connector with no TLS at all, which cannot reach that code.
///
/// An `https://` endpoint keeps the default. An empty trust store there would reject every connection,
/// and a deployment with a private CA needs the platform store to find it.
fn is_plain_http(endpoint: &str) -> bool {
    // Case-insensitive: a scheme is case-insensitive per RFC 3986, and getting this wrong would silently
    // send an `HTTP://` endpoint down the TLS path.
    endpoint.len() >= 7 && endpoint[..7].eq_ignore_ascii_case("http://")
}

/// The retry and timeout policy, applied to both constructors.
///
/// Stated on the AWS path too, rather than inherited from `aws_config::defaults()`. Two paths with
/// different resilience is the bug this function exists to remove, and one of them being implicit is
/// how it stayed invisible.
fn resilience() -> (
    aws_config::retry::RetryConfig,
    aws_config::timeout::TimeoutConfig,
) {
    (
        aws_config::retry::RetryConfig::standard().with_max_attempts(MAX_ATTEMPTS),
        aws_config::timeout::TimeoutConfig::builder()
            .operation_attempt_timeout(ATTEMPT_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build(),
    )
}

/// S3-compatible blob store.
#[derive(Debug, Clone)]
pub struct S3Store {
    client: Client,
    bucket: String,
    capabilities: Capabilities,
    latency_class: LatencyClass,
    driver: &'static str,
    /// Whether this store's client can do TLS at all. See [`is_plain_http`].
    tls: bool,
    /// The customer-managed KMS key every object this store writes is encrypted under (G10·3).
    ///
    /// `None` means the bucket's own default applies — SSE-S3, or whatever the operator set on the bucket.
    /// That is the default here because a key id we invented would fail every write, and because most
    /// deployments do not want BYOK.
    sse_kms_key_id: Option<String>,
}

/// Applies the store's encryption choice to a request that creates an object.
///
/// A trait over the three builder types rather than the same two lines at each call site, because there are
/// **seven** paths that create an object — `put`, the small promote copy, the large promote's multipart
/// create, the self-copy that performs a storage-class transition, a second multipart create, the one real
/// uploads go through in `multipart.rs`, and the presigned PUT — and a write that misses it does not fail. It
/// silently lands under the bucket's default key, which looks exactly like success until somebody audits the
/// bucket. One applicator, applied everywhere, is greppable in a way that seven remembered pairs of lines is
/// not.
pub(crate) trait Encrypted {
    fn encrypted_with(self, key_id: Option<&str>) -> Self;
}

macro_rules! encrypted_builder {
    ($($ty:path),+ $(,)?) => {
        $(impl Encrypted for $ty {
            fn encrypted_with(self, key_id: Option<&str>) -> Self {
                match key_id {
                    // Both headers, and both are needed: `ssekms_key_id` alone is ignored without the
                    // algorithm, so a request carrying only the key id encrypts under the default and reports
                    // success.
                    Some(key) => self
                        .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
                        .ssekms_key_id(key),
                    None => self,
                }
            }
        })+
    };
}

encrypted_builder!(
    aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    aws_sdk_s3::operation::copy_object::builders::CopyObjectFluentBuilder,
    aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
);

impl S3Store {
    /// A store against real AWS S3, using the ambient credential chain.
    pub async fn aws(bucket: &str, region: &str) -> Self {
        let (retry, timeout) = resilience();
        let conf = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region.to_owned()))
            .retry_config(retry)
            .timeout_config(timeout)
            // A credential cache that allows a refresh longer than five seconds. Stated rather than
            // defaulted for the same reason `resilience` is stated: see `CREDENTIAL_LOAD_TIMEOUT` for the
            // failure this prevents, which took a real AWS run to see at all.
            .identity_cache(
                aws_config::identity::IdentityCache::lazy()
                    .load_timeout(CREDENTIAL_LOAD_TIMEOUT)
                    .build(),
            )
            .load()
            .await;
        Self {
            client: Client::new(&conf),
            bucket: bucket.to_owned(),
            capabilities: Capabilities::full(),
            latency_class: LatencyClass::Instant,
            driver: "s3",
            // Real AWS is always HTTPS, so the root store is both needed and used.
            tls: true,
            sse_kms_key_id: None,
        }
    }

    /// Encrypt every object this store writes under a customer-managed KMS key (G10·3).
    ///
    /// A builder method rather than a constructor parameter, and that is not laziness: `aws` and
    /// `seaweedfs` are called from several dozen test fixtures, and threading an `Option<String>` through
    /// all of them would be a large diff that hides the seven lines that matter.
    ///
    /// **What this cannot do.** It sets the headers on every write *this process* makes. A presigned PUT is
    /// executed by the browser, and if it does not send the headers that were signed the object lands under
    /// the bucket's default key. The only thing that makes BYOK a guarantee rather than an intention is a
    /// bucket policy denying `s3:PutObject` without the expected key id — `docker/DEPLOY.md` states that as
    /// required, because a deployment that treats it as optional believes it has BYOK and does not.
    #[must_use]
    pub fn with_sse_kms(mut self, key_id: impl Into<String>) -> Self {
        // Blank means no key, matching what `StorageConfig` does with an empty variable. Without this the two
        // disagree: the config path would produce `None` and a direct call would produce `Some("")`, which
        // sends an empty key id and fails every write with an error naming the key rather than its absence.
        let key_id = key_id.into();
        let trimmed = key_id.trim();
        self.sse_kms_key_id = if trimmed.is_empty() {
            None
        } else if trimmed.len() == key_id.len() {
            Some(key_id)
        } else {
            Some(trimmed.to_owned())
        };
        self
    }

    /// The key every write is encrypted under, if any. Exposed so it is assertable rather than inferred.
    #[must_use]
    pub fn sse_kms_key_id(&self) -> Option<&str> {
        self.sse_kms_key_id.as_deref()
    }

    /// Whether this store's HTTP client has TLS support.
    ///
    /// `false` for a plain-HTTP endpoint, where the client is built without it so the platform root store
    /// is never loaded. Exposed so that decision is assertable rather than inferred from timing.
    pub fn uses_tls(&self) -> bool {
        self.tls
    }

    /// Retry attempts this store is configured for, including the first.
    ///
    /// Exposed so the policy can be asserted. `Config::builder()` silently defaults to no retries, and
    /// nothing about a store with retries disabled looks different until a backend returns a transient
    /// error under load — which is a bad time to find out.
    pub fn max_attempts(&self) -> Option<u32> {
        self.client
            .config()
            .retry_config()
            .map(|c| c.max_attempts())
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
        let (retry, timeout) = resilience();
        let mut conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.to_owned()))
            .endpoint_url(endpoint)
            .credentials_provider(creds)
            .force_path_style(true)
            .retry_config(retry)
            .timeout_config(timeout);

        let plain_http = is_plain_http(endpoint);
        if plain_http {
            // See `is_plain_http`. A connector with no TLS, so the native root store is never touched.
            conf = conf.http_client(aws_smithy_http_client::Builder::new().build_http());
        }
        let conf = conf.build();
        Self {
            client: Client::from_conf(conf),
            bucket: bucket.to_owned(),
            capabilities,
            latency_class: LatencyClass::Instant,
            driver,
            tls: !plain_http,
            sse_kms_key_id: None,
        }
    }

    /// A store against SeaweedFS, declaring what SeaweedFS actually implements.
    ///
    /// Versioning and object lock are real here — SeaweedFS refuses a version-scoped
    /// delete under a legal hold, which is the one thing `FakeS3Store` cannot prove,
    /// because the point of object lock is that the *server* says no.
    ///
    /// Storage classes and restore are **not** claimed: the header round-trips but
    /// behaviour is unchanged, so those cases run against the fake and, when a bucket is configured, against
    /// `aws_conformance`. That workflow has never actually executed — no runs, no secrets — so treat the
    /// AWS-side coverage as one recorded manual pass rather than as continuous.
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

    /// Server-side copy. Bytes never traverse the client.
    ///
    /// S3 rejects `CopyObject` above 5 GiB, so this is only the small path — see
    /// `content::promote`, which chooses between this and a multipart copy.
    pub async fn copy_object(&self, from: &Key, to: &Key, class: StorageClass) -> Result<()> {
        let mut req = self
            .client
            .copy_object()
            .bucket(&self.bucket)
            // The source is `bucket/key`, URL-encoded. Our keys are hex, UUIDs and slashes,
            // so nothing here needs escaping — but a key that did would silently copy the
            // wrong object, which is why keys are ours and validated (see `Key`).
            .copy_source(format!("{}/{}", self.bucket, from.as_str()))
            .key(to.as_str())
            .encrypted_with(self.sse_kms_key_id());
        if self.capabilities.storage_classes {
            req = req.storage_class(Self::to_sdk_class(class));
        }
        req.send()
            .await
            .map_err(|e| self.map_err(from, &e))
            .map(|_| ())
    }

    /// Server-side copy of a large object, as a multipart upload of ranged part copies.
    ///
    /// `ranges` are inclusive byte ranges covering the whole object, as produced by
    /// `content::copy_part_ranges`. On any failure the upload is aborted, so no orphan parts
    /// are left accruing charges.
    pub async fn copy_object_multipart(
        &self,
        from: &Key,
        to: &Key,
        ranges: &[(u64, u64)],
        class: StorageClass,
    ) -> Result<()> {
        if ranges.is_empty() {
            return Err(Error::Backend(
                "a multipart copy needs at least one part range".into(),
            ));
        }
        let mut create = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(to.as_str())
            .encrypted_with(self.sse_kms_key_id());
        if self.capabilities.storage_classes {
            create = create.storage_class(Self::to_sdk_class(class));
        }
        let upload_id = create
            .send()
            .await
            .map_err(|e| self.map_err(to, &e))?
            .upload_id()
            .ok_or_else(|| Error::Backend("create_multipart_upload returned no upload id".into()))?
            .to_owned();

        let source = format!("{}/{}", self.bucket, from.as_str());
        let mut parts = Vec::with_capacity(ranges.len());
        for (index, (start, end)) in ranges.iter().enumerate() {
            let part_number = i32::try_from(index + 1)
                .map_err(|_| Error::Backend("part count overflowed".into()))?;
            let copied = self
                .client
                .upload_part_copy()
                .bucket(&self.bucket)
                .key(to.as_str())
                .upload_id(&upload_id)
                .part_number(part_number)
                .copy_source(&source)
                .copy_source_range(format!("bytes={start}-{end}"))
                .send()
                .await;
            match copied {
                Ok(out) => {
                    let e_tag = out
                        .copy_part_result()
                        .and_then(|r| r.e_tag())
                        .ok_or_else(|| {
                            Error::Backend(format!("part {part_number} copy returned no ETag"))
                        })?
                        .to_owned();
                    parts.push(
                        aws_sdk_s3::types::CompletedPart::builder()
                            .part_number(part_number)
                            .e_tag(e_tag)
                            .build(),
                    );
                }
                Err(e) => {
                    // Abort before returning: parts already copied are billed until the
                    // upload is aborted or a lifecycle rule expires it.
                    let _ = self
                        .client
                        .abort_multipart_upload()
                        .bucket(&self.bucket)
                        .key(to.as_str())
                        .upload_id(&upload_id)
                        .send()
                        .await;
                    return Err(self.map_err(from, &e));
                }
            }
        }

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(to.as_str())
            .upload_id(&upload_id)
            .multipart_upload(
                aws_sdk_s3::types::CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(|e| self.map_err(to, &e))
            .map(|_| ())
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
            .body(ByteStream::from(body))
            .encrypted_with(self.sse_kms_key_id());

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
            // Re-stated on a transition, not inherited. `MetadataDirective::Copy` carries metadata across;
            // it does not carry the encryption choice, so a transition without this rewrites the object
            // under the bucket default and silently drops the customer's key.
            .encrypted_with(self.sse_kms_key_id())
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

    async fn copy(&self, from: &Key, to: &Key, size: u64, class: StorageClass) -> Result<()> {
        // The threshold lives here, not in the caller: 5 GiB is S3's limit, and a driver for
        // a backend with a different one would choose differently.
        let ranges = crate::content::copy_part_ranges(size, crate::content::MAX_COPY_PART);
        if ranges.is_empty() {
            self.copy_object(from, to, class).await
        } else {
            self.copy_object_multipart(from, to, &ranges, class).await
        }
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
            // Signed into the URL, so the client *may* satisfy it — and the client is what executes this
            // request, so it can also decline to. See `with_sse_kms`: the bucket policy is the enforcement,
            // this is the cooperation.
            .encrypted_with(self.sse_kms_key_id())
            .presigned(cfg)
            .await
            .map_err(|e| Error::Backend(format!("presign put: {e:?}")))?;
        Ok(req.uri().to_owned())
    }
}

#[async_trait]
impl crate::ResumableStore for S3Store {
    async fn begin_resumable(&self, key: &Key, class: StorageClass) -> Result<String> {
        let mut req = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key.as_str())
            .encrypted_with(self.sse_kms_key_id());
        if self.capabilities.storage_classes {
            req = req.storage_class(Self::to_sdk_class(class));
        }
        req.send()
            .await
            .map_err(|e| self.map_err(key, &e))?
            .upload_id()
            .map(str::to_owned)
            .ok_or_else(|| Error::Backend("create_multipart_upload returned no upload id".into()))
    }

    async fn upload_resumable_part(
        &self,
        key: &Key,
        upload_id: &str,
        part_number: i32,
        body: Bytes,
    ) -> Result<String> {
        self.client
            .upload_part()
            .bucket(&self.bucket)
            .key(key.as_str())
            .upload_id(upload_id)
            .part_number(part_number)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))?
            .e_tag()
            .map(str::to_owned)
            .ok_or_else(|| {
                // Without the ETag the upload cannot be completed, and the session has no way
                // to record the part — so the caller must retry it rather than carry on.
                Error::Backend(format!("part {part_number} of {key} returned no ETag"))
            })
    }

    async fn finish_resumable(
        &self,
        key: &Key,
        upload_id: &str,
        parts: &[crate::resumable::PartRecord],
    ) -> Result<()> {
        if parts.is_empty() {
            return Err(Error::Backend(format!(
                "refusing to complete {key} with no parts"
            )));
        }
        let completed: Vec<_> = parts
            .iter()
            .map(|p| {
                aws_sdk_s3::types::CompletedPart::builder()
                    .part_number(p.number)
                    .e_tag(&p.etag)
                    .build()
            })
            .collect();
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key.as_str())
            .upload_id(upload_id)
            .multipart_upload(
                aws_sdk_s3::types::CompletedMultipartUpload::builder()
                    .set_parts(Some(completed))
                    .build(),
            )
            .send()
            .await
            .map_err(|e| self.map_err(key, &e))
            .map(|_| ())
    }

    async fn abort_resumable(&self, key: &Key, upload_id: &str) -> Result<()> {
        match self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key.as_str())
            .upload_id(upload_id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                // An upload the server has already forgotten is the desired end state, so a
                // retried cleanup must not fail.
                if format!("{e:?}").contains("NoSuchUpload") {
                    Ok(())
                } else {
                    Err(self.map_err(key, &e))
                }
            }
        }
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
