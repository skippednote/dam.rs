//! The restore request lifecycle (3.4, §6.5).
//!
//! Two properties carry it, and both cost real money when they fail.
//!
//! **Duplicate requests coalesce.** Two people asking for the same archived asset must share one request and
//! one S3 call — a second `RestoreObject` on an ongoing restore is billed, so a naive INSERT pays twice for
//! one retrieval.
//!
//! **The expiry sweep is not optional.** A restored copy is temporary and the storage class never changed, so
//! when the window lapses a delivery URL starts failing at S3 with nothing in our system explaining why.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::restore::{self, Budget, Candidate, Plan, RetrievalPrices};
use dam_core::storage::{RestoreTier, StorageClass};
use dam_db::restores::{self, Outcome, RestoreSpec};
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

/// A plan for one 1 GB Glacier object, at `tier`.
fn plan_for(tier: RestoreTier, needs_approval: bool) -> Plan {
    let budget = Budget {
        per_request_cents: None,
        monthly_cents: None,
        approval_threshold_cents: if needs_approval { Some(0) } else { None },
        spent_this_month_cents: 0,
    };
    restore::plan(
        &[Candidate {
            bytes: 1_073_741_824,
            class: StorageClass::Glacier,
        }],
        tier,
        RetrievalPrices {
            standard_per_gb: 10_000_000_000,
            bulk_per_gb: 2_500_000_000,
            expedited_per_gb: 30_000_000_000,
            per_1000_requests: 5_000_000_000,
        },
        &budget,
        7,
        now(),
    )
    .expect("plan")
}

/// A connection from the schema-scoped pool.
///
/// The module takes a `&mut PgConnection` rather than a pool, because a tenant table needs the tenant's
/// `search_path` and that lives on a connection — which is what made the pool-shaped version unusable from
/// the worker. In a test the pool already carries the right search path, so one line borrows from it.
async fn conn(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("connection")
}

fn spec<'a>(key: &'a str, pool_id: Uuid) -> RestoreSpec<'a> {
    RestoreSpec {
        object_key: key,
        pool_id,
        asset_id: None,
        tier: RestoreTier::Bulk,
        keep_warm_days: 7,
        requested_by: None,
        batch_id: None,
        notify: serde_json::json!({}),
    }
}

// ─── coalescing ─────────────────────────────────────────────────────────────

async fn a_second_request_for_the_same_object_adopts_the_first(pool: &PgPool) {
    // A second `RestoreObject` on an ongoing restore is billed. Paying twice for one retrieval is a real
    // charge, and from the caller's side "somebody already asked" and "you asked" have the same answer — it
    // will be ready at the same time.
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Bulk, false);

    let first = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/aa/bb/one", pool_id),
        &plan,
    )
    .await
    .expect("first");
    assert!(matches!(first, Outcome::Created(_)));

    let second = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/aa/bb/one", pool_id),
        &plan,
    )
    .await
    .expect("second");
    match &second {
        Outcome::AlreadyInFlight(existing) => {
            assert_eq!(
                existing.id,
                first.request().id,
                "the second caller must adopt the first request, not create a rival"
            );
        }
        Outcome::Created(_) => panic!("a duplicate must not create a second request"),
    }

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM restore_requests WHERE object_key = $1")
            .bind("acme/o/aa/bb/one")
            .fetch_one(pool)
            .await
            .expect("count");
    assert_eq!(count, 1, "and there must be exactly one row to bill for");
}

async fn the_same_object_in_a_different_pool_is_a_different_restore(pool: &PgPool) {
    // Coalescing is per `(object_key, pool_id)`. The same key in two pools is two objects in two places, and
    // restoring one says nothing about the other.
    let key = "acme/o/cc/dd/two";
    let plan = plan_for(RestoreTier::Bulk, false);
    for _ in 0..2 {
        let outcome = restores::request(&mut *conn(pool).await, &spec(key, Uuid::new_v4()), &plan)
            .await
            .expect("request");
        assert!(matches!(outcome, Outcome::Created(_)));
    }
}

async fn a_finished_restore_does_not_block_a_new_one(pool: &PgPool) {
    // The unique index covers the in-flight states only. Once a copy has expired the asset is archived again,
    // and asking for it must work — otherwise an asset restored in March can never be restored in April.
    let pool_id = Uuid::new_v4();
    let key = "acme/o/ee/ff/three";
    let plan = plan_for(RestoreTier::Bulk, false);

    let first = restores::request(&mut *conn(pool).await, &spec(key, pool_id), &plan)
        .await
        .expect("first");
    let id = first.request().id;

    // Drive it to a terminal state.
    sqlx::query("UPDATE restore_requests SET state = 'expired' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("expire");

    let again = restores::request(&mut *conn(pool).await, &spec(key, pool_id), &plan)
        .await
        .expect("second");
    assert!(
        matches!(again, Outcome::Created(_)),
        "an expired restore must not block a fresh one"
    );
    assert_ne!(again.request().id, id);
}

// ─── approval ───────────────────────────────────────────────────────────────

async fn a_plan_needing_approval_is_held_rather_than_queued(pool: &PgPool) {
    // Decided from the plan rather than by the caller, so a request that needs a human cannot be enqueued by
    // a caller that forgot to look at `needs_approval`.
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Expedited, true);
    assert!(
        plan.needs_approval,
        "the fixture must actually need approval"
    );

    let outcome = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/gg/hh/held", pool_id),
        &plan,
    )
    .await
    .expect("request");
    assert_eq!(outcome.request().state, "awaiting_approval");

    // And it is not claimable while held — the whole point.
    let claimed = restores::claim_queued(&mut *conn(pool).await, 10, now())
        .await
        .expect("claim");
    assert!(
        claimed.iter().all(|r| r.id != outcome.request().id),
        "a held request must not be picked up by a worker"
    );
}

async fn approving_moves_it_to_queued_and_reports_only_the_first_call(pool: &PgPool) {
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Expedited, true);
    let outcome = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/ii/jj/appr", pool_id),
        &plan,
    )
    .await
    .expect("request");
    let id = outcome.request().id;
    let approver = Uuid::new_v4();

    assert!(
        restores::approve(&mut *conn(pool).await, id, approver, now())
            .await
            .expect("approve"),
        "the first approval reports that it acted"
    );
    assert!(
        !restores::approve(&mut *conn(pool).await, id, approver, now())
            .await
            .expect("approve"),
        "a repeat must be idempotent, so an audit entry is written once"
    );

    let state: String = sqlx::query_scalar("SELECT state FROM restore_requests WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("state");
    assert_eq!(state, "queued");
}

async fn approving_something_that_was_never_held_does_nothing(pool: &PgPool) {
    // Approving a request that did not need it would put an administrator's name against a decision they were
    // never asked to make.
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Bulk, false);
    let outcome = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/kk/ll/free", pool_id),
        &plan,
    )
    .await
    .expect("request");
    assert_eq!(outcome.request().state, "queued");
    assert!(
        !restores::approve(
            &mut *conn(pool).await,
            outcome.request().id,
            Uuid::new_v4(),
            now()
        )
        .await
        .expect("approve")
    );
}

// ─── the worker path ────────────────────────────────────────────────────────

async fn claiming_moves_queued_to_requested(pool: &PgPool) {
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Bulk, false);
    let outcome = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/mm/nn/claim", pool_id),
        &plan,
    )
    .await
    .expect("request");

    let claimed = restores::claim_queued(&mut *conn(pool).await, 100, now())
        .await
        .expect("claim");
    let mine = claimed
        .iter()
        .find(|r| r.id == outcome.request().id)
        .expect("my request must be claimed");
    assert_eq!(mine.state, "requested");

    // Claimed once. A second worker must not pick it up.
    let again = restores::claim_queued(&mut *conn(pool).await, 100, now())
        .await
        .expect("claim");
    assert!(again.iter().all(|r| r.id != outcome.request().id));
}

async fn availability_recomputes_the_expiry_from_when_it_actually_landed(pool: &PgPool) {
    // The plan's `expires_at` came from an *estimated* ETA. A Bulk restore that landed early would otherwise
    // expire early too, giving the user less warm time than they were promised — and the difference is a
    // second restore, billed again.
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Bulk, false);
    let outcome = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/oo/pp/avail", pool_id),
        &plan,
    )
    .await
    .expect("request");
    let id = outcome.request().id;
    restores::claim_queued(&mut *conn(pool).await, 100, now())
        .await
        .expect("claim");

    // Landed six hours early.
    let landed = now() + Duration::hours(6);
    assert!(
        restores::mark_available(&mut *conn(pool).await, id, landed)
            .await
            .expect("available")
    );

    let (state, available_at, expires_at): (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT state, available_at, expires_at FROM restore_requests WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("row");
    assert_eq!(state, "available");
    assert_eq!(available_at, Some(landed));
    assert_eq!(
        expires_at,
        Some(landed + Duration::days(7)),
        "seven days from availability, not from the estimate"
    );
}

async fn a_failure_keeps_its_reason_and_cannot_overwrite_an_available_copy(pool: &PgPool) {
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Bulk, false);

    let failing = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/qq/rr/fail", pool_id),
        &plan,
    )
    .await
    .expect("request");
    assert!(
        restores::mark_failed(
            &mut *conn(pool).await,
            failing.request().id,
            "glacier said no",
            now()
        )
        .await
        .expect("fail")
    );
    let reason: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM restore_requests WHERE id = $1")
            .bind(failing.request().id)
            .fetch_one(pool)
            .await
            .expect("read");
    assert_eq!(reason.as_deref(), Some("glacier said no"));

    // A late failure notification must not undo an already-available copy — the bytes are there, and marking
    // it failed would make the delivery path restore something it already has.
    let good = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/ss/tt/good", pool_id),
        &plan,
    )
    .await
    .expect("request");
    restores::claim_queued(&mut *conn(pool).await, 100, now())
        .await
        .expect("claim");
    restores::mark_available(&mut *conn(pool).await, good.request().id, now())
        .await
        .expect("available");
    assert!(
        !restores::mark_failed(
            &mut *conn(pool).await,
            good.request().id,
            "late error",
            now()
        )
        .await
        .expect("fail"),
        "a late failure must not overwrite an available copy"
    );
}

// ─── batching ───────────────────────────────────────────────────────────────

async fn siblings_share_a_batch_so_one_s3_call_serves_them_all(pool: &PgPool) {
    // §6.5: one collection restore becomes one bulk job, not 400 expedited ones. The batch id is what lets a
    // worker issue one call and then mark all of them.
    let pool_id = Uuid::new_v4();
    let batch = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Bulk, false);

    for n in 0..5 {
        let key = format!("acme/o/uu/vv/batch-{n}");
        restores::request(
            &mut *conn(pool).await,
            &RestoreSpec {
                batch_id: Some(batch),
                ..spec(&key, pool_id)
            },
            &plan,
        )
        .await
        .expect("request");
    }

    let members = restores::in_batch(&mut *conn(pool).await, batch)
        .await
        .expect("batch");
    assert_eq!(members.len(), 5);
    assert!(members.iter().all(|r| r.batch_id == Some(batch)));
}

// ─── the expiry sweep ───────────────────────────────────────────────────────

async fn the_sweep_expires_lapsed_copies_and_reports_them(pool: &PgPool) {
    // Without it the copy disappears at S3 and a delivery URL starts failing with a 403 that nothing in our
    // system explains. The sweep returns what lapsed so the caller can invalidate whatever pointed at it.
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Bulk, false);
    let outcome = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/ww/xx/sweep", pool_id),
        &plan,
    )
    .await
    .expect("request");
    let id = outcome.request().id;
    restores::claim_queued(&mut *conn(pool).await, 100, now())
        .await
        .expect("claim");
    restores::mark_available(&mut *conn(pool).await, id, now())
        .await
        .expect("available");

    // Nothing has lapsed yet.
    let early = restores::sweep_expired(&mut *conn(pool).await, now() + Duration::days(1), 100)
        .await
        .expect("sweep");
    assert!(early.iter().all(|r| r.id != id));

    let swept = restores::sweep_expired(&mut *conn(pool).await, now() + Duration::days(8), 100)
        .await
        .expect("sweep");
    let mine = swept.iter().find(|r| r.id == id).expect("must have lapsed");
    assert_eq!(mine.state, "expired");
    assert_eq!(
        mine.object_key, "acme/o/ww/xx/sweep",
        "the caller needs the key to invalidate what pointed at it"
    );

    // Idempotent: a second sweep must not report it again, or a notification goes out twice.
    let again = restores::sweep_expired(&mut *conn(pool).await, now() + Duration::days(9), 100)
        .await
        .expect("sweep");
    assert!(again.iter().all(|r| r.id != id));
}

// ─── the monthly budget ─────────────────────────────────────────────────────

async fn spend_counts_failures_because_a_failed_retrieval_is_still_billed(pool: &PgPool) {
    // A budget that ignored failures would let a retry loop spend without limit — which is the shape a broken
    // integration actually takes.
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Expedited, false);

    let before = restores::spent_this_month(&mut *conn(pool).await, now())
        .await
        .expect("spend");

    let failing = restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/yy/zz/spend", pool_id),
        &plan,
    )
    .await
    .expect("request");
    restores::claim_queued(&mut *conn(pool).await, 100, now())
        .await
        .expect("claim");
    restores::mark_failed(&mut *conn(pool).await, failing.request().id, "nope", now())
        .await
        .expect("fail");

    let after = restores::spent_this_month(&mut *conn(pool).await, now())
        .await
        .expect("spend");
    assert!(
        after > before,
        "a failed retrieval is still billed, so it must count against the budget"
    );
}

async fn a_queued_request_does_not_count_against_spend_yet(pool: &PgPool) {
    // It has not reached S3, so nothing has been charged. Counting it would make a queue of pending requests
    // block itself.
    let pool_id = Uuid::new_v4();
    let plan = plan_for(RestoreTier::Expedited, false);
    let before = restores::spent_this_month(&mut *conn(pool).await, now())
        .await
        .expect("spend");
    restores::request(
        &mut *conn(pool).await,
        &spec("acme/o/a1/b1/queued", pool_id),
        &plan,
    )
    .await
    .expect("request");
    assert_eq!(
        restores::spent_this_month(&mut *conn(pool).await, now())
            .await
            .expect("spend"),
        before,
        "a queued request has not been billed"
    );
}

#[tokio::test]
async fn the_restore_lifecycle_invariants_hold() {
    let (_pg, pool) = db().await;

    a_second_request_for_the_same_object_adopts_the_first(&pool).await;
    the_same_object_in_a_different_pool_is_a_different_restore(&pool).await;
    a_finished_restore_does_not_block_a_new_one(&pool).await;

    a_plan_needing_approval_is_held_rather_than_queued(&pool).await;
    approving_moves_it_to_queued_and_reports_only_the_first_call(&pool).await;
    approving_something_that_was_never_held_does_nothing(&pool).await;

    claiming_moves_queued_to_requested(&pool).await;
    availability_recomputes_the_expiry_from_when_it_actually_landed(&pool).await;
    a_failure_keeps_its_reason_and_cannot_overwrite_an_available_copy(&pool).await;

    siblings_share_a_batch_so_one_s3_call_serves_them_all(&pool).await;
    the_sweep_expires_lapsed_copies_and_reports_them(&pool).await;

    // Last: these read a tenant-wide sum, so they must run after the cases that create requests.
    a_queued_request_does_not_count_against_spend_yet(&pool).await;
    spend_counts_failures_because_a_failed_retrieval_is_still_billed(&pool).await;
}
