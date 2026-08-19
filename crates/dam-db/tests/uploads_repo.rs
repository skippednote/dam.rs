//! Persistence and reaping for resumable uploads (task 1.6).
//!
//! `dam_store::resumable` is stateless by design: the session is a value the caller stores and
//! hands back, which is what lets any node serve any TUS PATCH. This is the other half — the
//! store, and the reaper that stops an abandoned upload from being billed forever.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::StorageClass;
use dam_db::{migrate, testing::PostgresHarness, uploads};
use dam_store::{
    BlobStore, MIN_PART_SIZE,
    resumable::{PartRecord, SessionStatus},
    testing::SeaweedfsHarness,
};
use sqlx::PgPool;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x0da3_0000_0000_0000_0000_0000_0000_0005);

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

#[tokio::test]
async fn a_session_round_trips_through_the_database_unchanged() {
    // The hand-off between nodes is only real if what comes back is what went in. Every field
    // is compared, because a silently-dropped one — the part list, say — would assemble the
    // wrong bytes under a content-addressed key.
    let (_pg, pool) = db().await;
    let mut created = uploads::create(
        &pool,
        TENANT,
        "01J8Z9QX4E",
        Some(4096),
        Some("holiday.jpg"),
        Some("image/jpeg"),
        None,
        None,
    )
    .await
    .expect("create");

    created.offset = 3000;
    created.tail_len = 3000;
    uploads::save(&pool, &created).await.expect("save");

    let loaded = uploads::load(&pool, TENANT, "01J8Z9QX4E")
        .await
        .expect("load")
        .expect("present");
    assert_eq!(loaded, created);
}

#[tokio::test]
async fn the_part_list_survives_persistence_in_order() {
    // Part order is the completion list's order, and S3 concatenates by it. A store that
    // returned the parts sorted differently would produce a scrambled object that still
    // completes successfully.
    let (_pg, pool) = db().await;
    let mut s = uploads::create(&pool, TENANT, "parts", None, None, None, None, None)
        .await
        .expect("create");
    s.s3_upload_id = Some("mpu-1".into());
    s.parts = vec![
        PartRecord {
            number: 1,
            etag: "\"aaa\"".into(),
        },
        PartRecord {
            number: 2,
            etag: "\"bbb\"".into(),
        },
        PartRecord {
            number: 3,
            etag: "\"ccc\"".into(),
        },
    ];
    s.offset = 3 * MIN_PART_SIZE as u64;
    uploads::save(&pool, &s).await.expect("save");

    let loaded = uploads::load(&pool, TENANT, "parts")
        .await
        .expect("load")
        .expect("present");
    assert_eq!(loaded.parts, s.parts);
}

#[tokio::test]
async fn a_session_round_trips_through_a_tenant_conn_transaction() {
    // The point of the executor-generic signatures. §5.2 makes `TenantConn` the isolation mechanism —
    // one shared pool, a per-request transaction with `SET LOCAL search_path` — so the request path has
    // to work against a *connection inside a transaction*, not only against a pool. A function that
    // accepted only a pool would force one pool per tenant, which is a thousand pools at a thousand
    // tenants.
    let (pg, _pool) = db().await;
    let shared = pg.pool();
    let slug = dam_core::TenantSlug::new("acme").expect("slug");

    // Create inside one transaction and commit.
    let mut conn = dam_db::TenantConn::begin(shared, &slug)
        .await
        .expect("begin");
    let mut session = uploads::create(
        conn.executor(),
        TENANT,
        "via-tenant-conn",
        Some(64),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("create");
    session.offset = 64;
    session.tail_len = 64;
    uploads::save(conn.executor(), &session)
        .await
        .expect("save");
    conn.commit().await.expect("commit");

    // Read it back in a *separate* transaction, which is what a second request would do.
    let mut conn = dam_db::TenantConn::begin(shared, &slug)
        .await
        .expect("begin");
    let loaded = uploads::load(conn.executor(), TENANT, "via-tenant-conn")
        .await
        .expect("load")
        .expect("present");
    conn.commit().await.expect("commit");
    assert_eq!(loaded.offset, 64);
    assert_eq!(loaded.tail_len, 64);
}

#[tokio::test]
async fn work_in_an_uncommitted_tenant_conn_is_not_visible_elsewhere() {
    // The transaction boundary is load-bearing for TUS: a PATCH that fails after writing the tail but
    // before committing the offset must leave no session at all, or a client resumes against state the
    // server does not have.
    let (pg, pool) = db().await;
    let slug = dam_core::TenantSlug::new("acme").expect("slug");

    let mut conn = dam_db::TenantConn::begin(pg.pool(), &slug)
        .await
        .expect("begin");
    uploads::create(
        conn.executor(),
        TENANT,
        "rolled-back",
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("create");
    conn.rollback().await.expect("rollback");

    assert!(
        uploads::load(&pool, TENANT, "rolled-back")
            .await
            .expect("load")
            .is_none(),
        "a rolled-back create must leave nothing behind"
    );
}

#[tokio::test]
async fn an_unknown_upload_id_loads_as_absent_rather_than_erroring() {
    let (_pg, pool) = db().await;
    assert!(
        uploads::load(&pool, TENANT, "never-existed")
            .await
            .expect("query must succeed")
            .is_none(),
        "a missing session is a normal outcome — a client polling a completed upload's id \
         gets here"
    );
}

#[tokio::test]
async fn the_database_refuses_a_session_the_rust_should_never_have_built() {
    // The constraints and the engine must agree. A tail at or above the part minimum means the
    // engine failed to flush, and saving it would let a later completion produce an upload S3
    // rejects at the last step — after every byte had been paid for.
    let (_pg, pool) = db().await;
    let mut s = uploads::create(&pool, TENANT, "bad-tail", None, None, None, None, None)
        .await
        .expect("create");
    s.tail_len = MIN_PART_SIZE as u64;
    s.offset = MIN_PART_SIZE as u64;
    let err = uploads::save(&pool, &s).await.expect_err("must be refused");
    assert!(
        format!("{err}").to_lowercase().contains("tail"),
        "the error should name the constraint: {err}"
    );
}

#[tokio::test]
async fn only_expired_active_sessions_are_reapable() {
    let (_pg, pool) = db().await;

    // Expired and still active: the only reapable shape.
    let stale = uploads::create(&pool, TENANT, "stale", None, None, None, None, None)
        .await
        .expect("create");
    uploads::force_expiry_for_test(&pool, &stale.id, -1)
        .await
        .expect("age it");

    // Expired but completed: its bytes were promoted, so there is nothing to reclaim and the
    // row is history.
    let mut done = uploads::create(&pool, TENANT, "done", Some(4), None, None, None, None)
        .await
        .expect("create");
    done.offset = 4;
    done.status = SessionStatus::Completed;
    uploads::save(&pool, &done).await.expect("save");
    uploads::force_expiry_for_test(&pool, &done.id, -1)
        .await
        .expect("age it");

    // Active but not yet expired.
    uploads::create(&pool, TENANT, "fresh", None, None, None, None, None)
        .await
        .expect("create");

    let reapable = uploads::reapable(&pool, 100).await.expect("reapable");
    let ids: Vec<&str> = reapable.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["stale"],
        "expected only the expired active session, got {ids:?}"
    );
}

#[tokio::test]
async fn reaping_aborts_the_multipart_upload_and_removes_every_staged_object() {
    // An abandoned upload that keeps its parts is billed for them until a bucket lifecycle rule
    // notices. This is the path that stops the meter, so it is tested against a real server —
    // aborting is precisely the operation a fake cannot prove.
    let (_pg, pool) = db().await;
    let s3 = SeaweedfsHarness::start().await.expect("start seaweedfs");
    let store = s3.store();

    let mut session = uploads::create(&pool, TENANT, "abandoned", None, None, None, None, None)
        .await
        .expect("create");

    // Drive a real upload far enough to have both a part and a tail.
    let big = bytes::Bytes::from(vec![7u8; MIN_PART_SIZE + 2048]);
    dam_store::resumable::patch(&store, &mut session, 0, big.clone(), StorageClass::Standard)
        .await
        .expect("patch");
    dam_store::resumable::patch(
        &store,
        &mut session,
        big.len() as u64,
        bytes::Bytes::from(vec![8u8; 64]),
        StorageClass::Standard,
    )
    .await
    .expect("patch tail");
    uploads::save(&pool, &session).await.expect("save");
    assert!(
        session.s3_upload_id.is_some() && session.tail_len > 0,
        "precondition: a real multipart upload and a leftover tail"
    );
    assert!(
        store.head(&session.tail_key().expect("key")).await.is_ok(),
        "precondition: the tail object exists"
    );

    uploads::force_expiry_for_test(&pool, &session.id, -1)
        .await
        .expect("age it");
    let reaped = uploads::reap(&pool, &store, 100).await.expect("reap");
    assert_eq!(reaped, 1);

    let left = store
        .list(&format!("{TENANT}/staging/"), 100)
        .await
        .expect("list");
    assert!(
        left.is_empty(),
        "no staged object may survive the reaper: {left:?}"
    );
    let after = uploads::load(&pool, TENANT, "abandoned")
        .await
        .expect("load")
        .expect("row is kept as history");
    assert_eq!(
        after.status,
        SessionStatus::Terminated,
        "the row records that it was reclaimed rather than vanishing, so a client asking \
         about its upload gets an answer"
    );
}

#[tokio::test]
async fn reaping_is_idempotent_and_reaping_nothing_is_not_an_error() {
    let (_pg, pool) = db().await;
    let s3 = SeaweedfsHarness::start().await.expect("start seaweedfs");
    let store = s3.store();

    assert_eq!(
        uploads::reap(&pool, &store, 100).await.expect("empty reap"),
        0,
        "a reaper that errors when there is nothing to do fails its own cron every minute"
    );

    let session = uploads::create(&pool, TENANT, "twice", None, None, None, None, None)
        .await
        .expect("create");
    uploads::force_expiry_for_test(&pool, &session.id, -1)
        .await
        .expect("age it");
    assert_eq!(uploads::reap(&pool, &store, 100).await.expect("first"), 1);
    assert_eq!(
        uploads::reap(&pool, &store, 100).await.expect("second"),
        0,
        "the second pass must find nothing — a reaper that keeps reclaiming the same row \
         never drains its queue"
    );
}

#[tokio::test]
async fn the_reaper_respects_its_batch_limit() {
    // Unbounded, a reaper that wakes to a backlog of a million abandoned uploads takes the
    // database with it.
    let (_pg, pool) = db().await;
    for i in 0..5 {
        let s = uploads::create(
            &pool,
            TENANT,
            &format!("batch-{i}"),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("create");
        uploads::force_expiry_for_test(&pool, &s.id, -1)
            .await
            .expect("age it");
    }
    assert_eq!(uploads::reapable(&pool, 2).await.expect("limited").len(), 2);
}
