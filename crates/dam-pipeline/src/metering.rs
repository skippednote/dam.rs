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
