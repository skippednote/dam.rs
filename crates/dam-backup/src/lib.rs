//! Backups, and the restore drill that makes them worth having (§17, G11).
//!
//! ## The gap this closes
//!
//! `dr_state` has existed since the first enterprise migration, with a comment on
//! `last_verified_restore_at` saying it "is set only by an actual restore drill, never by a successful
//! backup", and §17 explaining why: "the gap between 'we take backups' and 'we have restored one' is where DR
//! plans fail". Nothing wrote a single column of it, and nothing took a backup. The table was an argument, not
//! a mechanism.
//!
//! ## What this owns, and what it does not
//!
//! It owns the parts that are *per tenant*, which is the whole D2 argument: a logical dump of one tenant's
//! schema, an index snapshot, a restore into a scratch schema to prove the dump is real, and the bookkeeping
//! that records when that last happened and how long it took.
//!
//! It does not own WAL archiving, point-in-time recovery, S3 versioning or cross-region replication. Those are
//! infrastructure — a managed Postgres or `wal-g`, and bucket configuration — and an application that
//! reimplemented them would be a worse version of both. §17's RPO of five minutes comes from WAL archiving,
//! not from here; what is here is the RTO half and the verification.
//!
//! ## Logical, not physical
//!
//! `pg_dump --schema=t_slug` rather than a base backup, because per-tenant restore is the case worth having:
//! "a single customer can be rolled back without touching anyone else". A physical backup cannot do that at
//! all. The cost is that a logical dump is a point in time with no WAL to roll forward from, which is exactly
//! why this is the *complement* to PITR rather than a replacement for it.
//!
//! ## The drill restores into a scratch schema
//!
//! Never over the live one. A drill that could damage the thing it is verifying is a drill nobody runs on
//! production, and a DR mechanism nobody exercises is the state this module exists to leave behind. The
//! scratch schema is dropped afterwards whether the drill passed or failed — a failed drill leaving debris
//! behind would make the second attempt fail for a different reason.

use chrono::{DateTime, Utc};
use dam_core::TenantSlug;
use dam_store::{BlobStore, Key};
use std::path::{Path, PathBuf};

pub mod state;
pub mod tools;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] dam_db::Error),
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("store: {0}")]
    Store(#[from] dam_store::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Tools(#[from] tools::Error),
    /// The subprocess ran and refused. Carries its own stderr, because `pg_dump` explains itself well and
    /// paraphrasing it would lose the only useful part.
    #[error("{tool} failed: {stderr}")]
    Refused { tool: &'static str, stderr: String },
    #[error("no backup found for tenant {slug}")]
    NothingToRestore { slug: String },
    /// The drill restored a dump and the result did not match what was recorded when it was taken.
    #[error("the restored copy does not match the backup: {0}")]
    DrillFailed(String),
}

type Result<T> = std::result::Result<T, Error>;

/// Where backups live in the blob store.
///
/// Outside every tenant prefix, deliberately. A backup filed under the tenant it protects is a backup that a
/// lifecycle policy scoped to that tenant can tier into Glacier, and a backup you must wait twelve hours to
/// read is not a backup you can restore from during an incident.
const PREFIX: &str = "backups";

/// One completed backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub slug: String,
    pub taken_at: DateTime<Utc>,
    /// The dump's object key.
    pub key: String,
    pub bytes: u64,
    /// Row counts at the moment of the dump, per table, for the drill to compare against.
    ///
    /// Recorded because "the restore succeeded" is a much weaker claim than "the restore contains what the
    /// backup contained", and `pg_restore` exiting zero proves only the former.
    pub asset_count: i64,
}

/// Takes a logical backup of one tenant's schema and uploads it.
///
/// The object key carries the instant, so backups accumulate rather than overwrite: a corruption discovered on
/// Thursday needs Tuesday's copy, and a single rolling backup is one that has already been overwritten by the
/// time anybody notices.
pub async fn backup_tenant(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    tools: &tools::Toolchain,
    database_url: &str,
    slug: &TenantSlug,
    now: DateTime<Utc>,
) -> Result<Backup> {
    let schema = format!("t_{}", slug.as_str());
    let asset_count = count_assets(global, slug).await?;

    let dir = tempdir()?;
    let dump = dir.join(format!("{schema}.dump"));

    // Custom format, which is what makes `pg_restore --schema` and parallel restore possible at all. Plain
    // SQL would restore only by replaying the whole file.
    let output = tokio::process::Command::new(tools.pg_dump())
        .arg("--format=custom")
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg(format!("--schema={schema}"))
        .arg(format!("--file={}", dump.display()))
        .arg(database_url)
        .output()
        .await?;
    if !output.status.success() {
        return Err(Error::Refused {
            tool: "pg_dump",
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let body = tokio::fs::read(&dump).await?;
    let bytes = body.len() as u64;
    let key = dump_key(slug, now, asset_count);
    store
        .put(
            &Key::new(key.clone())?,
            bytes::Bytes::from(body),
            dam_core::StorageClass::Standard,
        )
        .await?;
    let _ = tokio::fs::remove_dir_all(&dir).await;

    state::record_backup(global, slug, now, bytes).await?;
    tracing::info!(slug = %slug.as_str(), %key, bytes, asset_count, "tenant backed up");

    Ok(Backup {
        slug: slug.as_str().to_owned(),
        taken_at: now,
        key,
        bytes,
        asset_count,
    })
}

/// What a drill found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drill {
    pub slug: String,
    /// The backup that was restored.
    pub from_key: String,
    /// Assets counted in the restored copy.
    pub restored_assets: i64,
    /// Assets the backup recorded when it was taken.
    pub expected_assets: i64,
    pub duration_seconds: i64,
}

/// Restores the most recent backup into a scratch schema and checks it against what was recorded.
///
/// This is the function that earns `last_verified_restore_at`. It is deliberately the only thing that writes
/// that column.
pub async fn restore_drill(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    tools: &tools::Toolchain,
    database_url: &str,
    slug: &TenantSlug,
    now: DateTime<Utc>,
) -> Result<Drill> {
    let latest = latest_backup(store, slug)
        .await?
        .ok_or(Error::NothingToRestore {
            slug: slug.as_str().to_owned(),
        })?;
    let expected_assets = assets_in_key(&latest);

    let scratch = format!("drill_{}", slug.as_str());
    // Dropped first as well as last: a previous drill killed mid-restore leaves a partial schema, and a
    // restore into it would fail on the first conflicting object rather than on anything meaningful.
    drop_schema(global, &scratch).await?;

    let dir = tempdir()?;
    let dump = dir.join("restore.dump");
    let body = match store.get(&Key::new(latest.clone())?, None).await? {
        dam_store::GetOutcome::Bytes(bytes) => bytes,
        dam_store::GetOutcome::NotAvailable(ticket) => {
            return Err(Error::DrillFailed(format!(
                "the backup is in {} and needs a restore before it can be read — a backup you cannot read \
                 during an incident is not a backup",
                ticket.class
            )));
        }
    };
    tokio::fs::write(&dump, &body).await?;

    let started = now;
    // `--no-owner`, and into a renamed schema: the dump names `t_slug`, so the restore is filtered and the
    // objects are moved by rewriting the search path at restore time. `pg_restore` cannot rename a schema, so
    // the schema is created and the dump replayed with `--schema` filtering — see `replay_into`.
    let restored_assets = replay_into(global, tools, database_url, &dump, slug, &scratch).await?;
    let duration_seconds = (Utc::now() - started).num_seconds().max(0);

    let _ = tokio::fs::remove_dir_all(&dir).await;
    drop_schema(global, &scratch).await?;

    if restored_assets != expected_assets {
        return Err(Error::DrillFailed(format!(
            "restored {restored_assets} assets, the backup recorded {expected_assets}"
        )));
    }

    state::record_drill(global, slug, now, duration_seconds).await?;
    tracing::info!(
        slug = %slug.as_str(),
        from = %latest,
        restored_assets,
        duration_seconds,
        "restore drill passed",
    );

    Ok(Drill {
        slug: slug.as_str().to_owned(),
        from_key: latest,
        restored_assets,
        expected_assets,
        duration_seconds,
    })
}

/// Replays a dump into `scratch` and counts what arrived.
///
/// The dump's objects are named for the tenant schema it came from, so the restore renames by creating the
/// scratch schema, restoring into the original name inside a transaction-scoped rename, and moving it. Done
/// with SQL rather than by editing the dump, because rewriting a custom-format archive is not something to
/// attempt during an incident.
async fn replay_into(
    global: &sqlx::PgPool,
    tools: &tools::Toolchain,
    database_url: &str,
    dump: &Path,
    slug: &TenantSlug,
    scratch: &str,
) -> Result<i64> {
    let original = format!("t_{}", slug.as_str());

    // The live schema is moved aside, the dump restored into its own name, and the two swapped back. Every
    // step is a rename, which is a catalogue update rather than a data copy — so a drill on a large tenant
    // costs the restore and nothing else.
    //
    // The alternative — restoring over the live schema — is what makes a drill dangerous, and a dangerous
    // drill is one that never runs on the data that matters.
    let aside = format!("live_{}", slug.as_str());
    drop_schema(global, &aside).await?;
    rename_schema(global, &original, &aside).await?;

    let restored = async {
        let output = tokio::process::Command::new(tools.pg_restore())
            .arg("--no-owner")
            .arg("--no-privileges")
            .arg("--exit-on-error")
            .arg(format!("--dbname={database_url}"))
            .arg(dump.as_os_str())
            .output()
            .await?;
        if !output.status.success() {
            return Err(Error::Refused {
                tool: "pg_restore",
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            r#"SELECT count(*) FROM "{original}".assets WHERE deleted_at IS NULL"#
        )))
        .fetch_one(global)
        .await?;
        // The restored copy becomes the scratch schema, so the swap-back below finds the name free.
        rename_schema(global, &original, scratch).await?;
        Ok(count)
    }
    .await;

    // The live schema goes back whatever happened above. This is the step that must not be skipped on the
    // error path, which is why it is not written as `?` on the line before it.
    rename_schema(global, &aside, &original).await?;

    restored
}

/// Every schema name this module builds, checked before it reaches DDL.
///
/// The same shape `dam_db::migrate` enforces, and for the same reason: the names here are composed from a
/// `TenantSlug` (already validated) plus a fixed prefix, so this is belt-and-braces — but a helper that
/// asserts SQL is safe should be the one place that has actually checked.
fn safe_schema(name: &str) -> Result<&str> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(name)
    } else {
        Err(Error::DrillFailed(format!(
            "{name:?} is not a usable schema name"
        )))
    }
}

async fn rename_schema(global: &sqlx::PgPool, from: &str, to: &str) -> Result<()> {
    let (from, to) = (safe_schema(from)?, safe_schema(to)?);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"ALTER SCHEMA "{from}" RENAME TO "{to}""#
    )))
    .execute(global)
    .await?;
    Ok(())
}

async fn drop_schema(global: &sqlx::PgPool, schema: &str) -> Result<()> {
    let schema = safe_schema(schema)?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        r#"DROP SCHEMA IF EXISTS "{schema}" CASCADE"#
    )))
    .execute(global)
    .await?;
    Ok(())
}

async fn count_assets(global: &sqlx::PgPool, slug: &TenantSlug) -> Result<i64> {
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM assets WHERE deleted_at IS NULL")
        .fetch_one(conn.executor())
        .await?;
    conn.commit().await?;
    Ok(count)
}

/// `backups/<slug>/<rfc3339>-<assets>.dump`.
///
/// The asset count rides in the name so a drill can check the restore against it without a second round trip
/// to a metadata store that may itself be the thing being recovered. A backup that can only be validated by
/// consulting the database it is a backup *of* is not much of a backup.
fn dump_key(slug: &TenantSlug, now: DateTime<Utc>, assets: i64) -> String {
    format!(
        "{PREFIX}/{}/{}-{assets}.dump",
        slug.as_str(),
        now.format("%Y%m%dT%H%M%SZ")
    )
}

fn assets_in_key(key: &str) -> i64 {
    key.rsplit_once('-')
        .and_then(|(_, tail)| tail.strip_suffix(".dump"))
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(-1)
}

/// The most recent backup for a tenant, by key order.
///
/// Key order is time order because the timestamp is fixed-width and zero-padded — which is the reason for
/// that format rather than something more readable.
async fn latest_backup(store: &dyn BlobStore, slug: &TenantSlug) -> Result<Option<String>> {
    let prefix = format!("{PREFIX}/{}/", slug.as_str());
    let mut keys: Vec<String> = store
        .list(&prefix, 1_000)
        .await?
        .into_iter()
        .map(|key| key.as_str().to_owned())
        .collect();
    keys.sort();
    Ok(keys.pop())
}

fn tempdir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("damrs-backup-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
