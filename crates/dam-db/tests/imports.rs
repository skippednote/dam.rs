//! Phased migration in (G7).
//!
//! `import_jobs` and `import_records` have been in the schema since tenant 0008 with nothing reading them. The
//! properties worth defending are the ones that make a 400k-asset migration survivable:
//!
//! **A run cannot skip its own review.** §G7's failure mode is a library moved under a crosswalk nobody looked
//! at, so the phase machine only goes forward — and `discover → transfer` is refused by name.
//!
//! **`failed` is not terminal.** A run that failed on a bad mapping is fixed by changing the mapping, which is
//! the whole reason 0008 calls the crosswalk "editable between phases". `complete` and `rolled_back` *are*
//! terminal.
//!
//! **A resumed run does not duplicate, and a dry run cannot un-migrate.** `(job, source_id)` is the primary key
//! and the idempotency key at once, so re-running discovery over changed data updates a pending record and
//! leaves a migrated one alone.
//!
//! **Records are never deleted, not even by a rollback.** 0008 retains `source_id` permanently because "two
//! years later, 'which Widen asset did this come from' is a question that gets asked" — and a second attempt
//! needs to know what the first one did.
//!
//! **A rollback takes only what the job created.** A record whose asset was deleted since is excluded, so an
//! escape hatch cannot become a second incident.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::imports::{self, ImportRefusal, NewImport, Phase};
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

async fn job(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::now_v7();
    let mut conn = pool.acquire().await.expect("conn");
    imports::create(
        &mut conn,
        &NewImport {
            id,
            source: "csv",
            label,
            config: json!({ "path": "/tmp/library.csv" }),
            batch_size: 100,
            created_by: None,
        },
    )
    .await
    .expect("create");
    id
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
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

#[tokio::test]
async fn a_run_cannot_skip_its_own_review() {
    // §G7's failure mode: a library moved under a crosswalk nobody looked at.
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = job(&pool, "Widen, phase one").await;

    match imports::advance(&mut conn, id, Phase::Transfer).await {
        Err(ImportRefusal::NotForward { from, to }) => {
            assert_eq!(from, "discover");
            assert_eq!(to, "transfer");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Forward, one step at a time, and each step is allowed.
    for phase in [
        Phase::CrosswalkReview,
        Phase::DryRun,
        Phase::Transfer,
        Phase::Verify,
        Phase::Complete,
    ] {
        imports::advance(&mut conn, id, phase)
            .await
            .unwrap_or_else(|error| panic!("advancing to {phase:?}: {error}"));
    }

    // And backwards is refused, so a completed run cannot be quietly reopened.
    match imports::advance(&mut conn, id, Phase::Transfer).await {
        Err(ImportRefusal::NotForward { from, .. }) => assert_eq!(from, "complete"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Nor may a *smaller* jump skip a step: dry-run straight to verify would move nothing and call it verified.
    let second = job(&pool, "Widen, phase two").await;
    imports::advance(&mut conn, second, Phase::CrosswalkReview)
        .await
        .expect("review");
    match imports::advance(&mut conn, second, Phase::Verify).await {
        Err(ImportRefusal::NotForward { from, to }) => {
            assert_eq!(from, "crosswalk_review");
            assert_eq!(to, "verify");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.phase, Phase::Complete);
    // Stamped once each: "when did this run", not "when did we last look".
    assert!(found.started_at.is_some());
    assert!(found.finished_at.is_some());
}

#[tokio::test]
async fn transfer_and_verify_loop_because_a_migration_moves_in_batches() {
    // The property I nearly lost by writing the rule as "forward only". §G7 asks for "phased/incremental
    // transfer rather than single cutover" with "QA checkpoints" — which means the transfer/verify pair runs
    // once per batch, many times, before anything is complete. A strictly forward machine would allow exactly
    // one batch.
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = job(&pool, "400k assets").await;

    imports::advance(&mut conn, id, Phase::CrosswalkReview)
        .await
        .expect("review");
    imports::advance(&mut conn, id, Phase::DryRun)
        .await
        .expect("dry run");

    for batch in 0..3 {
        imports::advance(&mut conn, id, Phase::Transfer)
            .await
            .unwrap_or_else(|error| panic!("batch {batch}: {error}"));
        imports::advance(&mut conn, id, Phase::Verify)
            .await
            .unwrap_or_else(|error| panic!("verifying batch {batch}: {error}"));
    }

    // And only then complete.
    imports::advance(&mut conn, id, Phase::Complete)
        .await
        .expect("complete");
    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.phase, Phase::Complete);

    // The loop does not reopen a finished run.
    match imports::advance(&mut conn, id, Phase::Transfer).await {
        Err(ImportRefusal::NotForward { .. }) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failed_run_is_fixable_and_a_complete_one_is_not() {
    // The distinction 0008's "editable between phases" depends on: a run that failed on a bad mapping is fixed
    // by changing the mapping, so `failed` has to be a state you can leave.
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = job(&pool, "Bynder").await;

    imports::advance(&mut conn, id, Phase::CrosswalkReview)
        .await
        .expect("review");
    imports::advance(&mut conn, id, Phase::Failed)
        .await
        .expect("fail");
    assert!(!Phase::Failed.is_terminal());

    // Back to review, then forward again.
    imports::advance(&mut conn, id, Phase::CrosswalkReview)
        .await
        .expect("a failed run is fixable");
    imports::advance(&mut conn, id, Phase::DryRun)
        .await
        .expect("dry run");

    // The crosswalk is editable in every non-terminal phase, which is what makes the fix possible at all.
    imports::set_crosswalk(
        &mut conn,
        id,
        &json!({ "rules": [{ "source": "Title", "target": "caption" }] }),
        &json!({}),
        &json!(["Photographer"]),
    )
    .await
    .expect("crosswalk");
    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.unmapped_fields, json!(["Photographer"]));

    // But not once it is over. A completed migration's mapping is history.
    for phase in [Phase::Transfer, Phase::Verify, Phase::Complete] {
        imports::advance(&mut conn, id, phase)
            .await
            .unwrap_or_else(|error| panic!("advancing to {phase:?}: {error}"));
    }
    assert!(Phase::Complete.is_terminal());
    match imports::set_crosswalk(&mut conn, id, &json!({}), &json!({}), &json!([])).await {
        Err(ImportRefusal::NotForward { from, .. }) => assert_eq!(from, "complete"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_resumed_run_updates_a_pending_record_and_leaves_a_migrated_one() {
    // `(job, source_id)` is the primary key and the idempotency key at once. A batch that half-finished has to
    // re-run without duplicating, and a second dry run must never un-migrate what a transfer already did.
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = job(&pool, "Brandfolder").await;
    let one = asset(&pool, "arrived").await;

    imports::note(&mut conn, id, "src-1", Some("aaa"), &json!([]), None)
        .await
        .expect("note");
    imports::note(&mut conn, id, "src-2", Some("bbb"), &json!([]), None)
        .await
        .expect("note");
    imports::migrated(&mut conn, id, "src-1", one)
        .await
        .expect("migrated");

    // A second discovery pass over changed data.
    imports::note(
        &mut conn,
        id,
        "src-1",
        Some("aaa-changed"),
        &json!([{ "code": "unmapped_field" }]),
        None,
    )
    .await
    .expect("note");
    imports::note(
        &mut conn,
        id,
        "src-2",
        Some("bbb-changed"),
        &json!([]),
        None,
    )
    .await
    .expect("note");

    let records = imports::records(&mut conn, id, 100).await.expect("records");
    assert_eq!(records.len(), 2, "updated, not duplicated: {records:?}");
    let migrated = records
        .iter()
        .find(|r| r.source_id == "src-1")
        .expect("src-1");
    assert_eq!(migrated.state, "migrated", "a dry run cannot un-migrate");
    assert_eq!(migrated.asset_id, Some(one));
    // The warnings *do* update, because a re-discovery finding a new problem with an already-migrated asset is
    // worth recording — it is the next run's work.
    assert_eq!(migrated.warnings, json!([{ "code": "unmapped_field" }]));

    // And only the pending one is still pending.
    let pending = imports::pending(&mut conn, id, 10).await.expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].source_id, "src-2");
}

#[tokio::test]
async fn the_counters_are_a_convenience_and_the_records_are_the_truth() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = job(&pool, "Canto").await;

    for n in 0..4 {
        imports::note(&mut conn, id, &format!("src-{n}"), None, &json!([]), None)
            .await
            .expect("note");
    }
    let one = asset(&pool, "one").await;
    let two = asset(&pool, "two").await;
    imports::migrated(&mut conn, id, "src-0", one)
        .await
        .expect("migrated");
    imports::migrated(&mut conn, id, "src-1", two)
        .await
        .expect("migrated");
    imports::failed(&mut conn, id, "src-2", "no bytes at the source")
        .await
        .expect("failed");

    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.migrated_count, 2);
    assert_eq!(found.failed_count, 1);

    // Migrating the same record twice does not double the count — the update is guarded on the state, so a
    // retried batch cannot inflate a progress display into nonsense.
    imports::migrated(&mut conn, id, "src-0", one)
        .await
        .expect("again");
    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.migrated_count, 2, "still two");

    // Counters can drift — a crash between the record write and the bump. The records are the truth, so
    // `recount` recomputes rather than adjusting.
    sqlx::query("UPDATE import_jobs SET migrated_count = 99, failed_count = 0 WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("drift");
    imports::recount(&mut conn, id).await.expect("recount");
    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.migrated_count, 2);
    assert_eq!(found.failed_count, 1);
}

#[tokio::test]
async fn a_rollback_takes_only_what_the_job_created_and_keeps_the_records() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = job(&pool, "Aprimo").await;
    let kept = asset(&pool, "still-here").await;
    let gone = asset(&pool, "deleted-since").await;

    imports::note(&mut conn, id, "src-1", None, &json!([]), None)
        .await
        .expect("note");
    imports::note(&mut conn, id, "src-2", None, &json!([]), None)
        .await
        .expect("note");
    imports::migrated(&mut conn, id, "src-1", kept)
        .await
        .expect("migrated");
    imports::migrated(&mut conn, id, "src-2", gone)
        .await
        .expect("migrated");

    // Somebody deleted one of them afterwards. A rollback must not try to take it — an escape hatch that
    // touched things the job did not bring would be a second incident.
    sqlx::query("UPDATE assets SET deleted_at = now(), status = 'deleted' WHERE id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("delete");

    let created = imports::created_assets(&mut conn, id)
        .await
        .expect("created");
    assert_eq!(
        created,
        vec![kept],
        "only what is still there and still ours"
    );

    let rolled = imports::mark_rolled_back(&mut conn, id)
        .await
        .expect("rollback");
    assert_eq!(rolled, 2, "both migrated records are marked");

    // Kept, never deleted. 0008 retains `source_id` permanently, and a second attempt needs to know what the
    // first one did.
    let records = imports::records(&mut conn, id, 100).await.expect("records");
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.state == "rolled_back"));
    assert!(
        records.iter().all(|r| r.asset_id.is_none()),
        "the asset is gone; the source id is the thing worth keeping",
    );
    assert!(records.iter().any(|r| r.source_id == "src-1"));

    imports::advance(&mut conn, id, Phase::RolledBack)
        .await
        .expect("phase");
    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert!(found.phase.is_terminal(), "a rollback is the end of a run");
    assert!(found.finished_at.is_some());
    // And the token is what makes "everything this job created" one operation.
    assert_ne!(found.rollback_token, Uuid::nil());
}

#[tokio::test]
async fn the_report_is_stored_so_the_signed_off_artifact_survives_the_run() {
    // §G7: the dry-run report is "the artifact the customer signs off on". A report that only existed in a
    // terminal window could not be pointed at afterwards.
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = job(&pool, "Widen").await;

    let report = json!({
        "records": 40_000,
        "would_arrive": 39_120,
        "coverage": { "Photographer": { "present": 40_000, "mapped": 0 } },
    });
    imports::set_report(&mut conn, id, 40_000, &report)
        .await
        .expect("report");

    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.discovered_count, 40_000);
    assert_eq!(found.report, report);
    // Still readable after the run finishes, which is the point.
    imports::advance(&mut conn, id, Phase::CrosswalkReview)
        .await
        .expect("review");
    imports::advance(&mut conn, id, Phase::DryRun)
        .await
        .expect("dry");
    imports::advance(&mut conn, id, Phase::Transfer)
        .await
        .expect("transfer");
    imports::advance(&mut conn, id, Phase::Verify)
        .await
        .expect("verify");
    imports::advance(&mut conn, id, Phase::Complete)
        .await
        .expect("complete");
    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.report, report);
}

#[tokio::test]
async fn a_job_needs_a_label_and_a_source_the_schema_allows() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");

    match imports::create(
        &mut conn,
        &NewImport {
            id: Uuid::now_v7(),
            source: "csv",
            label: "   ",
            config: json!({}),
            batch_size: 10,
            created_by: None,
        },
    )
    .await
    {
        Err(ImportRefusal::Invalid(message)) => {
            assert!(message.contains("needs a label"), "{message}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // An invented source is the schema's refusal, surfaced by name rather than as a database error.
    match imports::create(
        &mut conn,
        &NewImport {
            id: Uuid::now_v7(),
            source: "dropbox",
            label: "Dropbox",
            config: json!({}),
            batch_size: 10,
            created_by: None,
        },
    )
    .await
    {
        Err(ImportRefusal::Invalid(_)) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }

    // And the batch size is clamped rather than trusted: a batch of zero would move nothing forever, and a
    // batch of a million is one transaction nobody can retry.
    let id = Uuid::now_v7();
    imports::create(
        &mut conn,
        &NewImport {
            id,
            source: "csv",
            label: "Clamped",
            config: json!({}),
            batch_size: 0,
            created_by: None,
        },
    )
    .await
    .expect("create");
    let found = imports::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.batch_size, 1);
}
