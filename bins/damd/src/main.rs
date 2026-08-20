//! API and delivery server.

#![forbid(unsafe_code)]

use anyhow::{Context, bail};
use dam_core::{Config, Secret, TenantSlug};
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

    // Delivery resolves its tenant from configuration for now rather than from the token. Recorded here
    // rather than hidden: a single-tenant deployment is the shape this serves, and 3.x makes the tenant part
    // of the signed claim so one process can deliver for many.
    let (delivery_tenant, delivery_slug) =
        delivery_tenant(&global, cfg.server.delivery_tenant.as_deref()).await?;

    // Pinned to the delivery tenant's schema. The delivery route reads `assets`, `derivatives`, the rights
    // tables and `share_links` unqualified, so the global pool resolves none of them — it failed with
    // `relation "derivatives" does not exist` the first time there was a real derivative to serve.
    let delivery_pool = dam_db::tenant_conn::single_tenant_pool(
        cfg.database.url.expose(),
        &delivery_slug,
        cfg.database.max_connections.min(8),
    )
    .await
    .context("connecting to the delivery tenant's schema")?;

    // Cloned before the closure below takes it: the public origin is also what the delivery URLs use, and the
    // MCP transport validates `Host` against it.
    let public_url = cfg.server.public_url.clone();
    let app = dam_api::app::router(
        &cfg,
        dam_api::app::AppDeps {
            global,
            delivery_pool,
            store: Arc::clone(&store) as Arc<dyn dam_store::ResumableStore>,
            delivery_store: store as Arc<dyn dam_store::BlobStore>,
            indexes,
            keyring: dam_core::signed_url::Keyring::single(
                SIGNING_KEY_ID,
                Secret::new(cfg.server.url_signing_key.expose().to_owned()),
            ),
            delivery_tenant,
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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving")?;
    tracing::info!("damd stopped");
    Ok(())
}

/// The blob store, from configuration.
async fn build_store(cfg: &Config) -> anyhow::Result<dam_store::S3Store> {
    match cfg.storage.endpoint.as_deref() {
        // No endpoint means AWS, which takes its credentials from the environment's provider chain —
        // instance role, SSO, or web identity. Static keys are for the self-hosted case below.
        None => Ok(dam_store::S3Store::aws(&cfg.storage.bucket, &cfg.storage.region).await),
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

/// The tenant the delivery routes serve.
///
/// Named in configuration when there is one to name, and inferred only when the answer is unambiguous. A
/// deployment with several tenants and a delivery path that silently picked the first would serve one
/// tenant's derivatives under another's URLs — the worst available failure, and one that would read as a
/// caching bug rather than a cross-tenant read.
async fn delivery_tenant(
    global: &sqlx::PgPool,
    configured: Option<&str>,
) -> anyhow::Result<(uuid::Uuid, TenantSlug)> {
    if let Some(slug) = configured {
        let slug = TenantSlug::new(slug).context("server.delivery_tenant")?;
        let id: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM dam_global.tenants WHERE slug = $1 AND status = 'active'",
        )
        .bind(slug.as_str())
        .fetch_optional(global)
        .await
        .context("looking up the delivery tenant")?;

        return id.map(|id| (id, slug.clone())).ok_or_else(|| {
            anyhow::anyhow!("server.delivery_tenant names {slug}, which is not an active tenant")
        });
    }

    let slugs: Vec<String> = sqlx::query_scalar(
        "SELECT slug FROM dam_global.tenants WHERE status = 'active' ORDER BY slug",
    )
    .fetch_all(global)
    .await
    .context("listing tenants")?;

    match slugs.as_slice() {
        [only] => {
            let slug = TenantSlug::new(only).context("stored tenant slug")?;
            let id: uuid::Uuid =
                sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
                    .bind(slug.as_str())
                    .fetch_one(global)
                    .await
                    .context("resolving the only tenant")?;
            Ok((id, slug))
        }
        [] => bail!("no active tenant; run `damctl provision-tenant --slug <slug>` first"),
        many => bail!(
            "{} active tenants ({}), and the delivery path resolves its tenant from configuration rather \
             than from the signed token (3.x). Serving several from one process would mint URLs for the \
             wrong tenant's objects, so this refuses rather than guessing — set \
             DAMRS_SERVER__DELIVERY_TENANT (or server.delivery_tenant) to the slug this process serves.",
            many.len(),
            many.join(", ")
        ),
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
