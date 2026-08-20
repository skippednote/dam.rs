//! Spend caps against a real database (G20, M5a·4).
//!
//! The arithmetic has unit tests beside it. What needs a database is the part that is a *statement*: the charge
//! is one upsert carrying a sub-unit remainder, and the properties worth defending are the ones a second worker
//! could break.
//!
//! - **Sub-unit charges accumulate.** A hundred charges of a third of a cent are thirty-three cents, not zero.
//!   Rounding either way makes an AI spend cap decoration — which is the failure the column was added for.
//! - **Concurrent charges do not lose an increment.** Two workers enriching the same tenant is the normal case,
//!   not the edge one.
//! - **A period is a calendar month**, and last month's spend does not follow the tenant into this one.
//! - **The thresholds are stamped once.** `warned_at` answers "when did this start", so a value that moved with
//!   every charge would answer "when did we last look".

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::quotas::{self, Enforcement, Quota, Verdict};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool, Uuid) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    migrate::global(&pg.url()).await.expect("global");
    let pool = pg.pool().clone();
    let tenant: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("tenant");
    (pg, pool, tenant)
}

macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

fn cap(limit: i64, enforcement: Enforcement) -> Quota {
    Quota {
        limit_value: limit,
        warn_at_fraction: 0.8,
        enforcement,
    }
}

fn today() -> chrono::NaiveDate {
    quotas::month_start(chrono::Utc::now())
}

#[tokio::test]
async fn an_unconfigured_tenant_is_allowed_rather_than_refused() {
    let (_pg, pool, tenant) = db().await;
    // Absence of a cap is not a cap of zero. Defaulting the other way would stop every tenant nobody has got
    // around to configuring.
    assert_eq!(
        quotas::check(c!(pool), tenant, quotas::AI_SPEND, today())
            .await
            .expect("check"),
        Verdict::Allowed
    );
    assert!(
        quotas::quota(c!(pool), tenant, quotas::AI_SPEND)
            .await
            .expect("quota")
            .is_none()
    );
}

#[tokio::test]
async fn charges_below_one_whole_unit_accumulate_instead_of_rounding_away() {
    let (_pg, pool, tenant) = db().await;
    let period = today();
    // A third of a cent, three hundred times: a dollar. Rounded down that is nothing at all, and a cap of any
    // size would never be reached — the exact way an `ai_spend_cents_month` limit becomes decoration.
    let third = quotas::MICRO / 3;
    for _ in 0..300 {
        quotas::charge(c!(pool), tenant, quotas::AI_SPEND, period, third)
            .await
            .expect("charge");
    }
    let used = quotas::used(c!(pool), tenant, quotas::AI_SPEND, period)
        .await
        .expect("used");
    assert_eq!(used, 99, "300 × 1/3 of a cent, less the part still pending");

    let remainder: i64 = sqlx::query_scalar(
        "SELECT spend_remainder_micro FROM dam_global.tenant_spend \
         WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant)
    .bind(quotas::AI_SPEND)
    .bind(period)
    .fetch_one(&pool)
    .await
    .expect("remainder");
    assert!(
        remainder > 0 && remainder < quotas::MICRO,
        "the pending fraction stays on the row: {remainder}"
    );
    // And the total is conserved: whole units plus the pending fraction is exactly what was charged.
    assert_eq!(
        i128::from(used) * i128::from(quotas::MICRO) + i128::from(remainder),
        i128::from(third) * 300
    );
}

#[tokio::test]
async fn two_workers_charging_at_once_lose_nothing() {
    let (_pg, pool, tenant) = db().await;
    let period = today();
    // Twenty concurrent charges of a whole cent each. A read-modify-write would drop some; one statement cannot.
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            quotas::charge(c!(pool), tenant, quotas::AI_SPEND, period, quotas::MICRO)
                .await
                .expect("charge")
        }));
    }
    for task in tasks {
        task.await.expect("join");
    }
    assert_eq!(
        quotas::used(c!(pool), tenant, quotas::AI_SPEND, period)
            .await
            .expect("used"),
        20
    );
}

#[tokio::test]
async fn a_hard_cap_refuses_and_stamps_when_it_started() {
    let (_pg, pool, tenant) = db().await;
    let period = today();
    quotas::set(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        &cap(100, Enforcement::Hard),
    )
    .await
    .expect("set");

    let verdict = quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        period,
        80 * quotas::MICRO,
    )
    .await
    .expect("charge");
    assert_eq!(
        verdict,
        Verdict::Warned {
            used: 80,
            limit: 100
        }
    );

    let verdict = quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        period,
        30 * quotas::MICRO,
    )
    .await
    .expect("charge");
    assert_eq!(
        verdict,
        Verdict::Refused {
            used: 110,
            limit: 100
        }
    );
    assert!(!verdict.allowed(), "an enrichment job reads this and stops");

    let (warned, exceeded): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT warned_at, exceeded_at FROM dam_global.tenant_spend \
         WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant)
    .bind(quotas::AI_SPEND)
    .bind(period)
    .fetch_one(&pool)
    .await
    .expect("stamps");
    let warned = warned.expect("warned when it crossed the fraction");
    let exceeded = exceeded.expect("exceeded when it passed the limit");

    // A further charge must not move either stamp: the question is "when did this start".
    quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        period,
        10 * quotas::MICRO,
    )
    .await
    .expect("charge");
    let (warned_after, exceeded_after): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT warned_at, exceeded_at FROM dam_global.tenant_spend \
         WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant)
    .bind(quotas::AI_SPEND)
    .bind(period)
    .fetch_one(&pool)
    .await
    .expect("stamps");
    assert_eq!(warned_after, Some(warned));
    assert_eq!(exceeded_after, Some(exceeded));
}

#[tokio::test]
async fn a_second_warning_does_not_move_the_first_ones_stamp() {
    // Separate from the hard-cap case on purpose. There, every charge after the first crosses the *limit*, so it
    // takes the exceeded branch and the warning branch is never asked to be idempotent again. This stays inside
    // the warning band, which is where a stamp that advanced with every charge would answer "when did we last
    // look" instead of "when did this start".
    let (_pg, pool, tenant) = db().await;
    let period = today();
    quotas::set(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        &cap(100, Enforcement::Hard),
    )
    .await
    .expect("set");

    async fn warned_at(
        pool: &PgPool,
        tenant: Uuid,
        period: chrono::NaiveDate,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT warned_at FROM dam_global.tenant_spend \
             WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
        )
        .bind(tenant)
        .bind(quotas::AI_SPEND)
        .bind(period)
        .fetch_one(pool)
        .await
        .expect("stamp")
    }

    quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        period,
        80 * quotas::MICRO,
    )
    .await
    .expect("charge");
    let first = warned_at(&pool, tenant, period)
        .await
        .expect("warned when it crossed the fraction");

    let verdict = quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        period,
        5 * quotas::MICRO,
    )
    .await
    .expect("charge");
    assert_eq!(
        verdict,
        Verdict::Warned {
            used: 85,
            limit: 100
        },
        "still inside the band, so the warning branch runs again"
    );
    assert_eq!(warned_at(&pool, tenant, period).await, Some(first));
}

#[tokio::test]
async fn a_soft_cap_warns_and_keeps_serving() {
    let (_pg, pool, tenant) = db().await;
    let period = today();
    quotas::set(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        &cap(100, Enforcement::Soft),
    )
    .await
    .expect("set");
    let verdict = quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        period,
        500 * quotas::MICRO,
    )
    .await
    .expect("charge");
    assert!(verdict.allowed(), "a soft cap is a warning, not a stop");
    assert!(matches!(verdict, Verdict::Warned { .. }), "{verdict:?}");
    // It is still recorded as having been warned, so somebody can be told.
    let warned: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT warned_at FROM dam_global.tenant_spend \
         WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant)
    .bind(quotas::AI_SPEND)
    .bind(period)
    .fetch_one(&pool)
    .await
    .expect("warned");
    assert!(warned.is_some());
}

#[tokio::test]
async fn last_months_spend_does_not_follow_the_tenant_into_this_one() {
    let (_pg, pool, tenant) = db().await;
    let this_month = today();
    let last_month = this_month
        .checked_sub_months(chrono::Months::new(1))
        .expect("a previous month");
    quotas::set(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        &cap(100, Enforcement::Hard),
    )
    .await
    .expect("set");
    quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        last_month,
        500 * quotas::MICRO,
    )
    .await
    .expect("charge");

    assert_eq!(
        quotas::check(c!(pool), tenant, quotas::AI_SPEND, this_month)
            .await
            .expect("check"),
        Verdict::Allowed,
        "a monthly cap that carried the previous month's overspend would never reset"
    );
    assert_eq!(
        quotas::used(c!(pool), tenant, quotas::AI_SPEND, last_month)
            .await
            .expect("used"),
        500,
        "and the history is still there"
    );
}

#[tokio::test]
async fn a_cap_can_be_changed_without_losing_what_has_been_spent() {
    let (_pg, pool, tenant) = db().await;
    let period = today();
    quotas::set(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        &cap(100, Enforcement::Soft),
    )
    .await
    .expect("set");
    quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        period,
        90 * quotas::MICRO,
    )
    .await
    .expect("charge");
    // Raising the cap must not zero the counter — an operator raising a limit is not forgiving the spend.
    quotas::set(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        &cap(1_000, Enforcement::Hard),
    )
    .await
    .expect("raise");
    assert_eq!(
        quotas::used(c!(pool), tenant, quotas::AI_SPEND, period)
            .await
            .expect("used"),
        90
    );
    assert_eq!(
        quotas::check(c!(pool), tenant, quotas::AI_SPEND, period)
            .await
            .expect("check"),
        Verdict::Allowed
    );
}

#[tokio::test]
async fn the_remainder_column_refuses_a_whole_unit() {
    let (_pg, pool, _tenant) = db().await;
    // The CHECK is the specification: a remainder of one whole unit means a charge that was not carried, and the
    // row would under-report spend for as long as it lasted.
    let refused = sqlx::query(
        "INSERT INTO dam_global.tenant_spend \
         (tenant_id, quota_key, period_start, used_value, spend_remainder_micro) \
         VALUES ((SELECT id FROM dam_global.tenants LIMIT 1), 'ai_spend_cents_month', \
                 current_date, 0, 1000000)",
    )
    .execute(&pool)
    .await;
    let error = refused.expect_err("a whole unit is not a remainder");
    assert!(
        error.to_string().contains("spend_remainder_micro"),
        "{error}"
    );
}
