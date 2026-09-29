//! API and delivery server.

#![forbid(unsafe_code)]

use anyhow::{Context, bail};
use dam_core::{Config, Secret};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::Duration;

/// The key id every URL this process signs carries.
///
/// One id, and it is in the token: rotation means adding the new key as current and the old one as retired,
/// so URLs already in flight keep verifying while new ones use the new key. A signing scheme with no key id
/// cannot be rotated without invalidating every outstanding URL at once.
const SIGNING_KEY_ID: &str = "k1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load(std::env::var("DAMRS_CONFIG").ok()).context("loading config")?;
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "damd=info,dam_api=info".into());
    // Held for the process lifetime: dropping it flushes pending OTLP spans.
    let _guard = dam_telemetry::init(&cfg.telemetry, &filter).context("initialising telemetry")?;

    // Before anything binds a port. A production deployment running on the development signing key would
    // issue delivery URLs anybody with the repository can forge, and finding that out at startup is the only
    // useful time to find it out.
    cfg.validate().context("validating configuration")?;

    let global = PgPoolOptions::new()
        .max_connections(cfg.database.max_connections)
        .min_connections(cfg.database.min_connections)
        .acquire_timeout(Duration::from_secs(cfg.database.acquire_timeout_secs))
        .connect(cfg.database.url.expose())
        .await
        .context("connecting to postgres")?;

    let store = Arc::new(build_store(&cfg).await?);
    let indexes = Arc::new(dam_search::IndexPool::new(
        dam_search::PoolConfig::new(&cfg.search.index_root)
            .with_max_open_indexes(cfg.search.max_open_indexes)
            .with_max_open_writers(cfg.search.max_open_writers)
            .with_writer_memory_bytes(cfg.search.writer_memory_mib * 1024 * 1024),
    ));

    // Cloned before the closure below takes it: the public origin is also what the delivery URLs use, and the
    // MCP transport validates `Host` against it.
    let public_url = cfg.server.public_url.clone();
    let app = dam_api::app::router(
        &cfg,
        dam_api::app::AppDeps {
            global,
            store: Arc::clone(&store) as Arc<dyn dam_store::ResumableStore>,
            delivery_store: store as Arc<dyn dam_store::BlobStore>,
            indexes,
            keyring: dam_core::signed_url::Keyring::single(
                SIGNING_KEY_ID,
                Secret::new(cfg.server.url_signing_key.expose().to_owned()),
            ),
            // The real thing, here and only here: every test drives a recorded transport instead. A failure to
            // build it is a TLS stack that cannot initialise, which is a reason not to start rather than a
            // surprise on the first enrichment.
            model_transport: Arc::new(
                dam_ai::http::HttpTransport::new().context("building the model http client")?,
            ),
            // The MCP server, wired here because `dam-mcp` depends on `dam-api` — it calls the REST handlers
            // rather than reimplementing them, which is what §8.5's "the same ABAC layer" means in practice.
            protocols: cfg.server.mcp_enabled.then(|| {
                let build: Box<dyn FnOnce(_, _) -> _> = Box::new(move |search, downloads| {
                    dam_mcp::router(
                        Arc::new(dam_mcp::McpState { search, downloads }),
                        public_url.as_deref(),
                    )
                });
                build
            }),
        },
    );

    // Permitted configurations that are probably wrong. Said once at startup, because every one of them is
    // invisible afterwards — the failure they describe arrives later and does not mention its cause.
    for advisory in cfg.advisories() {
        tracing::warn!("{advisory}");
    }

    let address = format!("{}:{}", cfg.server.host, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("binding {address}"))?;
    // Logged after the bind rather than before, so the line means "reachable" rather than "about to try".
    tracing::info!(
        environment = ?cfg.environment,
        address = %listener.local_addr().map_or_else(|_| address.clone(), |a| a.to_string()),
        "damd listening"
    );

    // `ConnectInfo` so the rate limiter can key on the peer address. Without it the extractor fails and
    // `throttle::limit` allows everything — which is why there is a test asserting a public route is actually
    // limited, rather than trusting that this line stays here.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .context("serving")?;
    tracing::info!("damd stopped");
    Ok(())
}

/// The blob store, from configuration.
///
/// The customer-managed key is applied to whichever store gets built, at the end, so a driver added later
/// cannot arrive without it — the alternative is `.with_sse_kms` at each `Ok(...)` and a fourth branch that
/// forgets. See `S3Store::with_sse_kms` for what setting it does and does not guarantee.
async fn build_store(cfg: &Config) -> anyhow::Result<dam_store::S3Store> {
    let store = build_store_inner(cfg).await?;
    Ok(match cfg.storage.sse_kms_key_id.as_deref() {
        Some(key) => store.with_sse_kms(key),
        None => store,
    })
}

async fn build_store_inner(cfg: &Config) -> anyhow::Result<dam_store::S3Store> {
    match cfg.storage.endpoint.as_deref() {
        // No endpoint means AWS, which takes its credentials from the environment's provider chain —
        // instance role, SSO, or web identity. Static keys are for the self-hosted case below.
        None => {
            let store = dam_store::S3Store::aws(&cfg.storage.bucket, &cfg.storage.region).await;
            // Inventory is AWS-only, so it is wired here rather than in `build_store` alongside the CMK:
            // pointing the SeaweedFS branch at a prefix that never fills would make the scrub verify
            // nothing. `config.advisories()` warns if a prefix is set against an endpoint anyway.
            Ok(match cfg.storage.inventory_prefix.as_deref() {
                Some(prefix) => store.with_inventory_prefix(prefix),
                None => store,
            })
        }
        Some(endpoint) => {
            let (Some(access), Some(secret)) = (
                cfg.storage.access_key_id.as_ref(),
                cfg.storage.secret_access_key.as_ref(),
            ) else {
                // Refused rather than attempted anonymously: an anonymous client against a self-hosted
                // endpoint fails on the first write with a permissions error that sends somebody to the
                // bucket policy instead of to the missing credential.
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

/// Resolves on SIGINT or SIGTERM.
///
/// SIGTERM as well as SIGINT, because a container runtime sends SIGTERM — a server that only handled Ctrl-C
/// would be killed rather than drained on every deploy, and an in-flight upload would fail for no reason a
/// user could see.
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
