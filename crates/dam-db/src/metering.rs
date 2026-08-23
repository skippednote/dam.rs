//! Fleet metering: one row per tenant per day (M6c).
//!
//! ## This is the operator's number, and it is deliberately not scoped
//!
//! Every other count in this codebase runs through the caller's predicate, because §7 says a count is a
//! disclosure. This one does not, and the reason is what the number is *for*: it is the tenant's bill and the
//! input to quota enforcement. A bill narrowed to what one reader can see is not a bill. `dam_global.
//! tenant_usage_daily`'s own comment says fleet reporting is served from rollups the worker writes there, never
//! from a cross-tenant join — so this module reads one tenant schema at a time and writes one row.
//!
//! Nothing tenant-facing reads it. `dam_db::insights` is the customer's view of their own activity and is
//! scoped; this is the operator's view of the whole tenant and is not. Serving an Insights screen from this
//! table would hand a scoped curator the library-wide totals in one field.
//!
//! ## A level is not a flow, and the difference is a bug waiting to happen
//!
//! `downloads`, `restores`, `restore_bytes` and the token counters are **flows**: things that happened between
//! midnight and midnight, and they can be measured for any past day because the rows carry their own timestamps.
//!
//! `asset_count` and `bytes_by_pool` are **levels**: how much was stored, which `object_placements` only knows
//! as of *now*. There is no history to recover it from. So [`measure`] takes the day it is measuring and refuses
//! one whose level cannot honestly be observed — anything before yesterday. A backfill that quietly wrote
//! today's storage against last March would produce a cost curve that looks flat because it is the same number
//! repeated.
//!
//! ## A day is a day in the database's timezone
//!
//! The flow queries compare `recorded_at::date` against a `date`, which resolves in the session timezone —
//! `Etc/UTC` on a damrs database. Same assumption as `crate::insights`, and same reason for saying it: a
//! Postgres running on a local timezone would move every boundary, and a bill is a bad place to discover it.
//!
//! ## Re-running corrects rather than doubles
//!
//! [`upsert`] is keyed on `(tenant_id, day)` and replaces. A metering job that added would turn one retry into
//! an invoice, which is the sort of arithmetic nobody notices until a customer does.

use crate::Error;
use chrono::NaiveDate;
use sqlx::PgPool;
use uuid::Uuid;

/// How far back a level can still be observed: yesterday.
///
/// One day, not zero, so the ordinary shape works — a job that runs after midnight measuring the day that has
/// just ended. Today is also permitted, for an operator asking what the current partial day looks like.
pub const LEVEL_HORIZON_DAYS: i64 = 1;

/// One tenant-day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayTotals {
    /// A level, as of the measurement. Current library rows only, matching what the grid counts.
    pub asset_count: i64,
    /// A level: `{storage_class: bytes}` over present placements. Keyed by class rather than pool id because a
    /// cost report reads classes, and a pool id means nothing without a second lookup into the control plane.
    pub bytes_by_pool: serde_json::Value,
    /// A flow: downloads recorded in the ledger that day, share-link ones included.
    pub downloads: i64,
    /// A flow: restores requested that day.
    pub restores: i64,
    pub restore_bytes: i64,
    /// Flows, from `enrichment_runs` — which 0003 said all along would roll up here.
    pub ai_input_tokens: i64,
    pub ai_output_tokens: i64,
    pub ai_cached_tokens: i64,
    /// A flow: restore retrieval plus AI spend, in whole cents. Rounded once, here, so a fleet total is the sum
    /// of what each row says rather than a re-derivation that disagrees with every row.
    pub est_cost_cents: i64,
}

impl DayTotals {
    /// Everything stored, across every class.
    ///
    /// Here rather than in each caller: `bytes_by_pool` is a JSON object, and every consumer that wants one
    /// number would otherwise write the same fold — and `damctl` would need a `serde_json` dependency to add
    /// up a column it is only printing.
    ///
    /// A malformed value contributes nothing rather than failing. The column is written by [`upsert`] from an
    /// object of integers, so a non-integer in there is corruption; refusing to print a usage table because
    /// one day's JSON is wrong would hide the other twenty-nine.
    #[must_use]
    pub fn stored_bytes(&self) -> i64 {
        self.bytes_by_pool
            .as_object()
            .map(|classes| {
                classes
                    .values()
                    .filter_map(serde_json::Value::as_i64)
                    .sum::<i64>()
            })
            .unwrap_or_default()
    }
}

/// A day on which nothing happened and nothing is stored.
///
/// `bytes_by_pool` is an empty **object**, not `null`, which is why this is hand-written rather than derived:
/// a consumer reading `{class: bytes}` should not need a branch for the empty case, and `Value::default()` is
/// `Null`. The column is `NOT NULL DEFAULT '{}'` for the same reason.
impl Default for DayTotals {
    fn default() -> Self {
        Self {
            asset_count: 0,
            bytes_by_pool: serde_json::Value::Object(serde_json::Map::new()),
            downloads: 0,
            restores: 0,
            restore_bytes: 0,
            ai_input_tokens: 0,
            ai_output_tokens: 0,
            ai_cached_tokens: 0,
            est_cost_cents: 0,
        }
    }
}

/// Why a day cannot be measured.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// The day is old enough that its storage level is no longer observable.
    #[error(
        "{day} is before {today} minus {LEVEL_HORIZON_DAYS} day(s), and object_placements only knows what \
         is stored now — measuring it would record today's storage against a past day"
    )]
    LevelUnobservable { day: NaiveDate, today: NaiveDate },
    #[error(transparent)]
    Db(#[from] Error),
}

impl From<sqlx::Error> for Refusal {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(Error::from(error))
    }
}

/// Measures one day in the tenant schema `conn` is pointed at.
///
/// `today` is passed in rather than read from the clock so a test can measure a fixed day, and so the horizon
/// check compares two dates from the same source.
pub async fn measure(
    conn: &mut sqlx::PgConnection,
    day: NaiveDate,
    today: NaiveDate,
) -> Result<DayTotals, Refusal> {
    if (today - day).num_days() > LEVEL_HORIZON_DAYS {
        return Err(Refusal::LevelUnobservable { day, today });
    }

    // The levels. `LIBRARY_ROWS` for the same reason everything else uses it: the count has to agree with what
    // the tenant sees, and a library with three versions of one asset has one of that asset in it.
    let asset_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assets \
         WHERE assets.deleted_at IS NULL AND assets.is_current AND assets.attached_to IS NULL",
    )
    .fetch_one(&mut *conn)
    .await?;

    // Present placements only: an object mid-upload is not stored yet, and a `missing` one is a scrub finding
    // rather than a line on a bill. Derivative placements are included — a thumbnail costs money too.
    let by_class: Vec<(String, i64)> = sqlx::query_as(
        "SELECT storage_class, coalesce(sum(size_bytes), 0)::bigint \
         FROM object_placements WHERE state = 'present' GROUP BY storage_class",
    )
    .fetch_all(&mut *conn)
    .await?;
    let bytes_by_pool = serde_json::Value::Object(
        by_class
            .into_iter()
            .map(|(class, bytes)| (class, serde_json::Value::from(bytes)))
            .collect(),
    );

    // The flows, each bounded to the day. One statement, so they describe the same read.
    let flows: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM rights_usage \
             WHERE source = 'download' AND recorded_at::date = $1), \
            (SELECT count(*) FROM restore_requests WHERE requested_at::date = $1), \
            (SELECT coalesce(sum(bytes), 0)::bigint FROM restore_requests \
             WHERE requested_at::date = $1), \
            (SELECT coalesce(sum(est_cost_cents), 0)::bigint FROM restore_requests \
             WHERE requested_at::date = $1), \
            (SELECT coalesce(sum(input_tokens), 0)::bigint FROM enrichment_runs \
             WHERE started_at::date = $1), \
            (SELECT coalesce(sum(output_tokens), 0)::bigint FROM enrichment_runs \
             WHERE started_at::date = $1), \
            (SELECT coalesce(sum(cached_tokens), 0)::bigint FROM enrichment_runs \
             WHERE started_at::date = $1)",
    )
    .bind(day)
    .fetch_one(&mut *conn)
    .await?;

    // AI spend is `numeric(12, 4)` cents, so it is summed as numeric and rounded once. Truncating per row
    // would lose most of a cheap enrichment's cost, and a million of those is the whole bill.
    let ai_cents: i64 = sqlx::query_scalar(
        "SELECT coalesce(round(sum(est_cost_cents)), 0)::bigint FROM enrichment_runs \
         WHERE started_at::date = $1",
    )
    .bind(day)
    .fetch_one(&mut *conn)
    .await?;

    let (downloads, restores, restore_bytes, restore_cents, input, output, cached) = flows;
    Ok(DayTotals {
        asset_count,
        bytes_by_pool,
        downloads,
        restores,
        restore_bytes,
        ai_input_tokens: input,
        ai_output_tokens: output,
        ai_cached_tokens: cached,
        est_cost_cents: restore_cents.saturating_add(ai_cents),
    })
}

/// Writes one tenant-day into the control plane, replacing any previous measurement of it.
///
/// Replaces rather than adds. A retry, a manual re-run and a job that ran twice because a lease lapsed all have
/// to converge on the same row, because this table is what a customer is billed from.
pub async fn upsert(
    global: &PgPool,
    tenant_id: Uuid,
    day: NaiveDate,
    totals: &DayTotals,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO dam_global.tenant_usage_daily \
         (tenant_id, day, asset_count, bytes_by_pool, downloads, restores, restore_bytes, \
          ai_input_tokens, ai_output_tokens, ai_cached_tokens, est_cost_cents) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (tenant_id, day) DO UPDATE SET \
            asset_count = excluded.asset_count, \
            bytes_by_pool = excluded.bytes_by_pool, \
            downloads = excluded.downloads, \
            restores = excluded.restores, \
            restore_bytes = excluded.restore_bytes, \
            ai_input_tokens = excluded.ai_input_tokens, \
            ai_output_tokens = excluded.ai_output_tokens, \
            ai_cached_tokens = excluded.ai_cached_tokens, \
            est_cost_cents = excluded.est_cost_cents",
    )
    .bind(tenant_id)
    .bind(day)
    .bind(totals.asset_count)
    .bind(&totals.bytes_by_pool)
    .bind(totals.downloads)
    .bind(totals.restores)
    .bind(totals.restore_bytes)
    .bind(totals.ai_input_tokens)
    .bind(totals.ai_output_tokens)
    .bind(totals.ai_cached_tokens)
    .bind(totals.est_cost_cents)
    .execute(global)
    .await?;
    Ok(())
}

/// One stored tenant-day as it comes back from the control plane.
type StoredRow = (
    NaiveDate,
    i64,
    serde_json::Value,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
);

/// One tenant-day as stored, for an operator's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub tenant_id: Uuid,
    pub day: NaiveDate,
    pub totals: DayTotals,
}

/// Reads a window of days for one tenant, oldest first.
///
/// Operator-facing. There is no per-tenant API route onto this — see the module docs — and the argument is a
/// tenant id rather than a slug because the caller is `damctl` or a fleet report, not a request.
pub async fn window(
    global: &PgPool,
    tenant_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<Row>, Error> {
    let rows: Vec<StoredRow> = sqlx::query_as(
        "SELECT day, asset_count, bytes_by_pool, downloads, restores, restore_bytes, \
                ai_input_tokens, ai_output_tokens, ai_cached_tokens, est_cost_cents \
         FROM dam_global.tenant_usage_daily \
         WHERE tenant_id = $1 AND day BETWEEN $2 AND $3 ORDER BY day",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(global)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                day,
                asset_count,
                bytes_by_pool,
                downloads,
                restores,
                restore_bytes,
                ai_input_tokens,
                ai_output_tokens,
                ai_cached_tokens,
                est_cost_cents,
            )| Row {
                tenant_id,
                day,
                totals: DayTotals {
                    asset_count,
                    bytes_by_pool,
                    downloads,
                    restores,
                    restore_bytes,
                    ai_input_tokens,
                    ai_output_tokens,
                    ai_cached_tokens,
                    est_cost_cents,
                },
            },
        )
        .collect())
}
