//! Ephemeral SeaweedFS for tests (D17: real containers, never mocks).
//!
//! Behind the `testing` feature so testcontainers stays out of a production build.
//!
//! There is no testcontainers module for SeaweedFS, so this drives `GenericImage`
//! directly.
//!
//! The container runs **with** an `-s3.config`, even though a throwaway server would work
//! without one. Governance-retention bypass is permission-gated: SeaweedFS only honours
//! `x-amz-bypass-governance-retention` for an identity holding `BypassGovernanceRetention`
//! or `Admin`, and an unconfigured server has no identity to hold it. Without the config
//! the bypass is refused with `AccessDenied` — which looks exactly like retention working,
//! so the test would pass for the wrong reason. Two identities are declared so that both
//! directions are provable: one that may bypass, and one that may not.

use crate::{Error, Result, s3::S3Store};
use std::time::Duration;
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

const IMAGE: &str = "chrislusf/seaweedfs";
/// Pinned rather than `latest`: SeaweedFS's S3 gateway gains capabilities between
/// releases, and a capability appearing under us would make the conformance suite start
/// covering more without anyone deciding to.
///
/// Must be a 4.3x or newer release. Versioning and object lock are recent additions to the
/// S3 gateway — 3.80 answers `PutBucketVersioning` with `501 NotImplemented`, which makes
/// every object-lock case unprovable and, worse, makes a version-scoped delete fail with
/// `AccessDenied` for the wrong reason. See DECISIONS.md D19.
const TAG: &str = "4.42";

const S3_PORT: u16 = 8333;
const MASTER_PORT: u16 = 9333;

const CONFIG_PATH: &str = "/etc/seaweedfs/s3.json";

/// Access key of the identity that may bypass governance retention.
pub const ADMIN_KEY: &str = "damrsdev";
pub const ADMIN_SECRET: &str = "damrsdevsecret";

/// Access key of an identity that can read, write, and *set* retention but may **not**
/// bypass it. This is the shape a normal application credential should have: able to apply
/// a hold, unable to lift one.
pub const LIMITED_KEY: &str = "damrslimited";
pub const LIMITED_SECRET: &str = "damrslimitedsecret";

/// Identity config copied into the container.
///
/// `Admin` is what grants the bypass; the limited identity is given every object-lock
/// action *except* that one, so a refused bypass proves the permission is enforced rather
/// than the operation being unimplemented.
const S3_CONFIG: &str = r#"{
  "identities": [
    {
      "name": "damrs-admin",
      "credentials": [{ "accessKey": "damrsdev", "secretKey": "damrsdevsecret" }],
      "actions": ["Admin", "Read", "Write", "List", "Tagging", "DeleteBucket"]
    },
    {
      "name": "damrs-limited",
      "credentials": [{ "accessKey": "damrslimited", "secretKey": "damrslimitedsecret" }],
      "actions": ["Read", "Write", "List", "Tagging",
                  "GetObjectRetention", "PutObjectRetention",
                  "GetObjectLegalHold", "PutObjectLegalHold"]
    }
  ]
}"#;

/// A running SeaweedFS with one bucket.
///
/// Hold it for the lifetime of the test — dropping it stops the container, and every
/// subsequent request fails with a connection error.
pub struct SeaweedfsHarness {
    endpoint: String,
    bucket: String,
    _container: ContainerAsync<GenericImage>,
}

impl std::fmt::Debug for SeaweedfsHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeaweedfsHarness")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .finish()
    }
}

/// A unique, valid object key for a test.
///
/// Unique per call so tests sharing a container cannot collide, and so a re-run against a
/// surviving container starts clean. `label` is carried through to make a failing
/// assertion say which test wrote the object.
pub fn unique_key(label: &str) -> crate::Key {
    crate::Key::new(format!("test/{label}/{}", uuid::Uuid::new_v4()))
        .unwrap_or_else(|e| panic!("test key for {label:?} must be valid: {e}"))
}

impl SeaweedfsHarness {
    /// Starts a container and creates a bucket.
    pub async fn start() -> Result<Self> {
        Self::start_inner(false).await
    }

    /// Starts a container whose bucket has **object lock enabled**.
    ///
    /// A separate constructor because object lock can only be enabled at bucket creation
    /// and forces versioning on — a bucket cannot be retrofitted, so a test needing a hold
    /// needs its own.
    pub async fn start_with_object_lock() -> Result<Self> {
        Self::start_inner(true).await
    }

    async fn start_inner(object_lock: bool) -> Result<Self> {
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(S3_PORT.tcp())
            .with_exposed_port(MASTER_PORT.tcp())
            // The master's readiness line, on **stderr** — SeaweedFS logs everything
            // there, and waiting on stdout times out having seen nothing at all.
            //
            // This is necessary but not sufficient: the S3 gateway registers volumes for
            // a moment afterwards and returns 500s until it has, which the bucket-create
            // retry below covers. A localhost probe cannot be used as the wait strategy
            // because SeaweedFS binds to the container's own IP, so a request to
            // 127.0.0.1 from inside the container is refused.
            .with_wait_for(WaitFor::message_on_stderr("Start Seaweed Master"))
            .with_copy_to(CONFIG_PATH, S3_CONFIG.as_bytes().to_vec())
            .with_cmd([
                "server",
                "-s3",
                "-dir=/data",
                &format!("-s3.config={CONFIG_PATH}"),
                // Small volumes so the container starts fast; a test writes a few MiB at
                // most.
                "-master.volumeSizeLimitMB=64",
            ])
            .start()
            .await
            .map_err(|e| Error::Backend(format!("starting {IMAGE}:{TAG}: {e}")))?;

        let port = container
            .get_host_port_ipv4(S3_PORT)
            .await
            .map_err(|e| Error::Backend(format!("resolving mapped S3 port: {e}")))?;
        let endpoint = format!("http://127.0.0.1:{port}");
        let bucket = "damrs-test".to_owned();

        let harness = Self {
            endpoint,
            bucket,
            _container: container,
        };
        harness.create_bucket_with_retry(object_lock).await?;
        Ok(harness)
    }

    /// Creates the bucket, retrying while the S3 gateway finishes coming up.
    ///
    /// The master's readiness message is necessary but not sufficient: the gateway
    /// registers volumes for a moment afterwards and returns 500s until it has.
    async fn create_bucket_with_retry(&self, object_lock: bool) -> Result<()> {
        const ATTEMPTS: u32 = 60;
        let store = self.store();
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            let created = if object_lock {
                store.create_bucket_with_object_lock().await
            } else {
                store.create_bucket().await
            };
            match created {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last = e.to_string();
                    tokio::time::sleep(Duration::from_millis(250 * u64::from(attempt.min(4) + 1)))
                        .await;
                }
            }
        }
        Err(Error::Backend(format!(
            "seaweedfs S3 gateway did not accept a bucket create after {ATTEMPTS} attempts: {last}"
        )))
    }

    /// A driver pointed at this container as the admin identity.
    pub fn store(&self) -> S3Store {
        S3Store::seaweedfs(&self.endpoint, &self.bucket, ADMIN_KEY, ADMIN_SECRET)
    }

    /// A driver pointed at this container as an identity that may set a retention but not
    /// bypass one.
    pub fn store_without_bypass_permission(&self) -> S3Store {
        S3Store::seaweedfs(&self.endpoint, &self.bucket, LIMITED_KEY, LIMITED_SECRET)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}
