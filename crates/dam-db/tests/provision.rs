//! Tenant provisioning: row, schema, migrations, seeded defaults.
//!
//! The seeded state is itself a guarantee, not a convenience. `face_identify` must
//! arrive **off** (D14) — a tenant that had to remember to disable biometric
//! identification would be processing Article 9 data by default, which is the wrong
//! way round. Asserting the default here means a future change to the seed data
//! fails the build.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::TenantSlug;
use dam_db::{migrate, provision, testing::PostgresHarness};
use sqlx::PgPool;

async fn ready() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start");
    migrate::global(&pg.url()).await.expect("global");
    let pool = pg.pool().clone();
    (pg, pool)
}

fn slug(s: &str) -> TenantSlug {
    TenantSlug::new(s).expect("valid slug")
}

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

#[tokio::test]
async fn provisioning_creates_the_row_the_schema_and_the_migrations() {
    let (pg, pool) = ready().await;
    let t = provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme Corp", &test_pool())
        .await
        .expect("provision");

    assert_eq!(t.slug.as_str(), "acme");
    assert_eq!(t.schema_name, "t_acme");

    let row: (String, String, String) =
        sqlx::query_as("SELECT slug, schema_name, status FROM dam_global.tenants WHERE id = $1")
            .bind(t.id)
            .fetch_one(&pool)
            .await
            .expect("tenant row");
    assert_eq!(row, ("acme".into(), "t_acme".into(), "active".into()));

    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables \
         WHERE table_schema = 't_acme' AND table_type = 'BASE TABLE' \
           AND table_name <> '_sqlx_migrations'",
    )
    .fetch_one(&pool)
    .await
    .expect("count tables");
    // 61 since migration 0015 added `metadata_types` and `metadata_type_fields`; 62 since 0017 added
    // `upload_profiles`; 63 since 0018 added `auto_import_mappings`; 66 since 0019 added ratings,
    // favourites and watches; 68 since 0020 added comments and their routing; 73 since 0027 added a tenant's sealed provider credentials; 72 since 0025 added orders and their items; 70 since 0023 added `conversions`; 69 since 0021 added the
    // events default partition; 74 since 0028 added `enrichment_settings`; 75 since 0030 added `portals`;
    // 76 since 0032 added `search_facets`; 77 since 0036 added `site_branding`; 80 since 0038 added
    // `proof_rounds` and its two lists.
    //
    // The same number as `migrate.rs` asserts, and deliberately duplicated: that suite proves a migration run
    // produces the schema, and this one proves *provisioning* produces the same schema. A tenant created
    // through `provision` and one migrated by hand diverging is the failure this pair exists to catch, so the
    // two counts agreeing is the assertion — sharing a constant would only prove they were the same constant.
    assert_eq!(tables, 80);
}

/// D14. The whole point of the flag is that it starts off.
#[tokio::test]
async fn face_identify_is_seeded_off_and_requires_a_dpia() {
    let (pg, pool) = ready().await;
    let t = provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme", &test_pool())
        .await
        .expect("provision");

    let (enabled, requires_dpia): (bool, bool) = sqlx::query_as(
        "SELECT enabled, requires_dpia FROM dam_global.feature_flags \
         WHERE tenant_id = $1 AND key = 'face_identify'",
    )
    .bind(t.id)
    .fetch_one(&pool)
    .await
    .expect("flag row");

    assert!(!enabled, "face_identify must be seeded OFF (D14)");
    assert!(requires_dpia, "face_identify must be DPIA-gated");
}

#[tokio::test]
async fn the_seeded_flags_cannot_be_flipped_on_without_a_dpia() {
    // Belt and braces: the seed sets requires_dpia, and the CHECK then makes the
    // flag unenableable without a reference and a legal basis.
    let (pg, pool) = ready().await;
    let t = provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme", &test_pool())
        .await
        .expect("provision");

    let err = sqlx::query(
        "UPDATE dam_global.feature_flags SET enabled = true \
         WHERE tenant_id = $1 AND key = 'face_identify'",
    )
    .bind(t.id)
    .execute(&pool)
    .await
    .expect_err("must be refused without a DPIA reference");
    assert!(err.to_string().contains("dpia"), "{err}");
}

#[tokio::test]
async fn defaults_are_seeded_into_the_tenant_schema() {
    let (pg, pool) = ready().await;
    provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme", &test_pool())
        .await
        .expect("provision");

    let counts: Vec<(String, i64)> = vec![
        (
            "field_defs".into(),
            sqlx::query_scalar("SELECT count(*) FROM t_acme.field_defs")
                .fetch_one(&pool)
                .await
                .expect("field_defs"),
        ),
        (
            "asset_groups".into(),
            sqlx::query_scalar("SELECT count(*) FROM t_acme.asset_groups")
                .fetch_one(&pool)
                .await
                .expect("asset_groups"),
        ),
        (
            "roles".into(),
            sqlx::query_scalar("SELECT count(*) FROM t_acme.roles")
                .fetch_one(&pool)
                .await
                .expect("roles"),
        ),
    ];
    for (what, n) in counts {
        assert!(n > 0, "{what} should have seeded rows, got {n}");
    }

    // The default group must exist and be the one marked default, because the ABAC

    // compiler (0.10) resolves an unscoped grant through it.
    let default_key: String =
        sqlx::query_scalar("SELECT key FROM t_acme.asset_groups WHERE is_default")
            .fetch_one(&pool)
            .await
            .expect("exactly one default group");
    assert_eq!(default_key, "everyone");
}

#[tokio::test]
async fn provisioning_creates_the_tenant_a_storage_pool() {
    // A tenant without one cannot ingest: finalisation records a placement and refuses without a pool. That
    // was found the hard way — the first real upload through the pipeline failed with "no instant storage pool
    // is configured" and went straight to `dead`, correctly, on a tenant `provision-tenant` had just created.
    let (pg, pool) = ready().await;
    let t = provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme", &test_pool())
        .await
        .expect("provision");

    let row: (String, String, String, bool, String, String) = sqlx::query_as(
        "SELECT name, driver, bucket, force_path_style, credentials_ref, latency_class \
         FROM dam_global.storage_pools WHERE tenant_id = $1",
    )
    .bind(t.id)
    .fetch_one(&pool)
    .await
    .expect("the tenant's pool");

    assert_eq!(row.0, "hot");
    assert_eq!(row.1, "s3");
    assert_eq!(row.2, "damrs-test");
    assert!(row.3, "path style, which every non-AWS endpoint needs");
    assert_eq!(
        row.4, "test",
        "a reference, never the credential: a secret here would be a secret in every backup"
    );
    assert_eq!(
        row.5, "instant",
        "the pool ingest looks for is the instant one — `finalise::default_pool` selects on this"
    );
}

#[tokio::test]
async fn provisioning_is_idempotent() {
    // A retried provisioning run — a crashed CLI, a re-run CI job — must not create a
    // second tenant or duplicate the seed data.
    let (pg, pool) = ready().await;
    let a = provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme", &test_pool())
        .await
        .expect("first");
    let b = provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme", &test_pool())
        .await
        .expect("second must succeed");
    assert_eq!(a.id, b.id, "the same tenant must be returned");

    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM dam_global.tenants")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(tenants, 1);

    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM t_acme.asset_groups")
        .fetch_one(&pool)
        .await
        .expect("count groups");
    assert_eq!(groups, 1, "seed data must not be duplicated");
}

#[tokio::test]
async fn two_tenants_are_fully_isolated() {
    let (pg, pool) = ready().await;
    provision::tenant(&pool, &pg.url(), &slug("one"), "One", &test_pool())
        .await
        .expect("one");
    provision::tenant(&pool, &pg.url(), &slug("two"), "Two", &test_pool())
        .await
        .expect("two");

    sqlx::query(
        "INSERT INTO t_one.collections (id, key, label) \
         VALUES (gen_random_uuid(), 'campaign', 'Campaign')",
    )
    .execute(&pool)
    .await
    .expect("insert into one");

    let in_two: i64 = sqlx::query_scalar("SELECT count(*) FROM t_two.collections")
        .fetch_one(&pool)
        .await
        .expect("count two");
    assert_eq!(in_two, 0);
}

#[tokio::test]
async fn a_failed_provisioning_does_not_leave_a_half_built_tenant() {
    // Provisioning spans a control-plane row, a schema, migrations, and seed data.
    // A tenant row pointing at a schema that does not exist is worse than no tenant
    // at all: every request for it fails deep in the stack rather than at lookup.
    let (pg, pool) = ready().await;

    // Occupy the schema name with something the migrations will collide on.
    sqlx::query("CREATE SCHEMA t_acme")
        .execute(&pool)
        .await
        .expect("create schema");
    sqlx::query("CREATE TABLE t_acme.assets (wrong_shape int)")
        .execute(&pool)
        .await
        .expect("create conflicting table");

    let result = provision::tenant(&pool, &pg.url(), &slug("acme"), "Acme", &test_pool()).await;
    assert!(result.is_err(), "provisioning should have failed");

    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM dam_global.tenants")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        tenants, 0,
        "a failed provisioning must not leave a tenant row behind"
    );
}
