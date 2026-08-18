//! The job queue (D6: global table, not per tenant).
//!
//! Four properties matter, and each has a failure mode that only shows up under
//! load — which is why they are tested against a real Postgres rather than reasoned
//! about:
//!
//! 1. **No double-claim.** Two workers must never run the same job.
//! 2. **Leases, not locks.** A worker that dies must have its work reclaimed with no
//!    reaper process running.
//! 3. **Fairness.** One tenant with a large backlog must not starve the others.
//! 4. **Dedupe.** Enqueueing the same logical work twice while it is pending is a
//!    no-op, not two jobs.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::TenantSlug;
use dam_db::{
    jobs::{self, ClaimOptions, JobSpec},
    migrate, provision,
    testing::PostgresHarness,
};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// The storage pool a provisioned tenant gets.
///
/// Every field is a placeholder: these tests do not write objects, and provisioning only records where objects
/// *would* go. `credentials_ref` is a reference by design — a credential in this column would be a credential
/// in every backup.
fn test_pool() -> dam_db::provision::StoragePool<'static> {
    dam_db::provision::StoragePool {
        endpoint: Some("http://127.0.0.1:1"),
        region: "us-east-1",
        bucket: "damrs-test",
        force_path_style: true,
        credentials_ref: "test",
    }
}

async fn queue_db(slugs: &[&str]) -> (PostgresHarness, PgPool, Vec<Uuid>) {
    let pg = PostgresHarness::start().await.expect("start");
    migrate::global(&pg.url()).await.expect("global");
    let pool = pg.pool().clone();
    let mut ids = Vec::new();
    for s in slugs {
        let slug = TenantSlug::new(s).expect("slug");
        let t = provision::tenant(&pool, &pg.url(), &slug, s, &test_pool())
            .await
            .expect("provision");
        ids.push(t.id);
    }
    (pg, pool, ids)
}

fn spec(tenant: Uuid, kind: &str) -> JobSpec {
    JobSpec::new(tenant, kind)
}

#[tokio::test]
async fn a_claimed_job_comes_back_with_its_payload() {
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    let id = jobs::enqueue(
        &pool,
        spec(t[0], "derive_thumbnail").payload(serde_json::json!({"asset_id": "abc"})),
    )
    .await
    .expect("enqueue");

    let claimed = jobs::claim(&pool, "worker-1", ClaimOptions::default())
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, id);
    assert_eq!(claimed[0].kind, "derive_thumbnail");
    assert_eq!(claimed[0].payload["asset_id"], "abc");
    assert_eq!(claimed[0].attempts, 1, "claiming counts as an attempt");
}

#[tokio::test]
async fn concurrent_workers_never_claim_the_same_job() {
    // The property that matters most. Ten workers, fifty jobs, no job run twice and
    // none lost.
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    for i in 0..50 {
        jobs::enqueue(&pool, spec(t[0], "work").dedupe_key(format!("k{i}")))
            .await
            .expect("enqueue");
    }

    let mut tasks = Vec::new();
    for w in 0..10 {
        let p = pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            loop {
                let batch = jobs::claim(&p, &format!("worker-{w}"), ClaimOptions::default())
                    .await
                    .expect("claim");
                if batch.is_empty() {
                    break;
                }
                mine.extend(batch.into_iter().map(|j| j.id));
            }
            mine
        }));
    }

    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.expect("task"));
    }
    let unique: std::collections::HashSet<_> = all.iter().copied().collect();
    assert_eq!(all.len(), unique.len(), "a job was claimed twice");
    assert_eq!(unique.len(), 50, "jobs were lost: got {}", unique.len());
}

#[tokio::test]
async fn a_dead_workers_lease_is_reclaimed_without_a_reaper() {
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    let id = jobs::enqueue(&pool, spec(t[0], "work"))
        .await
        .expect("enqueue");

    let claimed = jobs::claim(&pool, "doomed", ClaimOptions::default())
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);

    // Nothing else can claim it while the lease holds.
    assert!(
        jobs::claim(&pool, "other", ClaimOptions::default())
            .await
            .expect("claim")
            .is_empty(),
        "a live lease must not be stealable"
    );

    // Simulate the worker dying: expire the lease directly rather than sleeping.
    sqlx::query(
        "UPDATE dam_global.jobs SET lease_expires_at = now() - interval '1 minute' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .expect("expire lease");

    // Reclaim happens inside claim(), so no separate reaper process is required.
    let reclaimed = jobs::claim(&pool, "worker-2", ClaimOptions::default())
        .await
        .expect("claim");
    assert_eq!(reclaimed.len(), 1, "an expired lease must be reclaimable");
    assert_eq!(reclaimed[0].id, id);
    assert_eq!(
        reclaimed[0].attempts, 2,
        "the retry counts as a second attempt"
    );
}

#[tokio::test]
async fn one_tenants_backlog_does_not_starve_another() {
    // The fairness property. Without it, a tenant that bulk-imports 100k assets
    // stalls every other tenant's thumbnails behind their backlog.
    let (_pg, pool, t) = queue_db(&["flood", "quiet"]).await;
    for i in 0..200 {
        jobs::enqueue(&pool, spec(t[0], "work").dedupe_key(format!("f{i}")))
            .await
            .expect("enqueue flood");
    }
    // The quiet tenant enqueues last, so a strictly FIFO queue would put it behind
    // all 200.
    jobs::enqueue(&pool, spec(t[1], "work").dedupe_key("q0"))
        .await
        .expect("enqueue quiet");

    let batch = jobs::claim(
        &pool,
        "worker-1",
        ClaimOptions {
            limit: 10,
            ..Default::default()
        },
    )
    .await
    .expect("claim");

    assert_eq!(
        batch.len(),
        10,
        "rank ordering must still fill the batch — fairness should not cost throughput"
    );
    assert!(
        batch.iter().any(|j| j.tenant_id == t[1]),
        "the quiet tenant's single job must appear in the first batch of 10, \
         not behind 200 from the flooding tenant"
    );
}

#[tokio::test]
async fn a_single_tenant_cannot_take_the_whole_batch_when_others_are_waiting() {
    let (_pg, pool, t) = queue_db(&["aa", "bb", "cc"]).await;
    for (n, tenant) in t.iter().enumerate() {
        for i in 0..20 {
            jobs::enqueue(&pool, spec(*tenant, "work").dedupe_key(format!("t{n}-{i}")))
                .await
                .expect("enqueue");
        }
    }
    let batch = jobs::claim(
        &pool,
        "w",
        ClaimOptions {
            limit: 9,
            ..Default::default()
        },
    )
    .await
    .expect("claim");

    let mut per_tenant = std::collections::HashMap::new();
    for j in &batch {
        *per_tenant.entry(j.tenant_id).or_insert(0usize) += 1;
    }
    assert_eq!(
        per_tenant.len(),
        3,
        "all three tenants should be represented"
    );
    for (tenant, n) in per_tenant {
        assert_eq!(
            n, 3,
            "tenant {tenant} took {n} of 9; round-robin over 3 tenants should give 3 each"
        );
    }
}

#[tokio::test]
async fn dedupe_prevents_a_duplicate_while_one_is_pending() {
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    let a = jobs::enqueue(&pool, spec(t[0], "derive").dedupe_key("asset-1"))
        .await
        .expect("first");
    let b = jobs::enqueue(&pool, spec(t[0], "derive").dedupe_key("asset-1"))
        .await
        .expect("second must be a no-op, not an error");
    assert_eq!(a, b, "the existing job's id must be returned");

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM dam_global.jobs")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 1);
}

#[tokio::test]
async fn dedupe_allows_a_new_job_once_the_previous_one_finished() {
    // The partial unique index covers queued+running only. Re-deriving a thumbnail
    // after the first derive completed is legitimate work, not a duplicate.
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    let a = jobs::enqueue(&pool, spec(t[0], "derive").dedupe_key("asset-1"))
        .await
        .expect("first");
    let claimed = jobs::claim(&pool, "w", ClaimOptions::default())
        .await
        .expect("claim");
    jobs::complete(&pool, claimed[0].id)
        .await
        .expect("complete");

    let b = jobs::enqueue(&pool, spec(t[0], "derive").dedupe_key("asset-1"))
        .await
        .expect("second after completion");
    assert_ne!(a, b, "a new job should be created once the first finished");
}

#[tokio::test]
async fn a_failure_backs_off_and_eventually_goes_dead() {
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    jobs::enqueue(&pool, spec(t[0], "flaky").max_attempts(3))
        .await
        .expect("enqueue");

    for attempt in 1..=3 {
        // Clear any backoff so the test does not have to wait for it.
        sqlx::query("UPDATE dam_global.jobs SET run_after = now() - interval '1 hour'")
            .execute(&pool)
            .await
            .expect("clear backoff");

        let batch = jobs::claim(&pool, "w", ClaimOptions::default())
            .await
            .expect("claim");
        assert_eq!(batch.len(), 1, "attempt {attempt} should be claimable");
        jobs::fail(&pool, batch[0].id, "boom").await.expect("fail");
    }

    let (state, last_error): (String, Option<String>) =
        sqlx::query_as("SELECT state, last_error FROM dam_global.jobs")
            .fetch_one(&pool)
            .await
            .expect("read state");
    assert_eq!(state, "dead", "a job past max_attempts must go dead");
    assert_eq!(last_error.as_deref(), Some("boom"));

    assert!(
        jobs::claim(&pool, "w", ClaimOptions::default())
            .await
            .expect("claim")
            .is_empty(),
        "a dead job must not be claimable"
    );
}

#[tokio::test]
async fn backoff_delays_the_retry() {
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    jobs::enqueue(&pool, spec(t[0], "flaky"))
        .await
        .expect("enqueue");
    let batch = jobs::claim(&pool, "w", ClaimOptions::default())
        .await
        .expect("claim");
    jobs::fail(&pool, batch[0].id, "boom").await.expect("fail");

    assert!(
        jobs::claim(&pool, "w", ClaimOptions::default())
            .await
            .expect("claim")
            .is_empty(),
        "the retry must be delayed, not immediate"
    );

    let due_in: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (run_after - now()))::double precision \
         FROM dam_global.jobs",
    )
    .fetch_one(&pool)
    .await
    .expect("read run_after");
    assert!(
        due_in > 0.0,
        "run_after should be in the future, got {due_in}"
    );
}

#[tokio::test]
async fn a_scheduled_job_is_not_claimable_before_its_time() {
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    jobs::enqueue(
        &pool,
        spec(t[0], "nightly").run_after(chrono::Utc::now() + chrono::Duration::hours(1)),
    )
    .await
    .expect("enqueue");
    assert!(
        jobs::claim(&pool, "w", ClaimOptions::default())
            .await
            .expect("claim")
            .is_empty()
    );
}

#[tokio::test]
async fn priority_orders_within_a_tenant() {
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    jobs::enqueue(&pool, spec(t[0], "low").dedupe_key("l").priority(200))
        .await
        .expect("low");
    jobs::enqueue(&pool, spec(t[0], "high").dedupe_key("h").priority(10))
        .await
        .expect("high");

    let batch = jobs::claim(
        &pool,
        "w",
        ClaimOptions {
            limit: 1,
            ..Default::default()
        },
    )
    .await
    .expect("claim");
    assert_eq!(batch[0].kind, "high", "lower priority number runs first");
}

#[tokio::test]
async fn deleting_a_tenant_removes_its_jobs() {
    // `jobs.tenant_id` cascades. A deprovisioned tenant must not leave work behind
    // that a worker then tries to run against a dropped schema.
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    jobs::enqueue(&pool, spec(t[0], "work"))
        .await
        .expect("enqueue");
    sqlx::query("DELETE FROM dam_global.tenants WHERE id = $1")
        .bind(t[0])
        .execute(&pool)
        .await
        .expect("delete tenant");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM dam_global.jobs")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 0);
}

#[tokio::test]
async fn a_lease_can_be_extended_by_the_worker_holding_it() {
    // Long jobs — a 200 GB transcode — must be able to renew rather than be
    // reclaimed mid-flight.
    let (_pg, pool, t) = queue_db(&["acme"]).await;
    jobs::enqueue(&pool, spec(t[0], "transcode"))
        .await
        .expect("enqueue");
    let batch = jobs::claim(&pool, "w1", ClaimOptions::default())
        .await
        .expect("claim");
    let id = batch[0].id;

    jobs::heartbeat(&pool, id, "w1", Duration::from_secs(600))
        .await
        .expect("heartbeat");

    // A different worker must not be able to extend someone else's lease.
    let stolen = jobs::heartbeat(&pool, id, "w2", Duration::from_secs(600)).await;
    assert!(
        stolen.is_err(),
        "a lease must only be extendable by its holder"
    );
}
