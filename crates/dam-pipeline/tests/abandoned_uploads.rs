//! Uploads whose client never came back (3.x item 1).
//!
//! The failure this suite exists for is not that a stale session lingers. It is that reclaiming one **deletes
//! a file the user successfully uploaded**: a presigned PUT goes straight from the browser to the bucket, so
//! nothing in this process observes it, and a client that finishes and then closes the tab leaves a session
//! `active` with a real object behind it. The reaper then terminated the session and deleted the object —
//! having reported success to the user at every step.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::StorageClass;
use dam_db::{migrate, testing::PostgresHarness, uploads};
use dam_store::{BlobStore, FakeS3Store, Key};
use sqlx::PgPool;
use uuid::Uuid;

async fn fixture() -> (PostgresHarness, PgPool, PgPool, Uuid, FakeS3Store) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");
    let (store, _) = FakeS3Store::with_test_clock();
    (pg, global, tenant, tenant_id, store)
}

/// Opens a presigned-shaped session: a row, and no multipart upload.
async fn session(tenant: &PgPool, tenant_id: Uuid, upload_id: &str, declared: Option<i64>) {
    sqlx::query(
        "INSERT INTO upload_sessions (id, tenant_id, upload_id, declared_length) \
         VALUES (gen_random_uuid(), $1, $2, $3)",
    )
    .bind(tenant_id)
    .bind(upload_id)
    .bind(declared)
    .execute(tenant)
    .await
    .expect("session");
}

async fn status(tenant: &PgPool, upload_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM upload_sessions WHERE upload_id = $1")
        .bind(upload_id)
        .fetch_one(tenant)
        .await
        .expect("status")
}

async fn queued_finalisations(global: &PgPool, tenant_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs \
         WHERE tenant_id = $1 AND kind = 'finalise_upload' AND state = 'queued'",
    )
    .bind(tenant_id)
    .fetch_one(global)
    .await
    .expect("count")
}

#[tokio::test]
async fn an_upload_the_client_finished_is_finalised_rather_than_deleted() {
    let (_pg, global, tenant, tenant_id, store) = fixture().await;

    // The client presigned, PUT its bytes straight to the bucket, and never called back.
    session(&tenant, tenant_id, "finished", Some(11)).await;
    let key = Key::staging(tenant_id, "finished").expect("key");
    store
        .put(&key, "hello world".into(), StorageClass::Standard)
        .await
        .expect("the client's PUT");
    uploads::force_expiry_for_test(&tenant, "finished", -1)
        .await
        .expect("age it");

    let swept = dam_pipeline::abandoned::sweep_tenant(
        &global,
        &mut tenant.acquire().await.expect("conn"),
        &store,
        tenant_id,
    )
    .await
    .expect("sweep");

    assert_eq!(swept.rescued, 1, "the object was there: {swept:?}");
    assert_eq!(swept.reclaimed, 0, "nothing should have been reclaimed");
    assert!(
        store.head(&key).await.is_ok(),
        "the user's bytes must still exist — deleting them is the bug this closes"
    );
    assert_eq!(
        queued_finalisations(&global, tenant_id).await,
        1,
        "finalisation goes through the ordinary path rather than a second ingest here"
    );
    assert_eq!(
        status(&tenant, "finished").await,
        "active",
        "the session stays active: the finalise handler is what completes it"
    );
}

#[tokio::test]
async fn an_upload_with_nothing_behind_it_is_still_reclaimed() {
    let (_pg, global, tenant, tenant_id, store) = fixture().await;

    session(&tenant, tenant_id, "empty-handed", Some(10)).await;
    uploads::force_expiry_for_test(&tenant, "empty-handed", -1)
        .await
        .expect("age it");

    let swept = dam_pipeline::abandoned::sweep_tenant(
        &global,
        &mut tenant.acquire().await.expect("conn"),
        &store,
        tenant_id,
    )
    .await
    .expect("sweep");

    assert_eq!(swept.reclaimed, 1, "{swept:?}");
    assert_eq!(swept.rescued, 0);
    assert_eq!(status(&tenant, "empty-handed").await, "terminated");
    assert_eq!(queued_finalisations(&global, tenant_id).await, 0);
}

/// A zero-byte object is a PUT that started and produced nothing.
///
/// Treated as absent rather than as a finished upload: finalising it would create an empty asset, which is a
/// worse outcome than reclaiming, and one somebody would have to find and delete by hand.
#[tokio::test]
async fn a_zero_byte_object_is_reclaimed_rather_than_finalised() {
    let (_pg, global, tenant, tenant_id, store) = fixture().await;

    session(&tenant, tenant_id, "empty-object", None).await;
    let key = Key::staging(tenant_id, "empty-object").expect("key");
    store
        .put(&key, Vec::new().into(), StorageClass::Standard)
        .await
        .expect("empty put");
    uploads::force_expiry_for_test(&tenant, "empty-object", -1)
        .await
        .expect("age it");

    let swept = dam_pipeline::abandoned::sweep_tenant(
        &global,
        &mut tenant.acquire().await.expect("conn"),
        &store,
        tenant_id,
    )
    .await
    .expect("sweep");

    assert_eq!(swept.reclaimed, 1, "{swept:?}");
    assert_eq!(swept.rescued, 0);
}

/// The property that makes reclaiming safe to run beside rescuing.
///
/// Two expired sessions, one with an object and one without, and the one *without* sorts first. An earlier
/// draft reclaimed by position after checking by name — it would have deleted the finished upload's object
/// because the empty one was due first.
#[tokio::test]
async fn reclaiming_acts_on_the_session_that_was_checked_not_the_oldest() {
    let (_pg, global, tenant, tenant_id, store) = fixture().await;

    session(&tenant, tenant_id, "aaa-empty", Some(5)).await;
    session(&tenant, tenant_id, "zzz-finished", Some(11)).await;
    let kept = Key::staging(tenant_id, "zzz-finished").expect("key");
    store
        .put(&kept, "hello world".into(), StorageClass::Standard)
        .await
        .expect("put");

    // The empty one expires first, so it is the one a positional reap would take.
    uploads::force_expiry_for_test(&tenant, "aaa-empty", -2)
        .await
        .expect("age");
    uploads::force_expiry_for_test(&tenant, "zzz-finished", -1)
        .await
        .expect("age");

    let swept = dam_pipeline::abandoned::sweep_tenant(
        &global,
        &mut tenant.acquire().await.expect("conn"),
        &store,
        tenant_id,
    )
    .await
    .expect("sweep");

    assert_eq!(swept.rescued, 1, "{swept:?}");
    assert_eq!(swept.reclaimed, 1, "{swept:?}");
    assert_eq!(status(&tenant, "aaa-empty").await, "terminated");
    assert_eq!(status(&tenant, "zzz-finished").await, "active");
    assert!(
        store.head(&kept).await.is_ok(),
        "the finished upload's object must survive a pass that also reclaimed another"
    );
}

/// An upload whose finalisation has already died is left alone, not enqueued forever.
///
/// This is the loop the dedupe key does not close: it stops a second *live* finalise, but its unique index
/// only covers queued and running jobs, so once one dies the sweep would enqueue the same work on every pass.
/// For an upload the pipeline permanently refuses that is a five-minute-forever churn. The object is kept —
/// unfinishable is not junk — but no new job is queued.
#[tokio::test]
async fn a_dead_finalisation_is_not_re_enqueued() {
    let (_pg, global, tenant, tenant_id, store) = fixture().await;

    session(&tenant, tenant_id, "unfinishable", Some(11)).await;
    let key = Key::staging(tenant_id, "unfinishable").expect("key");
    store
        .put(&key, "hello world".into(), StorageClass::Standard)
        .await
        .expect("the client's PUT");
    uploads::force_expiry_for_test(&tenant, "unfinishable", -1)
        .await
        .expect("age it");

    // A finalisation for this upload that ran out of attempts. `dead`, the terminal state.
    sqlx::query(
        "INSERT INTO dam_global.jobs          (id, tenant_id, kind, dedupe_key, state, attempts, max_attempts, finished_at)          VALUES (gen_random_uuid(), $1, 'finalise_upload', 'finalise:unfinishable', 'dead', 5, 5, now())",
    )
    .bind(tenant_id)
    .execute(&global)
    .await
    .expect("dead job");

    let swept = dam_pipeline::abandoned::sweep_tenant(
        &global,
        &mut tenant.acquire().await.expect("conn"),
        &store,
        tenant_id,
    )
    .await
    .expect("sweep");

    assert_eq!(
        swept.rescued, 0,
        "a dead finalisation must not be retried: {swept:?}"
    );
    assert_eq!(
        swept.deferred, 1,
        "it is deferred for review, not churned: {swept:?}"
    );
    assert_eq!(
        queued_finalisations(&global, tenant_id).await,
        0,
        "no new finalise job may be queued behind a dead one"
    );
    assert!(
        store.head(&key).await.is_ok(),
        "and the object is kept — unfinishable is not the same as junk"
    );
}

/// A *failed* finalisation — between retries, not terminal — is left to the queue rather than re-enqueued.
///
/// The distinction from `dead` matters: a `failed` job runs again on its own, so enqueuing beside it would
/// be fighting the queue. The dedupe key already blocks a live duplicate, so the sweep enqueues nothing new
/// and the existing job is what retries.
#[tokio::test]
async fn a_failed_finalisation_between_retries_is_left_to_the_queue() {
    let (_pg, global, tenant, tenant_id, store) = fixture().await;

    session(&tenant, tenant_id, "retrying", Some(11)).await;
    let key = Key::staging(tenant_id, "retrying").expect("key");
    store
        .put(&key, "hello world".into(), StorageClass::Standard)
        .await
        .expect("put");
    uploads::force_expiry_for_test(&tenant, "retrying", -1)
        .await
        .expect("age it");

    // A finalise that failed but has attempts left: it is back to `queued`, which the dedupe key covers.
    sqlx::query(
        "INSERT INTO dam_global.jobs          (id, tenant_id, kind, dedupe_key, state, attempts, max_attempts)          VALUES (gen_random_uuid(), $1, 'finalise_upload', 'finalise:retrying', 'queued', 2, 5)",
    )
    .bind(tenant_id)
    .execute(&global)
    .await
    .expect("queued job");

    let swept = dam_pipeline::abandoned::sweep_tenant(
        &global,
        &mut tenant.acquire().await.expect("conn"),
        &store,
        tenant_id,
    )
    .await
    .expect("sweep");

    // The enqueue is a no-op against the live dedupe key, so nothing new appears and the existing job stands.
    assert_eq!(
        queued_finalisations(&global, tenant_id).await,
        1,
        "the one live job stands; the sweep adds nothing: {swept:?}"
    );
}
