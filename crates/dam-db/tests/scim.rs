//! SCIM provisioning at the database level (G10·2b).
//!
//! The API suite drives the whole provider lifecycle over HTTP. What lives here are the two properties that
//! are about the schema rather than the protocol:
//!
//! - **An external id belongs to the client that issued it.** `0002_enterprise.sql` made it unique across the
//!   whole of `identities`, which is a global table — so two customers' providers numbering their users
//!   independently collided, and the failure landed on whichever of them synced second.
//! - **A token is its own credential class.** Its digest must never be able to collide with an API key's, and
//!   the plaintext must never appear in a debug rendering.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::scim::{self, Filter, NewUser, ScimRefusal};
use dam_db::{auth, migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    acme_id: Uuid,
    globex_id: Uuid,
}

async fn db() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    migrate::tenant(&url, "t_globex").await.expect("globex");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("pool");
    Fixture {
        _pg: pg,
        acme_id: tenant(&global, "acme", "t_acme").await,
        globex_id: tenant(&global, "globex", "t_globex").await,
        global,
        acme,
    }
}

async fn tenant(global: &PgPool, slug: &str, schema: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), $1, $2, $1, $3, 'active') RETURNING id",
    )
    .bind(slug)
    .bind(schema)
    .bind(format!("{slug}/"))
    .fetch_one(global)
    .await
    .expect("tenant")
}

fn new_user(user_name: &str, external_id: &str) -> NewUser {
    NewUser {
        user_name: user_name.to_owned(),
        external_id: Some(external_id.to_owned()),
        display_name: Some("Somebody".to_owned()),
        active: true,
        roles: Vec::new(),
    }
}

#[tokio::test]
async fn two_tenants_can_use_the_same_external_id() {
    // The collision `0005_scim_client_scope.sql` exists for. `identities` is global and 0002 indexed
    // `scim_external_id` uniquely across the whole table, so the second customer to provision a user whose
    // provider-side id happened to match got a constraint violation — in a sync they do not control, naming a
    // row they cannot see. Okta's default `externalId` is an opaque per-org id, so the collision is not
    // hypothetical.
    let f = db().await;
    let (_, acme_token) = scim::issue(&f.global, f.acme_id, "Their Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    let (_, globex_token) = scim::issue(&f.global, f.globex_id, "Our Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    let acme = scim::authenticate(&f.global, &acme_token)
        .await
        .expect("query")
        .expect("client");
    let globex = scim::authenticate(&f.global, &globex_token)
        .await
        .expect("query")
        .expect("client");

    let mut conn = f.acme.acquire().await.expect("conn");
    scim::provision(&mut conn, &acme, &new_user("a@acme.test", "00u1234"))
        .await
        .expect("acme provisions");
    // The same external id, a different customer. Under 0002's index this was a unique violation.
    scim::provision(&mut conn, &globex, &new_user("b@globex.test", "00u1234"))
        .await
        .expect("globex provisions the same external id");

    // And each provider still finds its own by that id, which is the point of keeping it.
    let found = scim::page(
        &mut conn,
        f.acme_id,
        &Filter::ExternalId("00u1234".to_owned()),
        1,
        10,
    )
    .await
    .expect("page");
    assert_eq!(found.total, 1);
    assert_eq!(found.users[0].user_name, "a@acme.test");

    // The same id twice within *one* client is still refused, because that is a provider contradicting itself.
    let clash = scim::provision(&mut conn, &acme, &new_user("c@acme.test", "00u1234")).await;
    assert!(
        matches!(clash, Err(ScimRefusal::Database(_))),
        "a duplicate within one client is still a constraint violation: {clash:?}"
    );
}

#[tokio::test]
async fn two_tenants_can_provision_the_same_person() {
    // What `0006_scim_link_is_per_tenant.sql` exists for, and what 0005 did not fix. The columns were
    // single-valued on the shared `identities` row, so the second provider silently took ownership and the
    // first tenant's sync then failed its own ownership check — its provisioning broke because a different
    // customer provisioned the same consultant.
    let f = db().await;
    let (_, acme_token) = scim::issue(&f.global, f.acme_id, "Acme Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    let (_, globex_token) =
        scim::issue(&f.global, f.globex_id, "Globex Okta", &["Users".to_owned()])
            .await
            .expect("issue");
    let acme = scim::authenticate(&f.global, &acme_token)
        .await
        .expect("query")
        .expect("client");
    let globex = scim::authenticate(&f.global, &globex_token)
        .await
        .expect("query")
        .expect("client");

    let mut conn = f.acme.acquire().await.expect("conn");
    let here = scim::provision(
        &mut conn,
        &acme,
        &new_user("consultant@example.test", "acme-1"),
    )
    .await
    .expect("acme provisions");
    let there = scim::provision(
        &mut conn,
        &globex,
        &new_user("consultant@example.test", "globex-1"),
    )
    .await
    .expect("globex provisions the same person");
    assert_eq!(
        here.identity_id, there.identity_id,
        "one person, one identity — that is the point of a global identities table"
    );

    // Each provider still sees its own external id for them, which the shared column could not hold.
    assert_eq!(here.external_id.as_deref(), Some("acme-1"));
    assert_eq!(there.external_id.as_deref(), Some("globex-1"));

    // And each still owns its own membership: the second provisioning did not take the first's away.
    scim::set_active(&mut conn, &acme, here.identity_id, true)
        .await
        .expect("acme still manages its own");
    scim::set_active(&mut conn, &globex, there.identity_id, true)
        .await
        .expect("and so does globex");

    // Removing them from one tenant leaves the other's provisioning intact.
    scim::deprovision(&mut conn, &acme, here.identity_id)
        .await
        .expect("acme offboards");
    assert!(
        scim::by_id(&mut conn, f.acme_id, here.identity_id)
            .await
            .expect("read")
            .is_none()
    );
    assert!(
        scim::by_id(&mut conn, f.globex_id, there.identity_id)
            .await
            .expect("read")
            .is_some(),
        "the other customer's account survives"
    );
}

#[tokio::test]
async fn a_provider_managed_person_is_still_an_ordinary_colleague_elsewhere() {
    // The second half of the same bug. `scim_managed` on the identity made somebody uneditable *everywhere*:
    // provisioned by one customer's provider, an administrator in another tenant — where no provider manages
    // them at all — could no longer change their roles.
    let f = db().await;
    let (_, token) = scim::issue(&f.global, f.acme_id, "Acme Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    let acme = scim::authenticate(&f.global, &token)
        .await
        .expect("query")
        .expect("client");

    let mut conn = f.acme.acquire().await.expect("conn");
    let provisioned = scim::provision(&mut conn, &acme, &new_user("shared@example.test", "acme-9"))
        .await
        .expect("provision");

    // Acme's own administrator is refused, correctly: the provider owns that membership.
    let refused =
        dam_db::members::set_roles(&mut conn, f.acme_id, provisioned.identity_id, &[], false).await;
    assert!(
        matches!(refused, Err(dam_db::members::MemberRefusal::ScimManaged)),
        "{refused:?}"
    );

    // Globex adds the same person by hand and may administer them freely.
    dam_db::members::add(
        &mut conn,
        f.globex_id,
        &dam_db::members::NewMember {
            email: "admin@globex.test".to_owned(),
            display_name: None,
            role_names: Vec::new(),
            is_tenant_admin: true,
        },
    )
    .await
    .expect("their admin");
    let by_hand = dam_db::members::add(
        &mut conn,
        f.globex_id,
        &dam_db::members::NewMember {
            email: "shared@example.test".to_owned(),
            display_name: None,
            role_names: Vec::new(),
            is_tenant_admin: false,
        },
    )
    .await
    .expect("added by hand elsewhere");
    assert_eq!(by_hand.identity_id, provisioned.identity_id);
    dam_db::members::set_roles(&mut conn, f.globex_id, by_hand.identity_id, &[], false)
        .await
        .expect("and this tenant's administrator is the only authority here");
}

#[tokio::test]
async fn a_provisioning_token_is_its_own_credential_class() {
    let token = scim::Token::generate();
    let plaintext = token.hash().to_owned();
    assert_eq!(plaintext.len(), 64, "a blake3 hex digest");

    let fresh = scim::Token::generate();
    let readable = fresh.into_plaintext();
    assert!(
        readable.starts_with("damrs_scim_"),
        "prefixed so a secret scanner matches it and whoever finds one knows which surface to revoke"
    );
    assert_ne!(
        scim::Token::hash_of(&readable),
        auth::ApiKey::hash_of(&readable),
        "domain-separated: a digest from scim_clients must never collide with one from api_keys"
    );
    assert_eq!(
        scim::Token::hash_of(&readable),
        scim::Token::hash_of(&readable)
    );

    // The plaintext must not appear in a debug rendering: a token that reaches a log is a token to rotate,
    // and `{:?}` on a struct is how that happens.
    let rendered = format!("{:?}", scim::Token::generate());
    assert!(rendered.contains("REDACTED"), "{rendered}");
    assert!(!rendered.contains("damrs_scim_"), "{rendered}");
}

#[tokio::test]
async fn a_revoked_client_stops_authenticating_and_revoking_is_terminal() {
    let f = db().await;
    let (id, token) = scim::issue(&f.global, f.acme_id, "Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    assert!(
        scim::authenticate(&f.global, &token)
            .await
            .expect("query")
            .is_some()
    );

    assert!(scim::revoke(&f.global, id).await.expect("revoke"));
    assert!(
        scim::authenticate(&f.global, &token)
            .await
            .expect("query")
            .is_none(),
        "a leaked provisioning token can create and remove accounts, so revocation is immediate"
    );
    assert!(
        !scim::revoke(&f.global, id).await.expect("revoke again"),
        "and terminal: there is nothing to un-revoke"
    );
}

#[tokio::test]
async fn a_suspended_tenants_provider_cannot_sync() {
    // The same rule `auth::authenticate` follows for API keys. Suspending a tenant that leaves its identity
    // provider able to create accounts is a suspension that does not suspend.
    let f = db().await;
    let (_, token) = scim::issue(&f.global, f.acme_id, "Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    for status in ["suspended", "deprovisioning", "migration_failed"] {
        sqlx::query("UPDATE dam_global.tenants SET status = $2 WHERE id = $1")
            .bind(f.acme_id)
            .bind(status)
            .execute(&f.global)
            .await
            .expect("set status");
        assert!(
            scim::authenticate(&f.global, &token)
                .await
                .expect("query")
                .is_none(),
            "a {status} tenant's provider must not sync"
        );
    }
    sqlx::query("UPDATE dam_global.tenants SET status = 'active' WHERE id = $1")
        .bind(f.acme_id)
        .execute(&f.global)
        .await
        .expect("reactivate");
    assert!(
        scim::authenticate(&f.global, &token)
            .await
            .expect("query")
            .is_some(),
        "and it comes back when the tenant does, without reissuing the token"
    );
}

#[tokio::test]
async fn a_scope_a_client_does_not_hold_is_visible_as_such() {
    let f = db().await;
    let (_, token) = scim::issue(&f.global, f.acme_id, "Users only", &["Users".to_owned()])
        .await
        .expect("issue");
    let client = scim::authenticate(&f.global, &token)
        .await
        .expect("query")
        .expect("client");
    assert!(client.may(scim::USERS));
    assert!(!client.may(scim::GROUPS));
}

#[tokio::test]
async fn paging_is_one_based_and_capped() {
    // SCIM's `startIndex` is 1-based. Treating it as an offset drops the first user of every sync, and a
    // provider sending 0 or a negative must not wrap into the end of the list.
    let f = db().await;
    let (_, token) = scim::issue(&f.global, f.acme_id, "Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    let client = scim::authenticate(&f.global, &token)
        .await
        .expect("query")
        .expect("client");
    let mut conn = f.acme.acquire().await.expect("conn");
    for index in 0..5 {
        scim::provision(
            &mut conn,
            &client,
            &new_user(&format!("user{index}@acme.test"), &format!("okta-{index}")),
        )
        .await
        .expect("provision");
    }

    let first = scim::page(&mut conn, f.acme_id, &Filter::All, 1, 2)
        .await
        .expect("page");
    assert_eq!(first.total, 5);
    assert_eq!(first.users.len(), 2);
    assert_eq!(first.users[0].user_name, "user0@acme.test");

    let second = scim::page(&mut conn, f.acme_id, &Filter::All, 3, 2)
        .await
        .expect("page");
    assert_eq!(second.users[0].user_name, "user2@acme.test");

    // Zero and negative both mean "the beginning", not "the end".
    for start in [0, -7] {
        let clamped = scim::page(&mut conn, f.acme_id, &Filter::All, start, 1)
            .await
            .expect("page");
        assert_eq!(clamped.users[0].user_name, "user0@acme.test");
    }

    // A provider asking for everything gets a page, because a hundred thousand rows in one response is a
    // timeout on both sides.
    let capped = scim::page(&mut conn, f.acme_id, &Filter::All, 1, 100_000)
        .await
        .expect("page");
    assert_eq!(capped.users.len(), 5, "all five here, but bounded");

    // Zero is how a provider asks for a count without paging.
    let counted = scim::page(&mut conn, f.acme_id, &Filter::All, 1, 0)
        .await
        .expect("page");
    assert!(counted.users.is_empty());
    assert_eq!(counted.total, 5);
}
