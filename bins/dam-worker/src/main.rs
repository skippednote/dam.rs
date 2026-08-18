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

    let context = dam_pipeline::worker::Context {
        global,
        store,
        indexes,
        worker: worker.clone(),
    };

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

/// The blob store, from configuration. Same resolution as `damd`.
async fn build_store(cfg: &Config) -> anyhow::Result<dam_store::S3Store> {
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
