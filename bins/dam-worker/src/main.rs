//! Job runner: finalisation, derivatives, indexing.

#![forbid(unsafe_code)]

use anyhow::{Context as _, bail};
use dam_core::Config;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load(std::env::var("DAMRS_CONFIG").ok()).context("loading config")?;
    let filter =
        std::env::var("RUST_LOG").unwrap_or_else(|_| "dam_worker=info,dam_pipeline=info".into());
    let _guard = dam_telemetry::init(&cfg.telemetry, &filter).context("initialising telemetry")?;

    cfg.validate().context("validating configuration")?;

    let global = PgPoolOptions::new()
        .max_connections(cfg.database.max_connections)
        .min_connections(cfg.database.min_connections)
        .acquire_timeout(Duration::from_secs(cfg.database.acquire_timeout_secs))
        .connect(cfg.database.url.expose())
        .await
        .context("connecting to postgres")?;

    let store: Arc<dyn dam_store::ResumableStore> = Arc::new(build_store(&cfg).await?);
    let indexes = Arc::new(dam_search::IndexPool::new(
        dam_search::PoolConfig::new(&cfg.search.index_root)
            .with_max_open_indexes(cfg.search.max_open_indexes)
            .with_max_open_writers(cfg.search.max_open_writers)
            .with_writer_memory_bytes(cfg.search.writer_memory_mib * 1024 * 1024),
    ));

    // Host and pid, so two workers on one machine do not share a lease — and so a stuck lease in
    // `dam_global.jobs.locked_by` names something an operator can go and look at.
    let worker = format!(
        "{}#{}",
        hostname().unwrap_or_else(|| "worker".to_owned()),
        std::process::id()
    );

    // The hosted-model half. Built here so a worker that cannot construct an HTTP client says so at startup
    // rather than at the first enrichment, and passed as `Some` unconditionally: whether anything actually runs
    // is the tenant's `enrichment_settings.is_enabled`, not a deployment flag, because two ways to switch one
    // feature off is one way too many.
    let ai = dam_pipeline::enrich::AiContext {
        keyring: cfg.ai.keyring(),
        prices: dam_ai::pricing::Prices::with_overrides(&cfg.ai.prices),
        transport: Arc::new(
            dam_ai::http::HttpTransport::new().context("building the model http client")?,
        ),
    };

    let context = dam_pipeline::worker::Context {
        global,
        store,
        indexes,
        ai: Some(ai),
        http: webhook_client().context("building the webhook http client")?,
        // Built from configuration here rather than inside the pipeline, like the store. `None` when no
        // `clamd` is configured, which scans nothing — see `security.clamd_address`.
        // A signing identity only when both halves are configured. One without the other is a
        // misconfiguration rather than a partial capability, and refusing to start would be worse than
        // rendering unsigned and letting `provenance_gaps` say so.
        signing_identity: match (
            cfg.security.signing_cert_pem.as_deref(),
            cfg.security.signing_key_pem.as_ref(),
        ) {
            (Some(cert), Some(key)) => {
                match dam_media::provenance::SigningIdentity::from_pem(
                    cert.as_bytes(),
                    key.expose().as_bytes(),
                    &cfg.security.signing_algorithm,
                    cfg.security.timestamp_authority.clone(),
                ) {
                    Ok(identity) => {
                        tracing::info!("content credential signing enabled");
                        Some(identity)
                    }
                    // Logged loudly and not fatal: a certificate problem should not stop a library from
                    // producing thumbnails, and the gap is reportable.
                    Err(error) => {
                        tracing::error!(%error, "signing certificate unusable; derivatives will be unsigned");
                        None
                    }
                }
            }
            (None, None) => None,
            _ => {
                tracing::error!(
                    "only one of security.signing_cert_pem and security.signing_key_pem is set; \
                     derivatives will be unsigned"
                );
                None
            }
        },
        scanner: cfg.security.clamd_address.as_deref().map(|address| {
            tracing::info!(%address, "virus scanning enabled");
            dam_media::antivirus::Scanner::new(address, cfg.security.max_scan_bytes)
        }),
        worker: worker.clone(),
    };

    // Permitted configurations that are probably wrong. Said once at startup, because every one of them is
    // invisible afterwards — the failure they describe arrives later and does not mention its cause.
    for advisory in cfg.advisories() {
        tracing::warn!("{advisory}");
    }

    tracing::info!(
        environment = ?cfg.environment,
        worker = %worker,
        index_root = %cfg.search.index_root.display(),
        "dam-worker starting"
    );

    dam_pipeline::worker::run(&context, shutdown()).await;
    tracing::info!("dam-worker stopped");
    Ok(())
}

/// The blob store, from configuration. Same resolution as `damd`, including the customer-managed key —
/// which matters more here than there: the worker is what promotes, derives and tiers, so a store built
/// without the key would write most of a library under the bucket default while the API's writes were
/// correctly encrypted.
async fn build_store(cfg: &Config) -> anyhow::Result<dam_store::S3Store> {
    let store = build_store_inner(cfg).await?;
    Ok(match cfg.storage.sse_kms_key_id.as_deref() {
        Some(key) => store.with_sse_kms(key),
        None => store,
    })
}

async fn build_store_inner(cfg: &Config) -> anyhow::Result<dam_store::S3Store> {
    match cfg.storage.endpoint.as_deref() {
        None => Ok(dam_store::S3Store::aws(&cfg.storage.bucket, &cfg.storage.region).await),
        Some(endpoint) => {
            let (Some(access), Some(secret)) = (
                cfg.storage.access_key_id.as_ref(),
                cfg.storage.secret_access_key.as_ref(),
            ) else {
                bail!(
                    "storage.endpoint is set to {endpoint} but no credentials are configured; set \
                     storage.access_key_id and storage.secret_access_key"
                );
            };
            Ok(dam_store::S3Store::seaweedfs(
                endpoint,
                &cfg.storage.bucket,
                access.expose(),
                secret.expose(),
            ))
        }
    }
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Resolves on SIGINT or SIGTERM.
///
/// The client the webhook dispatcher sends with.
///
/// One per process. A `reqwest::Client` is a connection pool, so building one per delivery would pay a fresh
/// TLS handshake per webhook — for a tenant publishing in bulk that is most of the cost of the operation.
///
/// `redirect::Policy::none()` is the load-bearing setting: a redirect from a webhook endpoint is a
/// misconfiguration, and following one would post a customer's data to a host they never nominated.
/// `dam_connect::webhooks::classify` turns the 3xx into a rejection with that sentence in the delivery log.
///
/// Propagated rather than defaulted, and that is the point of it being a function. `unwrap_or_default()` here
/// would fall back to a client that *does* follow redirects — silently undoing the one security property this
/// builder exists to set. A worker that cannot build its client should refuse to start.
fn webhook_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("damrs/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// SIGTERM as well as SIGINT, because a container runtime sends SIGTERM — and a worker killed rather than
/// drained leaves its claimed jobs locked until the lease lapses, which is a two-minute stall on every deploy
/// for whatever was in flight.
async fn shutdown() {
    let interrupt = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM; only Ctrl-C will drain");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("SIGINT — draining"),
        () = terminate => tracing::info!("SIGTERM — draining"),
    }
}
