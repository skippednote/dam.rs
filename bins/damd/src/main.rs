//! API and delivery server.

#![forbid(unsafe_code)]

use anyhow::Context;
use dam_core::Config;

fn main() -> anyhow::Result<()> {
    let cfg = Config::load(std::env::var("DAMRS_CONFIG").ok()).context("loading config")?;
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "damd=info,dam_api=info".into());
    // Held for the process lifetime: dropping it flushes pending OTLP spans.
    let _guard = dam_telemetry::init(&cfg.telemetry, &filter).context("initialising telemetry")?;

    tracing::info!(
        environment = ?cfg.environment,
        port = cfg.server.port,
        "damd starting"
    );
    tracing::warn!("damd has no server yet — M1");
    Ok(())
}
