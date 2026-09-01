//! A tenant's own C2PA signing identity (G10·3b).
//!
//! The properties worth pinning are the ones the feature exists for: that a tenant with none falls back
//! rather than stopping, that only one signs at a time, that a superseded one is kept, and that a row lifted
//! into another tenant's schema cannot be opened.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::{Secret, sealed::SealingKeyring};
use dam_db::signing_identities::{self, NewIdentity};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;

const CERT: &str = "-----BEGIN CERTIFICATE-----\nnot a real one\n-----END CERTIFICATE-----";

async fn db() -> (PostgresHarness, PgPool, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    migrate::tenant(&url, "t_globex").await.expect("globex");
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");
    let globex = pg.pool_for_schema("t_globex").await.expect("globex pool");
    (pg, acme, globex)
}

fn keyring() -> SealingKeyring {
    SealingKeyring::single(
        "k1",
        &Secret::new("a-sealing-key-of-adequate-length".to_owned()),
    )
}

#[tokio::test]
async fn the_signing_identity_invariants_hold() {
    let (_pg, acme, globex) = db().await;
    let ring = keyring();

    a_tenant_with_none_falls_back_rather_than_stopping(&acme).await;
    let first = activating_makes_it_the_one_that_signs(&acme, &ring).await;
    a_second_identity_stands_the_first_down_and_keeps_it(&acme, &ring, first).await;
    standing_down_leaves_no_active_identity_and_says_so(&acme).await;
    a_row_lifted_into_another_tenant_cannot_be_opened(&acme, &globex, &ring).await;
}

/// The fallback, and it is first because it is what every tenant looks like today.
async fn a_tenant_with_none_falls_back_rather_than_stopping(pool: &PgPool) {
    let mut conn = pool.acquire().await.expect("conn");
    assert!(
        signing_identities::active(&mut conn)
            .await
            .expect("active")
            .is_none(),
        "a tenant with no identity must resolve to nothing, so the deployment's applies"
    );
}

async fn activating_makes_it_the_one_that_signs(
    pool: &PgPool,
    ring: &SealingKeyring,
) -> uuid::Uuid {
    let mut conn = pool.acquire().await.expect("conn");
    let id = uuid::Uuid::new_v4();
    let sealed = ring
        .seal(
            &Secret::new("-----BEGIN PRIVATE KEY-----first-----END PRIVATE KEY-----".to_owned()),
            &signing_identities::associated_data("acme", id),
        )
        .expect("seal");

    let made = signing_identities::activate(
        &mut conn,
        &NewIdentity {
            id,
            label: "Acme production",
            cert_pem: CERT,
            sealed_key: &sealed,
            sealing_key_id: "k1",
            algorithm: "ps256",
            timestamp_authority: Some("http://timestamp.example/tsa"),
        },
    )
    .await
    .expect("activate");
    assert!(made.is_active);

    let found = signing_identities::active(&mut conn)
        .await
        .expect("active")
        .expect("an identity");
    assert_eq!(found.id, made.id);
    assert_eq!(found.algorithm, "ps256");
    assert_eq!(
        found.timestamp_authority.as_deref(),
        Some("http://timestamp.example/tsa"),
        "without a timestamper a manifest stops verifying when the certificate lapses"
    );
    made.id
}

/// Rotation supersedes and keeps.
///
/// A manifest signed under the old certificate is still verified against it, so the row is the only record of
/// which identity produced those assets — the question asked when a certificate turns out to be mis-issued.
async fn a_second_identity_stands_the_first_down_and_keeps_it(
    pool: &PgPool,
    ring: &SealingKeyring,
    first: uuid::Uuid,
) {
    let mut conn = pool.acquire().await.expect("conn");
    let id = uuid::Uuid::new_v4();
    let sealed = ring
        .seal(
            &Secret::new("-----BEGIN PRIVATE KEY-----second-----END PRIVATE KEY-----".to_owned()),
            &signing_identities::associated_data("acme", id),
        )
        .expect("seal");
    let second = signing_identities::activate(
        &mut conn,
        &NewIdentity {
            id,
            label: "Acme 2027",
            cert_pem: CERT,
            sealed_key: &sealed,
            sealing_key_id: "k1",
            algorithm: "ps256",
            timestamp_authority: None,
        },
    )
    .await
    .expect("rotate");

    let active = signing_identities::active(&mut conn)
        .await
        .expect("active")
        .expect("one");
    assert_eq!(active.id, second.id);

    let all = signing_identities::list(&mut conn).await.expect("list");
    assert_eq!(all.len(), 2, "the superseded identity is kept: {all:?}");
    let old = all.iter().find(|i| i.id == first).expect("the first");
    assert!(!old.is_active);
}

async fn standing_down_leaves_no_active_identity_and_says_so(pool: &PgPool) {
    let mut conn = pool.acquire().await.expect("conn");
    assert!(
        signing_identities::deactivate(&mut conn)
            .await
            .expect("deactivate"),
        "there was one to stand down"
    );
    assert!(
        !signing_identities::deactivate(&mut conn)
            .await
            .expect("again"),
        "a second call changes nothing and says so, so a retry is not read as a fresh act"
    );
    assert!(
        signing_identities::active(&mut conn)
            .await
            .expect("active")
            .is_none(),
        "and the tenant is back to the deployment's identity rather than signing nothing"
    );
}

/// The property the associated data exists for.
///
/// Copying the row into another tenant's schema is the attack: the ciphertext is intact, the sealing key is
/// the same one, and only the AAD stops it opening. Without that binding, the schema-per-tenant boundary would
/// end at this table and one tenant could sign as another.
async fn a_row_lifted_into_another_tenant_cannot_be_opened(
    acme: &PgPool,
    globex: &PgPool,
    ring: &SealingKeyring,
) {
    let mut acme_conn = acme.acquire().await.expect("conn");
    let id = uuid::Uuid::new_v4();
    let sealed = ring
        .seal(
            &Secret::new("-----BEGIN PRIVATE KEY-----acme-----END PRIVATE KEY-----".to_owned()),
            &signing_identities::associated_data("acme", id),
        )
        .expect("seal");
    let made = signing_identities::activate(
        &mut acme_conn,
        &NewIdentity {
            id,
            label: "Acme",
            cert_pem: CERT,
            sealed_key: &sealed,
            sealing_key_id: "k1",
            algorithm: "ps256",
            timestamp_authority: None,
        },
    )
    .await
    .expect("activate");

    // It opens for its owner.
    assert!(
        ring.open(&made.sealed_key, &made.associated_data("acme"))
            .is_ok(),
        "the owner can use its own key"
    );

    // Copied verbatim into another tenant's schema — same bytes, same sealing key.
    let mut globex_conn = globex.acquire().await.expect("conn");
    let stolen = signing_identities::activate(
        &mut globex_conn,
        &NewIdentity {
            id: made.id,
            label: "lifted",
            cert_pem: &made.cert_pem,
            sealed_key: &made.sealed_key,
            sealing_key_id: &made.sealing_key_id,
            algorithm: &made.algorithm,
            timestamp_authority: None,
        },
    )
    .await
    .expect("insert");

    assert!(
        ring.open(&stolen.sealed_key, &stolen.associated_data("globex"))
            .is_err(),
        "a lifted row must not open for the tenant it was lifted into — that is the whole point of the AAD"
    );
}
