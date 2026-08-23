//! API-key authentication and grant loading (1.6's foundation).
//!
//! The model is not invented here — `api_keys`, `tenant_members` and `roles` already prescribe it. A key
//! identifies a tenant and an identity; the identity's `role_names` resolve to rows in the tenant's
//! `roles` table; those compile to the `AccessPredicate` from 0.10. This suite checks the two halves that
//! carry security weight: that a key which should not work does not, and that a set of roles turns into
//! exactly the grants it describes and no more.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use chrono::{Duration, Utc};
use dam_core::policy::{self, Action};
use dam_db::{auth, migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

/// The control-plane pool plus one provisioned tenant.
async fn setup() -> (PostgresHarness, PgPool, PgPool, Uuid) {
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
    .expect("insert tenant");

    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");
    (pg, global, tenant, tenant_id)
}

async fn identity(global: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(global)
    .await
    .expect("insert identity")
}

/// Issues a key and returns the plaintext the caller would present.
async fn issue(
    global: &PgPool,
    tenant_id: Uuid,
    identity_id: Uuid,
    scopes: &[&str],
    expires_in: Option<Duration>,
) -> String {
    let key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes, expires_at) \
         VALUES (gen_random_uuid(), $1, $2, 'test key', $3, $4, $5, $6)",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .bind(key.prefix())
    .bind(key.hash())
    .bind(scopes.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    .bind(expires_in.map(|d| Utc::now() + d))
    .execute(global)
    .await
    .expect("issue key");
    key.into_plaintext()
}

async fn add_role(pool: &PgPool, key: &str, permissions: &[&str], groups: &[Uuid], all: bool) {
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), $1, $1, $2, $3, $4)",
    )
    .bind(key)
    .bind(
        permissions
            .iter()
            .map(|p| (*p).to_owned())
            .collect::<Vec<_>>(),
    )
    .bind(groups.to_vec())
    .bind(all)
    .execute(pool)
    .await
    .expect("insert role");
}

async fn make_member(
    global: &PgPool,
    tenant_id: Uuid,
    identity_id: Uuid,
    roles: &[&str],
    admin: bool,
) {
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .bind(roles.iter().map(|r| (*r).to_owned()).collect::<Vec<_>>())
    .bind(admin)
    .execute(global)
    .await
    .expect("insert membership");
}

// ─── the key itself ─────────────────────────────────────────────────────────

#[test]
fn a_generated_key_is_high_entropy_and_carries_a_readable_prefix() {
    let key = auth::ApiKey::generate();
    let plaintext = key.plaintext().to_owned();
    assert!(
        plaintext.starts_with("damrs_"),
        "a recognisable prefix is what lets a secret scanner catch a leaked key: {plaintext}"
    );
    // 256 bits, base32-ish. Enough that guessing is not a threat model — which is why the stored hash
    // is a fast digest rather than a password hash.
    assert!(plaintext.len() >= 50, "got {} chars", plaintext.len());
    assert_ne!(
        auth::ApiKey::generate().plaintext(),
        plaintext,
        "two generated keys must differ"
    );
}

#[test]
fn the_stored_prefix_identifies_a_key_without_revealing_it() {
    let key = auth::ApiKey::generate();
    assert!(key.plaintext().starts_with(key.prefix()));
    assert!(
        key.plaintext().len() > key.prefix().len() + 20,
        "the prefix must be a small fraction of the key"
    );
    assert!(
        !key.hash().contains(key.plaintext()),
        "the hash must not contain the secret"
    );
}

#[test]
fn the_same_key_always_hashes_the_same_and_a_different_one_does_not() {
    // Deterministic and unsalted, deliberately: `api_keys` has a UNIQUE index on `key_hash`, so
    // authentication is a single indexed lookup rather than a scan-and-verify. That is only safe
    // because the key is 256 bits of entropy — a salt defends against dictionary attacks on
    // low-entropy secrets, and there is no dictionary for these.
    let key = auth::ApiKey::generate();
    assert_eq!(auth::ApiKey::hash_of(key.plaintext()), key.hash());
    assert_ne!(auth::ApiKey::hash_of("damrs_something_else"), key.hash());
}

// ─── authentication ─────────────────────────────────────────────────────────

#[tokio::test]
async fn only_an_active_tenant_authenticates() {
    // Suspension is the one thing suspension is for. The query used to join `tenants` and check nothing about
    // it, so a tenant suspended for non-payment or abuse kept every one of its API keys working — and nothing
    // asserted otherwise, which a surviving mutation proved: turning the join into a `LEFT JOIN` broke no test.
    //
    // Every non-active status is refused, and each for its own reason: `provisioning` may have no schema yet,
    // `deprovisioning` is being torn down, and `migration_failed` is at an unknown schema version where every
    // later query fails from inside a handler.
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let identity_id = identity(&global, "suspended@example.com").await;
    let key = issue(&global, tenant_id, identity_id, &[], None).await;

    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_some(),
        "the premise: this key works while the tenant is active"
    );

    for status in [
        "suspended",
        "provisioning",
        "deprovisioning",
        "migration_failed",
    ] {
        sqlx::query("UPDATE dam_global.tenants SET status = $2 WHERE id = $1")
            .bind(tenant_id)
            .bind(status)
            .execute(&global)
            .await
            .expect("set status");

        assert!(
            auth::authenticate(&global, &key)
                .await
                .expect("query")
                .is_none(),
            "a key on a {status} tenant must not authenticate"
        );
    }

    // And it comes back when the tenant does: suspension is reversible, so it must not need the key reissued.
    sqlx::query("UPDATE dam_global.tenants SET status = 'active' WHERE id = $1")
        .bind(tenant_id)
        .execute(&global)
        .await
        .expect("reactivate");
    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_some(),
        "reactivating a tenant must restore its keys rather than requiring new ones"
    );
}

#[tokio::test]
async fn a_key_belonging_to_a_person_who_is_no_longer_one_is_refused() {
    // `identities.status` has existed since 0001 and `deprovisioned_at` since 0002, and this query looked at
    // neither — so disabling somebody did nothing at all, in every tenant they belonged to. That makes
    // deprovisioning a flag, which is precisely what a security questionnaire means when it asks how an
    // account is removed.
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let identity_id = identity(&global, "leaver@example.com").await;
    let key = issue(&global, tenant_id, identity_id, &[], None).await;

    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_some(),
        "the premise: this key works while the person does"
    );

    // Allowlisted rather than denylisted, so `invited` — a status nothing writes yet — refuses by default
    // instead of authenticating by omission.
    for status in ["disabled", "invited"] {
        sqlx::query("UPDATE dam_global.identities SET status = $2 WHERE id = $1")
            .bind(identity_id)
            .bind(status)
            .execute(&global)
            .await
            .expect("set status");
        assert!(
            auth::authenticate(&global, &key)
                .await
                .expect("query")
                .is_none(),
            "a key issued to a {status} identity must not authenticate"
        );
    }

    // Reversible, like a tenant suspension: re-enabling somebody must not need their key reissued.
    sqlx::query("UPDATE dam_global.identities SET status = 'active' WHERE id = $1")
        .bind(identity_id)
        .execute(&global)
        .await
        .expect("reactivate");
    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_some()
    );

    // Deprovisioning is the terminal one, and it refuses even while the status still reads active — the two
    // columns mean different things and either alone must be enough.
    sqlx::query("UPDATE dam_global.identities SET deprovisioned_at = now() WHERE id = $1")
        .bind(identity_id)
        .execute(&global)
        .await
        .expect("deprovision");
    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_none(),
        "a deprovisioned identity must not authenticate even with status active"
    );
}

#[tokio::test]
async fn a_machine_key_with_no_identity_still_authenticates() {
    // The identity check is a LEFT JOIN for exactly this: `api_keys.identity_id` is nullable, and an inner
    // join would have refused every machine integration in the fleet.
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, NULL, 'machine', $2, $3, '{}')",
    )
    .bind(tenant_id)
    .bind(key.prefix())
    .bind(key.hash())
    .execute(&global)
    .await
    .expect("issue");

    let found = auth::authenticate(&global, key.plaintext())
        .await
        .expect("query")
        .expect("a machine key authenticates");
    assert_eq!(found.identity_id, None);
}

#[tokio::test]
async fn a_valid_key_authenticates_to_its_tenant_and_identity() {
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let who = identity(&global, "alice@example.com").await;
    let key = issue(&global, tenant_id, who, &[], None).await;

    let authenticated = auth::authenticate(&global, &key)
        .await
        .expect("query")
        .expect("a valid key");
    assert_eq!(authenticated.tenant_id, tenant_id);
    assert_eq!(authenticated.tenant_slug.as_str(), "acme");
    assert_eq!(authenticated.identity_id, Some(who));
}

#[tokio::test]
async fn a_revoked_key_is_refused() {
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let who = identity(&global, "bob@example.com").await;
    let key = issue(&global, tenant_id, who, &[], None).await;
    sqlx::query("UPDATE dam_global.api_keys SET revoked_at = now()")
        .execute(&global)
        .await
        .expect("revoke");

    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_none(),
        "revocation must take effect immediately, not at the next expiry"
    );
}

#[tokio::test]
async fn an_expired_key_is_refused() {
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let who = identity(&global, "carol@example.com").await;
    let key = issue(&global, tenant_id, who, &[], Some(Duration::seconds(-1))).await;
    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_none()
    );
}

#[tokio::test]
async fn an_unknown_key_and_a_malformed_one_fail_the_same_way() {
    // Same outcome for "no such key" and "not even a key". Distinguishing them would tell a prober
    // which of their guesses had the right shape, and the shape is the cheap half to brute-force.
    let (_pg, global, _tenant, _tenant_id) = setup().await;
    for candidate in [
        "damrs_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "not-a-key",
        "",
        "damrs_",
    ] {
        assert!(
            auth::authenticate(&global, candidate)
                .await
                .expect("query")
                .is_none(),
            "{candidate:?} must be refused"
        );
    }
}

#[tokio::test]
async fn a_key_for_a_tenant_that_no_longer_exists_is_refused() {
    // `api_keys.tenant_id` cascades on delete, so this should be impossible — but authentication is the
    // wrong place to rely on a foreign key holding. The join is inner for that reason.
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let who = identity(&global, "dan@example.com").await;
    let key = issue(&global, tenant_id, who, &[], None).await;
    sqlx::query("DELETE FROM dam_global.tenants WHERE id = $1")
        .bind(tenant_id)
        .execute(&global)
        .await
        .expect("delete tenant");
    assert!(
        auth::authenticate(&global, &key)
            .await
            .expect("query")
            .is_none()
    );
}

#[tokio::test]
async fn last_used_at_is_recorded_but_not_on_every_single_request() {
    // The column exists for key hygiene — finding keys nobody uses. Writing it on every request turns a
    // read-only endpoint into a write, and one row of WAL per API call is a cost nobody chose. So it is
    // updated only when the recorded value is already stale.
    let (_pg, global, _tenant, tenant_id) = setup().await;
    let who = identity(&global, "erin@example.com").await;
    let key = issue(&global, tenant_id, who, &[], None).await;

    auth::authenticate(&global, &key).await.expect("first");
    let first: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM dam_global.api_keys")
            .fetch_one(&global)
            .await
            .expect("read");
    assert!(first.is_some(), "the first use must be recorded");

    auth::authenticate(&global, &key).await.expect("second");
    let second: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM dam_global.api_keys")
            .fetch_one(&global)
            .await
            .expect("read");
    assert_eq!(
        first, second,
        "a second use moments later must not write again"
    );
}

// ─── grants ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_role_with_the_column_defaults_grants_nothing() {
    // The dangerous default. 0001 carried a comment saying an empty `asset_group_ids` meant *all*
    // groups, which would make a role created with the defaults an all-access role. 0011 corrects the
    // comment; this asserts the behaviour, which is the part that actually protects anything.
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "frank@example.com").await;
    sqlx::query("INSERT INTO roles (id, key, label) VALUES (gen_random_uuid(), 'blank', 'Blank')")
        .execute(&tenant)
        .await
        .expect("insert bare role");
    make_member(&global, tenant_id, who, &["blank"], false).await;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &[])
        .await
        .expect("load grants");
    let predicate = policy::compile(&grants, Action::Read, Utc::now());
    assert!(!predicate.permits_action(), "no permissions were granted");
    assert!(!predicate.all_groups(), "and certainly not every group");
    assert!(predicate.matches_nothing());
}

#[tokio::test]
async fn a_role_with_permissions_but_no_groups_sees_nothing_rather_than_everything() {
    // The test above does not actually exercise the group semantics: its role has no permissions
    // either, so the predicate is empty for that reason and would pass however `asset_group_ids` were
    // read. This one grants the verb and no groups, which is the shape that distinguishes the two
    // readings — and it is the shape a half-configured role in production actually has.
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "lena@example.com").await;
    add_role(
        &tenant,
        "verb-only",
        &["asset:read", "asset:download"],
        &[],
        false,
    )
    .await;
    make_member(&global, tenant_id, who, &["verb-only"], false).await;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &[])
        .await
        .expect("load grants");
    let predicate = policy::compile(&grants, Action::Read, Utc::now());
    assert!(
        predicate.permits_action(),
        "the verb is granted — that half is not in question"
    );
    assert!(
        !predicate.all_groups(),
        "an empty asset_group_ids means NO groups; reading it as ALL would make a \
         half-configured role an all-access role"
    );
    assert!(predicate.matches_nothing(), "so it can see nothing");
}

#[tokio::test]
async fn several_roles_load_as_the_union_of_their_grants() {
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "grace@example.com").await;
    let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
    add_role(&tenant, "contributor", &["asset:read"], &[a, b], false).await;
    add_role(
        &tenant,
        "reviewer",
        &["asset:read", "asset:download"],
        &[b, c],
        false,
    )
    .await;
    make_member(&global, tenant_id, who, &["contributor", "reviewer"], false).await;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &[])
        .await
        .expect("load grants");
    let read = policy::compile(&grants, Action::Read, Utc::now());
    let mut groups = read.allowed_groups().to_vec();
    groups.sort();
    assert_eq!(groups, vec![a, b, c]);
    assert!(policy::compile(&grants, Action::Download, Utc::now()).permits_action());
    assert!(!policy::compile(&grants, Action::Manage, Utc::now()).permits_action());
}

#[tokio::test]
async fn a_role_name_that_does_not_exist_is_ignored_rather_than_failing_the_request() {
    // A membership can name a role that was since deleted. Failing the whole request would lock a user
    // out over an administrator's tidy-up; granting nothing for that name is the safe reading.
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "heidi@example.com").await;
    add_role(
        &tenant,
        "real",
        &["asset:read"],
        &[Uuid::from_u128(1)],
        false,
    )
    .await;
    make_member(&global, tenant_id, who, &["real", "deleted-role"], false).await;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &[])
        .await
        .expect("load grants");
    assert!(policy::compile(&grants, Action::Read, Utc::now()).permits_action());
}

#[tokio::test]
async fn a_tenant_admin_gets_every_group_without_a_role_row() {
    // `is_tenant_admin` is a shortcut on the membership, so it has to synthesise a grant. Per ABAC 5 it
    // bypasses group scoping — and per the same decision it still does not bypass expiry or a legal
    // hold, which `policy::evaluate` enforces rather than this loader.
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "admin@example.com").await;
    make_member(&global, tenant_id, who, &[], true).await;
    let _ = &tenant;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &[])
        .await
        .expect("load grants");
    for action in [Action::Read, Action::Download, Action::Manage] {
        let predicate = policy::compile(&grants, action, Utc::now());
        assert!(predicate.permits_action(), "{action:?}");
        assert!(predicate.all_groups(), "{action:?}");
    }
}

#[tokio::test]
async fn an_identity_with_no_membership_gets_nothing() {
    let (_pg, global, tenant, tenant_id) = setup().await;
    let stranger = identity(&global, "stranger@example.com").await;
    let grants = auth::grants_for(&global, &tenant, tenant_id, stranger, &[])
        .await
        .expect("load grants");
    assert!(grants.is_empty());
}

// ─── key scopes narrow, never widen ─────────────────────────────────────────

#[tokio::test]
async fn key_scopes_narrow_the_identitys_permissions() {
    // A key is a credential for a subset of what its owner can do — that is what makes it safe to paste
    // into a CI job. Scopes intersect; they never add.
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "ivan@example.com").await;
    add_role(
        &tenant,
        "power",
        &["asset:read", "asset:download", "asset:manage"],
        &[Uuid::from_u128(1)],
        false,
    )
    .await;
    make_member(&global, tenant_id, who, &["power"], false).await;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &["asset:read"])
        .await
        .expect("load grants");
    assert!(policy::compile(&grants, Action::Read, Utc::now()).permits_action());
    assert!(
        !policy::compile(&grants, Action::Download, Utc::now()).permits_action(),
        "a read-scoped key must not download"
    );
    assert!(!policy::compile(&grants, Action::Manage, Utc::now()).permits_action());
}

#[tokio::test]
async fn a_scope_the_identity_does_not_hold_grants_nothing() {
    // The direction that matters. If scopes were a union, pasting a broader scope into a key would
    // escalate its owner's own privileges.
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "judy@example.com").await;
    add_role(
        &tenant,
        "reader",
        &["asset:read"],
        &[Uuid::from_u128(1)],
        false,
    )
    .await;
    make_member(&global, tenant_id, who, &["reader"], false).await;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &["asset:manage"])
        .await
        .expect("load grants");
    for action in [Action::Read, Action::Download, Action::Manage] {
        assert!(
            !policy::compile(&grants, action, Utc::now()).permits_action(),
            "{action:?} must not be granted by a scope its owner lacks"
        );
    }
}

#[tokio::test]
async fn an_empty_scope_list_means_the_identitys_full_permissions() {
    // Empty means unscoped here, unlike `roles.asset_group_ids` where empty means none. The asymmetry is
    // uncomfortable but it is what each column's absence naturally means: a key with no scopes stated is
    // an unrestricted key, a role with no groups stated covers no groups.
    let (_pg, global, tenant, tenant_id) = setup().await;
    let who = identity(&global, "ken@example.com").await;
    add_role(
        &tenant,
        "reader",
        &["asset:read"],
        &[Uuid::from_u128(1)],
        false,
    )
    .await;
    make_member(&global, tenant_id, who, &["reader"], false).await;

    let grants = auth::grants_for(&global, &tenant, tenant_id, who, &[])
        .await
        .expect("load grants");
    assert!(policy::compile(&grants, Action::Read, Utc::now()).permits_action());
}
