//! Bulk operations (2.10, G18).
//!
//! The schema states the difficulty: "partial failure is the hard part — an operation over 40,000 assets that
//! fails at 31,000 must be resumable and must report exactly which rows did not apply."
//!
//! So the cases here are about the failure paths, not the happy one: resumption after a crash, a state that
//! cannot report green over failures, and counters that survive a retried worker.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, TimeZone, Utc};
use dam_db::bulk::{self, ItemOutcome, OperationSpec};
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

async fn assets(pool: &PgPool, label: &str, count: usize) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
             VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
        )
        .bind(id)
        .bind(
            blake3::hash(format!("{label}-{n}").as_bytes())
                .to_hex()
                .to_string(),
        )
        .bind(format!("{label}-{n}.jpg"))
        .execute(pool)
        .await
        .expect("asset");
        ids.push(id);
    }
    ids
}

fn spec<'a>(kind: &'a str) -> OperationSpec<'a> {
    OperationSpec {
        kind,
        actor_id: None,
        predicate: None,
        params: serde_json::json!({}),
    }
}

// ─── the snapshot ───────────────────────────────────────────────────────────

async fn the_target_set_is_materialised_at_creation(pool: &PgPool) {
    // The schema calls for this: "a predicate is snapshotted to a materialised id list at start, so a
    // long-running operation applies to the set the user saw rather than a set that shifts under it."
    let ids = assets(pool, "snapshot", 5).await;
    let op = bulk::create(pool, &spec("tag_add"), &ids)
        .await
        .expect("create");
    assert_eq!(op.target_count, 5);

    // An asset created after the operation started is not in it, however well it would match a predicate.
    let latecomer = assets(pool, "latecomer", 1).await;
    let items = bulk::items(pool, op.id, 100).await.expect("items");
    assert_eq!(items.len(), 5);
    assert!(
        items.iter().all(|i| i.asset_id != latecomer[0]),
        "the set must not shift under a running operation"
    );
}

async fn a_repeated_id_is_deduplicated_and_the_count_corrected(pool: &PgPool) {
    // A selection assembled from several pages can repeat an id. Left alone the primary key aborts the whole
    // operation; counted twice, `done + failed = target` never holds and a UI never sees it finish.
    let ids = assets(pool, "dupes", 3).await;
    let with_repeats = vec![ids[0], ids[1], ids[0], ids[2], ids[1]];
    let op = bulk::create(pool, &spec("tag_add"), &with_repeats)
        .await
        .expect("create");
    assert_eq!(
        op.target_count, 3,
        "five entries naming three assets is a three-asset operation"
    );
    assert_eq!(bulk::items(pool, op.id, 100).await.expect("items").len(), 3);
}

async fn an_operation_over_nothing_is_refused(pool: &PgPool) {
    // A mis-built query or a stale selection. Recording it as instantly complete would hide it in the actor's
    // history as a success.
    assert!(
        bulk::create(pool, &spec("tag_add"), &[]).await.is_err(),
        "zero targets is a mistake, not an empty job"
    );
}

async fn a_dry_run_writes_nothing_at_all(pool: &PgPool) {
    // Not even a `bulk_operations` row. A dry run that left one behind would put abandoned operations in the
    // history, which is exactly where somebody looks to find what they actually ran.
    let ids = assets(pool, "dry", 30).await;
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM bulk_operations")
        .fetch_one(pool)
        .await
        .expect("count");

    let preview = bulk::dry_run("delete", &ids);
    assert_eq!(preview.target_count, 30);
    assert_eq!(
        preview.sample.len(),
        bulk::DRY_RUN_SAMPLE,
        "a sample, not thirty thousand rows"
    );

    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM bulk_operations")
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(before, after, "a dry run must record nothing");
}

// ─── resumption ─────────────────────────────────────────────────────────────

async fn a_worker_that_dies_resumes_after_the_last_recorded_asset(pool: &PgPool) {
    // The property the schema names. An operation that fails at 31,000 of 40,000 must pick up where it stopped,
    // not re-scan from the beginning and not skip the remainder.
    let ids = assets(pool, "resume", 10).await;
    let op = bulk::create(pool, &spec("metadata_set"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");

    // First worker does four and then "dies".
    let first_batch = bulk::next_batch(pool, op.id, 4).await.expect("batch");
    assert_eq!(first_batch.len(), 4);
    for asset in &first_batch {
        bulk::record_outcome(pool, op.id, *asset, ItemOutcome::Done)
            .await
            .expect("record");
    }

    // Second worker starts cold and must not redo the first four.
    let second_batch = bulk::next_batch(pool, op.id, 4).await.expect("batch");
    assert_eq!(second_batch.len(), 4);
    for asset in &second_batch {
        assert!(
            !first_batch.contains(asset),
            "a resumed worker must not redo work already recorded"
        );
    }

    // And the whole set is eventually covered exactly once.
    let mut seen: Vec<Uuid> = first_batch
        .iter()
        .chain(second_batch.iter())
        .copied()
        .collect();
    for asset in &second_batch {
        bulk::record_outcome(pool, op.id, *asset, ItemOutcome::Done)
            .await
            .expect("record");
    }
    loop {
        let batch = bulk::next_batch(pool, op.id, 4).await.expect("batch");
        if batch.is_empty() {
            break;
        }
        for asset in &batch {
            bulk::record_outcome(pool, op.id, *asset, ItemOutcome::Done)
                .await
                .expect("record");
            seen.push(*asset);
        }
    }
    seen.sort_unstable();
    let unique = {
        let mut s = seen.clone();
        s.dedup();
        s
    };
    assert_eq!(seen.len(), unique.len(), "no asset may be processed twice");
    assert_eq!(unique.len(), 10, "and none may be skipped");
}

async fn recording_out_of_order_does_not_orphan_the_lower_ids(pool: &PgPool) {
    // A worker that fans a batch out concurrently records outcomes in *completion* order, not id order. If batch
    // selection cursored on a high-water mark of what had been recorded, recording the highest id first would step
    // the cursor past every lower pending item — they would never be served again, `done + failed = target` would
    // never hold, and the operation could not legitimately finish. Selection is therefore on item state, which
    // cannot skip a row it has not seen an outcome for.
    let ids = assets(pool, "out-of-order", 5).await;
    let op = bulk::create(pool, &spec("tag_add"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");

    let mut ordered = ids.clone();
    ordered.sort_unstable();
    let highest = *ordered.last().expect("five ids");

    bulk::record_outcome(pool, op.id, highest, ItemOutcome::Done)
        .await
        .expect("record");

    let remaining = bulk::next_batch(pool, op.id, 10).await.expect("batch");
    assert_eq!(
        remaining.len(),
        4,
        "the four lower items are still pending and must still be served"
    );
    assert!(!remaining.contains(&highest));
}

async fn a_retried_worker_cannot_inflate_the_counters(pool: &PgPool) {
    // `done + failed = target` is the invariant a UI reads to decide whether an operation is finished. A worker
    // that re-records the same asset after a network blip would otherwise push the count past the target, and the
    // operation would never look complete.
    let ids = assets(pool, "retry", 3).await;
    let op = bulk::create(pool, &spec("tag_add"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");

    for _ in 0..4 {
        bulk::record_outcome(pool, op.id, ids[0], ItemOutcome::Done)
            .await
            .expect("record");
    }
    let reloaded = bulk::load(pool, op.id)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(
        reloaded.done_count, 1,
        "four recordings of one asset is one completion"
    );
}

// ─── outcomes ───────────────────────────────────────────────────────────────

async fn an_operation_with_failures_is_partial_not_completed(pool: &PgPool) {
    // Reporting `completed` over 9,000 failures is the kind of thing somebody discovers a month later. `partial`
    // is a distinct state so a UI cannot show a green tick over it — and the caller does not get to choose,
    // because the state is derived from the counters.
    let ids = assets(pool, "partial", 4).await;
    let op = bulk::create(pool, &spec("license_assign"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");

    bulk::record_outcome(pool, op.id, ids[0], ItemOutcome::Done)
        .await
        .expect("record");
    bulk::record_outcome(pool, op.id, ids[1], ItemOutcome::Done)
        .await
        .expect("record");
    bulk::record_outcome(
        pool,
        op.id,
        ids[2],
        ItemOutcome::Failed("licence not found"),
    )
    .await
    .expect("record");
    bulk::record_outcome(
        pool,
        op.id,
        ids[3],
        ItemOutcome::Skipped("already licensed"),
    )
    .await
    .expect("record");

    let finished = bulk::finish(pool, op.id, now()).await.expect("finish");
    assert_eq!(finished.state, "partial");
    assert_eq!(finished.done_count, 2);
    assert_eq!(finished.failed_count, 1);
    assert!(finished.finished_at.is_some());
}

async fn an_operation_where_everything_failed_is_failed(pool: &PgPool) {
    let ids = assets(pool, "allfail", 2).await;
    let op = bulk::create(pool, &spec("reprocess"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");
    for id in &ids {
        bulk::record_outcome(pool, op.id, *id, ItemOutcome::Failed("boom"))
            .await
            .expect("record");
    }
    assert_eq!(
        bulk::finish(pool, op.id, now())
            .await
            .expect("finish")
            .state,
        "failed"
    );
}

async fn a_clean_operation_is_completed(pool: &PgPool) {
    let ids = assets(pool, "clean", 2).await;
    let op = bulk::create(pool, &spec("group_add"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");
    for id in &ids {
        bulk::record_outcome(pool, op.id, *id, ItemOutcome::Done)
            .await
            .expect("record");
    }
    let finished = bulk::finish(pool, op.id, now()).await.expect("finish");
    assert_eq!(finished.state, "completed");
    assert!(finished.is_terminal());
}

async fn a_skip_counts_as_neither_done_nor_failed(pool: &PgPool) {
    // A skipped asset is one the operation deliberately did not touch — already tagged, not eligible. Counting it
    // as done would claim work that never happened, and as failed would raise an alarm about nothing.
    let ids = assets(pool, "skipped", 3).await;
    let op = bulk::create(pool, &spec("tag_add"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");
    for id in &ids {
        bulk::record_outcome(pool, op.id, *id, ItemOutcome::Skipped("already tagged"))
            .await
            .expect("record");
    }
    let finished = bulk::finish(pool, op.id, now()).await.expect("finish");
    assert_eq!(finished.done_count, 0);
    assert_eq!(finished.failed_count, 0);
    assert_eq!(
        finished.state, "completed",
        "an operation that skipped everything did not fail"
    );

    // And the reason survives, because a silent skip is indistinguishable from a bug.
    let items = bulk::items(pool, op.id, 100).await.expect("items");
    assert!(
        items
            .iter()
            .all(|i| i.state == "skipped" && i.reason.as_deref() == Some("already tagged"))
    );
}

async fn failures_are_reported_row_by_row(pool: &PgPool) {
    // "Must report exactly which rows did not apply." A count is not a report — somebody has to be able to retry
    // the 9,000 that failed without redoing the 31,000 that worked.
    let ids = assets(pool, "reported", 5).await;
    let op = bulk::create(pool, &spec("metadata_set"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");

    bulk::record_outcome(pool, op.id, ids[0], ItemOutcome::Done)
        .await
        .expect("record");
    for id in &ids[1..3] {
        bulk::record_outcome(pool, op.id, *id, ItemOutcome::Failed("field is read-only"))
            .await
            .expect("record");
    }

    let failures = bulk::failures(pool, op.id, 100).await.expect("failures");
    assert_eq!(failures.len(), 2);
    assert!(
        failures
            .iter()
            .all(|f| f.reason.as_deref() == Some("field is read-only")),
        "each failure must carry its own reason"
    );
    let failed_ids: Vec<Uuid> = failures.iter().map(|f| f.asset_id).collect();
    assert!(!failed_ids.contains(&ids[0]));
}

async fn the_error_sample_is_bounded(pool: &PgPool) {
    // It is a sample for the UI; the full list lives in `bulk_operation_items`. Unbounded, a 40,000-failure
    // operation would put 40,000 reasons in one jsonb column and every read of the operation row would carry them.
    let ids = assets(pool, "sampled", 25).await;
    let op = bulk::create(pool, &spec("reprocess"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");
    for id in &ids {
        bulk::record_outcome(pool, op.id, *id, ItemOutcome::Failed("nope"))
            .await
            .expect("record");
    }

    let sample: serde_json::Value =
        sqlx::query_scalar("SELECT error_sample FROM bulk_operations WHERE id = $1")
            .bind(op.id)
            .fetch_one(pool)
            .await
            .expect("sample");
    let len = sample.as_array().expect("an array").len();
    assert!(len <= 20, "the sample must be bounded, got {len}");
    assert!(len > 0, "and it must actually contain something");

    // The full set is still available row by row.
    assert_eq!(
        bulk::failures(pool, op.id, 100)
            .await
            .expect("failures")
            .len(),
        25
    );
}

// ─── control ────────────────────────────────────────────────────────────────

async fn pausing_stops_the_batch_and_resuming_continues(pool: &PgPool) {
    let ids = assets(pool, "paused", 6).await;
    let op = bulk::create(pool, &spec("tier"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");

    let batch = bulk::next_batch(pool, op.id, 2).await.expect("batch");
    for id in &batch {
        bulk::record_outcome(pool, op.id, *id, ItemOutcome::Done)
            .await
            .expect("record");
    }
    assert!(bulk::pause(pool, op.id).await.expect("pause"));
    assert_eq!(
        bulk::load(pool, op.id)
            .await
            .expect("load")
            .expect("present")
            .state,
        "paused"
    );

    // Restarting picks up after what was recorded.
    assert!(bulk::start(pool, op.id, now()).await.expect("restart"));
    let resumed = bulk::next_batch(pool, op.id, 10).await.expect("batch");
    assert_eq!(resumed.len(), 4);
    assert!(resumed.iter().all(|id| !batch.contains(id)));
}

async fn cancelling_leaves_applied_work_applied_and_says_what_was_done(pool: &PgPool) {
    // Nothing is rolled back, and that is deliberate rather than lazy: a bulk tag over 31,000 assets cannot be
    // undone by a cancellation without a second bulk operation, and pretending otherwise would be worse than
    // saying so. The remaining items stay `pending`, so what was and was not done stays readable.
    let ids = assets(pool, "cancelled", 5).await;
    let op = bulk::create(pool, &spec("delete"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");

    bulk::record_outcome(pool, op.id, ids[0], ItemOutcome::Done)
        .await
        .expect("record");
    assert!(bulk::cancel(pool, op.id, now()).await.expect("cancel"));

    let cancelled = bulk::load(pool, op.id)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(cancelled.state, "cancelled");
    assert_eq!(
        cancelled.done_count, 1,
        "the work already applied stays counted"
    );

    let items = bulk::items(pool, op.id, 100).await.expect("items");
    let pending = items.iter().filter(|i| i.state == "pending").count();
    assert_eq!(
        pending, 4,
        "the untouched items stay pending, so what was not done is still readable"
    );

    // A cancelled operation cannot be finished into a success afterwards.
    let after = bulk::finish(pool, op.id, now()).await.expect("finish");
    assert_eq!(after.state, "cancelled");
}

async fn a_finished_operation_cannot_be_restarted(pool: &PgPool) {
    let ids = assets(pool, "done-already", 1).await;
    let op = bulk::create(pool, &spec("export"), &ids)
        .await
        .expect("create");
    bulk::start(pool, op.id, now()).await.expect("start");
    bulk::record_outcome(pool, op.id, ids[0], ItemOutcome::Done)
        .await
        .expect("record");
    bulk::finish(pool, op.id, now()).await.expect("finish");

    assert!(
        !bulk::start(pool, op.id, now()).await.expect("restart"),
        "a completed operation must not be reopened"
    );
    assert!(!bulk::pause(pool, op.id).await.expect("pause"));
    assert!(!bulk::cancel(pool, op.id, now()).await.expect("cancel"));
}

#[tokio::test]
async fn the_bulk_operation_invariants_hold() {
    let (_pg, pool) = db().await;

    the_target_set_is_materialised_at_creation(&pool).await;
    a_repeated_id_is_deduplicated_and_the_count_corrected(&pool).await;
    an_operation_over_nothing_is_refused(&pool).await;
    a_dry_run_writes_nothing_at_all(&pool).await;

    a_worker_that_dies_resumes_after_the_last_recorded_asset(&pool).await;
    recording_out_of_order_does_not_orphan_the_lower_ids(&pool).await;
    a_retried_worker_cannot_inflate_the_counters(&pool).await;

    an_operation_with_failures_is_partial_not_completed(&pool).await;
    an_operation_where_everything_failed_is_failed(&pool).await;
    a_clean_operation_is_completed(&pool).await;
    a_skip_counts_as_neither_done_nor_failed(&pool).await;
    failures_are_reported_row_by_row(&pool).await;
    the_error_sample_is_bounded(&pool).await;

    pausing_stops_the_batch_and_resuming_continues(&pool).await;
    cancelling_leaves_applied_work_applied_and_says_what_was_done(&pool).await;
    a_finished_operation_cannot_be_restarted(&pool).await;
}
