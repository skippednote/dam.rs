//! Admin CLI: migrate, provision-tenant, reindex, backfill, import.

#![forbid(unsafe_code)]

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use dam_core::{Config, TenantSlug};
use sqlx::postgres::PgPoolOptions;

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
    /// Create a tenant: row, schema, migrations, seeded defaults. Idempotent.
    ProvisionTenant {
        #[arg(long)]
        slug: String,
        /// Human-readable name. Defaults to the slug.
        #[arg(long)]
        name: Option<String>,
    },
    /// Print the resolved configuration. Secrets stay redacted.
    Config,

    /// Print the OpenAPI document, or write it to `openapi.json`.
    ///
    /// The document is checked in so the wire contract appears in review diffs, and the test suite
    /// asserts the checked-in copy matches this output — so a forgotten regeneration fails the build
    /// rather than shipping a client that disagrees with the server.
    Openapi {
        /// Write to openapi.json at the repository root instead of stdout.
        #[arg(long)]
        write: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_ref()).context("loading config")?;
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "damctl=info,dam_db=info".into());
    let _guard = dam_telemetry::init(&cfg.telemetry, &filter).context("initialising telemetry")?;

    match cli.command {
        Command::Migrate { all } => {
            let url = cfg.database.url.expose();
            let pool = connect(url, &cfg).await?;
            bootstrap(&pool).await.context("bootstrapping schemas")?;

            dam_db::migrate::global(url)
                .await
                .context("global migrations")?;
            tracing::info!(
                count = dam_db::migrate::global_migration_count(),
                "global migrations applied"
            );

            dam_db::migrate::template(url)
                .await
                .context("template migrations")?;

            if all {
                // Each tenant is migrated independently and a failure is recorded
                // rather than aborting the fleet — one tenant's bad state must not
                // block everyone else's upgrade (§5.3).
                let slugs: Vec<String> =
                    sqlx::query_scalar("SELECT slug FROM dam_global.tenants ORDER BY slug")
                        .fetch_all(&pool)
                        .await
                        .context("listing tenants")?;
                let mut failed = 0usize;
                for raw in &slugs {
                    let Ok(slug) = TenantSlug::new(raw) else {
                        tracing::error!(slug = %raw, "stored slug is not valid — skipping");
                        failed += 1;
                        continue;
                    };
                    match dam_db::migrate::tenant(url, &slug.schema_name()).await {
                        Ok(()) => tracing::info!(slug = %slug, "migrated"),
                        Err(e) => {
                            failed += 1;
                            tracing::error!(slug = %slug, error = %e, "migration failed");
                            sqlx::query(
                                "UPDATE dam_global.tenants SET status = 'migration_failed' \
                                 WHERE slug = $1",
                            )
                            .bind(raw)
                            .execute(&pool)
                            .await
                            .context("marking tenant failed")?;
                        }
                    }
                }
                tracing::info!(tenants = slugs.len(), failed, "tenant migrations complete");
                if failed > 0 {
                    bail!("{failed} of {} tenants failed to migrate", slugs.len());
                }
            }
        }

        Command::ProvisionTenant { slug, name } => {
            let slug = TenantSlug::new(&slug)
                .context("slug must match ^[a-z][a-z0-9_]{1,38}$ and not be reserved")?;
            let url = cfg.database.url.expose();
            let pool = connect(url, &cfg).await?;
            bootstrap(&pool).await.context("bootstrapping schemas")?;
            dam_db::migrate::global(url)
                .await
                .context("global migrations")?;

            let display = name.unwrap_or_else(|| slug.as_str().to_owned());
            let tenant = dam_db::provision::tenant(&pool, url, &slug, &display)
                .await
                .context("provisioning tenant")?;

            tracing::info!(
                tenant_id = %tenant.id,
                slug = %tenant.slug,
                schema = %tenant.schema_name,
                "tenant ready"
            );
            println!("{}\t{}\t{}", tenant.id, tenant.slug, tenant.schema_name);
        }

        Command::Openapi { write } => {
            let json =
                dam_api::openapi::document_json().context("serialising the OpenAPI document")?;
            if write {
                let path = std::path::Path::new("openapi.json");
                std::fs::write(path, &json)
                    .with_context(|| format!("writing {}", path.display()))?;
                println!("wrote {} ({} bytes)", path.display(), json.len());
            } else {
                print!("{json}");
            }
        }

        Command::Config => {
            // Debug on Config is safe by construction: every secret is a
            // `Secret<T>`, which refuses to render. Asserted in dam-core's tests.
            println!("{cfg:#?}");
        }
    }
    Ok(())
}

async fn connect(url: &str, cfg: &Config) -> anyhow::Result<sqlx::PgPool> {
    PgPoolOptions::new()
        .max_connections(cfg.database.max_connections)
        .connect(url)
        .await
        .context("connecting to postgres")
}

/// The §5.3 bootstrap. Must run before the migrator: Postgres silently ignores
/// nonexistent schemas in a `search_path`, so a missing `dam_global` would send the
/// sqlx ledger somewhere else and every later run would think the database was
/// unmigrated.
async fn bootstrap(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    for stmt in [
        "CREATE SCHEMA IF NOT EXISTS dam_global",
        "CREATE SCHEMA IF NOT EXISTS extensions",
        "CREATE SCHEMA IF NOT EXISTS tenant_template",
        "CREATE EXTENSION IF NOT EXISTS vector SCHEMA extensions",
        "CREATE EXTENSION IF NOT EXISTS ltree SCHEMA extensions",
        "CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA extensions",
    ] {
        sqlx::raw_sql(stmt)
            .execute(pool)
            .await
            .with_context(|| format!("bootstrap: {stmt}"))?;
    }
    Ok(())
}
