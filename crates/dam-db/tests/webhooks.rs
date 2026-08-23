//! The webhook outbox: what gets queued, and in what order it is allowed to leave (Q.20c, §11).
//!
//! The schema for this has existed since migration 0004 with nothing writing to it, and its own comments name
//! the property that makes the whole thing worth building carefully:
//!
//! > Ordering matters for a CMS: an `asset.version_created` delivered after `asset.expired` would republish an
//! > expired asset.
//!
//! That is the case this suite is mostly about, because it is the one that looks fine in a demo. Two events for
//! one asset, both delivered, both acknowledged — and the customer's site is showing an asset whose rights were
//! withdrawn, because the withdrawal arrived first and the republication was already in flight. So [`claim`]
//! must hand out at most one delivery per `(subscription, asset)` and hold the rest back, and it must do that
//! under concurrent workers rather than only in a single-threaded test.
//!
//! The rest is the machinery that keeps a dead endpoint from becoming an unbounded queue: backoff, a bounded
//! attempt count that ends in a dead-letter rather than a retry forever, and a subscription that disables
//! itself with a reason somebody can read and undo.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::webhooks::{self, AfterFailure};
use dam_db::{migrate, testing::PostgresHarness};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn held(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("acquire")
}

async fn subscribe(pool: &PgPool, url: &str, kinds: &[&str]) -> Uuid {
    let owned: Vec<String> = kinds.iter().map(|k| (*k).to_owned()).collect();
    webhooks::subscribe(
        &mut *held(pool).await,
        &webhooks::NewSubscription {
            connector_id: None,
            url,
            secret: "a-signing-secret",
            event_kinds: &owned,
        },
    )
    .await
    .expect("subscribe")
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

// ─── who gets what ──────────────────────────────────────────────────────────

async fn an_empty_filter_means_every_kind(pool: &PgPool) {
    // The schema's default, and the useful reading: a client that has not thought about filtering wants
    // everything rather than nothing. `cardinality(event_kinds) = 0` in the predicate rather than a match, so
    // the empty array is admitted instead of matching nothing.
    let everything = subscribe(pool, "https://example.test/all", &[]).await;
    let narrow = subscribe(pool, "https://example.test/some", &["asset.expired"]).await;

    let queued = webhooks::enqueue(
        &mut *held(pool).await,
        "asset.published",
        None,
        &json!({"note": "one"}),
    )
    .await
    .expect("enqueue");
    assert_eq!(
        queued, 1,
        "only the unfiltered subscription wanted this kind"
    );

    let queued = webhooks::enqueue(
        &mut *held(pool).await,
        "asset.expired",
        None,
        &json!({"note": "two"}),
    )
    .await
    .expect("enqueue");
    assert_eq!(queued, 2, "both wanted this one");

    let all = webhooks::log(&mut *held(pool).await, everything, 50)
        .await
        .expect("log");
    assert_eq!(all.len(), 2);
    let some = webhooks::log(&mut *held(pool).await, narrow, 50)
        .await
        .expect("log");
    assert_eq!(some.len(), 1);
    assert_eq!(some[0].event_kind, "asset.expired");
}

async fn a_disabled_subscription_queues_nothing(pool: &PgPool) {
    // The point of disabling: the outbox stops growing. A disabled subscription that kept queueing would just
    // move the unbounded growth from the delivery attempts to the table.
    //
    // Its own database, because `enqueue` counts across *every* subscription in the tenant — so a case
    // asserting "zero were queued" cannot share a fixture with one that left an unfiltered subscription
    // listening. That is the assertion catching a real property of the function, not a nuisance.
    let (_pg, fresh) = db().await;
    let id = subscribe(&fresh, "https://example.test/dead", &[]).await;
    sqlx::query("UPDATE webhook_subscriptions SET active = false WHERE id = $1")
        .bind(id)
        .execute(&fresh)
        .await
        .expect("disable");

    let queued = webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        None,
        &json!({}),
    )
    .await
    .expect("enqueue");
    assert_eq!(queued, 0);

    // And reactivating forgives the count, so one more failure does not immediately disable it again — which
    // is what "enable" looks broken as.
    sqlx::query("UPDATE webhook_subscriptions SET consecutive_failures = 4 WHERE id = $1")
        .bind(id)
        .execute(&fresh)
        .await
        .expect("count");
    assert!(
        webhooks::reactivate(&mut *held(&fresh).await, id)
            .await
            .expect("reactivate")
    );
    let listed = webhooks::subscriptions(&mut *held(&fresh).await)
        .await
        .expect("list");
    let row = listed.iter().find(|one| one.id == id).expect("listed");
    assert!(row.active);
    assert_eq!(row.consecutive_failures, 0);
    assert!(row.disabled_reason.is_none());
    let _ = pool;
}

// ─── ordering: the property the schema exists for ───────────────────────────

async fn two_events_for_one_asset_leave_in_order(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let subscription = subscribe(&fresh, "https://example.test/cms", &[]).await;
    let photo = asset(&fresh, "photo").await;

    // The exact sequence from the schema's own comment: a withdrawal, then a republication.
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.expired",
        Some(photo),
        &json!({"seq": 1}),
    )
    .await
    .expect("first");
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.version_created",
        Some(photo),
        &json!({"seq": 2}),
    )
    .await
    .expect("second");

    // One claim, however many are asked for: the second is held until the first finishes.
    let claimed = webhooks::claim(&mut *held(&fresh).await, 10)
        .await
        .expect("claim");
    assert_eq!(
        claimed.len(),
        1,
        "two events for one asset are not in flight together"
    );
    assert_eq!(claimed[0].event_kind, "asset.expired");
    assert_eq!(
        claimed[0].attempts, 0,
        "attempts counts the ones before this"
    );

    // Still nothing, while the first is in flight. This is the assertion that fails if the `delivering`
    // exclusion is dropped, and it is the whole difference between an ordered stream and a race.
    assert!(
        webhooks::claim(&mut *held(&fresh).await, 10)
            .await
            .expect("claim")
            .is_empty(),
        "a second delivery for the same asset must wait for the first to finish"
    );

    webhooks::delivered(&mut *held(&fresh).await, claimed[0].id, 200)
        .await
        .expect("delivered");
    let next = webhooks::claim(&mut *held(&fresh).await, 10)
        .await
        .expect("claim");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].event_kind, "asset.version_created");
    let _ = subscription;
    let _ = pool;
}

async fn events_enqueued_in_one_transaction_keep_their_order(pool: &PgPool) {
    // The regression that migration 0035 exists for, in the shape it actually occurs.
    //
    // An outbox row is written in the same transaction as the change it describes, so "publish this version and
    // expire the old one" is one transaction with two events for one asset — 0004's own example. `now()` is the
    // *transaction* timestamp, identical for every statement in it, so ordering by `created_at` made these two
    // tie and the tie-break fall to `gen_random_uuid()`. Random order, on the table whose whole purpose is
    // order, and it would have shown up as a customer's site republishing an asset whose rights were withdrawn.
    let (_pg, fresh) = db().await;
    subscribe(&fresh, "https://example.test/one-txn", &[]).await;
    let photo = asset(&fresh, "one-txn").await;

    let mut tx = fresh.begin().await.expect("begin");
    for (seq, kind) in [(1, "asset.version_created"), (2, "asset.expired")] {
        webhooks::enqueue(&mut tx, kind, Some(photo), &json!({"seq": seq}))
            .await
            .expect("enqueue");
    }
    tx.commit().await.expect("commit");

    // Same `created_at` on both rows, which is the point.
    let distinct: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT created_at) FROM webhook_deliveries")
            .fetch_one(&fresh)
            .await
            .expect("count");
    assert_eq!(
        distinct, 1,
        "one transaction, one timestamp — so the order cannot come from it"
    );

    let first = webhooks::claim(&mut *held(&fresh).await, 10)
        .await
        .expect("claim");
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].payload["seq"], 1,
        "the order they were enqueued in, from the sequence"
    );
    webhooks::delivered(&mut *held(&fresh).await, first[0].id, 200)
        .await
        .expect("delivered");

    let second = webhooks::claim(&mut *held(&fresh).await, 10)
        .await
        .expect("claim");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].payload["seq"], 2);
    let _ = pool;
}

async fn different_assets_and_endpoints_go_in_parallel(pool: &PgPool) {
    // The other half of the rule. Ordering per asset is a correctness property; ordering *globally* would be a
    // performance disaster — one slow endpoint would stall every other integration in the tenant.
    let (_pg, fresh) = db().await;
    subscribe(&fresh, "https://example.test/one", &[]).await;
    subscribe(&fresh, "https://example.test/two", &[]).await;
    let left = asset(&fresh, "left").await;
    let right = asset(&fresh, "right").await;

    for id in [left, right] {
        webhooks::enqueue(
            &mut *held(&fresh).await,
            "asset.published",
            Some(id),
            &json!({}),
        )
        .await
        .expect("enqueue");
    }

    // Two assets × two subscriptions, all four independent.
    let claimed = webhooks::claim(&mut *held(&fresh).await, 10)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 4);
    let _ = pool;
}

async fn an_event_about_no_asset_is_still_ordered_per_endpoint(pool: &PgPool) {
    // `IS NOT DISTINCT FROM` rather than `=`, so two null-asset events to one endpoint are ordered against
    // each other. With `=` the null comparison is unknown, both would look unblocked, and a settings change
    // could overtake the one before it.
    let (_pg, fresh) = db().await;
    subscribe(&fresh, "https://example.test/settings", &[]).await;
    for seq in 1..=2 {
        webhooks::enqueue(
            &mut *held(&fresh).await,
            "taxonomy.changed",
            None,
            &json!({"seq": seq}),
        )
        .await
        .expect("enqueue");
    }

    let claimed = webhooks::claim(&mut *held(&fresh).await, 10)
        .await
        .expect("claim");
    assert_eq!(
        claimed.len(),
        1,
        "two null-asset events to one endpoint are ordered"
    );
    assert_eq!(claimed[0].payload["seq"], 1);
    let _ = pool;
}

async fn two_workers_do_not_claim_the_same_delivery(pool: &PgPool) {
    // `FOR UPDATE SKIP LOCKED` is what makes the ordering check safe under concurrency: the check for an
    // in-flight sibling and the claim are one statement, so a second worker cannot slip between another's
    // check and its write. Run as two real concurrent transactions, because a sequential test would pass with
    // the locking removed.
    let (_pg, fresh) = db().await;
    subscribe(&fresh, "https://example.test/race", &[]).await;
    for index in 0..6 {
        let id = asset(&fresh, &format!("race-{index}")).await;
        webhooks::enqueue(
            &mut *held(&fresh).await,
            "asset.published",
            Some(id),
            &json!({}),
        )
        .await
        .expect("enqueue");
    }

    let (left, right) = tokio::join!(
        async {
            let mut conn = held(&fresh).await;
            webhooks::claim(&mut conn, 6).await.expect("left")
        },
        async {
            let mut conn = held(&fresh).await;
            webhooks::claim(&mut conn, 6).await.expect("right")
        }
    );

    let mut ids: Vec<Uuid> = left.iter().chain(right.iter()).map(|one| one.id).collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), total, "no delivery was claimed twice");
    assert_eq!(total, 6, "and between them they took all six");
    let _ = pool;
}

// ─── failure, backoff, and the dead letter ──────────────────────────────────

async fn a_failure_backs_off_and_the_last_one_dead_letters(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let subscription = subscribe(&fresh, "https://example.test/flaky", &[]).await;
    let photo = asset(&fresh, "flaky").await;
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        Some(photo),
        &json!({}),
    )
    .await
    .expect("enqueue");
    // Two attempts is enough to reach the end quickly; the schema's default is eight.
    sqlx::query("UPDATE webhook_deliveries SET max_attempts = 2")
        .execute(&fresh)
        .await
        .expect("cap");

    let first = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim")
        .remove(0);
    assert_eq!(
        webhooks::failed(
            &mut *held(&fresh).await,
            first.id,
            Some(503),
            "service unavailable"
        )
        .await
        .expect("failed"),
        AfterFailure::Retrying
    );

    // Backed off, so it is not immediately claimable — which is the point: a hot loop against a failing
    // endpoint is how one broken integration becomes everybody's outage.
    assert!(
        webhooks::claim(&mut *held(&fresh).await, 1)
            .await
            .expect("claim")
            .is_empty(),
        "a failed delivery waits for its backoff"
    );
    let logged = webhooks::log(&mut *held(&fresh).await, subscription, 10)
        .await
        .expect("log");
    assert_eq!(logged[0].state, "failed");
    assert_eq!(logged[0].response_status, Some(503));
    assert_eq!(logged[0].attempts, 1);
    assert!(logged[0].next_attempt_at > logged[0].created_at);

    // Bring it forward, as the passage of time would.
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() - interval '1 minute'")
        .execute(&fresh)
        .await
        .expect("due");
    let second = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim")
        .remove(0);
    assert_eq!(second.attempts, 1, "one attempt before this one");
    assert_eq!(
        webhooks::failed(
            &mut *held(&fresh).await,
            second.id,
            None,
            "connection refused"
        )
        .await
        .expect("failed"),
        AfterFailure::DeadLettered
    );

    let logged = webhooks::log(&mut *held(&fresh).await, subscription, 10)
        .await
        .expect("log");
    assert_eq!(logged[0].state, "dead");
    // The absence is preserved rather than written as a zero: a DNS failure and a 500 are different
    // diagnoses, and an operator reading this row has to be able to tell them apart.
    assert_eq!(logged[0].response_status, None);
    assert!(logged[0].last_error.as_deref() == Some("connection refused"));

    // A dead row is never claimed again on its own, which is what makes the queue bounded.
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() - interval '1 day'")
        .execute(&fresh)
        .await
        .expect("due");
    assert!(
        webhooks::claim(&mut *held(&fresh).await, 10)
            .await
            .expect("claim")
            .is_empty()
    );

    // But an operator who has fixed their endpoint can revive it, with a fresh set of attempts — one attempt
    // would make the button useless.
    assert!(
        webhooks::revive(&mut *held(&fresh).await, second.id)
            .await
            .expect("revive")
    );
    let revived = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim");
    assert_eq!(revived.len(), 1);
    assert_eq!(revived[0].attempts, 0);
    let _ = pool;
}

async fn a_success_forgives_the_failure_count(pool: &PgPool) {
    // "Consecutive" is the whole meaning of the column: fail twice, succeed, fail twice more is not four
    // failures on the way to a disable.
    let (_pg, fresh) = db().await;
    let subscription = subscribe(&fresh, "https://example.test/blips", &[]).await;
    sqlx::query("UPDATE webhook_subscriptions SET consecutive_failures = 3 WHERE id = $1")
        .bind(subscription)
        .execute(&fresh)
        .await
        .expect("count");

    let photo = asset(&fresh, "recovered").await;
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        Some(photo),
        &json!({}),
    )
    .await
    .expect("enqueue");
    let claimed = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim")
        .remove(0);
    webhooks::delivered(&mut *held(&fresh).await, claimed.id, 204)
        .await
        .expect("delivered");

    let listed = webhooks::subscriptions(&mut *held(&fresh).await)
        .await
        .expect("list");
    assert_eq!(listed[0].consecutive_failures, 0);
    let _ = pool;
}

async fn a_persistently_dead_endpoint_disables_itself(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let subscription = subscribe(&fresh, "https://example.test/gone", &[]).await;
    sqlx::query("UPDATE webhook_subscriptions SET consecutive_failures = $2 WHERE id = $1")
        .bind(subscription)
        .bind(dam_db::webhooks::FAILURES_BEFORE_DISABLE - 1)
        .execute(&fresh)
        .await
        .expect("count");

    let photo = asset(&fresh, "gone").await;
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        Some(photo),
        &json!({}),
    )
    .await
    .expect("enqueue");
    sqlx::query("UPDATE webhook_deliveries SET max_attempts = 1")
        .execute(&fresh)
        .await
        .expect("cap");

    let claimed = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim")
        .remove(0);
    assert_eq!(
        webhooks::failed(&mut *held(&fresh).await, claimed.id, Some(410), "gone")
            .await
            .expect("failed"),
        AfterFailure::SubscriptionDisabled
    );

    let listed = webhooks::subscriptions(&mut *held(&fresh).await)
        .await
        .expect("list");
    assert!(!listed[0].active);
    // The reason is readable and says what to do about it, because an integration that silently stopped is a
    // support ticket rather than a state somebody can act on.
    let reason = listed[0].disabled_reason.as_deref().expect("a reason");
    assert!(reason.contains("disabled automatically"), "{reason}");
    assert!(reason.contains("gone"), "the last error is named: {reason}");
    let _ = pool;
}

async fn a_poisonous_payload_does_not_disable_a_healthy_endpoint(pool: &PgPool) {
    // The count moves only when a delivery *dies*, not on every failed attempt. Otherwise one event the
    // endpoint chokes on would spend all eight of its attempts and take the subscription down with it — and
    // the endpoint was fine.
    let (_pg, fresh) = db().await;
    let subscription = subscribe(&fresh, "https://example.test/picky", &[]).await;
    let photo = asset(&fresh, "picky").await;
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        Some(photo),
        &json!({}),
    )
    .await
    .expect("enqueue");

    // Six failed attempts, none of them the last.
    for _ in 0..6 {
        sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() - interval '1 day'")
            .execute(&fresh)
            .await
            .expect("due");
        let claimed = webhooks::claim(&mut *held(&fresh).await, 1)
            .await
            .expect("claim")
            .remove(0);
        assert_eq!(
            webhooks::failed(
                &mut *held(&fresh).await,
                claimed.id,
                Some(422),
                "cannot parse"
            )
            .await
            .expect("failed"),
            AfterFailure::Retrying
        );
    }

    let listed = webhooks::subscriptions(&mut *held(&fresh).await)
        .await
        .expect("list");
    assert!(
        listed[0].active,
        "six failed attempts on one event is not a dead endpoint"
    );
    assert_eq!(listed[0].consecutive_failures, 0);
    let _ = subscription;
    let _ = pool;
}

async fn a_worker_that_died_does_not_halt_an_assets_stream(pool: &PgPool) {
    // The failure mode that ordering creates: a row stuck in `delivering` blocks every later event for the
    // same asset, so a killed worker silently stops one asset's stream rather than raising an error.
    let (_pg, fresh) = db().await;
    subscribe(&fresh, "https://example.test/stalled", &[]).await;
    let photo = asset(&fresh, "stalled").await;
    for seq in 1..=2 {
        webhooks::enqueue(
            &mut *held(&fresh).await,
            "asset.published",
            Some(photo),
            &json!({"seq": seq}),
        )
        .await
        .expect("enqueue");
    }

    let claimed = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);
    // The worker vanishes without reporting. Nothing else can go out for this asset.
    assert!(
        webhooks::claim(&mut *held(&fresh).await, 10)
            .await
            .expect("claim")
            .is_empty()
    );

    // Aged to the same instant on purpose: this is what production does, because `now()` is the transaction
    // timestamp. If ordering depended on `created_at` the two rows would tie here and leave in random order —
    // which is exactly how this test found the bug that migration 0035 fixes.
    sqlx::query("UPDATE webhook_deliveries SET created_at = now() - interval '1 hour'")
        .execute(&fresh)
        .await
        .expect("age it");
    let reclaimed = webhooks::reclaim_stalled(&mut *held(&fresh).await, 600)
        .await
        .expect("reclaim");
    assert_eq!(reclaimed, 1);

    // Moving again, and in the right order.
    let after = webhooks::claim(&mut *held(&fresh).await, 10)
        .await
        .expect("claim");
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].payload["seq"], 1,
        "the reclaimed one goes first, not the one behind it"
    );
    // The attempt was kept, unlike a release: the request may well have reached the endpoint, so the
    // conservative reading is that it was tried.
    assert_eq!(after[0].attempts, 1);
    let _ = pool;
}

async fn a_released_delivery_costs_no_attempt(pool: &PgPool) {
    // For a worker shutting down cleanly: it never tried, so spending one of the eight attempts on a deploy
    // would make a rolling restart look like a failing endpoint.
    let (_pg, fresh) = db().await;
    subscribe(&fresh, "https://example.test/draining", &[]).await;
    let photo = asset(&fresh, "draining").await;
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        Some(photo),
        &json!({}),
    )
    .await
    .expect("enqueue");

    let claimed = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim")
        .remove(0);
    webhooks::release(&mut *held(&fresh).await, claimed.id)
        .await
        .expect("release");

    let again = webhooks::claim(&mut *held(&fresh).await, 1)
        .await
        .expect("claim");
    assert_eq!(again.len(), 1, "immediately claimable, with no backoff");
    assert_eq!(again[0].attempts, 0, "the aborted attempt was not charged");
    let _ = pool;
}

async fn removing_a_subscription_takes_its_queue(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let subscription = subscribe(&fresh, "https://example.test/leaving", &[]).await;
    let photo = asset(&fresh, "leaving").await;
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        Some(photo),
        &json!({}),
    )
    .await
    .expect("enqueue");

    assert!(
        webhooks::unsubscribe(&mut *held(&fresh).await, subscription)
            .await
            .expect("unsubscribe")
    );
    // By cascade. A queue left behind would be delivered to nobody and read by nobody.
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries")
        .fetch_one(&fresh)
        .await
        .expect("count");
    assert_eq!(remaining, 0);
    assert!(
        !webhooks::unsubscribe(&mut *held(&fresh).await, subscription)
            .await
            .expect("unsubscribe"),
        "removing it twice reports false rather than erroring"
    );

    // And a delivery that vanishes mid-flight is not an error to record against — it happens whenever
    // somebody removes a subscription while the dispatcher is working.
    assert_eq!(
        webhooks::failed(&mut *held(&fresh).await, Uuid::new_v4(), Some(500), "late")
            .await
            .expect("failed"),
        AfterFailure::DeadLettered
    );
    let _ = pool;
}

async fn the_log_withholds_the_payload(pool: &PgPool) {
    // A log endpoint returning every payload would be the cheapest way to read a tenant's whole change history
    // in one request — and it is the largest column, on the query a UI runs most often.
    let (_pg, fresh) = db().await;
    let subscription = subscribe(&fresh, "https://example.test/quiet", &[]).await;
    webhooks::enqueue(
        &mut *held(&fresh).await,
        "asset.published",
        None,
        &json!({"secret": "should not appear in a log"}),
    )
    .await
    .expect("enqueue");

    let logged = webhooks::log(&mut *held(&fresh).await, subscription, 10)
        .await
        .expect("log");
    assert_eq!(logged.len(), 1);
    let rendered = format!("{logged:?}");
    assert!(
        !rendered.contains("should not appear"),
        "the log row carries no payload: {rendered}"
    );
    let _ = pool;
}

#[tokio::test]
async fn the_outbox_delivers_in_order_and_stops_when_nobody_is_listening() {
    let (_pg, pool) = db().await;

    an_empty_filter_means_every_kind(&pool).await;
    a_disabled_subscription_queues_nothing(&pool).await;

    two_events_for_one_asset_leave_in_order(&pool).await;
    events_enqueued_in_one_transaction_keep_their_order(&pool).await;
    different_assets_and_endpoints_go_in_parallel(&pool).await;
    an_event_about_no_asset_is_still_ordered_per_endpoint(&pool).await;
    two_workers_do_not_claim_the_same_delivery(&pool).await;

    a_failure_backs_off_and_the_last_one_dead_letters(&pool).await;
    a_success_forgives_the_failure_count(&pool).await;
    a_persistently_dead_endpoint_disables_itself(&pool).await;
    a_poisonous_payload_does_not_disable_a_healthy_endpoint(&pool).await;
    a_worker_that_died_does_not_halt_an_assets_stream(&pool).await;
    a_released_delivery_costs_no_attempt(&pool).await;
    removing_a_subscription_takes_its_queue(&pool).await;
    the_log_withholds_the_payload(&pool).await;
}
