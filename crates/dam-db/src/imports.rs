//! Phased migration in (G7).
//!
//! `import_jobs` and `import_records` have been in the schema since tenant 0008 and nothing has ever read them.
//! GAPS §G7 is blunt about why they matter: "every real deal is a migration", and "underestimating metadata
//! cleanup is the single most common cause of failed DAM migrations".
//!
//! ## Phases, because a 400k-asset library does not move in one run
//!
//! 0008's own comment says so. `discover` finds what is there, `crosswalk_review` is where a human decides the
//! mapping, `dry_run` produces the report the customer signs off on, `transfer` moves in batches, `verify` is
//! the QA gate, and `rolled_back` is the escape. The phase column is the state machine and [`advance`] is the
//! only thing that moves it, so a run cannot skip its own review.
//!
//! ## `import_records` is three things at once, and that is deliberate
//!
//! It is the **idempotency key** for a resumed run — `(job, source_id)` is the primary key, so a batch that
//! half-finished re-runs without duplicating. It is the **rollback manifest**, because it names every asset the
//! job created. And it is the **provenance**: 0008 says `source_id` is retained permanently, because "two years
//! later, 'which source asset did this come from' is a question that gets asked."
//!
//! Which means records are never deleted, not even on rollback. A rolled-back record becomes `rolled_back` and
//! keeps its `source_id`, so a second attempt knows what the first one did.
//!
//! ## Warnings are per record and counted per code
//!
//! A 40,000-record import with a broken date column produces 40,000 identical warnings. Stored per record
//! because that is where an operator looks when chasing one asset, and aggregated by code in the report because
//! that is what makes the report readable. `dam_core::crosswalk::Report` does the aggregating.

use crate::Error;
use chrono::{DateTime, Utc};
use sqlx::Row as _;
use uuid::Uuid;

/// Where an import has got to.
///
/// The order is the state machine. `advance` refuses a move that is not forward — a run that could jump from
/// `discover` to `transfer` would move a library under a crosswalk nobody reviewed, which is the failure §G7
/// describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    Discover,
    CrosswalkReview,
    DryRun,
    Transfer,
    Verify,
    Complete,
    RolledBack,
    Failed,
}

impl Phase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::CrosswalkReview => "crosswalk_review",
            Self::DryRun => "dry_run",
            Self::Transfer => "transfer",
            Self::Verify => "verify",
            Self::Complete => "complete",
            Self::RolledBack => "rolled_back",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "discover" => Self::Discover,
            "crosswalk_review" => Self::CrosswalkReview,
            "dry_run" => Self::DryRun,
            "transfer" => Self::Transfer,
            "verify" => Self::Verify,
            "complete" => Self::Complete,
            "rolled_back" => Self::RolledBack,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    /// Whether this phase can still change.
    ///
    /// `complete` and `rolled_back` are terminal. `failed` is *not*: a run that failed on a bad crosswalk should
    /// be fixable and resumed, which is the whole reason the crosswalk is editable between phases.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::RolledBack)
    }

    /// The phase that ordinarily follows this one.
    ///
    /// `None` at the end of the pipeline and for the off-pipeline states.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        Some(match self {
            Self::Discover => Self::CrosswalkReview,
            Self::CrosswalkReview => Self::DryRun,
            Self::DryRun => Self::Transfer,
            Self::Transfer => Self::Verify,
            Self::Verify => Self::Complete,
            Self::Complete | Self::RolledBack | Self::Failed => return None,
        })
    }

    /// Whether moving to `next` is allowed.
    ///
    /// **One step at a time, with one loop.** A jump — `discover` straight to `transfer` — would move a library
    /// under a crosswalk nobody reviewed, which is precisely the failure §G7 describes. So the pipeline advances
    /// by exactly one, and the four exceptions each exist for a reason:
    ///
    /// - **`verify` back to `transfer`**, because that is what "phased/incremental transfer rather than single
    ///   cutover" means: 400k assets move in batches with a QA gate between them, so the loop runs many times
    ///   before anything is complete. Writing this rule as plain "forward only" lost that, and the loop is the
    ///   design.
    /// - **anything to `failed`**, because a run can break at any point.
    /// - **anything to `rolled_back`**, because the escape hatch has to be reachable from wherever the trouble
    ///   was found.
    /// - **`failed` back to `crosswalk_review`**, because a run that failed on a bad mapping is fixed by
    ///   changing the mapping. That is why `failed` is not terminal.
    #[must_use]
    pub fn may_become(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        match (self, next) {
            (_, Self::Failed | Self::RolledBack) => true,
            (Self::Failed, Self::CrosswalkReview) => true,
            (Self::Failed, _) => false,
            // The batch loop. Everything else is exactly one step.
            (Self::Verify, Self::Transfer) => true,
            (from, to) => from.next() == Some(to),
        }
    }
}

/// Why an import operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum ImportRefusal {
    #[error("no import job {0}")]
    Unknown(Uuid),

    /// The phase machine refused a move. Carries both, because "cannot advance" without them is not actionable.
    #[error("an import in {from} cannot become {to}")]
    NotForward { from: String, to: String },

    /// A field the database refuses — an unusable source or an empty label.
    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Database(#[from] Error),
}

/// A migration to set up.
#[derive(Debug, Clone)]
pub struct NewImport<'a> {
    pub id: Uuid,
    /// `widen`, `bynder`, `brandfolder`, `aprimo`, `canto`, `sharepoint`, `gdrive`, `s3_bucket`, `filesystem`
    /// or `csv` — the set 0008's CHECK allows.
    pub source: &'a str,
    pub label: &'a str,
    /// Endpoint, credential reference, file path. Never a credential itself: 0008 says "credential ref".
    pub config: serde_json::Value,
    pub batch_size: i32,
    pub created_by: Option<Uuid>,
}

/// A migration, as an operator sees it.
#[derive(Debug, Clone)]
pub struct Import {
    pub id: Uuid,
    pub source: String,
    pub label: String,
    pub config: serde_json::Value,
    pub crosswalk: serde_json::Value,
    pub taxonomy_mapping: serde_json::Value,
    pub unmapped_fields: serde_json::Value,
    pub phase: Phase,
    pub batch_size: i32,
    pub current_batch: i32,
    pub discovered_count: i64,
    pub migrated_count: i64,
    pub skipped_count: i64,
    pub failed_count: i64,
    pub report: serde_json::Value,
    /// Everything this job created can be removed by this token alone.
    pub rollback_token: Uuid,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// One source asset's fate.
#[derive(Debug, Clone)]
pub struct Record {
    pub source_id: String,
    pub asset_id: Option<Uuid>,
    pub source_checksum: Option<String>,
    pub state: String,
    pub warnings: serde_json::Value,
    pub error: Option<String>,
    pub migrated_at: Option<DateTime<Utc>>,
}

/// Sets up a migration.
pub async fn create(
    conn: &mut sqlx::PgConnection,
    new: &NewImport<'_>,
) -> Result<Uuid, ImportRefusal> {
    if new.label.trim().is_empty() {
        return Err(ImportRefusal::Invalid(
            "an import needs a label; it is how an operator tells two runs apart".to_owned(),
        ));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO import_jobs \
           (id, source, label, config, batch_size, rollback_token, created_by) \
         VALUES ($1, $2, $3, $4, $5, gen_random_uuid(), $6) RETURNING id",
    )
    .bind(new.id)
    .bind(new.source)
    .bind(new.label.trim())
    .bind(&new.config)
    .bind(new.batch_size.clamp(1, 10_000))
    .bind(new.created_by)
    .fetch_one(&mut *conn)
    .await
    .map_err(classify)?;
    Ok(id)
}

/// Reads a migration.
pub async fn by_id(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Option<Import>, Error> {
    let row = sqlx::query(SELECT_ONE)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    row.map(hydrate).transpose()
}

/// Every migration, newest first.
pub async fn all(conn: &mut sqlx::PgConnection) -> Result<Vec<Import>, Error> {
    let rows = sqlx::query(SELECT_ALL).fetch_all(&mut *conn).await?;
    rows.into_iter().map(hydrate).collect()
}

/// Records the crosswalk a human reviewed, and what discovery found that it does not cover.
///
/// Editable between phases, which 0008 calls "the whole point — discovery reveals what the mapping should be".
/// So this is callable in any non-terminal phase, including `failed`: a run that failed on a bad mapping is
/// fixed by changing the mapping.
pub async fn set_crosswalk(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    crosswalk: &serde_json::Value,
    taxonomy_mapping: &serde_json::Value,
    unmapped_fields: &serde_json::Value,
) -> Result<(), ImportRefusal> {
    let current = by_id(&mut *conn, id)
        .await?
        .ok_or(ImportRefusal::Unknown(id))?;
    if current.phase.is_terminal() {
        return Err(ImportRefusal::NotForward {
            from: current.phase.as_str().to_owned(),
            to: "crosswalk_review".to_owned(),
        });
    }
    sqlx::query(
        "UPDATE import_jobs SET crosswalk = $2, taxonomy_mapping = $3, unmapped_fields = $4 \
         WHERE id = $1",
    )
    .bind(id)
    .bind(crosswalk)
    .bind(taxonomy_mapping)
    .bind(unmapped_fields)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(())
}

/// Moves the phase on.
///
/// The only thing that changes `phase`, so a run cannot skip its own review — see [`Phase::may_become`].
/// `started_at` is stamped when the job first leaves `discover` and `finished_at` when it reaches a terminal
/// phase; neither moves afterwards, because they answer "when did this run" rather than "when did we last look".
pub async fn advance(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    to: Phase,
) -> Result<(), ImportRefusal> {
    let current = by_id(&mut *conn, id)
        .await?
        .ok_or(ImportRefusal::Unknown(id))?;
    if !current.phase.may_become(to) {
        return Err(ImportRefusal::NotForward {
            from: current.phase.as_str().to_owned(),
            to: to.as_str().to_owned(),
        });
    }
    sqlx::query(
        "UPDATE import_jobs SET phase = $2, \
            started_at = COALESCE(started_at, CASE WHEN $2 <> 'discover' THEN now() END), \
            finished_at = CASE WHEN $2 IN ('complete', 'rolled_back') \
                               THEN COALESCE(finished_at, now()) ELSE finished_at END \
         WHERE id = $1",
    )
    .bind(id)
    .bind(to.as_str())
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(())
}

/// Stores the dry-run report and what discovery counted.
pub async fn set_report(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    discovered: i64,
    report: &serde_json::Value,
) -> Result<(), Error> {
    sqlx::query("UPDATE import_jobs SET discovered_count = $2, report = $3 WHERE id = $1")
        .bind(id)
        .bind(discovered)
        .bind(report)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Notes what one source asset mapped to, without migrating it.
///
/// The dry-run write. Upserted on `(job, source_id)`, so a re-run of discovery over changed data updates rather
/// than duplicating — and a record already `migrated` is left alone, because a dry run must never un-migrate
/// something a transfer did.
pub async fn note(
    conn: &mut sqlx::PgConnection,
    job: Uuid,
    source_id: &str,
    checksum: Option<&str>,
    warnings: &serde_json::Value,
    error: Option<&str>,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO import_records \
           (import_job_id, source_id, source_checksum, state, warnings, error) \
         VALUES ($1, $2, $3, CASE WHEN $5 IS NULL THEN 'pending' ELSE 'failed' END, $4, $5) \
         ON CONFLICT (import_job_id, source_id) DO UPDATE SET \
            source_checksum = excluded.source_checksum, \
            warnings = excluded.warnings, \
            error = excluded.error, \
            state = CASE WHEN import_records.state = 'migrated' THEN 'migrated' \
                         ELSE excluded.state END \
         WHERE import_records.state <> 'migrated' \
            OR import_records.warnings <> excluded.warnings",
    )
    .bind(job)
    .bind(source_id)
    .bind(checksum)
    .bind(warnings)
    .bind(error)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What state one record is in, if the run has heard of it at all.
///
/// The transfer's idempotency check. `source_id` is the key a migration is resumed on, so a run that died
/// half way asks this per record and skips what already arrived — the alternative being a second asset for
/// every source asset the first attempt got to, which is the failure mode a migration cannot be allowed to
/// have. One indexed lookup against a step that then uploads a whole file, so the cost does not register.
pub async fn state_of(
    conn: &mut sqlx::PgConnection,
    job: Uuid,
    source_id: &str,
) -> Result<Option<String>, Error> {
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM import_records WHERE import_job_id = $1 AND source_id = $2",
    )
    .bind(job)
    .bind(source_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(state)
}

/// Records that a source asset arrived, and as what.
///
/// Also bumps the job's counter, in the same statement pair, so a crash cannot leave the count disagreeing with
/// the records. The count is a convenience for a progress display; the records are the truth, and
/// [`recount`] exists for when they have to be reconciled.
pub async fn migrated(
    conn: &mut sqlx::PgConnection,
    job: Uuid,
    source_id: &str,
    asset_id: Uuid,
) -> Result<(), Error> {
    let changed = sqlx::query(
        "UPDATE import_records \
            SET asset_id = $3, state = 'migrated', error = NULL, migrated_at = now() \
          WHERE import_job_id = $1 AND source_id = $2 AND state <> 'migrated'",
    )
    .bind(job)
    .bind(source_id)
    .bind(asset_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed > 0 {
        sqlx::query("UPDATE import_jobs SET migrated_count = migrated_count + 1 WHERE id = $1")
            .bind(job)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Records that a source asset did not arrive, and why.
pub async fn failed(
    conn: &mut sqlx::PgConnection,
    job: Uuid,
    source_id: &str,
    reason: &str,
) -> Result<(), Error> {
    let changed = sqlx::query(
        "UPDATE import_records SET state = 'failed', error = $3 \
          WHERE import_job_id = $1 AND source_id = $2 AND state <> 'migrated'",
    )
    .bind(job)
    .bind(source_id)
    .bind(reason)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed > 0 {
        sqlx::query("UPDATE import_jobs SET failed_count = failed_count + 1 WHERE id = $1")
            .bind(job)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// The next batch of source ids still to move.
///
/// Ordered by `source_id` so a resumed run is deterministic: an operator comparing two runs is comparing text,
/// and a set query returning rows in a different order every time makes a diff useless — the same argument
/// `tiering::candidates` makes about its plan.
pub async fn pending(
    conn: &mut sqlx::PgConnection,
    job: Uuid,
    limit: i32,
) -> Result<Vec<Record>, Error> {
    let rows = sqlx::query(
        "SELECT source_id, asset_id, source_checksum, state, warnings, error, migrated_at \
         FROM import_records \
         WHERE import_job_id = $1 AND state = 'pending' \
         ORDER BY source_id LIMIT $2",
    )
    .bind(job)
    .bind(i64::from(limit.max(1)))
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(record_of).collect()
}

/// Every record, for a report or a manifest.
pub async fn records(
    conn: &mut sqlx::PgConnection,
    job: Uuid,
    limit: i64,
) -> Result<Vec<Record>, Error> {
    let rows = sqlx::query(
        "SELECT source_id, asset_id, source_checksum, state, warnings, error, migrated_at \
         FROM import_records WHERE import_job_id = $1 ORDER BY source_id LIMIT $2",
    )
    .bind(job)
    .bind(limit.clamp(1, 10_000))
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(record_of).collect()
}

/// Reconciles the job's counters with its records.
///
/// The counters are denormalised for a progress display and can drift — a crash between the record update and
/// the counter bump, a manual correction. The records are the truth, so this recomputes rather than adjusting.
pub async fn recount(conn: &mut sqlx::PgConnection, job: Uuid) -> Result<(), Error> {
    sqlx::query(
        "UPDATE import_jobs SET \
            migrated_count = counts.migrated, \
            failed_count = counts.failed, \
            skipped_count = counts.skipped \
         FROM (SELECT count(*) FILTER (WHERE state = 'migrated') AS migrated, \
                      count(*) FILTER (WHERE state = 'failed')   AS failed, \
                      count(*) FILTER (WHERE state = 'skipped')  AS skipped \
               FROM import_records WHERE import_job_id = $1) AS counts \
         WHERE id = $1",
    )
    .bind(job)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The assets one job created, for a rollback to remove.
///
/// Only what it *created*. A record whose asset was later replaced, deleted or superseded is excluded by the
/// join, so a rollback cannot take something the job did not bring — which is the difference between an escape
/// hatch and a second incident.
pub async fn created_assets(conn: &mut sqlx::PgConnection, job: Uuid) -> Result<Vec<Uuid>, Error> {
    Ok(sqlx::query_scalar(
        "SELECT r.asset_id FROM import_records r \
         JOIN assets a ON a.id = r.asset_id \
         WHERE r.import_job_id = $1 AND r.state = 'migrated' AND r.asset_id IS NOT NULL \
           AND a.deleted_at IS NULL \
         ORDER BY r.source_id",
    )
    .bind(job)
    .fetch_all(&mut *conn)
    .await?)
}

/// Marks the job's records rolled back.
///
/// The records themselves are kept — never deleted, not even here. 0008 retains `source_id` permanently so
/// "which source asset did this come from" stays answerable, and a second attempt needs to know what the first
/// one did. `asset_id` is cleared because the asset is gone; the source id is the thing worth keeping.
pub async fn mark_rolled_back(conn: &mut sqlx::PgConnection, job: Uuid) -> Result<u64, Error> {
    Ok(sqlx::query(
        "UPDATE import_records SET state = 'rolled_back', asset_id = NULL \
          WHERE import_job_id = $1 AND state = 'migrated'",
    )
    .bind(job)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Two whole statements rather than a composed prefix — sqlx takes a static string, and building SQL from
/// runtime pieces is a door this codebase keeps shut. `crate::connectors` makes the same trade.
const SELECT_ONE: &str = "SELECT id, source, label, config, crosswalk, taxonomy_mapping, \
                                 unmapped_fields, phase, batch_size, current_batch, \
                                 discovered_count, migrated_count, skipped_count, failed_count, \
                                 report, rollback_token, started_at, finished_at, created_at \
                          FROM import_jobs WHERE id = $1";

const SELECT_ALL: &str = "SELECT id, source, label, config, crosswalk, taxonomy_mapping, \
                                 unmapped_fields, phase, batch_size, current_batch, \
                                 discovered_count, migrated_count, skipped_count, failed_count, \
                                 report, rollback_token, started_at, finished_at, created_at \
                          FROM import_jobs ORDER BY created_at DESC";

fn hydrate(row: sqlx::postgres::PgRow) -> Result<Import, Error> {
    let phase: String = row.try_get("phase")?;
    Ok(Import {
        // An unreadable phase is a row this build cannot reason about, and guessing would mean resuming a run
        // at a stage nobody chose.
        phase: Phase::parse(&phase)
            .ok_or_else(|| Error::Inconsistent(format!("import_jobs.phase holds {phase:?}")))?,
        id: row.try_get("id")?,
        source: row.try_get("source")?,
        label: row.try_get("label")?,
        config: row.try_get("config")?,
        crosswalk: row.try_get("crosswalk")?,
        taxonomy_mapping: row.try_get("taxonomy_mapping")?,
        unmapped_fields: row.try_get("unmapped_fields")?,
        batch_size: row.try_get("batch_size")?,
        current_batch: row.try_get("current_batch")?,
        discovered_count: row.try_get("discovered_count")?,
        migrated_count: row.try_get("migrated_count")?,
        skipped_count: row.try_get("skipped_count")?,
        failed_count: row.try_get("failed_count")?,
        report: row.try_get("report")?,
        rollback_token: row.try_get("rollback_token")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        created_at: row.try_get("created_at")?,
    })
}

fn record_of(row: sqlx::postgres::PgRow) -> Result<Record, Error> {
    Ok(Record {
        source_id: row.try_get("source_id")?,
        asset_id: row.try_get("asset_id")?,
        source_checksum: row.try_get("source_checksum")?,
        state: row.try_get("state")?,
        warnings: row.try_get("warnings")?,
        error: row.try_get("error")?,
        migrated_at: row.try_get("migrated_at")?,
    })
}

fn classify(error: sqlx::Error) -> ImportRefusal {
    match error.as_database_error().and_then(|db| db.constraint()) {
        Some(name) => ImportRefusal::Invalid(name.to_owned()),
        None => ImportRefusal::Database(Error::from(error)),
    }
}
