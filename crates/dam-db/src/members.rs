//! Who has access to a tenant, and with what role (G10·2a).
//!
//! ## There was no way to add a colleague
//!
//! `tenant_members` is read by `caller` to compile a predicate, by `auth` to resolve roles, by `browse` and by
//! `comments` — and written in exactly one place: connector registration, inserting a service account. No
//! endpoint invited a person, granted a role, or removed somebody who had left. This is that surface.
//!
//! It has to land before SCIM rather than after. Built the other way round, an IdP would be the only way to
//! provision anybody — leaving a customer without one unable to add a second user — and SCIM drives these same
//! operations, so building it first means building them twice.
//!
//! ## An identity is global, and that is a disclosure problem
//!
//! `identities` has no tenant column and `identities_email_idx` is unique on the lowercased address, so one
//! person is one row across the whole fleet. Adding somebody therefore has to *find or create*, not insert.
//!
//! And the finding must not be visible. A 409 "that person already exists" would tell whoever is adding them
//! whether the address is already in the system — which, across tenants, is an oracle for "does this company
//! use damrs". So the answer is the same either way: a member was added. [`Added::identity_existed`] is
//! returned for the caller's own log and is deliberately not something the API surface repeats back.
//!
//! ## A role name that does not exist grants nothing, silently
//!
//! `tenant_members.role_names` is a `text[]` with no foreign key to `roles`, and `auth` ignores a name it
//! cannot resolve — which is the right behaviour there (a deleted role must not fail every request) and a trap
//! here. Granting `editors` when the role is called `editor` produces a member who can see nothing, with
//! nothing anywhere saying why. So the names are checked against the tenant's own `roles` before the
//! membership is written, and the unknown ones come back by name.
//!
//! ## Removal is the half that matters
//!
//! `0002_enterprise.sql` says it about SCIM and it is just as true here: "SSO alone leaves orphaned accounts
//! when someone leaves, which is exactly what a security questionnaire asks about." An account marked gone
//! that keeps its credentials is a flag, not a removal. So [`remove`] revokes the identity's keys for this
//! tenant, drops the membership, and reports how many keys it revoked — a number a screen can show, because
//! "removed" with no effect is indistinguishable from "removed" with one.
//!
//! **The identity is only disabled if this was their last tenant.** Somebody who works with two customers of
//! the same deployment must not lose their other account because one of them let them go, and
//! `identities.deprovisioned_at` is global. Checking the remaining memberships is what makes that safe.
//!
//! ## The membership and its audit entry are one transaction
//!
//! `tenant_members` is in `dam_global` and `audit_log` is in the tenant schema, and the connector
//! registration path treats that as a reason they cannot be atomic — "the identity, membership and key live in
//! the control plane, so they cannot be in the tenant transaction". That is true of two *databases*. These are
//! two schemas in one, so a transaction opened by `TenantConn` reaches both, and every function here takes a
//! connection rather than a pool so its caller's transaction covers the governance entry as well as the
//! change.
//!
//! It matters because the alternative has no good ordering. Audit first and a failed write leaves a permanent
//! record, in an append-only log, of a grant that never happened; effect first and a failed write leaves a
//! grant with nothing saying who made it. Neither is correctable. One transaction removes the choice.
//!
//! ## A connected site is not a colleague
//!
//! Registering a site creates an identity, a membership, a role and a key — deliberately, so a connector goes
//! through the same access predicate as everybody else. The consequence nobody would predict is that a
//! website then appears in the list of people, with the same controls: change its roles, or remove it. Both
//! are wrong. Changing them would hand a website an editor role or take away the one that makes it work, and
//! removing it would revoke its key while the Sites screen went on listing it as connected.
//!
//! So they are excluded, by joining `connectors` on `api_key_id` rather than by matching the
//! `connector+…@connectors.invalid` address. The join is the fact; the address is a convention that a later
//! change could break silently.
//!
//! This came from reading the dev tenant's real list. A fixture would never have contained one.
//!
//! ## The last administrator cannot be removed, and cannot demote themselves
//!
//! A tenant with no administrator is a tenant nobody can add one to — recoverable only by an operator with
//! database access, which is a support ticket that should never have to exist. Both directions are refused,
//! because demotion reaches the same state as removal and a rule that only guarded one of them would be a rule
//! with a documented workaround.
//!
//! It is a check on a *set*, so a row lock on the membership being changed does not protect it: two
//! administrators stepping down at once each count two and each see somebody remaining. Membership changes
//! therefore serialise per tenant on an advisory lock taken before anything is read — see
//! [`lock_memberships`], including why locking the administrators' rows instead would trade the race for a
//! deadlock.

use crate::Error;
use crate::auth::ApiKey;
use chrono::{DateTime, Utc};
use sqlx::Connection as _;
use sqlx::{PgConnection, Row as _};
use uuid::Uuid;

/// Why a change was refused.
#[derive(Debug, thiserror::Error)]
pub enum MemberRefusal {
    #[error("`{0}` is not an email address")]
    EmailInvalid(String),

    /// Named, because "one of your role names is wrong" sends somebody to check all of them.
    #[error("no such role: {}", .0.join(", "))]
    UnknownRoles(Vec<String>),

    #[error("that person is already a member of this tenant")]
    AlreadyAMember,

    #[error("no such member")]
    NotAMember,

    /// A tenant with no administrator cannot appoint one.
    #[error("this is the tenant's only administrator")]
    LastAdmin,

    /// 0002: "SCIM-managed identities must not be editable in the damrs UI, or the IdP will overwrite local
    /// edits on next sync and the customer will report it as data loss."
    #[error("this account is managed by your identity provider; change it there")]
    ScimManaged,

    #[error(transparent)]
    Database(#[from] Error),
}

impl From<sqlx::Error> for MemberRefusal {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(Error::from(error))
    }
}

/// One person's access to one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub identity_id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub role_names: Vec<String>,
    pub is_tenant_admin: bool,
    /// `identities.status` — and since `auth` now allowlists `active`, a non-active value here means their
    /// keys do not work.
    pub status: String,
    pub scim_managed: bool,
    pub last_login_at: Option<DateTime<Utc>>,
    /// How many of their keys for *this* tenant still work.
    ///
    /// Shown because it is the difference between an account that has been removed and one that has been
    /// marked removed. A member with live keys and no roles can still authenticate.
    pub live_keys: i64,
    pub joined_at: DateTime<Utc>,
}

/// Everyone with access to this tenant, alphabetically by address.
///
/// By address rather than by display name, which is nullable: a null sorting first or last would order the
/// list by accident rather than by decision.
///
/// A connected site's service account is not in here — see the module note on why a website appearing among
/// the people, with a "change roles" button, is worse than it sounds.
pub async fn list(conn: &mut PgConnection, tenant_id: Uuid) -> Result<Vec<Member>, Error> {
    let rows = sqlx::query(
        "SELECT i.id, i.email, i.display_name, m.role_names, m.is_tenant_admin, i.status, \
                m.scim_managed, i.last_login_at, m.created_at, \
                (SELECT count(*) FROM dam_global.api_keys k \
                 WHERE k.identity_id = i.id AND k.tenant_id = m.tenant_id \
                   AND k.revoked_at IS NULL \
                   AND (k.expires_at IS NULL OR k.expires_at > now())) AS live_keys \
         FROM dam_global.tenant_members m \
         JOIN dam_global.identities i ON i.id = m.identity_id \
         WHERE m.tenant_id = $1 \
           AND NOT EXISTS ( \
               SELECT 1 FROM connectors c \
               JOIN dam_global.api_keys ck ON ck.id = c.api_key_id \
               WHERE ck.identity_id = i.id) \
         ORDER BY i.email_lower",
    )
    .bind(tenant_id)
    .fetch_all(conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(Member {
                identity_id: row.try_get("id")?,
                email: row.try_get("email")?,
                display_name: row.try_get("display_name")?,
                role_names: row.try_get("role_names")?,
                is_tenant_admin: row.try_get("is_tenant_admin")?,
                status: row.try_get("status")?,
                scim_managed: row.try_get("scim_managed")?,
                last_login_at: row.try_get("last_login_at")?,
                live_keys: row.try_get("live_keys")?,
                joined_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// The prefix `connectors::register` gives the role it creates for a connected site.
///
/// Not shared from that module because the dependency would point the wrong way — `dam_db` cannot reach
/// `dam_api` — so it is asserted against the real format in the tests instead.
const CONNECTOR_ROLE_PREFIX: &str = "connector:";

/// The role keys a person can be given.
///
/// Takes a tenant connection because `roles` is a tenant table while the memberships are control plane.
///
/// **Connector roles are excluded**, and that came from reading the real list rather than a fixture: every
/// registered site creates a role called `connector:<uuid>`, scoped to the asset groups that one site may
/// render. Offering those alongside `editor` and `viewer` invites somebody to grant a person a role that
/// exists to describe a machine — and the dev tenant already had two of them, sorted into the middle of the
/// list where they look like they belong.
pub async fn known_roles(conn: &mut PgConnection) -> Result<Vec<String>, Error> {
    Ok(
        sqlx::query_scalar("SELECT key FROM roles WHERE key NOT LIKE $1 || '%' ORDER BY key")
            .bind(CONNECTOR_ROLE_PREFIX)
            .fetch_all(conn)
            .await?,
    )
}

/// Which of `wanted` this tenant does not define.
#[must_use]
pub fn unknown_roles(wanted: &[String], known: &[String]) -> Vec<String> {
    let mut missing: Vec<String> = wanted
        .iter()
        .filter(|name| !known.iter().any(|k| k == *name))
        .cloned()
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}

/// Somebody to add.
#[derive(Debug, Clone)]
pub struct NewMember {
    pub email: String,
    pub display_name: Option<String>,
    pub role_names: Vec<String>,
    pub is_tenant_admin: bool,
}

/// What adding somebody produced.
#[derive(Debug)]
pub struct Added {
    pub identity_id: Uuid,
    /// The credential, in readable form, once.
    ///
    /// There is no login flow in this system — the web application authenticates with an API key — so the key
    /// *is* the invitation. Setting `identities.status = 'invited'` instead would be marking a state nothing
    /// can leave.
    pub api_key: String,
    /// Whether the person already had an identity, from another tenant or an earlier membership here.
    ///
    /// For the caller's own log. Not for a response body: across tenants it answers "does this company use
    /// damrs" about an address somebody merely typed.
    pub identity_existed: bool,
}

/// Adds somebody to a tenant and issues them a credential.
///
/// Roles are *not* validated here — [`known_roles`] and [`unknown_roles`] do that, against a tenant
/// connection this function does not hold. Splitting it that way keeps the check where the data is rather than
/// giving this module a second pool.
pub async fn add(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    new: &NewMember,
) -> Result<Added, MemberRefusal> {
    let mut tx = conn.begin().await?;
    let attached = attach(&mut tx, tenant_id, new, Reactivate::UnlessProviderOwned).await?;

    let api_key = ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, '{}')",
    )
    .bind(tenant_id)
    .bind(attached.identity_id)
    .bind(format!("issued with access for {}", new.email.trim()))
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Added {
        identity_id: attached.identity_id,
        api_key: api_key.into_plaintext(),
        identity_existed: attached.identity_existed,
    })
}

/// Whether a disabled identity is brought back, and on whose authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reactivate {
    /// A person adding somebody by hand. An account the identity provider owns is left alone: the provider may
    /// have deprovisioned them for a reason, and the next sync would undo the change anyway.
    UnlessProviderOwned,
    /// The identity provider itself, which *is* the authority for an account it owns.
    Always,
}

/// A membership, without a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attached {
    pub identity_id: Uuid,
    pub identity_existed: bool,
}

/// Attach somebody to a tenant, creating their identity if this deployment has never seen them.
///
/// **No credential.** [`add`] wraps this and mints one, because there is no login flow and a membership with
/// nothing to authenticate with is inert. SCIM provisioning deliberately does *not*: the identity provider
/// signs its people in, and putting an API key in a SCIM response would hand the provider a bearer token for a
/// person, into its own logs, for an account it does not authenticate with. See `crate::scim`, which also
/// documents what that leaves not working until SSO exists.
pub async fn attach(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    new: &NewMember,
    reactivate: Reactivate,
) -> Result<Attached, MemberRefusal> {
    let email = new.email.trim();
    // Deliberately shallow. A full grammar rejects addresses that work, and the authoritative check is
    // whether mail arrives — which nothing here can do. This catches the typo that would otherwise become a
    // permanent row.
    if !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        return Err(MemberRefusal::EmailInvalid(email.to_owned()));
    }

    // A `BEGIN` when the caller is not in a transaction and a `SAVEPOINT` when they are — the same
    // arrangement `audit::record` uses, and for the same reason: the read and the write have to be one unit
    // whether or not the caller remembered to make them one.
    let mut tx = conn.begin().await?;

    // Find or create, on the lowercased address, because that is what the unique index is on. A plain insert
    // would fail for anybody who already exists anywhere in the fleet.
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM dam_global.identities WHERE email_lower = lower($1)")
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;

    let (identity_id, identity_existed) = match existing {
        Some(id) => (id, true),
        None => {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO dam_global.identities (id, email, display_name) \
                 VALUES (gen_random_uuid(), $1, $2) RETURNING id",
            )
            .bind(email)
            .bind(new.display_name.as_deref())
            .fetch_one(&mut *tx)
            .await?;
            (id, false)
        }
    };

    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM dam_global.tenant_members \
         WHERE tenant_id = $1 AND identity_id = $2)",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .fetch_one(&mut *tx)
    .await?;
    if already {
        // Rolled back, so a re-add of an existing member does not leave a newly created identity behind.
        tx.rollback().await?;
        return Err(MemberRefusal::AlreadyAMember);
    }

    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .bind(&new.role_names)
    .bind(new.is_tenant_admin)
    .execute(&mut *tx)
    .await?;

    // Somebody re-added after being removed still has their old identity row, and it may be disabled — which,
    // since `auth` allowlists `active`, would mean a membership whose keys do not work.
    //
    // Whose authority applies is the caller's to state. A person adding somebody by hand does not get to
    // re-enable an account the identity provider owns: the provider may have deprovisioned them for a reason,
    // and the next sync would undo the change anyway. The provider itself does, because for an account it owns
    // it *is* the authority. `Reactivate` makes that a decision rather than a default.
    //
    // The check is on *this tenant's* membership, not on the identity: since `0006` the link is per-tenant,
    // because a person provisioned by one customer's provider is an ordinary colleague in another's, where an
    // administrator is the only authority there is.
    let provider_owned: bool = sqlx::query_scalar(
        "SELECT coalesce(bool_or(scim_managed), false) FROM dam_global.tenant_members \
         WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .fetch_one(&mut *tx)
    .await?;
    if reactivate == Reactivate::Always || !provider_owned {
        sqlx::query(
            "UPDATE dam_global.identities \
         SET status = 'active', deprovisioned_at = NULL, updated_at = now() \
         WHERE id = $1",
        )
        .bind(identity_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(Attached {
        identity_id,
        identity_existed,
    })
}

/// What somebody held before a change.
///
/// Returned rather than left to the caller to look up, because the lookup has to happen *before* the write and
/// a caller doing it afterwards on another connection would be relying on isolation semantics to get the old
/// value back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub role_names: Vec<String>,
    pub is_tenant_admin: bool,
}

/// Replaces a member's roles and administrator flag.
///
/// A replacement rather than a patch, because the set is what a screen shows and a partial update of a
/// `text[]` is two round trips racing each other.
pub async fn set_roles(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    identity_id: Uuid,
    role_names: &[String],
    is_tenant_admin: bool,
) -> Result<Held, MemberRefusal> {
    let mut tx = conn.begin().await?;
    // Before the read, not after: see `lock_memberships` on why a row lock here is the wrong shape.
    lock_memberships(&mut tx, tenant_id).await?;

    let row = sqlx::query(
        "SELECT m.role_names, m.is_tenant_admin, m.scim_managed \
         FROM dam_global.tenant_members m \
         WHERE m.tenant_id = $1 AND m.identity_id = $2",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(MemberRefusal::NotAMember);
    };
    let previous: Vec<String> = row.try_get("role_names")?;
    let was_admin: bool = row.try_get("is_tenant_admin")?;
    let scim_managed: bool = row.try_get("scim_managed")?;
    if scim_managed {
        return Err(MemberRefusal::ScimManaged);
    }

    // Demotion reaches the same state as removal, so it is refused for the same reason — and the count locks
    // every administrator's row, not just this one, so two of them cannot step down at the same moment each
    // believing the other remains.
    if was_admin && !is_tenant_admin && admin_count(&mut tx, tenant_id).await? <= 1 {
        return Err(MemberRefusal::LastAdmin);
    }

    sqlx::query(
        "UPDATE dam_global.tenant_members \
         SET role_names = $3, is_tenant_admin = $4, updated_at = now() \
         WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .bind(role_names)
    .bind(is_tenant_admin)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Held {
        role_names: previous,
        is_tenant_admin: was_admin,
    })
}

/// What removing somebody actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// Keys revoked for this tenant. The number that makes "removed" mean something.
    pub keys_revoked: u64,
    /// Whether the identity itself was disabled, which happens only when this was their last tenant.
    pub identity_disabled: bool,
    pub roles_held: Vec<String>,
    pub was_tenant_admin: bool,
}

/// Removes somebody's access to a tenant.
pub async fn remove(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    identity_id: Uuid,
) -> Result<Removed, MemberRefusal> {
    let mut tx = conn.begin().await?;
    lock_memberships(&mut tx, tenant_id).await?;

    let row = sqlx::query(
        "SELECT role_names, is_tenant_admin FROM dam_global.tenant_members \
         WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Err(MemberRefusal::NotAMember);
    };
    let roles_held: Vec<String> = row.try_get("role_names")?;
    let was_tenant_admin: bool = row.try_get("is_tenant_admin")?;

    if was_tenant_admin && admin_count(&mut tx, tenant_id).await? <= 1 {
        return Err(MemberRefusal::LastAdmin);
    }

    // Keys first. Everything after this can fail and leave an account that cannot get in; the other order
    // leaves one that can.
    let keys_revoked = sqlx::query(
        "UPDATE dam_global.api_keys SET revoked_at = now() \
         WHERE tenant_id = $1 AND identity_id = $2 AND revoked_at IS NULL",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    sqlx::query("DELETE FROM dam_global.tenant_members WHERE tenant_id = $1 AND identity_id = $2")
        .bind(tenant_id)
        .bind(identity_id)
        .execute(&mut *tx)
        .await?;

    // Only if this was their last tenant. `deprovisioned_at` is global, and somebody who works with two
    // customers of the same deployment must not lose their other account because one of them let them go.
    let elsewhere: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM dam_global.tenant_members WHERE identity_id = $1)",
    )
    .bind(identity_id)
    .fetch_one(&mut *tx)
    .await?;
    let identity_disabled = if elsewhere {
        false
    } else {
        sqlx::query(
            "UPDATE dam_global.identities \
             SET status = 'disabled', deprovisioned_at = now(), updated_at = now() \
             WHERE id = $1",
        )
        .bind(identity_id)
        .execute(&mut *tx)
        .await?;
        true
    };

    tx.commit().await?;
    Ok(Removed {
        keys_revoked,
        identity_disabled,
        roles_held,
        was_tenant_admin,
    })
}

/// Serialise membership changes for one tenant, for the rest of the transaction.
///
/// The last-administrator rule is a check on a *set*, and `FOR UPDATE` on the row being changed does not
/// protect it: two administrators stepping down at the same moment each count two, each see somebody
/// remaining, and the tenant ends with none — recoverable only by an operator with database access.
///
/// Row locks over the whole administrator set would close that and introduce a deadlock instead, because a
/// demotion and a removal would each hold one administrator's row while waiting for the other's. An advisory
/// lock taken *before* anything is read has no such ordering to get wrong.
///
/// The two-argument form, which has its own keyspace: `audit::record`'s lock uses the one-argument form, so
/// the two can never collide however their keys hash. The first key names the resource, the second the tenant.
///
/// Not taken by [`add`], which cannot reduce the count: a concurrent demotion either sees the new
/// administrator or does not, and not seeing them refuses — the safe direction.
async fn lock_memberships(conn: &mut PgConnection, tenant_id: Uuid) -> Result<(), Error> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('dam_global.tenant_members'), hashtext($1::text))",
    )
    .bind(tenant_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// How many administrators this tenant has.
///
/// Only meaningful under [`lock_memberships`], which every caller takes first.
async fn admin_count(conn: &mut PgConnection, tenant_id: Uuid) -> Result<i64, Error> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.tenant_members \
         WHERE tenant_id = $1 AND is_tenant_admin",
    )
    .bind(tenant_id)
    .fetch_one(conn)
    .await?)
}
