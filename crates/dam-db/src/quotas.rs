//! Spend caps, and the one question they exist to answer (G20).
//!
//! `tenant_quotas` and `tenant_spend` have been in the schema since global 0002 and nothing has ever read them.
//! This is the reader. Hosted-model enrichment is what makes them urgent: storage grows predictably and a
//! mis-triggered re-enrichment of a large library is a five-figure event in an afternoon.
//!
//! ## Check before, charge after
//!
//! The cost of a model call is not known until it returns — tokens are reported, not predicted. So the flow is
//! [`check`] before the call and [`charge`] after it, which means a cap can be overshot by the calls already in
//! flight when the limit was crossed. That is the deliberate trade: the alternative is a reservation ledger with
//! a compensating write on every failure, and it buys precision that nobody needs. What matters is that the
//! *next* call is refused, and that the overshoot is bounded by concurrency rather than by library size.
//!
//! ## Sub-unit charges accumulate rather than rounding away
//!
//! `used_value` is a `bigint` and one enrichment call costs a fraction of a cent — 0.45¢ on a small model. Cents
//! rounded down would make a million calls cost nothing and a hard cap unreachable; rounded up they would
//! overstate a cheap model by thirtyfold. So a charge is made in *micro-units* — millionths of the quota's own
//! unit — and `spend_remainder_micro` (global 0003) carries the part below one whole unit into the next charge.
//! `used_value` keeps exactly the meaning it had: whole units spent.
//!
//! ## Soft and hard
//!
//! `enforcement` is per quota, not per tenant, because the right answer differs by what is being capped: a hard
//! cap on ingest loses a customer's work, a hard cap on AI enrichment prevents a surprise invoice. A soft cap
//! warns and keeps serving. Both record when the threshold was first crossed, so "we were not told" is
//! answerable.

use crate::Error;
use chrono::{Datelike, NaiveDate};
use uuid::Uuid;

/// One millionth of a quota's unit. A cost in cents becomes an integer count of these.
pub const MICRO: i64 = 1_000_000;

/// The quota keys this module understands. The column's CHECK holds the full list; these are the ones with a
/// reader.
pub const AI_SPEND: &str = "ai_spend_cents_month";

/// What a quota does when it is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// Warn, keep serving.
    Soft,
    /// Refuse new work.
    Hard,
}

impl Enforcement {
    fn parse(value: &str) -> Self {
        // Anything unrecognised is soft. A quota row this build cannot read must not silently become a hard stop
        // on a tenant's library — failing open here is the safe direction, and the CHECK constraint is what
        // keeps the set small.
        match value {
            "hard" => Self::Hard,
            _ => Self::Soft,
        }
    }
}

/// A cap, as configured.
#[derive(Debug, Clone, PartialEq)]
pub struct Quota {
    pub limit_value: i64,
    pub warn_at_fraction: f32,
    pub enforcement: Enforcement,
}

/// Whether work may proceed, and what to say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No quota is configured, or there is room.
    Allowed,
    /// Past the warning fraction and still allowed. Carries whole units.
    Warned { used: i64, limit: i64 },
    /// Over a hard cap. The caller must not start the work.
    Refused { used: i64, limit: i64 },
}

impl Verdict {
    /// Whether the work may start.
    ///
    /// A warning is an allowance: the point of a soft cap is that the customer keeps working.
    pub fn allowed(self) -> bool {
        !matches!(self, Self::Refused { .. })
    }
}

/// The period a monthly quota is counted in.
///
/// Calendar month in UTC. Not a rolling 30 days: an invoice is a calendar month, and a cap that did not line up
/// with the bill it protects would be explaining itself forever.
pub fn month_start(at: chrono::DateTime<chrono::Utc>) -> NaiveDate {
    let date = at.date_naive();
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date)
}

/// The configured cap, if there is one.
///
/// `overage_cents_per_unit` is deliberately not read. It is a *billing* input — what to charge beyond the
/// limit — and enforcement needs only the limit, the warning line and whether the cap is hard. Reading it would
/// mean binding a `numeric` column and choosing a decimal type for a value nothing here can act on.
pub async fn quota(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    quota_key: &str,
) -> Result<Option<Quota>, Error> {
    let row = sqlx::query_as::<_, (i64, f32, String)>(
        "SELECT limit_value, warn_at_fraction, enforcement
           FROM dam_global.tenant_quotas
          WHERE tenant_id = $1 AND quota_key = $2",
    )
    .bind(tenant_id)
    .bind(quota_key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(
        row.map(|(limit_value, warn_at_fraction, enforcement)| Quota {
            limit_value,
            warn_at_fraction,
            enforcement: Enforcement::parse(&enforcement),
        }),
    )
}

/// Whole units spent in a period. Zero when nothing has been charged.
pub async fn used(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    quota_key: &str,
    period_start: NaiveDate,
) -> Result<i64, Error> {
    let used = sqlx::query_scalar::<_, i64>(
        "SELECT used_value FROM dam_global.tenant_spend
          WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant_id)
    .bind(quota_key)
    .bind(period_start)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(used.unwrap_or(0))
}

/// May this work start?
///
/// One indexed read of each table, which is why the counter exists separately from `tenant_usage_daily`: a sum
/// over a date range on the request path is the thing G20's schema note set out to avoid.
///
/// A tenant with no quota row is [`Verdict::Allowed`]. Absence of a cap is not a cap of zero, and defaulting the
/// other way would stop every tenant who has never been configured.
pub async fn check(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    quota_key: &str,
    period_start: NaiveDate,
) -> Result<Verdict, Error> {
    let Some(quota) = quota(conn, tenant_id, quota_key).await? else {
        return Ok(Verdict::Allowed);
    };
    let used = used(conn, tenant_id, quota_key, period_start).await?;
    Ok(verdict(&quota, used))
}

/// The verdict for a known cap and a known spend. Separate so the arithmetic is testable without a database.
pub fn verdict(quota: &Quota, used: i64) -> Verdict {
    let limit = quota.limit_value;
    if used >= limit {
        return match quota.enforcement {
            Enforcement::Hard => Verdict::Refused { used, limit },
            // A soft cap that is over is still a warning — the customer keeps working and somebody is told.
            Enforcement::Soft => Verdict::Warned { used, limit },
        };
    }
    // Rounded *down*, and compared with `>=`. The column is a `real`, so a configured 0.8 is stored as
    // 0.800000011920929, and rounding up turns "warn at 80% of 100" into "warn at 81" — the warning fires a unit
    // late, or on a small limit never at all. Down, it can fire a unit early, which is the direction a warning
    // exists for: the point is time to react.
    let warn_at = (f64::from(limit as f32) * f64::from(quota.warn_at_fraction)).floor();
    if limit > 0 && warn_at.is_finite() && (used as f64) >= warn_at {
        return Verdict::Warned { used, limit };
    }
    Verdict::Allowed
}

/// Adds to a period's spend and returns the verdict *after* the charge.
///
/// `micro` is in millionths of the quota's unit; see the module note on why. The remainder below one whole unit
/// is kept on the row, so a stream of sub-unit charges accumulates instead of rounding to nothing.
///
/// `warned_at` and `exceeded_at` are stamped the first time each threshold is crossed and never moved, because
/// the question they answer is "when did this start", and a value that advanced with every charge would answer
/// "when did we last look".
pub async fn charge(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    quota_key: &str,
    period_start: NaiveDate,
    micro: i64,
) -> Result<Verdict, Error> {
    // One statement, so two workers charging the same tenant cannot lose an increment between a read and a
    // write. The remainder arithmetic is in SQL for the same reason.
    let (used, _remainder) = sqlx::query_as::<_, (i64, i64)>(
        "INSERT INTO dam_global.tenant_spend
                (tenant_id, quota_key, period_start, used_value, spend_remainder_micro)
         VALUES ($1, $2, $3, $4 / $5, $4 % $5)
         ON CONFLICT (tenant_id, quota_key, period_start) DO UPDATE
            SET used_value = dam_global.tenant_spend.used_value
                             + ($4 + dam_global.tenant_spend.spend_remainder_micro) / $5,
                spend_remainder_micro =
                             ($4 + dam_global.tenant_spend.spend_remainder_micro) % $5,
                updated_at = now()
         RETURNING used_value, spend_remainder_micro",
    )
    .bind(tenant_id)
    .bind(quota_key)
    .bind(period_start)
    .bind(micro)
    .bind(MICRO)
    .fetch_one(&mut *conn)
    .await?;

    let Some(quota) = quota(conn, tenant_id, quota_key).await? else {
        return Ok(Verdict::Allowed);
    };
    let verdict = verdict(&quota, used);
    match verdict {
        Verdict::Warned { .. } => {
            sqlx::query(
                "UPDATE dam_global.tenant_spend SET warned_at = now()
                  WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3
                    AND warned_at IS NULL",
            )
            .bind(tenant_id)
            .bind(quota_key)
            .bind(period_start)
            .execute(&mut *conn)
            .await?;
        }
        Verdict::Refused { .. } => {
            sqlx::query(
                "UPDATE dam_global.tenant_spend
                    SET exceeded_at = now(),
                        warned_at = COALESCE(warned_at, now())
                  WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3
                    AND exceeded_at IS NULL",
            )
            .bind(tenant_id)
            .bind(quota_key)
            .bind(period_start)
            .execute(&mut *conn)
            .await?;
        }
        Verdict::Allowed => {}
    }
    Ok(verdict)
}

/// Sets or replaces a cap.
pub async fn set(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    quota_key: &str,
    quota: &Quota,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO dam_global.tenant_quotas
                (tenant_id, quota_key, limit_value, warn_at_fraction, enforcement, updated_at)
         VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (tenant_id, quota_key) DO UPDATE
            SET limit_value = EXCLUDED.limit_value,
                warn_at_fraction = EXCLUDED.warn_at_fraction,
                enforcement = EXCLUDED.enforcement,
                updated_at = now()",
    )
    .bind(tenant_id)
    .bind(quota_key)
    .bind(quota.limit_value)
    .bind(quota.warn_at_fraction)
    .bind(match quota.enforcement {
        Enforcement::Hard => "hard",
        Enforcement::Soft => "soft",
    })
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(limit: i64, enforcement: Enforcement) -> Quota {
        Quota {
            limit_value: limit,
            warn_at_fraction: 0.8,
            enforcement,
        }
    }

    #[test]
    fn a_warning_fires_at_the_fraction_and_not_a_unit_later() {
        // 0.8 of 1000 is 800, and a `real` column stores 0.8 as slightly more than 0.8 — so rounding up would
        // put the line at 801 and this is the assertion that catches it.
        let quota = cap(1000, Enforcement::Hard);
        assert_eq!(verdict(&quota, 799), Verdict::Allowed);
        assert_eq!(
            verdict(&quota, 800),
            Verdict::Warned {
                used: 800,
                limit: 1000
            }
        );
        // And on a limit small enough that one unit is 20% of it, the line is still where it was configured.
        let small = cap(5, Enforcement::Hard);
        assert_eq!(verdict(&small, 3), Verdict::Allowed);
        assert_eq!(verdict(&small, 4), Verdict::Warned { used: 4, limit: 5 });
    }

    #[test]
    fn a_hard_cap_refuses_at_the_limit_and_a_soft_one_never_does() {
        assert_eq!(
            verdict(&cap(1000, Enforcement::Hard), 1000),
            Verdict::Refused {
                used: 1000,
                limit: 1000
            }
        );
        assert_eq!(
            verdict(&cap(1000, Enforcement::Soft), 5000),
            Verdict::Warned {
                used: 5000,
                limit: 1000
            }
        );
        assert!(
            verdict(&cap(1000, Enforcement::Soft), 5000).allowed(),
            "a soft cap keeps the customer working"
        );
    }

    #[test]
    fn a_zero_limit_refuses_everything_rather_than_dividing_by_it() {
        assert_eq!(
            verdict(&cap(0, Enforcement::Hard), 0),
            Verdict::Refused { used: 0, limit: 0 }
        );
    }

    #[test]
    fn an_unreadable_enforcement_fails_open() {
        // A row written by a newer migration must not turn into a hard stop on a tenant's library.
        assert_eq!(Enforcement::parse("advisory"), Enforcement::Soft);
        assert_eq!(Enforcement::parse("hard"), Enforcement::Hard);
    }

    #[test]
    fn a_month_is_a_calendar_month_in_utc() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-08-20T23:59:59Z")
            .map(|value| value.with_timezone(&chrono::Utc));
        assert_eq!(
            at.map(month_start),
            Ok(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap_or_default())
        );
    }
}
