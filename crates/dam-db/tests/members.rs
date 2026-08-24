//! Who has access to a tenant, and what removing them actually does (G10·2a).
//!
//! `tenant_members` has been read by `caller`, `auth`, `browse` and `comments` since the first migration and
//! written in exactly one place — connector registration. The properties worth defending are the ones that
//! decide whether "removed" means anything:
//!
//! - **Removal revokes the credentials.** An account marked gone that keeps working is a flag, which is
//!   precisely what a security questionnaire asks about. The count of revoked keys comes back so a screen can
//!   show that something happened.
//! - **The identity survives if they belong to another tenant.** `deprovisioned_at` is global, and somebody
//!   who works with two customers of one deployment must not lose their other account.
//! - **The last administrator cannot leave, in either direction.** Demotion reaches the same state as removal,
//!   so guarding only one of them would be a rule with a documented workaround.
//! - **An unknown role name is named.** `role_names` has no foreign key and `auth` ignores what it cannot
//!   resolve, so `editors` for a role called `editor` is a member who sees nothing with nothing saying why.
//! - **Adding somebody who already exists elsewhere attaches them.** `identities` is global and unique on the
//!   address, so a plain insert would fail for anybody already in the fleet — and the answer must not reveal
//!   which case it was.
//! - **A re-added person's identity is re-enabled.** Since `auth` allowlists `status = 'active'`, a membership
//!   over a disabled identity is access that does not work.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::members::{self, MemberRefusal, NewMember};
use dam_db::{auth, migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    /// A pool whose search_path resolves the tenant schema first, as `TenantConn` would.
    tenant: PgPool,
    tenant_id: Uuid,
    other_tenant_id: Uuid,
}

async fn db() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    migrate::tenant(&url, "t_globex")
        .await
        .expect("other tenant");
    let global = pg.pool().clone();
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let tenant_id = tenant_row(&global, "acme", "t_acme").await;
    let other_tenant_id = tenant_row(&global, "globex", "t_globex").await;

    for key in ["editor", "viewer"] {
        sqlx::query(
            "INSERT INTO roles (id, key, label, permissions, all_asset_groups) \
             VALUES (gen_random_uuid(), $1, $1, '{asset:read}', true)",
        )
        .bind(key)
        .execute(&tenant)
        .await
        .expect("role");
    }

    Fixture {
        _pg: pg,
        global,
        tenant,
        tenant_id,
        other_tenant_id,
    }
}

async fn tenant_row(global: &PgPool, slug: &str, schema: &str) -> Uuid {
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

fn member(email: &str, roles: &[&str], admin: bool) -> NewMember {
    NewMember {
        email: email.to_owned(),
        display_name: Some("Somebody".to_owned()),
        role_names: roles.iter().map(|r| (*r).to_owned()).collect(),
        is_tenant_admin: admin,
    }
}

#[tokio::test]
async fn a_member_is_added_with_a_credential_and_appears_in_the_list() {
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");

    let added = members::add(
        &mut conn,
        f.tenant_id,
        &member("ada@example.com", &["editor"], true),
    )
    .await
    .expect("add");
    assert!(!added.identity_existed);
    // The key is the invitation: there is no login flow, so a membership with no credential is inert.
    assert!(added.api_key.starts_with("damrs_"));

    // And it works, which is the property that matters — not what the row says.
    let authenticated = auth::authenticate(&f.global, &added.api_key)
        .await
        .expect("query")
        .expect("the issued key authenticates");
    assert_eq!(authenticated.identity_id, Some(added.identity_id));
    assert_eq!(authenticated.tenant_id, f.tenant_id);

    let listed = members::list(&mut conn, f.tenant_id).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].email, "ada@example.com");
    assert_eq!(listed[0].role_names, vec!["editor".to_owned()]);
    assert!(listed[0].is_tenant_admin);
    assert_eq!(listed[0].status, "active");
    assert!(!listed[0].scim_managed);
    assert_eq!(
        listed[0].live_keys, 1,
        "the count that makes removal visible"
    );
}

#[tokio::test]
async fn somebody_who_already_exists_in_another_tenant_is_attached_rather_than_rejected() {
    // `identities` is global and unique on the lowercased address. A plain insert would fail for anybody
    // already in the fleet, and the failure would also disclose that they are.
    let f = db().await;
    let mut acme = f.tenant.acquire().await.expect("conn");

    let first = members::add(
        &mut acme,
        f.other_tenant_id,
        &member("shared@example.com", &[], true),
    )
    .await
    .expect("add to globex");
    let second = members::add(
        &mut acme,
        f.tenant_id,
        // Different case, because the unique index is on the lowercased column.
        &member("Shared@Example.com", &["viewer"], true),
    )
    .await
    .expect("add to acme");

    assert_eq!(
        first.identity_id, second.identity_id,
        "one person, one identity"
    );
    assert!(second.identity_existed);
    // Both credentials work, each against its own tenant.
    let a = auth::authenticate(&f.global, &first.api_key)
        .await
        .expect("query")
        .expect("globex key");
    let b = auth::authenticate(&f.global, &second.api_key)
        .await
        .expect("query")
        .expect("acme key");
    assert_eq!(a.tenant_id, f.other_tenant_id);
    assert_eq!(b.tenant_id, f.tenant_id);
}

#[tokio::test]
async fn adding_the_same_person_twice_is_refused_and_leaves_nothing_behind() {
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("ada@example.com", &[], true),
    )
    .await
    .expect("add");

    let again = members::add(
        &mut conn,
        f.tenant_id,
        &member("ada@example.com", &[], false),
    )
    .await;
    assert!(
        matches!(again, Err(MemberRefusal::AlreadyAMember)),
        "{again:?}"
    );

    // And no second key was minted for the first membership.
    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.api_keys WHERE tenant_id = $1 AND revoked_at IS NULL",
    )
    .bind(f.tenant_id)
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(keys, 1);
}

#[tokio::test]
async fn a_brand_new_identity_is_not_left_behind_by_a_refused_add() {
    // The find-or-create runs before the membership check, so a refusal has to roll back or a typo'd address
    // becomes a permanent identity nobody can see or remove.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("ada@example.com", &[], true),
    )
    .await
    .expect("add");
    let _ = members::add(
        &mut conn,
        f.tenant_id,
        &member("ada@example.com", &[], false),
    )
    .await;

    let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM dam_global.identities")
        .fetch_one(&f.global)
        .await
        .expect("count");
    assert_eq!(identities, 1);
}

#[tokio::test]
async fn an_address_that_is_not_one_is_refused() {
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    for bad in ["ada", "@example.com", "ada@", "   "] {
        let attempt = members::add(&mut conn, f.tenant_id, &member(bad, &[], false)).await;
        assert!(
            matches!(attempt, Err(MemberRefusal::EmailInvalid(_))),
            "{bad:?} should be refused, got {attempt:?}"
        );
    }
}

#[tokio::test]
async fn an_unknown_role_name_is_named_rather_than_silently_granting_nothing() {
    // The trap `auth` creates by design: it ignores a role name it cannot resolve, which is right there and
    // wrong here. `editors` for a role called `editor` is a member who can see nothing.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");

    let known = members::known_roles(&mut conn).await.expect("roles");
    assert_eq!(known, vec!["editor".to_owned(), "viewer".to_owned()]);

    let wanted = vec![
        "editor".to_owned(),
        "editors".to_owned(),
        "curator".to_owned(),
        "editors".to_owned(),
    ];
    let missing = members::unknown_roles(&wanted, &known);
    assert_eq!(
        missing,
        vec!["curator".to_owned(), "editors".to_owned()],
        "every unknown name, once each, so a form can point at all of them"
    );
    assert!(members::unknown_roles(&[], &known).is_empty());
}

#[tokio::test]
async fn a_connected_sites_role_is_not_offered_to_a_person() {
    // From reading the dev tenant's real list rather than a fixture: every registered site creates a role
    // called `connector:<uuid>`, scoped to the groups that one site may render, and they sort into the middle
    // of the list where they look like they belong. Granting one to a person is granting them a role that
    // exists to describe a machine.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    // The exact format `connectors::register` writes.
    let connector_role = format!("connector:{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, all_asset_groups) \
         VALUES (gen_random_uuid(), $1, 'A site', '{asset:read}', false)",
    )
    .bind(&connector_role)
    .execute(&f.tenant)
    .await
    .expect("connector role");

    let known = members::known_roles(&mut conn).await.expect("roles");
    assert_eq!(
        known,
        vec!["editor".to_owned(), "viewer".to_owned()],
        "a site's role is not something to give a colleague"
    );

    // And it is still refused as an explicit grant, rather than merely hidden from the picker.
    assert_eq!(
        members::unknown_roles(std::slice::from_ref(&connector_role), &known),
        vec![connector_role]
    );
}

#[tokio::test]
async fn roles_are_replaced_and_the_previous_set_comes_back() {
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    let admin = members::add(
        &mut conn,
        f.tenant_id,
        &member("admin@example.com", &[], true),
    )
    .await
    .expect("admin");
    let added = members::add(
        &mut conn,
        f.tenant_id,
        &member("ada@example.com", &["editor"], false),
    )
    .await
    .expect("add");

    let previous = members::set_roles(
        &mut conn,
        f.tenant_id,
        added.identity_id,
        &["viewer".to_owned()],
        false,
    )
    .await
    .expect("set");
    // Returned so the audit entry can say what changed rather than only what it changed to — and the admin
    // flag with it, because reading that back afterwards would mean reading the value just written.
    assert_eq!(previous.role_names, vec!["editor".to_owned()]);
    assert!(!previous.is_tenant_admin);

    let listed = members::list(&mut conn, f.tenant_id).await.expect("list");
    let ada = listed
        .iter()
        .find(|m| m.email == "ada@example.com")
        .expect("ada");
    assert_eq!(ada.role_names, vec!["viewer".to_owned()]);
    assert!(!ada.is_tenant_admin);
    assert!(!admin.identity_existed);
}

#[tokio::test]
async fn the_last_administrator_can_neither_be_removed_nor_demoted() {
    // A tenant with no administrator cannot appoint one. Both directions, because demotion reaches the same
    // state and a rule guarding only removal would be a rule with a workaround.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    let only = members::add(
        &mut conn,
        f.tenant_id,
        &member("admin@example.com", &[], true),
    )
    .await
    .expect("add");

    let demote = members::set_roles(&mut conn, f.tenant_id, only.identity_id, &[], false).await;
    assert!(
        matches!(demote, Err(MemberRefusal::LastAdmin)),
        "{demote:?}"
    );

    let remove = members::remove(&mut conn, f.tenant_id, only.identity_id).await;
    assert!(
        matches!(remove, Err(MemberRefusal::LastAdmin)),
        "{remove:?}"
    );

    // With a second administrator, both become allowed.
    members::add(
        &mut conn,
        f.tenant_id,
        &member("second@example.com", &[], true),
    )
    .await
    .expect("second admin");
    members::set_roles(&mut conn, f.tenant_id, only.identity_id, &[], false)
        .await
        .expect("demotion is fine once somebody else can administer");
    members::remove(&mut conn, f.tenant_id, only.identity_id)
        .await
        .expect("removal too");
}

#[tokio::test]
async fn membership_changes_serialise_per_tenant() {
    // The last-administrator rule is a check on a *set*, so `FOR UPDATE` on the membership being changed does
    // not protect it: two administrators stepping down at once each count two and each see somebody
    // remaining. Row locks over the whole administrator set would close that and deadlock instead, because a
    // demotion and a removal would each hold one administrator's row while waiting for the other's.
    //
    // Asserted deterministically rather than by racing two tasks and hoping: this holds the lock the
    // implementation takes and checks that a change *blocks*. The key is duplicated from `lock_memberships`
    // on purpose — if somebody changes it, this fails and points at the coupling.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("one@example.com", &[], true),
    )
    .await
    .expect("first");
    let two = members::add(
        &mut conn,
        f.tenant_id,
        &member("two@example.com", &[], true),
    )
    .await
    .expect("second");
    members::add(
        &mut conn,
        f.other_tenant_id,
        &member("elsewhere@example.com", &[], true),
    )
    .await
    .expect("another tenant");

    let mut blocker = f.tenant.begin().await.expect("begin");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('dam_global.tenant_members'), hashtext($1::text))",
    )
    .bind(f.tenant_id)
    .execute(&mut *blocker)
    .await
    .expect("hold the lock");

    let blocked = {
        let pool = f.tenant.clone();
        let tenant_id = f.tenant_id;
        tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("conn");
            members::remove(&mut conn, tenant_id, two.identity_id).await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !blocked.is_finished(),
        "a membership change must wait for another tenant-scoped change to finish"
    );

    // And the lock is per tenant, so a different customer's change is not held up behind it.
    let mut other = f.tenant.acquire().await.expect("conn");
    let elsewhere = members::list(&mut other, f.other_tenant_id)
        .await
        .expect("list");
    let their_admin = elsewhere.first().expect("their admin").identity_id;
    members::add(
        &mut other,
        f.other_tenant_id,
        &member("second@example.com", &[], true),
    )
    .await
    .expect("another tenant is not blocked");
    members::set_roles(&mut other, f.other_tenant_id, their_admin, &[], false)
        .await
        .expect("nor is a change there");

    blocker.rollback().await.expect("release");
    let outcome = blocked.await.expect("join");
    assert!(
        outcome.is_ok(),
        "and it goes through once the lock is free: {outcome:?}"
    );
}

#[tokio::test]
async fn removal_revokes_the_credentials_it_is_supposed_to() {
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("admin@example.com", &[], true),
    )
    .await
    .expect("admin");
    let added = members::add(
        &mut conn,
        f.tenant_id,
        &member("leaver@example.com", &["editor"], false),
    )
    .await
    .expect("add");

    let removed = members::remove(&mut conn, f.tenant_id, added.identity_id)
        .await
        .expect("remove");
    assert_eq!(removed.keys_revoked, 1);
    assert!(removed.identity_disabled, "this was their only tenant");
    assert_eq!(removed.roles_held, vec!["editor".to_owned()]);
    assert!(!removed.was_tenant_admin);

    // The property, not the row: the key stops working.
    assert!(
        auth::authenticate(&f.global, &added.api_key)
            .await
            .expect("query")
            .is_none(),
        "a removed member's key must stop working"
    );
    let listed = members::list(&mut conn, f.tenant_id).await.expect("list");
    assert!(listed.iter().all(|m| m.email != "leaver@example.com"));
}

#[tokio::test]
async fn removal_from_one_tenant_leaves_the_other_account_working() {
    // `deprovisioned_at` is global. Disabling the identity on the way out of one tenant would take away an
    // account belonging to a different customer.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("admin@example.com", &[], true),
    )
    .await
    .expect("admin");
    members::add(
        &mut conn,
        f.other_tenant_id,
        &member("admin2@example.com", &[], true),
    )
    .await
    .expect("other admin");

    let here = members::add(
        &mut conn,
        f.tenant_id,
        &member("shared@example.com", &[], false),
    )
    .await
    .expect("acme");
    let there = members::add(
        &mut conn,
        f.other_tenant_id,
        &member("shared@example.com", &[], false),
    )
    .await
    .expect("globex");
    assert_eq!(here.identity_id, there.identity_id);

    let removed = members::remove(&mut conn, f.tenant_id, here.identity_id)
        .await
        .expect("remove from acme");
    assert_eq!(removed.keys_revoked, 1, "only this tenant's key");
    assert!(!removed.identity_disabled, "they still work for globex");

    assert!(
        auth::authenticate(&f.global, &here.api_key)
            .await
            .expect("query")
            .is_none()
    );
    assert!(
        auth::authenticate(&f.global, &there.api_key)
            .await
            .expect("query")
            .is_some(),
        "the other tenant's access must survive"
    );
}

#[tokio::test]
async fn re_adding_somebody_re_enables_the_identity_removal_disabled() {
    // Since `auth` allowlists `status = 'active'`, a membership over a disabled identity is access that does
    // not work — and the person would report it as the invitation being broken.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("admin@example.com", &[], true),
    )
    .await
    .expect("admin");
    let first = members::add(
        &mut conn,
        f.tenant_id,
        &member("boomerang@example.com", &[], false),
    )
    .await
    .expect("add");
    members::remove(&mut conn, f.tenant_id, first.identity_id)
        .await
        .expect("remove");

    let again = members::add(
        &mut conn,
        f.tenant_id,
        &member("boomerang@example.com", &["viewer"], false),
    )
    .await
    .expect("re-add");
    assert_eq!(again.identity_id, first.identity_id);
    assert!(
        auth::authenticate(&f.global, &again.api_key)
            .await
            .expect("query")
            .is_some(),
        "the new key has to work, which means the identity had to be re-enabled"
    );
    // And the old one stays revoked: coming back is a new credential, not the return of an old one.
    assert!(
        auth::authenticate(&f.global, &first.api_key)
            .await
            .expect("query")
            .is_none()
    );
}

#[tokio::test]
async fn a_scim_managed_member_is_refused_locally() {
    // 0002: "SCIM-managed identities must not be editable in the damrs UI, or the IdP will overwrite local
    // edits on next sync and the customer will report it as data loss."
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("admin@example.com", &[], true),
    )
    .await
    .expect("admin");
    let managed = members::add(
        &mut conn,
        f.tenant_id,
        &member("idp@example.com", &["editor"], false),
    )
    .await
    .expect("add");
    sqlx::query("UPDATE dam_global.identities SET scim_managed = true WHERE id = $1")
        .bind(managed.identity_id)
        .execute(&f.global)
        .await
        .expect("mark managed");

    let attempt = members::set_roles(
        &mut conn,
        f.tenant_id,
        managed.identity_id,
        &["viewer".to_owned()],
        false,
    )
    .await;
    assert!(
        matches!(attempt, Err(MemberRefusal::ScimManaged)),
        "{attempt:?}"
    );

    // Removal is still allowed: taking away access to *this* tenant is not editing the account the IdP owns,
    // and refusing it would leave an offboarded person in place until somebody fixed the IdP.
    members::remove(&mut conn, f.tenant_id, managed.identity_id)
        .await
        .expect("removal stays available");
}

#[tokio::test]
async fn a_connected_sites_service_account_is_not_a_person() {
    // Registering a site creates an identity, a membership, a role and a key — deliberately, so it goes
    // through the same access predicate as everybody else. The consequence is that a website turns up in the
    // list of people with the same controls, and "change this website's roles" is not a thing anybody should
    // be offered. Found by reading the dev tenant's real list; a fixture would never have held one.
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    members::add(
        &mut conn,
        f.tenant_id,
        &member("ada@example.com", &[], true),
    )
    .await
    .expect("admin");

    // The four rows registration writes, in the shape it writes them.
    let connector_id = Uuid::now_v7();
    let service = members::add(
        &mut conn,
        f.tenant_id,
        &member(
            &format!("connector+{connector_id}@connectors.invalid"),
            &[],
            false,
        ),
    )
    .await
    .expect("service account");
    let key_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM dam_global.api_keys WHERE identity_id = $1 AND tenant_id = $2",
    )
    .bind(service.identity_id)
    .bind(f.tenant_id)
    .fetch_one(&f.global)
    .await
    .expect("its key");
    sqlx::query(
        "INSERT INTO connectors (id, kind, label, site_url, api_key_id, signing_secret, status) \
         VALUES ($1, 'drupal', 'Marketing site', 'https://example.test/', $2, 'sealed:x', 'active')",
    )
    .bind(connector_id)
    .bind(key_id)
    .execute(&f.tenant)
    .await
    .expect("connector");

    let listed = members::list(&mut conn, f.tenant_id).await.expect("list");
    assert_eq!(
        listed.iter().map(|m| m.email.as_str()).collect::<Vec<_>>(),
        vec!["ada@example.com"],
        "the site belongs on the Sites screen, not among the people"
    );

    // Excluded by the join on `api_key_id`, not by the address — so a connector whose email convention
    // changed would still be excluded, and a person who happened to use that domain would not be.
    sqlx::query("UPDATE dam_global.identities SET email = 'renamed@elsewhere.test' WHERE id = $1")
        .bind(service.identity_id)
        .execute(&f.global)
        .await
        .expect("rename");
    let listed = members::list(&mut conn, f.tenant_id).await.expect("list");
    assert_eq!(listed.len(), 1, "still excluded after the address changed");
}

#[tokio::test]
async fn changing_or_removing_somebody_who_is_not_a_member_says_so() {
    let f = db().await;
    let mut conn = f.tenant.acquire().await.expect("conn");
    let stranger = Uuid::now_v7();
    assert!(matches!(
        members::set_roles(&mut conn, f.tenant_id, stranger, &[], false).await,
        Err(MemberRefusal::NotAMember)
    ));
    assert!(matches!(
        members::remove(&mut conn, f.tenant_id, stranger).await,
        Err(MemberRefusal::NotAMember)
    ));
}

#[tokio::test]
async fn a_membership_and_its_audit_entry_commit_together() {
    // The reason every function here takes a connection rather than a pool: `tenant_members` is in
    // `dam_global` and `audit_log` is in the tenant schema, and they are two schemas in one database — so one
    // transaction covers both. Rolling back must leave neither.
    let f = db().await;
    let mut tx = f.tenant.begin().await.expect("begin");

    let added = members::add(&mut tx, f.tenant_id, &member("ada@example.com", &[], true))
        .await
        .expect("add");
    dam_db::audit::record(
        &mut tx,
        dam_db::audit::NewEntry::by_system(dam_db::audit::Action::IdentityProvisioned, "identity")
            .on(added.identity_id.to_string()),
    )
    .await
    .expect("audit");
    tx.rollback().await.expect("rollback");

    let members_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dam_global.tenant_members WHERE tenant_id = $1")
            .bind(f.tenant_id)
            .fetch_one(&f.global)
            .await
            .expect("count");
    let entries_left: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log")
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(members_left, 0, "the membership rolled back");
    assert_eq!(entries_left, 0, "and so did its record");
}
