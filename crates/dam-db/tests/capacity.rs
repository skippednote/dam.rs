//! Classifying "the deployment is out of capacity" apart from "the request is broken".
//!
//! Found by a load pass: 2056 uploads across four tenants at once, against a sixteen-connection pool. Thirty
//! came back **500**, and every one of them was "pool timed out while waiting for an open connection".
//!
//! A 500 tells a client the request is broken and must not be repeated, so a bulk uploader abandons the file.
//! The condition clears itself as connections return, so the useful answer is "try again shortly" — which is
//! the same distinction `Failure::Throttled` already draws between "fix something" and "retry".
//!
//! The classification lives on the error rather than at each HTTP surface because there are two of those, and
//! they had already diverged: the TUS handler has its own `Refusal` whose `From<dam_db::Error>` mapped
//! everything to `Internal`, so fixing the asset endpoints alone would have left uploads — the path that
//! actually saturates — returning 500 for a retryable condition.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use dam_db::Error;

#[test]
fn a_pool_timeout_is_capacity_and_a_broken_query_is_not() {
    assert!(
        Error::Sqlx(sqlx::Error::PoolTimedOut).is_capacity(),
        "a pool with nothing free is the deployment's limit, not the caller's mistake"
    );

    // Everything else stays a fault. Being generous here would turn a genuine bug into a 503 that clients
    // retry forever, which is worse than the 500 it replaced: a real error nobody is told about.
    assert!(!Error::Sqlx(sqlx::Error::RowNotFound).is_capacity());
    assert!(!Error::Migrate("bad migration".into()).is_capacity());
    assert!(!Error::TenantNotProvisioned("t_gone".into()).is_capacity());
    assert!(!Error::Inconsistent("a part count that disagrees".into()).is_capacity());
    assert!(!Error::Unsupported("cross-pool move".into()).is_capacity());
}

#[tokio::test]
async fn a_pool_with_no_free_connection_produces_that_error_rather_than_hanging() {
    // The condition itself, reproduced rather than asserted about: a one-connection pool with a very short
    // acquire timeout, and the single connection held. This is what saturation looks like from the inside, and
    // it is the error the classification has to recognise — the variant name is not enough on its own, because
    // sqlx could report saturation as something else and the test would still pass.
    let pg = dam_db::testing::PostgresHarness::start()
        .await
        .expect("start postgres");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(250))
        .connect(&pg.url())
        .await
        .expect("pool");

    let held = pool.acquire().await.expect("the only connection");
    let refused = sqlx::query("SELECT 1").execute(&pool).await;
    drop(held);

    let error =
        Error::from(refused.expect_err("a saturated pool has to refuse rather than wait forever"));
    assert!(
        error.is_capacity(),
        "saturation must classify as capacity, or every surface reports it as a server fault: {error}"
    );
}
