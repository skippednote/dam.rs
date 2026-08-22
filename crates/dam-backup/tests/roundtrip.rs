//! A backup that has actually been restored (§17, G11).
//!
//! The whole point of this crate is that `last_verified_restore_at` must be earned, so the suite earns it: a
//! real `pg_dump` of a real schema, uploaded to a store, downloaded, replayed into a live database, and
//! counted. Nothing here asserts that a function returned `Ok` without looking at what it did.
//!
//! The cases worth having are the ones about *not* being fooled:
//!
//! - A backup does not move `last_verified_restore_at`. That column is the difference between "we take
//!   backups" and "we have restored one", and §17 says the gap between those is where DR plans fail.
//! - A drill whose restore comes back with the wrong number of assets **fails**. `pg_restore` exiting zero
//!   proves the file parsed, not that the data is there.
//! - The live schema survives a drill, including when the drill fails. A verification that can damage the
//!   thing it verifies is one nobody runs on production.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::TenantSlug;
use dam_db::{migrate, testing::PostgresHarness};
use dam_store::{BlobStore, FakeS3Store};
use sqlx::PgPool;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    tenant: PgPool,
    url: String,
    store: FakeS3Store,
    tools: dam_backup::tools::Toolchain,
    slug: TenantSlug,
}

async fn fixture() -> Option<Fixture> {
    // A missing client tool is a *skip with a reason on stdout*, not a silent pass. The AWS nightly in this
    // repo spent months green while running nothing, and the lesson taken from it was that a skip has to be
    // audible. `mise install` provides pg_dump; CI does the same.
    let tools = match dam_backup::tools::Toolchain::discover() {
        Ok(tools) => tools,
        Err(error) => {
            println!("SKIPPING the backup round-trip: {error}");
            return None;
        }
    };

    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    sqlx::query(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active')",
    )
    .execute(&global)
    .await
    .expect("tenant row");

    Some(Fixture {
        _pg: pg,
        global,
        tenant,
        url,
        store: FakeS3Store::with_test_clock().0,
        tools,
        slug: TenantSlug::new("acme").expect("slug"),
    })
}

async fn asset(f: &Fixture, label: &str) {
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES (gen_random_uuid(), $1, $2, 'image/jpeg', 10, gen_random_uuid())",
    )
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(&f.tenant)
    .await
    .expect("asset");
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

#[tokio::test]
async fn a_backup_can_be_restored_and_the_drill_proves_it() {
    let Some(f) = fixture().await else { return };
    println!("pg_dump {:?}", f.tools.version());

    for n in 0..7 {
        asset(&f, &format!("plate-{n}")).await;
    }

    let backup = dam_backup::backup_tenant(&f.global, &f.store, &f.tools, &f.url, &f.slug, now())
        .await
        .expect("backup");

    assert_eq!(backup.asset_count, 7);
    assert!(backup.bytes > 0, "a dump of a migrated schema is not empty");
    assert!(
        f.store
            .head(&dam_store::Key::new(backup.key.clone()).unwrap())
            .await
            .is_ok(),
        "the dump must actually be in the store, not merely reported",
    );

    // A backup moves `last_backup_at` and must NOT move the verification column. This is the distinction the
    // whole table exists to make.
    let (backed_up, verified): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT last_backup_at, last_verified_restore_at FROM dam_global.dr_state \
             WHERE tenant_id = (SELECT id FROM dam_global.tenants WHERE slug = 'acme')",
    )
    .fetch_one(&f.global)
    .await
    .expect("dr_state");
    assert!(backed_up.is_some(), "the backup was recorded");
    assert!(
        verified.is_none(),
        "taking a backup must never claim a restore was verified — that is the gap §17 is about",
    );

    // The drill: a real restore of that dump, into a scratch schema, counted.
    let drill = dam_backup::restore_drill(&f.global, &f.store, &f.tools, &f.url, &f.slug, now())
        .await
        .expect("drill");
    assert_eq!(drill.restored_assets, 7);
    assert_eq!(drill.expected_assets, 7);

    let verified: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT last_verified_restore_at FROM dam_global.dr_state \
         WHERE tenant_id = (SELECT id FROM dam_global.tenants WHERE slug = 'acme')",
    )
    .fetch_one(&f.global)
    .await
    .expect("dr_state");
    assert!(
        verified.is_some(),
        "and *this* is what earns the column: a restore that happened",
    );

    // The live schema is intact and still the live one. A drill that leaves the tenant renamed or missing is
    // worse than no drill.
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM assets")
        .fetch_one(&f.tenant)
        .await
        .expect("the live schema still answers");
    assert_eq!(live, 7);
    let leftovers: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.schemata \
         WHERE schema_name IN ('drill_acme', 'live_acme')",
    )
    .fetch_one(&f.global)
    .await
    .expect("schemata");
    assert_eq!(leftovers, 0, "the drill cleans up after itself");
}

/// `pg_restore` exiting zero proves the file parsed. The drill has to prove the data arrived.
#[tokio::test]
async fn a_drill_fails_when_the_restore_does_not_match_the_backup() {
    let Some(f) = fixture().await else { return };

    for n in 0..3 {
        asset(&f, &format!("original-{n}")).await;
    }
    let backup = dam_backup::backup_tenant(&f.global, &f.store, &f.tools, &f.url, &f.slug, now())
        .await
        .expect("backup");
    assert_eq!(backup.asset_count, 3);

    // Rewrite the recorded expectation upwards by re-uploading the same dump under a key claiming more
    // assets. That is precisely the situation a silent partial restore would produce, and the drill must
    // refuse it rather than reporting a success.
    let body = match f
        .store
        .get(&dam_store::Key::new(backup.key.clone()).unwrap(), None)
        .await
        .expect("read the dump")
    {
        dam_store::GetOutcome::Bytes(bytes) => bytes,
        dam_store::GetOutcome::NotAvailable(_) => panic!("the fake store does not archive"),
    };
    let lying = backup.key.replace("-3.dump", "-99.dump");
    f.store
        .put(
            &dam_store::Key::new(lying.clone()).unwrap(),
            body,
            dam_core::StorageClass::Standard,
        )
        .await
        .expect("put");

    let outcome =
        dam_backup::restore_drill(&f.global, &f.store, &f.tools, &f.url, &f.slug, now()).await;
    let error = outcome.expect_err("a mismatched restore must fail the drill");
    assert!(
        error.to_string().contains("restored 3 assets"),
        "and say what it found against what it expected: {error}"
    );

    // Nothing was verified, and the tenant is untouched.
    let verified: Option<Option<chrono::DateTime<chrono::Utc>>> = sqlx::query_scalar(
        "SELECT last_verified_restore_at FROM dam_global.dr_state \
         WHERE tenant_id = (SELECT id FROM dam_global.tenants WHERE slug = 'acme')",
    )
    .fetch_optional(&f.global)
    .await
    .expect("dr_state");
    assert!(
        verified.flatten().is_none(),
        "a failed drill must not record a verification",
    );
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM assets")
        .fetch_one(&f.tenant)
        .await
        .expect("the live schema survived a failed drill");
    assert_eq!(live, 3);
}

/// The report leads with what has never been verified, including tenants with no row at all.
#[tokio::test]
async fn the_report_puts_the_unverified_first() {
    let Some(f) = fixture().await else { return };
    sqlx::query(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'never', 't_never', 'Never', 'never/', 'active')",
    )
    .execute(&f.global)
    .await
    .expect("second tenant");

    asset(&f, "one").await;
    dam_backup::backup_tenant(&f.global, &f.store, &f.tools, &f.url, &f.slug, now())
        .await
        .expect("backup");
    dam_backup::restore_drill(&f.global, &f.store, &f.tools, &f.url, &f.slug, now())
        .await
        .expect("drill");

    let rows = dam_backup::state::report(&f.global).await.expect("report");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].slug, "never",
        "a tenant with no dr_state row has never been backed up, which is the most urgent line and the \
         easiest to omit by joining the wrong way round",
    );
    assert!(rows[0].last_backup_at.is_none());
    assert_eq!(rows[1].slug, "acme");
    assert!(rows[1].last_verified_restore_at.is_some());
    assert!(
        rows[1].verified_restore_duration_s.is_some(),
        "measured, not configured"
    );
}
