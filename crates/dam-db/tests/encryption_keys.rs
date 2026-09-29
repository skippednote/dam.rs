//! The per-tenant key table, which until G10·3b nothing read or wrote.
//!
//! The properties worth pinning are not "a row round-trips" — they are the ones a deployment's encryption
//! posture actually rests on: that resolution falls back rather than refusing, that only one key is active,
//! and that a superseded key is kept rather than deleted.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::encryption_keys::{self, NewKey, Provider, Purpose, State};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    migrate::global(&pg.url()).await.expect("global");
    let pool = pg.pool().clone();
    (pg, pool)
}

async fn tenant(pool: &PgPool, slug: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), $1, $2, $1, $1, 'active') RETURNING id",
    )
    .bind(slug)
    .bind(format!("t_{slug}"))
    .fetch_one(pool)
    .await
    .expect("tenant")
}

#[tokio::test]
async fn the_key_table_invariants_hold() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let acme = tenant(&pool, "acme").await;
    let globex = tenant(&pool, "globex").await;

    a_tenant_with_no_key_resolves_to_nothing(&mut conn, acme).await;
    activating_makes_it_the_one_that_resolves(&mut conn, acme).await;
    a_second_key_retires_the_first_rather_than_deleting_it(&mut conn, acme).await;
    purposes_do_not_resolve_each_other(&mut conn, acme).await;
    tenants_do_not_resolve_each_other(&mut conn, acme, globex).await;
    a_revoked_key_stops_resolving_but_stays_on_the_record(&mut conn, globex).await;
    the_write_key_prefers_the_tenants_over_the_pools(&mut conn, &pool).await;
    a_deployment_wide_key_retires_its_predecessor(&mut conn).await;
}

/// Rotating a deployment-wide key (NULL tenant) must retire the previous one.
///
/// `WHERE tenant_id = $1` is never true when `$1` is NULL, so an `=` retire clause would leave the old
/// deployment key active beside the new — two active rows the partial unique index does not catch either,
/// because it treats NULLs as distinct. The retire uses `IS NOT DISTINCT FROM`, and this pins it. Nothing
/// creates a NULL-tenant key in the product yet, so without this test the path is entirely unexercised.
async fn a_deployment_wide_key_retires_its_predecessor(conn: &mut sqlx::PgConnection) {
    let first = encryption_keys::activate(
        &mut *conn,
        &NewKey {
            tenant_id: None,
            purpose: Purpose::Blob,
            provider: Provider::AwsKms,
            key_ref: "arn:deployment/first",
            customer_managed: false,
        },
    )
    .await
    .expect("first deployment key");

    let second = encryption_keys::activate(
        &mut *conn,
        &NewKey {
            tenant_id: None,
            purpose: Purpose::Blob,
            provider: Provider::AwsKms,
            key_ref: "arn:deployment/second",
            customer_managed: false,
        },
    )
    .await
    .expect("rotate the deployment key");

    // Only the second is active. The first must have been retired despite its NULL tenant.
    let active_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.encryption_keys \
         WHERE tenant_id IS NULL AND purpose = 'blob' AND state = 'active'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("count");
    assert_eq!(
        active_count, 1,
        "exactly one deployment-wide blob key may be active"
    );

    let first_state: String =
        sqlx::query_scalar("SELECT state FROM dam_global.encryption_keys WHERE id = $1")
            .bind(first.id)
            .fetch_one(&mut *conn)
            .await
            .expect("first state");
    assert_eq!(
        first_state, "retired",
        "the predecessor deployment key must be retired, not left active"
    );
    assert_ne!(first.id, second.id);
}

/// The precedence that makes BYOK mean something: a customer's key beats the bucket's.
///
/// Asserted as an *order* rather than three separate lookups, because the failure that matters is not "the
/// wrong key was returned" — it is a tenant believing their own key is in use while the deployment's is.
async fn the_write_key_prefers_the_tenants_over_the_pools(
    conn: &mut sqlx::PgConnection,
    pool: &PgPool,
) {
    let tenant_id = tenant(pool, "keyed").await;
    let pool_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO dam_global.storage_pools \
         (id, tenant_id, name, bucket, credentials_ref, kms_key_ref) \
         VALUES ($1, $2, 'hot', 'b', 'env', 'arn:pool-key')",
    )
    .bind(pool_id)
    .bind(tenant_id)
    .execute(&mut *conn)
    .await
    .expect("pool");

    // Nothing of the tenant's yet: the pool's key answers.
    let found = encryption_keys::key_for_write(&mut *conn, tenant_id, pool_id)
        .await
        .expect("resolve");
    assert_eq!(found.as_deref(), Some("arn:pool-key"));

    // Now the tenant brings its own, and it takes precedence.
    encryption_keys::activate(
        &mut *conn,
        &NewKey {
            tenant_id: Some(tenant_id),
            purpose: Purpose::Blob,
            provider: Provider::AwsKms,
            key_ref: "arn:customer-key",
            customer_managed: true,
        },
    )
    .await
    .expect("activate");
    let found = encryption_keys::key_for_write(&mut *conn, tenant_id, pool_id)
        .await
        .expect("resolve");
    assert_eq!(
        found.as_deref(),
        Some("arn:customer-key"),
        "a customer-managed key must beat the pool's, or BYOK is a claim rather than a fact"
    );

    // A pool with no key of its own, and a tenant with none either: nothing, so the caller falls back to
    // the deployment's configured key rather than writing unencrypted.
    let bare = tenant(pool, "bare").await;
    let bare_pool = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO dam_global.storage_pools (id, tenant_id, name, bucket, credentials_ref) \
         VALUES ($1, $2, 'hot', 'b', 'env')",
    )
    .bind(bare_pool)
    .bind(bare)
    .execute(&mut *conn)
    .await
    .expect("pool");
    assert!(
        encryption_keys::key_for_write(&mut *conn, bare, bare_pool)
            .await
            .expect("resolve")
            .is_none(),
        "no key anywhere resolves to nothing, not to an error"
    );
}

/// The fallback case, and the reason it is first: it is what every deployment looks like at upgrade.
///
/// Resolution must answer "nothing of your own" rather than failing, or turning this table on would stop
/// every correctly-encrypting deployment from writing.
async fn a_tenant_with_no_key_resolves_to_nothing(conn: &mut sqlx::PgConnection, acme: Uuid) {
    let found = encryption_keys::resolve(conn, acme, Purpose::Blob)
        .await
        .expect("resolve");
    assert!(
        found.is_none(),
        "a tenant with no key must resolve to nothing, so the caller falls back to the deployment's"
    );
}

async fn activating_makes_it_the_one_that_resolves(conn: &mut sqlx::PgConnection, acme: Uuid) {
    let made = encryption_keys::activate(
        conn,
        &NewKey {
            tenant_id: Some(acme),
            purpose: Purpose::Blob,
            provider: Provider::AwsKms,
            key_ref: "arn:aws:kms:eu-west-2:111122223333:key/first",
            customer_managed: true,
        },
    )
    .await
    .expect("activate");
    assert_eq!(made.state, State::Active);

    let found = encryption_keys::resolve(conn, acme, Purpose::Blob)
        .await
        .expect("resolve")
        .expect("a key");
    assert_eq!(found.id, made.id);
    assert_eq!(
        found.key_ref,
        "arn:aws:kms:eu-west-2:111122223333:key/first"
    );
    assert!(found.customer_managed, "BYOK is the whole point of the row");
}

/// Rotation supersedes; it does not erase.
///
/// A retired key has to be retained as long as anything it encrypted still exists, so a rotation that deleted
/// the old row would make already-written objects undecryptable — and it would do it silently, because
/// nothing reads a key it can no longer find.
async fn a_second_key_retires_the_first_rather_than_deleting_it(
    conn: &mut sqlx::PgConnection,
    acme: Uuid,
) {
    let second = encryption_keys::activate(
        conn,
        &NewKey {
            tenant_id: Some(acme),
            purpose: Purpose::Blob,
            provider: Provider::AwsKms,
            key_ref: "arn:aws:kms:eu-west-2:111122223333:key/second",
            customer_managed: true,
        },
    )
    .await
    .expect("rotate");

    let active = encryption_keys::resolve(conn, acme, Purpose::Blob)
        .await
        .expect("resolve")
        .expect("a key");
    assert_eq!(active.id, second.id, "the new key is the write key");

    let all = encryption_keys::for_tenant(conn, acme).await.expect("list");
    let blob: Vec<_> = all.iter().filter(|k| k.purpose == Purpose::Blob).collect();
    assert_eq!(blob.len(), 2, "the superseded key is kept: {blob:?}");
    let retired = blob.iter().find(|k| k.id != second.id).expect("the first");
    assert_eq!(retired.state, State::Retired);
    assert!(
        retired.retired_at.is_some(),
        "a retirement without a time cannot be reasoned about later"
    );
}

/// A blob key is not a signing key.
///
/// Worth a case of its own because the four purposes share a table and an index, and resolving the wrong one
/// would present as "encryption works" while using a key meant for something else entirely.
async fn purposes_do_not_resolve_each_other(conn: &mut sqlx::PgConnection, acme: Uuid) {
    for purpose in [Purpose::C2paSigning, Purpose::Field, Purpose::Backup] {
        let found = encryption_keys::resolve(conn, acme, purpose)
            .await
            .expect("resolve");
        assert!(
            found.is_none(),
            "{} must not resolve to the blob key",
            purpose.as_str()
        );
    }
}

/// The §7 property, for keys.
async fn tenants_do_not_resolve_each_other(
    conn: &mut sqlx::PgConnection,
    acme: Uuid,
    globex: Uuid,
) {
    let theirs = encryption_keys::resolve(conn, globex, Purpose::Blob)
        .await
        .expect("resolve");
    assert!(
        theirs.is_none(),
        "one tenant's key must not answer for another's"
    );
    assert!(
        encryption_keys::resolve(conn, acme, Purpose::Blob)
            .await
            .expect("resolve")
            .is_some(),
        "and the owner still resolves it"
    );
}

/// Revocation stops use without losing the record.
async fn a_revoked_key_stops_resolving_but_stays_on_the_record(
    conn: &mut sqlx::PgConnection,
    globex: Uuid,
) {
    let key = encryption_keys::activate(
        conn,
        &NewKey {
            tenant_id: Some(globex),
            purpose: Purpose::Backup,
            provider: Provider::Vault,
            key_ref: "vault://backups/globex",
            customer_managed: false,
        },
    )
    .await
    .expect("activate");

    assert!(
        encryption_keys::revoke(conn, key.id).await.expect("revoke"),
        "the first revocation takes effect"
    );
    assert!(
        !encryption_keys::revoke(conn, key.id).await.expect("again"),
        "a second revocation changes nothing and says so, so a retry is not read as a fresh act"
    );

    assert!(
        encryption_keys::resolve(conn, globex, Purpose::Backup)
            .await
            .expect("resolve")
            .is_none(),
        "a revoked key must not be handed out for new writes"
    );
    let all = encryption_keys::for_tenant(conn, globex)
        .await
        .expect("list");
    assert_eq!(all.len(), 1, "and it is still on the record: {all:?}");
    assert_eq!(all[0].state, State::Revoked);
}
