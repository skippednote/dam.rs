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

// ─── levels, as against flows (G19) ─────────────────────────────────────────
//
// `charge` accumulates, which is right for a flow: cents spent, bytes served. A **level** is a measurement of
// what exists right now — bytes stored, assets held — and the metering pass remeasures it daily. Feeding one
// through `charge` would add the whole library to the counter every pass, so a tenant holding a steady terabyte
// would trip a two-terabyte cap on the second day without having stored anything more.
//
// That is not a subtle failure and it is not one a reader would spot in a call site, so `observe` refuses a flow
// key outright rather than trusting the caller to have picked the right function.

#[tokio::test]
async fn a_level_is_replaced_by_each_measurement_rather_than_accumulated() {
    let (_pg, pool, tenant) = db().await;
    quotas::set(
        c!(pool),
        tenant,
        quotas::STORAGE_BYTES,
        &cap(1_000, Enforcement::Hard),
    )
    .await
    .expect("cap");

    // Three passes measuring the same library. A steady 400 must stay 400, not become 1,200.
    for _ in 0..3 {
        let verdict = quotas::observe(c!(pool), tenant, quotas::STORAGE_BYTES, today(), 400)
            .await
            .expect("observe");
        assert_eq!(verdict, Verdict::Allowed, "a steady level must not creep");
    }
    assert_eq!(
        quotas::used(c!(pool), tenant, quotas::STORAGE_BYTES, today())
            .await
            .expect("used"),
        400,
    );

    // And it goes *down* when the library does, which a counter could never do. An asset deleted has to release
    // the cap it was holding, or a tenant who tidied up would still be refused.
    quotas::observe(c!(pool), tenant, quotas::STORAGE_BYTES, today(), 120)
        .await
        .expect("observe");
    assert_eq!(
        quotas::used(c!(pool), tenant, quotas::STORAGE_BYTES, today())
            .await
            .expect("used"),
        120,
    );
}

#[tokio::test]
async fn observing_a_flow_is_refused_rather_than_silently_wrong() {
    // The failure mode of guessing is a counter reset to one call's worth on every pass — a cap that never
    // fires. Refusing by name is the only reading that cannot be exploited by a careless call site.
    let (_pg, pool, tenant) = db().await;
    let refused = quotas::observe(c!(pool), tenant, quotas::AI_SPEND, today(), 500).await;
    match refused {
        Err(dam_db::Error::Inconsistent(message)) => {
            assert!(message.contains("counts a flow"), "{message}");
            assert!(message.contains("use charge"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // And nothing was written, so a mistaken call leaves no half-state behind.
    assert_eq!(
        quotas::used(c!(pool), tenant, quotas::AI_SPEND, today())
            .await
            .expect("used"),
        0,
    );
}

#[tokio::test]
async fn a_level_crossing_a_cap_stamps_the_same_way_a_charge_does() {
    // `charge` and `observe` share one `stamp`, so the two cannot disagree about when a tenant was first told.
    let (_pg, pool, tenant) = db().await;
    quotas::set(
        c!(pool),
        tenant,
        quotas::ASSET_COUNT,
        &cap(100, Enforcement::Hard),
    )
    .await
    .expect("cap");

    assert_eq!(
        quotas::observe(c!(pool), tenant, quotas::ASSET_COUNT, today(), 80)
            .await
            .expect("observe"),
        Verdict::Warned {
            used: 80,
            limit: 100
        },
        "the warning line is 80% and it is crossed by a measurement, not by a charge",
    );
    let (warned, exceeded): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT warned_at, exceeded_at FROM dam_global.tenant_spend \
             WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant)
    .bind(quotas::ASSET_COUNT)
    .bind(today())
    .fetch_one(&pool)
    .await
    .expect("row");
    let first_warning = warned.expect("warned");
    assert!(exceeded.is_none(), "warned is not exceeded");

    assert_eq!(
        quotas::observe(c!(pool), tenant, quotas::ASSET_COUNT, today(), 100)
            .await
            .expect("observe"),
        Verdict::Refused {
            used: 100,
            limit: 100
        },
    );
    let (warned, exceeded): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT warned_at, exceeded_at FROM dam_global.tenant_spend \
             WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant)
    .bind(quotas::ASSET_COUNT)
    .bind(today())
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(
        warned,
        Some(first_warning),
        "the first warning's stamp does not move when the cap is later exceeded",
    );
    assert!(exceeded.is_some());

    // And dropping back under does *not* clear the stamps. They record that it happened, which is the whole
    // point — a tenant who deleted their way back under the line was still over it.
    quotas::observe(c!(pool), tenant, quotas::ASSET_COUNT, today(), 10)
        .await
        .expect("observe");
    let (warned, exceeded): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT warned_at, exceeded_at FROM dam_global.tenant_spend \
             WHERE tenant_id = $1 AND quota_key = $2 AND period_start = $3",
    )
    .bind(tenant)
    .bind(quotas::ASSET_COUNT)
    .bind(today())
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(warned, Some(first_warning));
    assert!(exceeded.is_some(), "'we were not told' stays answerable");
    // The *verdict* is what changes, because that is what gates work.
    assert_eq!(
        quotas::check(c!(pool), tenant, quotas::ASSET_COUNT, today())
            .await
            .expect("check"),
        Verdict::Allowed,
    );
}

#[tokio::test]
async fn the_standing_report_says_which_numbers_are_levels() {
    // "1.2 TB" is what exists if the key is `storage_bytes` and what happened if it is a monthly flow. A screen
    // that could not tell them apart would be misleading about the more alarming one.
    let (_pg, pool, tenant) = db().await;
    quotas::set(
        c!(pool),
        tenant,
        quotas::STORAGE_BYTES,
        &cap(1_000, Enforcement::Soft),
    )
    .await
    .expect("cap");
    quotas::set(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        &cap(500, Enforcement::Hard),
    )
    .await
    .expect("cap");
    quotas::observe(c!(pool), tenant, quotas::STORAGE_BYTES, today(), 900)
        .await
        .expect("observe");
    quotas::charge(
        c!(pool),
        tenant,
        quotas::AI_SPEND,
        today(),
        600 * quotas::MICRO,
    )
    .await
    .expect("charge");

    let standing = quotas::standing(c!(pool), tenant, today())
        .await
        .expect("standing");
    assert_eq!(standing.len(), 2, "{standing:?}");

    let spend = standing
        .iter()
        .find(|row| row.quota_key == quotas::AI_SPEND)
        .expect("ai spend");
    assert!(!spend.is_level, "cents spent this month is a flow");
    assert_eq!(spend.used, 600);
    assert_eq!(
        spend.verdict,
        Verdict::Refused {
            used: 600,
            limit: 500
        }
    );
    assert!(spend.exceeded_at.is_some());

    let storage = standing
        .iter()
        .find(|row| row.quota_key == quotas::STORAGE_BYTES)
        .expect("storage");
    assert!(storage.is_level, "bytes stored is a level");
    assert_eq!(storage.used, 900);
    // A soft cap at 90% of its limit is a warning and still allowed — the customer keeps working.
    assert_eq!(
        storage.verdict,
        Verdict::Warned {
            used: 900,
            limit: 1_000
        }
    );

    // A configured cap the tenant has never touched still appears, with zero used — an operator needs to see
    // the cap exists. What must *not* appear is a key with no cap at all.
    quotas::set(
        c!(pool),
        tenant,
        quotas::RESTORE_SPEND,
        &cap(50, Enforcement::Soft),
    )
    .await
    .expect("cap");
    let standing = quotas::standing(c!(pool), tenant, today())
        .await
        .expect("standing");
    assert_eq!(standing.len(), 3);
    let restore = standing
        .iter()
        .find(|row| row.quota_key == quotas::RESTORE_SPEND)
        .expect("restore");
    assert_eq!(restore.used, 0);
    assert_eq!(restore.verdict, Verdict::Allowed);
    assert!(restore.warned_at.is_none());
}

#[tokio::test]
async fn an_unconfigured_key_is_absent_rather_than_a_cap_of_zero() {
    // Listing one with a limit of nothing would read as exhausted. `check` already says an absent cap is not a
    // cap of zero; the report has to agree, or a screen contradicts the enforcement.
    let (_pg, pool, tenant) = db().await;
    assert!(
        quotas::standing(c!(pool), tenant, today())
            .await
            .expect("standing")
            .is_empty(),
    );
    // Even after something has been measured against it: a level with no cap is a number nobody capped.
    quotas::observe(c!(pool), tenant, quotas::STORAGE_BYTES, today(), 5_000)
        .await
        .expect("observe");
    assert!(
        quotas::standing(c!(pool), tenant, today())
            .await
            .expect("standing")
            .is_empty(),
    );
    assert_eq!(
        quotas::check(c!(pool), tenant, quotas::STORAGE_BYTES, today())
            .await
            .expect("check"),
        Verdict::Allowed,
    );
}
