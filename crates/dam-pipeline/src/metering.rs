//! The daily metering roll-up (M6c).
//!
//! ## Why a job and not a query
//!
//! `dam_global.tenant_usage_daily`'s own comment says fleet reporting is served from rollups the worker writes
//! there, never from a cross-tenant join — D2 forbids one. So the only way to a fleet number is one pass per
//! tenant schema, writing one row each, and that is what this is.
//!
//! ## It measures the day that has ended, and re-measures today
//!
//! Two rows per pass. Yesterday, because it is complete and will not change again; and today, because an
//! operator watching a spend cap wants the current partial day rather than a figure that appears at midnight.
//! Both are upserts, so today's row is simply replaced on the next pass and becomes final when the day rolls
//! over.
//!
//! Anything older is not attempted. `dam_db::metering` refuses it, and the reason is in that module: storage is
//! a level `object_placements` only knows as of now, and backdating it would draw a flat cost curve out of one
//! number repeated.
//!
//! ## It also feeds the quotas, and only the levels
//!
//! `storage_bytes` and `asset_count` are what this pass measures, and `dam_db::quotas::observe` is the write
//! that matches: a level is *set* from a measurement rather than accumulated. Feeding them through `charge`
//! would add the whole library to the counter every pass, so a tenant holding a steady terabyte would trip a
//! two-terabyte cap on the second day without having stored anything more.
//!
//! Only for today's measurement. Yesterday's row is history — writing it into `tenant_spend` would overwrite
//! the current standing with a stale number, and the enforcement path reads that row on every upload.
//!
//! ## A failure here must not stall the chain
//!
//! One tenant's schema being unreadable — mid-provision, mid-restore, briefly locked — is not a reason to stop
//! metering the fleet. The job logs and requeues, because the next pass measures the same days again and
//! corrects whatever this one missed.

use crate::Result;
use chrono::{Duration, NaiveDate, Utc};
use dam_core::TenantSlug;
use uuid::Uuid;

/// One pass per day. Metering is a bill, not a dashboard: a shorter cadence would rewrite the same two rows
/// for no new information.
pub const ROLL_EVERY: Duration = Duration::hours(24);

/// What one pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rolled {
    pub days: u32,
    pub refused: u32,
}

/// Measures and writes yesterday and today for one tenant.
pub async fn roll(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    tenant_id: Uuid,
    now: chrono::DateTime<Utc>,
) -> Result<Rolled> {
    let today: NaiveDate = now.date_naive();
    let mut rolled = Rolled::default();

    for day in [today - Duration::days(1), today] {
        // A fresh transaction per day rather than one for both: the flows for yesterday and today are separate
        // reads anyway, and a long-held tenant transaction is the thing a metering job has no excuse for.
        let mut conn = dam_db::TenantConn::begin(global, slug).await?;
        let measured = dam_db::metering::measure(conn.executor(), day, today).await;
        conn.commit().await?;

        match measured {
            Ok(totals) => {
                dam_db::metering::upsert(global, tenant_id, day, &totals).await?;

                // The quota levels, from today's measurement only — see the module docs on why yesterday's
                // must not be written.
                if day == today {
                    let period = dam_db::quotas::month_start(now);
                    let mut conn = global.acquire().await.map_err(dam_db::Error::from)?;
                    for (key, level) in [
                        (dam_db::quotas::STORAGE_BYTES, totals.stored_bytes()),
                        (dam_db::quotas::ASSET_COUNT, totals.asset_count),
                    ] {
                        let verdict =
                            dam_db::quotas::observe(&mut conn, tenant_id, key, period, level)
                                .await?;
                        // Logged at the level the verdict deserves. Nothing here *enforces* — a metering pass
                        // that refused work would be a cap applied at whatever hour the cron happens to run.
                        // Enforcement is at the points the quota binds: ingest, restore, delivery.
                        match verdict {
                            dam_db::quotas::Verdict::Refused { used, limit } => tracing::warn!(
                                %tenant_id, quota = key, used, limit,
                                "a tenant is over a hard cap",
                            ),
                            dam_db::quotas::Verdict::Warned { used, limit } => tracing::info!(
                                %tenant_id, quota = key, used, limit,
                                "a tenant is past its warning line",
                            ),
                            dam_db::quotas::Verdict::Allowed => {}
                        }
                    }
                }
                rolled.days += 1;
            }
            Err(dam_db::metering::Refusal::LevelUnobservable { .. }) => {
                // Reachable only if the clock moved backwards between the two arguments. Counted rather than
                // logged as an error: it is a fact about the day, not a fault.
                rolled.refused += 1;
            }
            Err(dam_db::metering::Refusal::Db(error)) => return Err(error.into()),
        }
    }

    Ok(rolled)
}
