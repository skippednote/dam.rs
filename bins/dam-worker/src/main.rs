//! Job runner: derivatives, enrichment, indexing, lifecycle.

#![forbid(unsafe_code)]

use anyhow::Context;
use dam_core::Config;

fn main() -> anyhow::Result<()> {
    let cfg = Config::load(std::env::var("DAMRS_CONFIG").ok()).context("loading config")?;
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "dam_worker=info".into());
    let _guard = dam_telemetry::init(&cfg.telemetry, &filter).context("initialising telemetry")?;

    tracing::info!(environment = ?cfg.environment, "dam-worker starting");
    tracing::warn!("dam-worker has no queue consumer yet — 0.9");
    Ok(())
}
