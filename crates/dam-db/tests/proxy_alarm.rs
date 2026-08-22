//! The §2 alarm: enrichment must read the proxy, and any exception must explain itself.
//!
//! §2 is what makes cold storage viable — originals tier, the proxy does not, and a model upgrade
//! re-embeds the library off proxies with zero restores. The failure mode is silent: nothing breaks
//! the day a stage starts reading originals, and the bill arrives at the next upgrade as a restore
//! storm. `used_original` is the alarm; these tests are what make it more than a column.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_db::{migrate, testing::PostgresHarness};
use sqlx::{Executor, PgPool, Row};

async fn tenant_db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn refused_by_constraint(pool: &PgPool, sql: &str) -> bool {
    match pool.execute(sqlx::AssertSqlSafe(sql.to_owned())).await {
        Ok(_) => false,
        Err(sqlx::Error::Database(db)) => {
            let code = db.code().unwrap_or_default().to_string();
            assert!(
                code.starts_with("23") || code == "P0001",
                "failed, but not because a constraint refused it (SQLSTATE {code}: {db})\n{sql}"
            );
            true
        }
        Err(e) => panic!("unexpected non-database error:\n{sql}\n{e}"),
    }
}

/// An asset to hang enrichment runs off.
async fn asset(pool: &PgPool, name: &str) -> String {
    let id: uuid::Uuid = sqlx::query_scalar(
        // version_group_id is NOT NULL: versioning uses a group id rather than a
        // self-referential chain, so "the current version" is an index lookup rather than a walk.
        "INSERT INTO assets (id, version_group_id, content_hash, filename, mime, bytes) \
         VALUES (gen_random_uuid(), gen_random_uuid(), repeat('a', 64), $1, 'image/jpeg', 1000) \
         RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert asset");
    id.to_string()
}

#[tokio::test]
async fn a_run_reading_the_proxy_needs_no_explanation_and_raises_no_alarm() {
    let (_pg, pool) = tenant_db().await;
    let asset_id = asset(&pool, "photo.jpg").await;

    pool.execute(sqlx::AssertSqlSafe(format!(
        "INSERT INTO enrichment_runs (id, asset_id, pipeline) \
         VALUES (gen_random_uuid(), '{asset_id}', 'image-v1')"
    )))
    .await
    .expect("the normal path must be the easy one");

    let row = sqlx::query("SELECT used_original, original_read_reason FROM enrichment_runs")
        .fetch_one(&pool)
        .await
        .expect("row");
    assert!(
        !row.get::<bool, _>("used_original"),
        "the default must be the correct behaviour, not the exception"
    );
    assert!(
        row.get::<Option<String>, _>("original_read_reason")
            .is_none()
    );

    let alarms: i64 = sqlx::query_scalar("SELECT count(*) FROM enrichment_original_reads")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(alarms, 0);
}

#[tokio::test]
async fn a_run_that_read_the_original_must_say_why() {
    // A boolean says the design is broken. A boolean with a reason says which stage broke it —
    // and by the time anyone looks, the change that caused it is months old.
    let (_pg, pool) = tenant_db().await;
    let asset_id = asset(&pool, "photo.jpg").await;

    assert!(
        refused_by_constraint(
            &pool,
            &format!(
                "INSERT INTO enrichment_runs (id, asset_id, pipeline, used_original) \
                 VALUES (gen_random_uuid(), '{asset_id}', 'image-v1', true)"
            )
        )
        .await,
        "an unexplained original read must be refused"
    );

    pool.execute(sqlx::AssertSqlSafe(format!(
        "INSERT INTO enrichment_runs (id, asset_id, pipeline, used_original, original_read_reason) \
         VALUES (gen_random_uuid(), '{asset_id}', 'c2pa-verify', true, \
                 'c2pa verification attests to the master bytes at ingest')"
    )))
    .await
    .expect("an explained read is allowed");
}

#[tokio::test]
async fn the_alarm_view_surfaces_every_original_read_with_enough_context_to_triage_it() {
    let (_pg, pool) = tenant_db().await;
    let small = asset(&pool, "icon.png").await;
    let big = asset(&pool, "master.tif").await;

    pool.execute(sqlx::AssertSqlSafe(format!(
        "INSERT INTO enrichment_runs (id, asset_id, pipeline, used_original, original_read_reason) \
         VALUES (gen_random_uuid(), '{big}', 'embed-v2', true, 'stage read the master directly'), \
                (gen_random_uuid(), '{small}', 'embed-v2', false, NULL)"
    )))
    .await
    .expect("insert");

    let rows = sqlx::query(
        "SELECT pipeline, original_read_reason, filename, asset_bytes \
         FROM enrichment_original_reads",
    )
    .fetch_all(&pool)
    .await
    .expect("query the view");

    assert_eq!(rows.len(), 1, "only the offending run is listed");
    assert_eq!(rows[0].get::<String, _>("pipeline"), "embed-v2");
    assert_eq!(
        rows[0].get::<String, _>("filename"),
        "master.tif",
        "the view joins the asset, because 'which files' is the first question asked"
    );
    assert!(
        rows[0].get::<i64, _>("asset_bytes") > 0,
        "and the size, because that is what turns a count into a restore-cost estimate"
    );
}

#[tokio::test]
async fn the_alarm_is_indexed_so_it_can_be_polled_cheaply() {
    // The alert runs forever on a table that grows with every enrichment of every asset. A
    // sequential scan is fine for a month and then is not.
    let (_pg, pool) = tenant_db().await;
    let indexed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_indexes WHERE schemaname='t_acme' \
         AND tablename='enrichment_runs' AND indexdef ILIKE '%used_original%'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert!(indexed >= 1, "used_original needs a partial index");
}
