//! `upload_sessions`: the durable half of a resumable upload (task 1.6).
//!
//! `dam_store::resumable` deliberately keeps no map of live uploads — every byte of state it
//! needs is a value the caller persists. This table is that value, and the constraints here are
//! what stop a session row from describing an upload that cannot exist.
//!
//! The reason to enforce them in the database rather than only in Rust: a session is written by
//! one request and read by another, possibly on another node, and an inconsistent row silently
//! assembles the *wrong bytes* under a content-addressed key. That is worse than a failed
//! upload, because it produces an object that looks canonical.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_db::{migrate, testing::PostgresHarness};
use sqlx::{Executor, PgPool, Row};

async fn tenant_db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global migrations");
    migrate::tenant(&url, "t_acme")
        .await
        .expect("tenant migrations");
    let pool = pg
        .pool_for_schema("t_acme")
        .await
        .expect("schema-scoped pool");
    (pg, pool)
}

/// Asserts the statement was refused **by a constraint**, not by a missing relation.
async fn refused_by_constraint(pool: &PgPool, sql: &str) -> bool {
    match pool.execute(sqlx::AssertSqlSafe(sql.to_owned())).await {
        Ok(_) => false,
        Err(sqlx::Error::Database(db)) => {
            let code = db.code().unwrap_or_default().to_string();
            assert!(
                code.starts_with("23") || code == "P0001",
                "statement failed, but not because a constraint refused it \
                 (SQLSTATE {code}: {db}).\n{sql}"
            );
            true
        }
        Err(e) => panic!("unexpected non-database error:\n{sql}\n{e}"),
    }
}

async fn ok(pool: &PgPool, sql: &str) {
    pool.execute(sqlx::AssertSqlSafe(sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("expected this to be accepted:\n{sql}\n{e}"));
}

/// A minimal valid session.
fn insert(id: &str, extra: &str) -> String {
    format!(
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes {}) \
         VALUES (gen_random_uuid(), gen_random_uuid(), '{id}', 'active', 0 {})",
        if extra.is_empty() { "" } else { ", " },
        if extra.is_empty() {
            String::new()
        } else {
            format!(", {extra}")
        }
    )
}

#[tokio::test]
async fn a_valid_session_is_accepted_and_defaults_are_sane() {
    let (_pg, pool) = tenant_db().await;
    ok(&pool, &insert("01J8Z9QX4E", "")).await;

    let row = sqlx::query(
        "SELECT status, offset_bytes, part_count, tail_bytes, declared_length, created_at, \
         expires_at FROM upload_sessions WHERE upload_id = '01J8Z9QX4E'",
    )
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(row.get::<String, _>("status"), "active");
    assert_eq!(row.get::<i64, _>("offset_bytes"), 0);
    assert_eq!(row.get::<i32, _>("part_count"), 0);
    assert_eq!(row.get::<i64, _>("tail_bytes"), 0);
    assert!(
        row.get::<Option<i64>, _>("declared_length").is_none(),
        "TUS Upload-Defer-Length means the size is genuinely unknown, so the column must be \
         nullable rather than defaulting to zero"
    );
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("expires_at")
            .is_some(),
        "every session must expire, or an abandoned upload's parts are billed forever"
    );
}

#[tokio::test]
async fn an_upload_id_is_unique_within_the_tenant() {
    // The id becomes part of an object key. Two sessions sharing one would write to the same
    // staging object and interleave their bytes.
    let (_pg, pool) = tenant_db().await;
    ok(&pool, &insert("dup", "")).await;
    assert!(
        refused_by_constraint(&pool, &insert("dup", "")).await,
        "a duplicate upload id must be refused"
    );
}

#[tokio::test]
async fn an_upload_id_that_could_escape_its_key_prefix_is_refused() {
    // Mirrors `Key::staging`'s validation. Enforced here too because a row can be written by a
    // bulk import or a support engineer, not only by the code path that validates.
    let (_pg, pool) = tenant_db().await;
    for bad in ["../escape", "has/slash", "has space", ""] {
        assert!(
            refused_by_constraint(&pool, &insert(bad, "")).await,
            "{bad:?} must be refused as an upload id"
        );
    }
    ok(&pool, &insert("ok-id_1", "")).await;
}

#[tokio::test]
async fn an_offset_beyond_the_declared_length_is_refused() {
    // The invariant the whole upload depends on: a session claiming more bytes accepted than the
    // client said it would send describes an upload that cannot be completed.
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, declared_length) \
             VALUES (gen_random_uuid(), gen_random_uuid(), 'over', 'active', 101, 100)"
        )
        .await,
        "offset must not exceed declared_length"
    );
    ok(
        &pool,
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, declared_length) \
         VALUES (gen_random_uuid(), gen_random_uuid(), 'exact', 'active', 100, 100)",
    )
    .await;
}

#[tokio::test]
async fn negative_counters_are_refused() {
    let (_pg, pool) = tenant_db().await;
    for bad in [
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes) VALUES (gen_random_uuid(), gen_random_uuid(), 'neg1', 'active', -1)",
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, tail_bytes) VALUES (gen_random_uuid(), gen_random_uuid(), 'neg2', 'active', 0, -1)",
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, part_count) VALUES (gen_random_uuid(), gen_random_uuid(), 'neg3', 'active', 0, -1)",
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, declared_length) VALUES (gen_random_uuid(), gen_random_uuid(), 'neg4', 'active', 0, -1)",
    ] {
        assert!(refused_by_constraint(&pool, bad).await, "{bad}");
    }
}

#[tokio::test]
async fn a_tail_larger_than_the_part_minimum_is_refused() {
    // A tail at or above 5 MiB should have become a part. A row saying otherwise means the
    // engine failed to flush, and completing from it would produce an upload S3 rejects.
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, tail_bytes) \
             VALUES (gen_random_uuid(), gen_random_uuid(), 'fat-tail', 'active', 9999999, 5242880)"
        )
        .await,
        "a 5 MiB tail must have been flushed as a part"
    );
    ok(
        &pool,
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, tail_bytes) \
         VALUES (gen_random_uuid(), gen_random_uuid(), 'thin-tail', 'active', 5242879, 5242879)",
    )
    .await;
}

#[tokio::test]
async fn a_completed_session_must_record_what_it_produced() {
    // Without this a session can read as complete while pointing at nothing, and the caller has
    // no key to promote.
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes) \
             VALUES (gen_random_uuid(), gen_random_uuid(), 'done-empty', 'completed', 10)"
        )
        .await,
        "a completed session needs its completion recorded"
    );
    ok(
        &pool,
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, completed_at, asset_id) \
         VALUES (gen_random_uuid(), gen_random_uuid(), 'done', 'completed', 10, now(), NULL)",
    )
    .await;
}

#[tokio::test]
async fn parts_recorded_without_a_backend_upload_id_are_refused() {
    // Parts only exist inside a multipart upload. A row with parts and no upload id cannot be
    // completed or aborted, so its parts would be billed until a lifecycle rule expired them.
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, part_count) \
             VALUES (gen_random_uuid(), gen_random_uuid(), 'orphan-parts', 'active', 6000000, 1)"
        )
        .await,
        "parts without a backend upload id are unreachable"
    );
    // The accepted form needs the part *list* too: `upload_part_count_matches_list` refuses a
    // counter that disagrees with the array, because completing from such a row would omit or
    // repeat a part — which S3 accepts, producing a corrupt object.
    ok(
        &pool,
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, part_count, \
         parts, backend_upload_id) VALUES (gen_random_uuid(), gen_random_uuid(), 'real-parts', \
         'active', 6000000, 1, '[{\"number\":1,\"etag\":\"a\"}]'::jsonb, 'mpu-1')",
    )
    .await;
}

#[tokio::test]
async fn an_unknown_status_is_refused() {
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes) \
             VALUES (gen_random_uuid(), gen_random_uuid(), 'bad-status', 'in_progress', 0)"
        )
        .await,
        "the status vocabulary must match SessionStatus in Rust"
    );
    for good in ["active", "completed", "terminated", "expired"] {
        ok(
            &pool,
            &format!(
                "INSERT INTO upload_sessions (id, tenant_id, upload_id, status, offset_bytes, completed_at) \
                 VALUES (gen_random_uuid(), gen_random_uuid(), 'st-{good}', '{good}', 0, \
                 CASE WHEN '{good}' = 'completed' THEN now() ELSE NULL END)"
            ),
        )
        .await;
    }
}

#[tokio::test]
async fn the_reaper_can_find_expired_sessions_by_index() {
    // The reaper runs on every deployment forever; a sequential scan over every upload ever made
    // is the kind of thing that is fine for a year and then is not.
    let (_pg, pool) = tenant_db().await;
    let indexed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE schemaname='t_acme' \
         AND tablename='upload_sessions' AND indexdef ILIKE '%expires_at%'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert!(indexed >= 1, "expires_at must be indexed for the reaper");
}
