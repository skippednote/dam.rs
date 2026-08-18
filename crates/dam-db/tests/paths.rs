//! Paths: triggered notifications (3.6, G9).
//!
//! The schema calls `path_firings` a ledger that "exists purely for idempotency", and that is the whole
//! difficulty. A daily "expiring in 30 days" sweep sees the same asset on all thirty days — so either the
//! ledger deduplicates it, or the recipient gets thirty emails, filters the path to trash, and then misses the
//! real warning.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_db::paths::{self, FireOutcome, Path, Subject};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn asset(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

/// Creates a path row and returns the loaded definition.
async fn create_path(
    pool: &PgPool,
    name: &str,
    trigger: &str,
    lead_days: Option<i32>,
    digest_window: Option<&str>,
    throttle: Option<&str>,
) -> Path {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO paths \
         (id, name, trigger_kind, lead_days, subject_template, body_template, \
          digest_window, throttle_per_asset) \
         VALUES ($1, $2, $3, $4, 'subject', 'body', $5::interval, $6::interval)",
    )
    .bind(id)
    .bind(name)
    .bind(trigger)
    .bind(lead_days)
    .bind(digest_window)
    .bind(throttle)
    .execute(pool)
    .await
    .expect("path");

    paths::enabled_for(pool, trigger)
        .await
        .expect("load")
        .into_iter()
        .find(|p| p.id == id)
        .expect("the path we just created")
}

// ─── the deduplication ──────────────────────────────────────────────────────

async fn a_daily_sweep_notifies_once_not_once_per_day(pool: &PgPool) {
    // The property the ledger exists for. Thirty sweeps see the same asset with the same expiry, and exactly
    // one notification should come out.
    let path = create_path(
        pool,
        "licence 30d",
        "license_expiring",
        Some(30),
        None,
        None,
    )
    .await;
    let id = asset(pool, "sweeping").await;
    let expiry = now() + Duration::days(30);

    let mut recorded = 0;
    for day in 0..30 {
        let sweep_ran_at = now() + Duration::days(day);
        let outcome = paths::fire(
            pool,
            &path,
            Some(id),
            &Subject::Deadline { at: expiry },
            1,
            sweep_ran_at,
        )
        .await
        .expect("fire");
        if matches!(outcome, FireOutcome::Recorded(_)) {
            recorded += 1;
        }
    }
    assert_eq!(
        recorded, 1,
        "thirty sweeps of one expiry must produce one notification"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM path_firings WHERE path_id = $1")
        .bind(path.id)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
}

async fn keying_a_deadline_on_the_sweep_time_instead_would_notify_every_day(pool: &PgPool) {
    // The trap, demonstrated rather than described — and it is a *call-site* mistake, not a bug inside
    // `digest_key`: reaching for `Subject::Recurring { bucket: now }` because "the sweep ran now" is the
    // natural thing to write, and it produces a fresh key on every run.
    //
    // Written as a separate case because the thirty-sweeps test above cannot catch it. Thirty iterations
    // complete inside one wall-clock second, so a `Utc::now()`-based key deduplicates by accident and the test
    // passes for a timing reason.
    let path = create_path(
        pool,
        "wrong subject",
        "license_expiring",
        Some(30),
        None,
        None,
    )
    .await;
    let id = asset(pool, "wrong-subject").await;

    let mut recorded = 0;
    for day in 0..5 {
        let sweep_ran_at = now() + Duration::days(day);
        let outcome = paths::fire(
            pool,
            &path,
            Some(id),
            // The mistake: keyed on when the sweep ran, with no digest window to collapse it.
            &Subject::Recurring {
                bucket: paths::digest_bucket(sweep_ran_at, None),
            },
            1,
            sweep_ran_at,
        )
        .await
        .expect("fire");
        if matches!(outcome, FireOutcome::Recorded(_)) {
            recorded += 1;
        }
    }
    assert_eq!(
        recorded, 5,
        "keyed on the sweep time, five sweeps produce five notifications — which is why a deadline must be \
         keyed on the deadline"
    );

    // The same five sweeps keyed on the deadline produce one, which is the contrast that makes the point.
    let correct = create_path(
        pool,
        "right subject",
        "license_expiring",
        Some(30),
        None,
        None,
    )
    .await;
    let expiry = now() + Duration::days(30);
    let mut once = 0;
    for day in 0..5 {
        let outcome = paths::fire(
            pool,
            &correct,
            Some(id),
            &Subject::Deadline { at: expiry },
            1,
            now() + Duration::days(day),
        )
        .await
        .expect("fire");
        if matches!(outcome, FireOutcome::Recorded(_)) {
            once += 1;
        }
    }
    assert_eq!(once, 1);
}

async fn a_recurring_subject_collapses_within_its_digest_window(pool: &PgPool) {
    // `Recurring` is not simply wrong — it is right for "this asset still has no AI disclosure", where the
    // point *is* to repeat. The digest window is what sets the cadence, and without one it repeats per sweep.
    let path = create_path(
        pool,
        "recurring digest",
        "ai_disclosure_missing",
        None,
        Some("1 day"),
        None,
    )
    .await;
    let id = asset(pool, "recurring").await;
    let window = path.digest_window;

    let mut recorded = 0;
    // Three sweeps three hours apart from noon: 12:00, 15:00, 18:00, all before midnight and so all in one
    // daily bucket.
    //
    // My first version used four sweeps six hours apart and asserted one notification — wrong, and the code
    // was right. Buckets are truncated **from the epoch**, so a one-day window has midnight boundaries, and
    // 12:00 + 18h lands on the next day. Truncating from the epoch rather than from a start time is what lets
    // two workers agree on the bucket without coordinating, so the boundaries are not negotiable.
    for step in 0..3 {
        let at = now() + Duration::hours(step * 3);
        let outcome = paths::fire(
            pool,
            &path,
            Some(id),
            &Subject::Recurring {
                bucket: paths::digest_bucket(at, window),
            },
            1,
            at,
        )
        .await
        .expect("fire");
        if matches!(outcome, FireOutcome::Recorded(_)) {
            recorded += 1;
        }
    }
    assert_eq!(
        recorded, 1,
        "three sweeps inside one daily bucket are one notification"
    );

    // And the next day is a new bucket, so the reminder recurs — which is the whole point of this subject.
    let tomorrow = now() + Duration::days(2);
    let outcome = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Recurring {
            bucket: paths::digest_bucket(tomorrow, window),
        },
        1,
        tomorrow,
    )
    .await
    .expect("fire");
    assert!(
        matches!(outcome, FireOutcome::Recorded(_)),
        "a recurring reminder must recur at the digest cadence"
    );
}

async fn a_renewed_licence_gets_a_fresh_warning(pool: &PgPool) {
    // The other side of the same key. If the key ignored the deadline, renewing an asset once would silence it
    // forever — and the next lapse would arrive unannounced.
    let path = create_path(
        pool,
        "licence renew",
        "license_expiring",
        Some(30),
        None,
        None,
    )
    .await;
    let id = asset(pool, "renewed").await;

    let first = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Deadline {
            at: now() + Duration::days(30),
        },
        1,
        now(),
    )
    .await
    .expect("fire");
    assert!(matches!(first, FireOutcome::Recorded(_)));

    let after_renewal = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Deadline {
            at: now() + Duration::days(395),
        },
        1,
        now(),
    )
    .await
    .expect("fire");
    assert!(
        matches!(after_renewal, FireOutcome::Recorded(_)),
        "a new expiry deserves its own warning"
    );
}

async fn the_fire_count_only_counts_real_notifications(pool: &PgPool) {
    // `fire_count` is the number somebody looks at to judge whether a path is too noisy. Incrementing it on a
    // deduplicated sweep would make every path look thirty times noisier than it is.
    let path = create_path(pool, "counted", "asset_uploaded", None, None, None).await;
    let id = asset(pool, "counted").await;
    let event = Uuid::new_v4();

    for _ in 0..5 {
        paths::fire(
            pool,
            &path,
            Some(id),
            &Subject::Event { id: event },
            1,
            now(),
        )
        .await
        .expect("fire");
    }

    let (count, last): (i64, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT fire_count, last_fired_at FROM paths WHERE id = $1")
            .bind(path.id)
            .fetch_one(pool)
            .await
            .expect("row");
    assert_eq!(count, 1, "five deduplicated attempts are one firing");
    assert_eq!(last, Some(now()));
}

async fn two_paths_on_the_same_asset_fire_independently(pool: &PgPool) {
    // The 60/30/7-day escalation pattern the schema describes: several paths on one trigger with different
    // lead times. They must not deduplicate against each other.
    let sixty = create_path(
        pool,
        "licence 60d",
        "license_expiring",
        Some(60),
        None,
        None,
    )
    .await;
    let seven = create_path(pool, "licence 7d", "license_expiring", Some(7), None, None).await;
    let id = asset(pool, "escalating").await;
    let expiry = now() + Duration::days(90);

    for path in [&sixty, &seven] {
        let outcome = paths::fire(
            pool,
            path,
            Some(id),
            &Subject::Deadline { at: expiry },
            1,
            now(),
        )
        .await
        .expect("fire");
        assert!(
            matches!(outcome, FireOutcome::Recorded(_)),
            "{} must fire on its own schedule",
            path.name
        );
    }
}

// ─── throttling ─────────────────────────────────────────────────────────────

async fn a_throttle_suppresses_a_second_firing_within_the_window(pool: &PgPool) {
    // The failure mode of a notification system is 4,000 emails when a bulk import lands. A throttle bounds
    // per-asset noise regardless of how many distinct events arrive.
    let path = create_path(
        pool,
        "throttled",
        "metadata_changed",
        None,
        None,
        Some("1 hour"),
    )
    .await;
    assert!(
        path.throttle_per_asset.is_some(),
        "the fixture must be throttled"
    );
    let id = asset(pool, "throttled").await;

    let first = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: Uuid::new_v4() },
        1,
        now(),
    )
    .await
    .expect("fire");
    assert!(matches!(first, FireOutcome::Recorded(_)));

    // A *different* event on the same asset, ten minutes later.
    let second = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: Uuid::new_v4() },
        1,
        now() + Duration::minutes(10),
    )
    .await
    .expect("fire");
    match second {
        FireOutcome::Throttled { until } => {
            assert_eq!(until, now() + Duration::hours(1));
        }
        other => panic!("expected a throttle, got {other:?}"),
    }

    // And past the window it fires again.
    let later = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: Uuid::new_v4() },
        1,
        now() + Duration::hours(2),
    )
    .await
    .expect("fire");
    assert!(matches!(later, FireOutcome::Recorded(_)));
}

async fn a_throttled_firing_leaves_no_ledger_row(pool: &PgPool) {
    // Recording a suppressed row would claim the digest key, so the notification would never fire even after
    // the throttle window passed — turning a rate limit into permanent silence.
    let path = create_path(
        pool,
        "no ghost rows",
        "metadata_changed",
        None,
        None,
        Some("1 hour"),
    )
    .await;
    let id = asset(pool, "ghost").await;
    let event = Uuid::new_v4();

    paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: Uuid::new_v4() },
        1,
        now(),
    )
    .await
    .expect("fire");
    let throttled = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: event },
        1,
        now() + Duration::minutes(1),
    )
    .await
    .expect("fire");
    assert!(matches!(throttled, FireOutcome::Throttled { .. }));

    // The throttled subject's key must be unclaimed, so it can fire once the window passes.
    let key = paths::digest_key(&path, Some(id), &Subject::Event { id: event });
    assert!(
        paths::by_key(pool, path.id, &key)
            .await
            .expect("lookup")
            .is_none(),
        "a throttled firing must not occupy its digest key"
    );

    let after = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: event },
        1,
        now() + Duration::hours(2),
    )
    .await
    .expect("fire");
    assert!(
        matches!(after, FireOutcome::Recorded(_)),
        "and the same subject must fire once the throttle lifts"
    );
}

async fn a_throttle_is_per_asset_not_per_path(pool: &PgPool) {
    // A bulk import touches many assets. Throttling the path globally would notify about the first asset and
    // silently drop the rest, which is worse than the flood it was meant to prevent.
    let path = create_path(
        pool,
        "per asset",
        "metadata_changed",
        None,
        None,
        Some("1 hour"),
    )
    .await;
    let first = asset(pool, "per-asset-a").await;
    let second = asset(pool, "per-asset-b").await;

    for id in [first, second] {
        let outcome = paths::fire(
            pool,
            &path,
            Some(id),
            &Subject::Event { id: Uuid::new_v4() },
            1,
            now(),
        )
        .await
        .expect("fire");
        assert!(
            matches!(outcome, FireOutcome::Recorded(_)),
            "each asset gets its own throttle window"
        );
    }
}

// ─── delivery ───────────────────────────────────────────────────────────────

async fn claiming_and_marking_moves_a_firing_through_its_states(pool: &PgPool) {
    let path = create_path(pool, "delivered", "restore_ready", None, None, None).await;
    let id = asset(pool, "delivered").await;
    let outcome = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: Uuid::new_v4() },
        3,
        now(),
    )
    .await
    .expect("fire");
    let firing = match outcome {
        FireOutcome::Recorded(f) => f,
        other => panic!("expected a recording, got {other:?}"),
    };

    let claimed = paths::claim_queued(pool, 100).await.expect("claim");
    assert!(claimed.iter().any(|f| f.id == firing.id));

    assert!(paths::mark_sent(pool, firing.id).await.expect("sent"));
    assert!(
        !paths::mark_sent(pool, firing.id).await.expect("sent"),
        "marking twice must be idempotent"
    );

    let claimed_again = paths::claim_queued(pool, 100).await.expect("claim");
    assert!(
        claimed_again.iter().all(|f| f.id != firing.id),
        "a sent firing must not be claimed again"
    );
}

async fn a_failed_delivery_keeps_its_row_so_the_key_stays_claimed(pool: &PgPool) {
    // Deleting a failed firing would let the next sweep fire it again — turning a provider outage into a flood
    // the moment it recovered.
    let path = create_path(pool, "failing", "restore_ready", None, None, None).await;
    let id = asset(pool, "failing").await;
    let event = Uuid::new_v4();
    let firing = match paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: event },
        1,
        now(),
    )
    .await
    .expect("fire")
    {
        FireOutcome::Recorded(f) => f,
        other => panic!("expected a recording, got {other:?}"),
    };

    assert!(
        paths::mark_failed(pool, firing.id, "smtp said no")
            .await
            .expect("failed")
    );
    let reason: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM path_firings WHERE id = $1")
            .bind(firing.id)
            .fetch_one(pool)
            .await
            .expect("read");
    assert_eq!(reason.as_deref(), Some("smtp said no"));

    // The key is still claimed, so a sweep does not re-fire it.
    let again = paths::fire(
        pool,
        &path,
        Some(id),
        &Subject::Event { id: event },
        1,
        now(),
    )
    .await
    .expect("fire");
    assert!(
        matches!(again, FireOutcome::AlreadyFired(_)),
        "a failed firing must not be re-fired by the next sweep"
    );
}

// ─── loading ────────────────────────────────────────────────────────────────

async fn only_enabled_paths_are_loaded_and_escalation_order_is_widest_first(pool: &PgPool) {
    // Widest lead time first, because that is the order an escalation reads in — 60 days, then 30, then 7.
    let trigger = "release_expiring";
    create_path(pool, "release 7d", trigger, Some(7), None, None).await;
    create_path(pool, "release 60d", trigger, Some(60), None, None).await;
    let disabled = create_path(pool, "release off", trigger, Some(14), None, None).await;
    sqlx::query("UPDATE paths SET enabled = false WHERE id = $1")
        .bind(disabled.id)
        .execute(pool)
        .await
        .expect("disable");

    let loaded = paths::enabled_for(pool, trigger).await.expect("load");
    let leads: Vec<Option<i32>> = loaded.iter().map(|p| p.lead_days).collect();
    assert_eq!(leads, vec![Some(60), Some(7)]);
    assert!(
        loaded.iter().all(|p| p.id != disabled.id),
        "a disabled path must not be loaded"
    );
}

async fn a_digest_window_is_read_back_as_a_duration(pool: &PgPool) {
    // Postgres intervals arrive as months/days/microseconds, and reading only the microseconds field would
    // silently turn a six-hour window into zero.
    let path = create_path(
        pool,
        "digested",
        "asset_uploaded",
        None,
        Some("6 hours"),
        None,
    )
    .await;
    assert_eq!(path.digest_window, Some(Duration::hours(6)));

    let daily = create_path(pool, "daily", "asset_uploaded", None, Some("1 day"), None).await;
    assert_eq!(
        daily.digest_window,
        Some(Duration::days(1)),
        "a day arrives in the days field, not as microseconds"
    );
}

#[tokio::test]
async fn the_path_invariants_hold() {
    let (_pg, pool) = db().await;

    a_daily_sweep_notifies_once_not_once_per_day(&pool).await;
    keying_a_deadline_on_the_sweep_time_instead_would_notify_every_day(&pool).await;
    a_recurring_subject_collapses_within_its_digest_window(&pool).await;
    a_renewed_licence_gets_a_fresh_warning(&pool).await;
    the_fire_count_only_counts_real_notifications(&pool).await;
    two_paths_on_the_same_asset_fire_independently(&pool).await;

    a_throttle_suppresses_a_second_firing_within_the_window(&pool).await;
    a_throttled_firing_leaves_no_ledger_row(&pool).await;
    a_throttle_is_per_asset_not_per_path(&pool).await;

    claiming_and_marking_moves_a_firing_through_its_states(&pool).await;
    a_failed_delivery_keeps_its_row_so_the_key_stays_claimed(&pool).await;

    only_enabled_paths_are_loaded_and_escalation_order_is_widest_first(&pool).await;
    a_digest_window_is_read_back_as_a_duration(&pool).await;
}
