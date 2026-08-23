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
    /// Print the daily usage rollup for one tenant, or for the whole fleet (M6c).
    ///
    /// Operator-facing, and there is deliberately no API route onto this. `tenant_usage_daily` is not scoped
    /// to a reader — it is the tenant's bill, and a bill narrowed to what one person can see is not a bill.
    /// The tenant-facing view of the same activity is `/insights`, where every number *is* scoped.
    Usage {
        /// One tenant. Omitted, every active tenant, which is the fleet view the table exists for.
        #[arg(long)]
        tenant: Option<String>,
        /// How many days back to read, ending today.
        #[arg(long, default_value_t = 30)]
        days: i64,
    },

    /// Print the resolved configuration. Secrets stay redacted.
    Config,

    /// Issue an API key for a tenant, printing the plaintext exactly once.
    ///
    /// The plaintext is never stored — only a hash — so it cannot be recovered afterwards. That is
    /// deliberate: a key an operator can read back out of the database is a key a database backup
    /// hands over.
    IssueKey {
        #[arg(long)]
        tenant: String,
        /// The person the key acts as. Created if absent, because a key with no identity behind it has
        /// no membership and therefore no grants at all — fail-closed, and confusing if unexplained.
        #[arg(long)]
        email: String,
        /// A label, so a key can be recognised in an audit later.
        #[arg(long, default_value = "damctl")]
        name: String,
        /// Restrict the key to these permission strings. Empty means unscoped, which is *not* the same as
        /// unlimited: the identity's roles still bound it.
        #[arg(long, value_delimiter = ',')]
        scope: Vec<String>,
        /// Make the identity a tenant administrator.
        #[arg(long)]
        admin: bool,
    },

    /// Rebuild a tenant's search index from Postgres.
    ///
    /// Postgres is the record and the index is derived, so this is the command that regenerates the
    /// derived thing. It replaces the index in one commit: a reader sees the old index or the new one,
    /// never a fraction of the library.
    /// Take a logical backup of one tenant's schema and upload it (§17, G11).
    ///
    /// A dump, not a base backup: per-tenant restore is the case worth having, and a physical backup cannot
    /// do it at all. This complements WAL archiving rather than replacing it — the five-minute RPO comes from
    /// the WAL, and this is the half that makes a single customer recoverable without touching anyone else.
    Backup {
        /// The tenant to back up. Omit for every active tenant.
        #[arg(long)]
        tenant: Option<String>,
    },
    /// Restore the latest backup into a scratch schema and check it (§17, G11).
    ///
    /// The only thing that writes `dr_state.last_verified_restore_at`, because §17's argument is that "the gap
    /// between 'we take backups' and 'we have restored one' is where DR plans fail". A successful backup must
    /// never claim a verified restore.
    RestoreDrill {
        #[arg(long)]
        tenant: String,
    },
    /// Report which tenants have never had a restore verified.
    ///
    /// Unverified first, because §17 says that list "should be short" and a report that buries them among the
    /// healthy rows is one nobody reads to the end.
    DrReport,
    Reindex {
        #[arg(long)]
        tenant: String,
        /// Rows per round trip.
        #[arg(long, default_value_t = dam_search::reindex::DEFAULT_BATCH)]
        batch: usize,
    },

    /// Score the tenant's relevance judgements against the live search path (G8).
    ///
    /// The point of the harness is that a ranking change reports its effect instead of being argued
    /// about, so this is meant to be run before and after one. It exits non-zero when `--min-ndcg` is
    /// not met, and also when any query in the corpus could not be run at all — a corpus that quietly
    /// dropped its broken queries would score *better* the more of it broke.
    Eval {
        #[arg(long)]
        tenant: String,
        /// Scoring depth. nDCG@10 by default; a depth that varied between runs would make them
        /// incomparable.
        #[arg(long, default_value_t = dam_search::eval_run::DEFAULT_AT)]
        at: usize,
        /// Fail if the mean nDCG falls below this. For a CI gate on a ranking change.
        #[arg(long)]
        min_ndcg: Option<f64>,
    },

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
            // From this deployment's configuration, so a provisioned tenant's pool matches the store `damd`
            // and the worker actually talk to. Passed in rather than read inside `dam-db`, which has no
            // business knowing how the deployment is configured.
            let storage = dam_db::provision::StoragePool {
                endpoint: cfg.storage.endpoint.as_deref(),
                region: &cfg.storage.region,
                bucket: &cfg.storage.bucket,
                force_path_style: cfg.storage.force_path_style,
                // A reference, never the secret. `damd` and the worker read the credential from configuration;
                // this column exists so an operator can see *which* credential a pool uses.
                credentials_ref: "config:storage",
            };

            let tenant = dam_db::provision::tenant(&pool, url, &slug, &display, &storage)
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

        Command::IssueKey {
            tenant,
            email,
            name,
            scope,
            admin,
        } => {
            let slug = TenantSlug::new(&tenant).context("tenant slug")?;
            let url = cfg.database.url.expose();
            let pool = connect(url, &cfg).await?;

            let tenant_id: uuid::Uuid =
                sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
                    .bind(slug.as_str())
                    .fetch_optional(&pool)
                    .await
                    .context("looking up the tenant")?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no tenant {slug}; run `damctl provision-tenant --slug {slug}` first"
                        )
                    })?;

            // Idempotent on the email, so re-running does not create a second identity for one person —
            // which would leave their roles on the first and their key on the second. The unique index is on
            // the *generated* `email_lower` column, so that is what `ON CONFLICT` has to name: addresses are
            // case-insensitive in practice and "Dev@" and "dev@" are one person.
            let identity_id: uuid::Uuid = sqlx::query_scalar(
                "INSERT INTO dam_global.identities (id, email, display_name) \
                 VALUES (gen_random_uuid(), $1, $1) \
                 ON CONFLICT (email_lower) DO UPDATE SET updated_at = now() RETURNING id",
            )
            .bind(&email)
            .fetch_one(&pool)
            .await
            .context("creating the identity")?;

            sqlx::query(
                "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
                 VALUES ($1, $2, '{}', $3) \
                 ON CONFLICT (tenant_id, identity_id) DO UPDATE SET is_tenant_admin = excluded.is_tenant_admin",
            )
            .bind(tenant_id)
            .bind(identity_id)
            .bind(admin)
            .execute(&pool)
            .await
            .context("recording the membership")?;

            let key = dam_db::auth::ApiKey::generate();
            sqlx::query(
                "INSERT INTO dam_global.api_keys \
                 (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
                 VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6)",
            )
            .bind(tenant_id)
            .bind(identity_id)
            .bind(&name)
            .bind(key.prefix())
            .bind(key.hash())
            .bind(&scope)
            .execute(&pool)
            .await
            .context("storing the key")?;

            // The prefix goes to the log; the secret goes to stdout and nowhere else. A key in a log file is a
            // key in every log aggregator it was shipped to.
            tracing::info!(
                tenant = %slug,
                identity = %identity_id,
                prefix = key.prefix(),
                admin,
                "api key issued"
            );
            println!("{}", key.into_plaintext());
        }

        Command::Backup { tenant } => {
            let url = cfg.database.url.expose();
            let pool = connect(url, &cfg).await?;
            let store = build_store(&cfg).await?;
            let tools = dam_backup::tools::Toolchain::discover().context(
                "locating pg_dump; it is pinned in .mise.toml and in the container image",
            )?;

            let slugs = match tenant {
                Some(one) => vec![TenantSlug::new(&one).context("tenant slug")?],
                None => active_tenants(&pool).await?,
            };
            if slugs.is_empty() {
                println!("no active tenants");
            }
            // Each tenant on its own, and a failure on one does not abandon the rest: a backup run that
            // stops at the first problem leaves every tenant after it in the alphabet unprotected, and
            // whichever one failed is the one an operator needs to hear about rather than the only one they
            // hear about.
            let mut failed = 0;
            for slug in &slugs {
                match dam_backup::backup_tenant(
                    &pool,
                    &store,
                    &tools,
                    url,
                    slug,
                    chrono::Utc::now(),
                )
                .await
                {
                    Ok(backup) => println!(
                        "{}\t{}\t{} bytes\t{} assets",
                        backup.slug, backup.key, backup.bytes, backup.asset_count
                    ),
                    Err(error) => {
                        failed += 1;
                        eprintln!("{}\tFAILED\t{error}", slug.as_str());
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} of {} tenants failed to back up", slugs.len());
            }
        }

        Command::RestoreDrill { tenant } => {
            let slug = TenantSlug::new(&tenant).context("tenant slug")?;
            let url = cfg.database.url.expose();
            let pool = connect(url, &cfg).await?;
            let store = build_store(&cfg).await?;
            let tools = dam_backup::tools::Toolchain::discover().context("locating pg_restore")?;

            let drill =
                dam_backup::restore_drill(&pool, &store, &tools, url, &slug, chrono::Utc::now())
                    .await
                    .context("the restore drill failed — this is the outcome that matters")?;
            println!("restored from\t{}", drill.from_key);
            println!("assets\t{}", drill.restored_assets);
            println!("seconds\t{}", drill.duration_seconds);
        }

        Command::DrReport => {
            let pool = connect(cfg.database.url.expose(), &cfg).await?;
            let rows = dam_backup::state::report(&pool)
                .await
                .context("reading dr_state")?;
            println!("tenant\tlast_backup\tlast_verified_restore\tseconds");
            for row in &rows {
                println!(
                    "{}\t{}\t{}\t{}",
                    row.slug,
                    row.last_backup_at
                        .map_or_else(|| "never".to_owned(), |at| at.to_rfc3339()),
                    row.last_verified_restore_at
                        .map_or_else(|| "NEVER".to_owned(), |at| at.to_rfc3339()),
                    row.verified_restore_duration_s
                        .map_or_else(|| "-".to_owned(), |s| s.to_string()),
                );
            }
            let unverified = rows
                .iter()
                .filter(|row| row.last_verified_restore_at.is_none())
                .count();
            if unverified > 0 {
                // A non-zero exit, so this is usable as a check rather than only as a thing to read. A DR
                // report that cannot fail a pipeline is a DR report that gets skimmed.
                bail!(
                    "{unverified} of {} tenants have never had a restore verified",
                    rows.len()
                );
            }
        }

        Command::Reindex { tenant, batch } => {
            let slug = TenantSlug::new(&tenant).context("tenant slug")?;
            let url = cfg.database.url.expose();
            let tenant_pool =
                dam_db::tenant_conn::single_tenant_pool(url, &slug, cfg.database.max_connections)
                    .await
                    .context("connecting to the tenant schema")?;
            let defs = dam_db::fields::load(&tenant_pool)
                .await
                .context("loading the tenant's field definitions")?;
            let index_schema = dam_search::IndexSchema::new(defs);
            let indexes = search_pool(&cfg);

            let stats =
                dam_search::reindex::tenant(&tenant_pool, &indexes, &slug, &index_schema, batch)
                    .await
                    .context("reindexing")?;

            tracing::info!(
                tenant = %slug,
                indexed = stats.indexed,
                tombstones = stats.tombstones,
                "reindex complete"
            );
            println!("indexed\t{}", stats.indexed);
            println!("tombstones\t{}", stats.tombstones);
        }

        Command::Eval {
            tenant,
            at,
            min_ndcg,
        } => {
            let slug = TenantSlug::new(&tenant).context("tenant slug")?;
            let url = cfg.database.url.expose();

            // Schema-scoped, like every other tenant read: the judgement corpus lives in the tenant's own
            // schema so a customer's labels measure their own library rather than somebody else's
            // vocabulary.
            let tenant_pool =
                dam_db::tenant_conn::single_tenant_pool(url, &slug, cfg.database.max_connections)
                    .await
                    .context("connecting to the tenant schema")?;

            let corpus = dam_db::judgements::corpus(&tenant_pool)
                .await
                .context("loading the judgement corpus")?;
            let parse_schema = dam_db::fields::search_schema(&tenant_pool)
                .await
                .context("loading the tenant's field definitions")?;
            let index_schema = dam_search::IndexSchema::new(parse_schema.fields().to_vec());
            let indexes = search_pool(&cfg);

            // Unrestricted, and said out loud: a run under a restricted scope measures the ranking and
            // the access filter together, and a regression that turns out to be a permission change is a
            // different bug from a regression in relevance.
            let access = dam_core::policy::compile(
                &dam_core::policy::Grants::from(vec![dam_core::policy::Grant {
                    permissions: vec!["asset:read".to_owned()],
                    asset_group_ids: vec![],
                    all_asset_groups: true,
                    valid_from: None,
                    valid_until: None,
                    requires_eula: false,
                    eula_accepted: true,
                }]),
                dam_core::policy::Action::Read,
                chrono::Utc::now(),
            );

            let queries = corpus.len();
            let run = dam_search::eval_run::run(
                &indexes,
                &slug,
                &index_schema,
                &parse_schema,
                &access,
                corpus,
                at,
            )
            .await
            .context("scoring the corpus")?;

            println!("tenant\t{slug}");
            println!("judged queries\t{queries}");
            println!("scored\t{}", run.report.scoreable);
            println!("unscoreable\t{}", run.report.unscoreable);
            println!("refused\t{}", run.refused.len());
            println!("at\t{}", run.at);
            println!("mean nDCG\t{}", render_metric(run.report.mean_ndcg));
            println!("MRR\t{}", render_metric(run.report.mrr));
            for query in &run.report.queries {
                println!(
                    "query\t{}\tndcg={}\trr={}\tjudged={}\tunjudged_returned={}",
                    query.query_text,
                    render_metric(query.ndcg),
                    render_metric(query.reciprocal_rank),
                    query.judged_total,
                    query.unjudged_returned
                );
            }
            for refusal in &run.refused {
                println!("refused\t{}\t{}", refusal.query_text, refusal.reason);
            }

            if !run.refused.is_empty() {
                bail!(
                    "{} of {queries} queries could not be run; the mean is over a different sample than \
                     a clean run and must not be compared to one",
                    run.refused.len()
                );
            }
            if let Some(floor) = min_ndcg {
                match run.report.mean_ndcg {
                    // `None` fails the gate rather than passing it: an unlabelled corpus cannot clear a
                    // floor, and treating "nothing to measure" as a pass is how a gate stops gating.
                    None => bail!("no query was scoreable, so --min-ndcg {floor} cannot be met"),
                    Some(mean) if mean < floor => {
                        bail!("mean nDCG {mean:.4} is below the --min-ndcg floor of {floor}")
                    }
                    Some(_) => {}
                }
            }
        }

        Command::Usage { tenant, days } => {
            let pool = connect(cfg.database.url.expose(), &cfg).await?;
            let today = chrono::Utc::now().date_naive();
            let from = today - chrono::Duration::days(days.clamp(1, 3650) - 1);

            // Resolved here rather than inside `dam_db::metering`, which takes ids: D2 means there is no join
            // that could do this, so the fleet view is a loop over tenants by construction.
            let tenants: Vec<(uuid::Uuid, String)> = match &tenant {
                Some(slug) => {
                    sqlx::query_as("SELECT id, slug FROM dam_global.tenants WHERE slug = $1")
                        .bind(slug)
                        .fetch_all(&pool)
                        .await
                        .context("looking up the tenant")?
                }
                None => sqlx::query_as(
                    "SELECT id, slug FROM dam_global.tenants WHERE status = 'active' ORDER BY slug",
                )
                .fetch_all(&pool)
                .await
                .context("listing tenants")?,
            };
            if tenants.is_empty() {
                bail!(
                    "no such tenant{}",
                    tenant.map_or_else(String::new, |slug| format!(": {slug}"))
                );
            }

            println!("tenant\tday\tassets\tstored_bytes\tdownloads\trestores\tai_tokens\tcents");
            let mut rows_printed = 0usize;
            for (tenant_id, slug) in &tenants {
                let rows = dam_db::metering::window(&pool, *tenant_id, from, today)
                    .await
                    .with_context(|| format!("reading usage for {slug}"))?;
                for row in &rows {
                    // Summed across classes for the one-line view. The per-class breakdown is in the column
                    // and an operator who needs it can read the JSON; a terminal table with a column per
                    // storage class would have seven of them, mostly empty.
                    let stored = row.totals.stored_bytes();
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        slug,
                        row.day,
                        row.totals.asset_count,
                        stored,
                        row.totals.downloads,
                        row.totals.restores,
                        row.totals.ai_input_tokens + row.totals.ai_output_tokens,
                        row.totals.est_cost_cents,
                    );
                }
                rows_printed += rows.len();
            }

            if rows_printed == 0 {
                // Said rather than printed as an empty table, because "no rows" here has one likely cause and
                // it is worth naming: nothing has metered yet. The chain starts when a worker starts.
                eprintln!(
                    "no rollup rows between {from} and {today}. The metering chain starts when dam-worker \
                     starts; days before that are not recoverable — object_placements only knows what is \
                     stored now."
                );
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

/// The index pool, built from configuration.
///
/// One place, so `reindex` and `eval` cannot disagree about where a tenant's index lives — an eval run
/// against an empty directory would report a total relevance collapse and send somebody looking at the
/// ranker.
fn search_pool(cfg: &Config) -> dam_search::IndexPool {
    dam_search::IndexPool::new(
        dam_search::PoolConfig::new(&cfg.search.index_root)
            .with_max_open_indexes(cfg.search.max_open_indexes)
            .with_max_open_writers(cfg.search.max_open_writers)
            .with_writer_memory_bytes(cfg.search.writer_memory_mib * 1024 * 1024),
    )
}

/// A metric, or why there isn't one.
///
/// `None` prints as `n/a` and never as `0` or `1.0`: an unscoreable query is a query nobody labelled, and
/// rendering that as either a perfect or a failing score is how an eval report starts lying.
fn render_metric(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.4}"))
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

/// The blob store, from configuration.
///
/// The same construction `damd` and the worker do, and deliberately a copy rather than a shared helper: it is
/// twelve lines, and the alternative is a crate that exists so three binaries can agree on something they
/// each read from the same config. If it grows a third case it should move.
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

/// Every tenant a backup run should cover.
///
/// Suspended tenants are included and only deleted ones are not: a tenant suspended for non-payment still has
/// data somebody may be entitled to get back, and the day they are deleted is the day a backup stops being
/// their business.
async fn active_tenants(pool: &sqlx::PgPool) -> anyhow::Result<Vec<TenantSlug>> {
    let slugs: Vec<String> = sqlx::query_scalar(
        "SELECT slug FROM dam_global.tenants WHERE status <> 'deleted' ORDER BY slug",
    )
    .fetch_all(pool)
    .await
    .context("listing tenants")?;
    slugs
        .into_iter()
        .map(|slug| TenantSlug::new(&slug).context("a stored tenant slug is not valid"))
        .collect()
}
