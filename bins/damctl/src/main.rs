//! Admin CLI: migrate, provision-tenant, reindex, backfill, import.

#![forbid(unsafe_code)]

use anyhow::Context;
use clap::{Parser, Subcommand};
use dam_core::Config;

#[derive(Parser, Debug)]
#[command(name = "damctl", version, about = "damrs administration")]
struct Cli {
    /// Path to a TOML config file. Env vars still override it.
    #[arg(long, env = "DAMRS_CONFIG")]
    config: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Apply global and tenant migrations.
    Migrate {
        /// Apply to every tenant as well as the control plane.
        #[arg(long)]
        all: bool,
    },
    /// Create a tenant: row, schema, migrations, seeded defaults.
    ProvisionTenant {
        #[arg(long)]
        slug: String,
    },
    /// Print the resolved configuration. Secrets stay redacted.
    Config,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_ref()).context("loading config")?;
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "damctl=info,dam_db=info".into());
    let _guard = dam_telemetry::init(&cfg.telemetry, &filter).context("initialising telemetry")?;

    match cli.command {
        Command::Migrate { all } => {
            tracing::info!(all, "migrate is not implemented yet — 0.5");
        }
        Command::ProvisionTenant { slug } => {
            tracing::info!(%slug, "provision-tenant is not implemented yet — 0.8");
        }
        Command::Config => {
            // Debug on Config is safe by construction: every secret is a
            // `Secret<T>`, which refuses to render. Asserted in dam-core's tests.
            println!("{cfg:#?}");
        }
    }
    Ok(())
}
