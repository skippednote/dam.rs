//! SCIM 2.0 provisioning, from the database side (G10·2b).
//!
//! `scim_clients` has existed since migration `0002_enterprise.sql` with nothing reading it, and
//! `scim_external_id`, `scim_managed` and `deprovisioned_at` beside it in the same state. 0002 says
//! why it matters: "SCIM 2.0 is an RFP pass/fail item, and the deprovisioning half is the part that matters:
//! SSO alone leaves orphaned accounts when someone leaves, which is exactly what a security questionnaire
//! asks about."
//!
//! ## Provisioning creates an account nobody can yet sign into, and that is deliberate
//!
//! There is no login flow in this deployment: a person authenticates with an API key. `members::add` therefore
//! mints one, because a membership with no credential is inert.
//!
//! SCIM must not do that. The identity provider is the thing that signs its people in, and putting an API key
//! in a SCIM response would hand the provider a long-lived bearer token for a person — into the provider's own
//! logs, for an account the provider does not authenticate with, and with nobody to give it to. So
//! [`provision`] creates the identity, the membership and the roles, and no credential.
//!
//! **The honest consequence: until SSO exists, a SCIM-provisioned person has access and no way to exercise
//! it.** An administrator can issue them a key from the People screen, which is the interim answer. The
//! *deprovisioning* half is unaffected and complete — it revokes whatever credentials exist and removes the
//! membership — which is the half a security questionnaire asks about and the half that is dangerous to fake.
//!
//! ## The link belongs to the membership, and it took two migrations to get there
//!
//! 0002 put `scim_external_id` and `scim_managed` on `identities` — one row per person across the whole
//! deployment — and indexed the id uniquely across the entire table. Two things were wrong with that, and only
//! the first was obvious.
//!
//! `0005_scim_client_scope.sql` fixed the obvious one: customers' providers number their users independently,
//! Okta's default `externalId` is an opaque per-org id, so the second tenant to provision a colliding id got a
//! constraint violation in a sync they do not control. Scoping the index to the client fixed the collision.
//!
//! `0006_scim_link_is_per_tenant.sql` fixed what was underneath. The columns were still single-valued on a
//! shared row, so two tenants' providers provisioning the same person overwrote each other's link — the second
//! silently took ownership and the first tenant's sync then failed its own [`ours`] check. And `scim_managed`
//! made somebody uneditable *everywhere*: provisioned by one customer's provider, they could no longer have
//! their roles changed by an administrator in another, where no provider manages them at all.
//!
//! So the three columns live on `tenant_members`, keyed exactly right. `status` and `deprovisioned_at` stay on
//! `identities`, which is not an inconsistency: whether somebody's account works is global, and who provisions
//! them is not.
//!
//! ## A SCIM client is not a tenant member, and authenticates on its own path
//!
//! This codebase deliberately has one place where access is decided, so a second authenticator needs saying
//! out loud rather than arriving as a convenience. A SCIM client holds no ABAC predicate and manages
//! identities rather than reading assets: there is nothing for `caller::authorize` to compile, and giving one a
//! membership so it could go through that path would make it a *user of* the library it administers. So it has
//! its own token, its own hash domain, and reaches only `/scim/v2`.
//!
//! ## `last_sync_at` is written by the reads, not only the writes
//!
//! Two more columns 0002 declared and nothing filled. A provider that has stopped syncing looks identical to
//! one that never started unless something records the contact, and the most common provider request is a
//! `GET` — so recording only mutations would leave a healthy integration looking dead.

use crate::Error;
use crate::members::{self, NewMember, Reactivate};
use chrono::{DateTime, Utc};
use sqlx::Connection as _;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder, Row as _};
use uuid::Uuid;

/// The SCIM resources a client may manage, as `scim_clients.scopes` spells them.
pub const USERS: &str = "Users";
pub const GROUPS: &str = "Groups";

/// A provisioning client, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_slug: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_sync_status: Option<String>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Client {
    #[must_use]
    pub fn may(&self, resource: &str) -> bool {
        self.scopes.iter().any(|scope| scope == resource)
    }
}

/// A client's bearer token.
///
/// Its own hash domain rather than `auth::ApiKey`'s, so a digest from `scim_clients.token_hash` can never
/// collide with one from `api_keys.key_hash`. They are different credential classes reaching different
/// surfaces, and the two columns living in one database is exactly the kind of thing that changes.
pub struct Token {
    plaintext: String,
    hash: String,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the plaintext. A token that reaches a log is a token that has to be rotated, and `{:?}` on a
        // struct is how that happens.
        f.debug_struct("Token")
            .field("plaintext", &"[REDACTED]")
            .finish()
    }
}

impl Token {
    const SECRET_BYTES: usize = 32;

    #[must_use]
    pub fn generate() -> Self {
        let bytes: [u8; Self::SECRET_BYTES] = rand::random();
        let secret = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        // Prefixed like an API key so a secret scanner has something to match on, and distinguishably so
        // whoever finds one knows which surface to revoke it from.
        let plaintext = format!("damrs_scim_{secret}");
        Self {
            hash: Self::hash_of(&plaintext),
            plaintext,
        }
    }

    #[must_use]
    pub fn hash_of(plaintext: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"damrs-scim-token-v1\0");
        hasher.update(plaintext.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    #[must_use]
    pub fn into_plaintext(self) -> String {
        self.plaintext
    }
}

/// Registers a provisioning client and returns its token once.
pub async fn issue(
    global: &PgPool,
    tenant_id: Uuid,
    label: &str,
    scopes: &[String],
) -> Result<(Uuid, String), Error> {
    let token = Token::generate();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.scim_clients (id, tenant_id, label, token_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4) RETURNING id",
    )
    .bind(tenant_id)
    .bind(label.trim())
    .bind(token.hash())
    .bind(scopes)
    .fetch_one(global)
    .await?;
    Ok((id, token.into_plaintext()))
}

/// Authenticates a presented SCIM token.
///
/// `Ok(None)` for every reason it does not work — unknown, revoked, or belonging to a tenant that is not
/// active — because telling a prober which of their guesses had the right shape hands them the cheap half of
/// the search. The same rule `auth::authenticate` follows, for the same reason.
pub async fn authenticate(global: &PgPool, presented: &str) -> Result<Option<Client>, Error> {
    let hash = Token::hash_of(presented);
    let row = sqlx::query(
        "SELECT c.id, c.tenant_id, t.slug, c.label, c.scopes, c.last_sync_at, c.last_sync_status, \
                c.revoked_at \
         FROM dam_global.scim_clients c \
         JOIN dam_global.tenants t ON t.id = c.tenant_id \
         WHERE c.token_hash = $1 AND c.revoked_at IS NULL AND t.status = 'active'",
    )
    .bind(&hash)
    .fetch_optional(global)
    .await?;

    let Some(row) = row else { return Ok(None) };
    Ok(Some(Client {
        id: row.try_get("id")?,
        tenant_id: row.try_get("tenant_id")?,
        tenant_slug: row.try_get("slug")?,
        label: row.try_get("label")?,
        scopes: row.try_get("scopes")?,
        last_sync_at: row.try_get("last_sync_at")?,
        last_sync_status: row.try_get("last_sync_status")?,
        revoked_at: row.try_get("revoked_at")?,
    }))
}

/// Revoking is terminal: a leaked provisioning token can create and delete accounts.
pub async fn revoke(global: &PgPool, id: Uuid) -> Result<bool, Error> {
    Ok(
        sqlx::query("UPDATE dam_global.scim_clients SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .execute(global)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Records that a provider made contact, and how it went.
///
/// Called from reads as well as writes — see the module note. Never fails a request: a provider whose sync
/// worked must not be told it failed because we could not write down that it worked.
pub async fn record_contact(global: &PgPool, id: Uuid, status: &str) {
    if let Err(error) = sqlx::query(
        "UPDATE dam_global.scim_clients SET last_sync_at = now(), last_sync_status = $2 WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .execute(global)
    .await
    {
        tracing::warn!(%error, client = %id, "recording a SCIM contact");
    }
}

/// Every provisioning client a tenant has, newest first.
pub async fn list(global: &PgPool, tenant_id: Uuid) -> Result<Vec<Client>, Error> {
    let rows = sqlx::query(
        "SELECT c.id, c.tenant_id, t.slug, c.label, c.scopes, c.last_sync_at, c.last_sync_status, \
                c.revoked_at \
         FROM dam_global.scim_clients c \
         JOIN dam_global.tenants t ON t.id = c.tenant_id \
         WHERE c.tenant_id = $1 ORDER BY c.created_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(global)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Client {
                id: row.try_get("id")?,
                tenant_id: row.try_get("tenant_id")?,
                tenant_slug: row.try_get("slug")?,
                label: row.try_get("label")?,
                scopes: row.try_get("scopes")?,
                last_sync_at: row.try_get("last_sync_at")?,
                last_sync_status: row.try_get("last_sync_status")?,
                revoked_at: row.try_get("revoked_at")?,
            })
        })
        .collect()
}

/// A provisioned person, in the terms SCIM asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// Ours. SCIM's `id`, and stable for the life of the account.
    pub identity_id: Uuid,
    /// The provider's. SCIM's `externalId`, unique per client.
    pub external_id: Option<String>,
    /// SCIM's `userName`. The email, because that is what `identities` is unique on and what every provider
    /// sends.
    pub user_name: String,
    pub display_name: Option<String>,
    /// SCIM's `active`. False once deprovisioned, and `auth` refuses a key for either state.
    pub active: bool,
    pub roles: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Why a provisioning call was refused.
#[derive(Debug, thiserror::Error)]
pub enum ScimRefusal {
    #[error("no such user")]
    NoSuchUser,

    /// SCIM's `uniqueness` error type: this provider already provisioned that person.
    #[error("a user with that userName already exists")]
    AlreadyProvisioned,

    /// A person this tenant added by hand, or another provider owns. Taking them over silently would make one
    /// provider's sync quietly reassign another's account.
    #[error("that person is already a member of this tenant and is not managed by this provider")]
    NotOurs,

    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Member(#[from] members::MemberRefusal),

    #[error(transparent)]
    Database(#[from] Error),
}

impl From<sqlx::Error> for ScimRefusal {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(Error::from(error))
    }
}

/// What a provider is asking us to create.
#[derive(Debug, Clone)]
pub struct NewUser {
    pub user_name: String,
    pub external_id: Option<String>,
    pub display_name: Option<String>,
    pub active: bool,
    /// Roles, if the provider maps them. Validated by the caller against the tenant's own.
    pub roles: Vec<String>,
}

/// The columns every user read selects.
///
/// Pushed through a `QueryBuilder` rather than interpolated into a `format!`: sqlx refuses a non-static SQL
/// string at the type level, which is the guardrail that stops a filter value ever reaching the statement as
/// text. See `assets::visible_among` for the same pattern.
const USER_COLUMNS: &str = "SELECT i.id, i.email, i.display_name, i.status, m.scim_external_id, \
                                   i.deprovisioned_at, i.created_at, i.updated_at, m.role_names \
                            FROM dam_global.identities i \
                            JOIN dam_global.tenant_members m ON m.identity_id = i.id \
                            WHERE m.tenant_id = ";

fn to_user(row: &sqlx::postgres::PgRow) -> Result<User, Error> {
    let status: String = row.try_get("status")?;
    let deprovisioned: Option<DateTime<Utc>> = row.try_get("deprovisioned_at")?;
    Ok(User {
        identity_id: row.try_get("id")?,
        external_id: row.try_get("scim_external_id")?,
        user_name: row.try_get("email")?,
        display_name: row.try_get("display_name")?,
        // The same rule `auth` enforces, so `active` cannot claim something the credential path contradicts.
        active: status == "active" && deprovisioned.is_none(),
        roles: row.try_get("role_names")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// One user by our id.
pub async fn by_id(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    identity_id: Uuid,
) -> Result<Option<User>, Error> {
    let row = sqlx::query(
        "SELECT i.id, i.email, i.display_name, i.status, m.scim_external_id, \
                i.deprovisioned_at, i.created_at, i.updated_at, m.role_names \
         FROM dam_global.identities i \
         JOIN dam_global.tenant_members m ON m.identity_id = i.id \
         WHERE m.tenant_id = $1 AND i.id = $2",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .fetch_optional(conn)
    .await?;
    row.as_ref().map(to_user).transpose()
}

/// One page, in SCIM's 1-based terms.
#[derive(Debug, Clone)]
pub struct Page {
    pub users: Vec<User>,
    pub total: i64,
}

/// The filters a provider actually sends.
///
/// Not a SCIM filter grammar. The specification's filter language is large and providers use a sliver of it:
/// `userName eq "x"` to decide whether to create or update, and `externalId eq "x"` to re-find somebody.
/// Implementing the grammar would be a parser nothing exercises; implementing these two is the integration.
/// Anything else is refused by name rather than silently ignored, because a filter we drop is a provider
/// receiving the whole directory and concluding every user already matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    All,
    UserName(String),
    ExternalId(String),
}

/// The most a provider gets in one page, however much it asks for.
///
/// A provider that requests everything is a provider that will retry; one that receives a hundred thousand
/// rows in a single response is a timeout on both sides.
const MAX_PAGE: i64 = 200;

fn push_filter(builder: &mut QueryBuilder<Postgres>, filter: &Filter) {
    match filter {
        Filter::All => {}
        Filter::UserName(value) => {
            builder.push(" AND i.email_lower = lower(");
            builder.push_bind(value.clone());
            builder.push(")");
        }
        Filter::ExternalId(value) => {
            builder.push(" AND m.scim_external_id = ");
            builder.push_bind(value.clone());
        }
    }
}

pub async fn page(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    filter: &Filter,
    start_index: i64,
    count: i64,
) -> Result<Page, Error> {
    // SCIM's `startIndex` is 1-based, so an offset needs the shift and a provider sending 0 or a negative
    // must not wrap into the end of the list.
    let offset = start_index.max(1) - 1;
    let limit = count.clamp(0, MAX_PAGE);

    let mut counting: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT count(*) FROM dam_global.identities i \
         JOIN dam_global.tenant_members m ON m.identity_id = i.id \
         WHERE m.tenant_id = ",
    );
    counting.push_bind(tenant_id);
    push_filter(&mut counting, filter);
    let total: i64 = counting.build_query_scalar().fetch_one(&mut *conn).await?;

    if limit == 0 {
        // A provider asking for zero wants the count, which is how Entra checks existence without paging.
        return Ok(Page {
            users: Vec::new(),
            total,
        });
    }

    let mut listing: QueryBuilder<Postgres> = QueryBuilder::new(USER_COLUMNS);
    listing.push_bind(tenant_id);
    push_filter(&mut listing, filter);
    listing.push(" ORDER BY i.email_lower LIMIT ");
    listing.push_bind(limit);
    listing.push(" OFFSET ");
    listing.push_bind(offset);
    let rows = listing.build().fetch_all(&mut *conn).await?;
    Ok(Page {
        users: rows.iter().map(to_user).collect::<Result<Vec<_>, _>>()?,
        total,
    })
}

/// Creates a person, or refuses because somebody already owns that account.
pub async fn provision(
    conn: &mut PgConnection,
    client: &Client,
    new: &NewUser,
) -> Result<User, ScimRefusal> {
    let user_name = new.user_name.trim();
    if user_name.is_empty() {
        return Err(ScimRefusal::Invalid("userName is required".to_owned()));
    }

    let mut tx = conn.begin().await?;

    // Whether this tenant already knows them, and if so who owns the account. A provider must not silently
    // take over somebody a person added by hand, or somebody another provider manages: one sync would
    // reassign the other's account and the two would fight over it every cycle.
    let existing: Option<(Uuid, bool, Option<Uuid>)> = sqlx::query_as(
        "SELECT i.id, m.scim_managed, m.scim_client_id \
         FROM dam_global.identities i \
         JOIN dam_global.tenant_members m ON m.identity_id = i.id \
         WHERE m.tenant_id = $1 AND i.email_lower = lower($2)",
    )
    .bind(client.tenant_id)
    .bind(user_name)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some((_, managed, owner)) = existing {
        if managed && owner == Some(client.id) {
            return Err(ScimRefusal::AlreadyProvisioned);
        }
        return Err(ScimRefusal::NotOurs);
    }

    let attached = members::attach(
        &mut tx,
        client.tenant_id,
        &NewMember {
            email: user_name.to_owned(),
            display_name: new.display_name.clone(),
            role_names: new.roles.clone(),
            is_tenant_admin: false,
        },
        // The provider is the authority for an account it owns, so it may bring one of its own back.
        Reactivate::Always,
    )
    .await?;

    sqlx::query(
        "UPDATE dam_global.tenant_members \
         SET scim_managed = true, scim_client_id = $3, scim_external_id = $4, updated_at = now() \
         WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(client.tenant_id)
    .bind(attached.identity_id)
    .bind(client.id)
    .bind(new.external_id.as_deref())
    .execute(&mut *tx)
    .await?;

    if !new.active {
        // A provider may create somebody already disabled. Honoured rather than ignored: creating them active
        // and waiting for a second call would grant access the provider did not ask for.
        set_active_on(&mut tx, client, attached.identity_id, false).await?;
    }

    let user = by_id(&mut tx, client.tenant_id, attached.identity_id)
        .await?
        .ok_or(ScimRefusal::NoSuchUser)?;
    tx.commit().await?;
    Ok(user)
}

/// Enables or disables a provisioned person.
///
/// `active: false` is how Entra deprovisions — it PATCHes rather than DELETEs — so this has to do everything
/// the delete does short of removing the membership: revoke the credentials and mark the identity. An `active`
/// flag that left working keys behind would be the flag-with-no-effect that 0002 warns about.
pub async fn set_active(
    conn: &mut PgConnection,
    client: &Client,
    identity_id: Uuid,
    active: bool,
) -> Result<User, ScimRefusal> {
    let mut tx = conn.begin().await?;
    ours(&mut tx, client, identity_id).await?;
    set_active_on(&mut tx, client, identity_id, active).await?;
    let user = by_id(&mut tx, client.tenant_id, identity_id)
        .await?
        .ok_or(ScimRefusal::NoSuchUser)?;
    tx.commit().await?;
    Ok(user)
}

async fn set_active_on(
    conn: &mut PgConnection,
    client: &Client,
    identity_id: Uuid,
    active: bool,
) -> Result<(), ScimRefusal> {
    if active {
        sqlx::query(
            "UPDATE dam_global.identities \
             SET status = 'active', deprovisioned_at = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(identity_id)
        .execute(&mut *conn)
        .await?;
        return Ok(());
    }

    sqlx::query(
        "UPDATE dam_global.identities \
         SET status = 'disabled', deprovisioned_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(identity_id)
    .execute(&mut *conn)
    .await?;
    // The part that makes it a removal rather than a flag.
    sqlx::query(
        "UPDATE dam_global.api_keys SET revoked_at = now() \
         WHERE tenant_id = $1 AND identity_id = $2 AND revoked_at IS NULL",
    )
    .bind(client.tenant_id)
    .bind(identity_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Replaces the mutable attributes of a provisioned person.
pub async fn replace(
    conn: &mut PgConnection,
    client: &Client,
    identity_id: Uuid,
    new: &NewUser,
) -> Result<User, ScimRefusal> {
    let mut tx = conn.begin().await?;
    ours(&mut tx, client, identity_id).await?;

    // A `userName` change is refused rather than ignored. It is the email, `identities` is unique on it
    // globally, and it is what a person signs in as — so renaming it here would either fail on the unique
    // index for somebody who already exists, or quietly move an account somebody else may be using. Dropping
    // it silently is worse than either: a provider told its rename applied will not send it again, and the two
    // directories disagree from then on.
    let current: String =
        sqlx::query_scalar("SELECT email FROM dam_global.identities WHERE id = $1")
            .bind(identity_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ScimRefusal::NoSuchUser)?;
    if !current.eq_ignore_ascii_case(new.user_name.trim()) {
        return Err(ScimRefusal::Invalid(format!(
            "userName cannot be changed here: this account is `{current}`. Remove it and provision the new \
             address, so whatever the old one still signs in to is closed deliberately."
        )));
    }

    sqlx::query(
        "UPDATE dam_global.identities SET display_name = $2, updated_at = now() WHERE id = $1",
    )
    .bind(identity_id)
    .bind(new.display_name.as_deref())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE dam_global.tenant_members \
         SET scim_external_id = coalesce($3, scim_external_id), updated_at = now() \
         WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(client.tenant_id)
    .bind(identity_id)
    .bind(new.external_id.as_deref())
    .execute(&mut *tx)
    .await?;

    // Roles are replaced rather than merged, which is what PUT means — and the provider is the authority for
    // an account it owns, so `set_roles`'s SCIM guard would refuse the very caller that should win. The
    // last-administrator rule still applies, and a provider cannot make somebody a tenant administrator: that
    // is a decision about this library, not about the directory.
    sqlx::query(
        "UPDATE dam_global.tenant_members SET role_names = $3, updated_at = now() \
         WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(client.tenant_id)
    .bind(identity_id)
    .bind(&new.roles)
    .execute(&mut *tx)
    .await?;

    set_active_on(&mut tx, client, identity_id, new.active).await?;
    let user = by_id(&mut tx, client.tenant_id, identity_id)
        .await?
        .ok_or(ScimRefusal::NoSuchUser)?;
    tx.commit().await?;
    Ok(user)
}

/// Removes a provisioned person's access entirely, which is how Okta deprovisions.
pub async fn deprovision(
    conn: &mut PgConnection,
    client: &Client,
    identity_id: Uuid,
) -> Result<members::Removed, ScimRefusal> {
    let mut tx = conn.begin().await?;
    ours(&mut tx, client, identity_id).await?;
    // Through `members::remove`, so a provider's offboarding and an administrator's do exactly the same thing:
    // revoke the keys, drop the membership, and disable the identity only if this was their last tenant.
    let removed = members::remove(&mut tx, client.tenant_id, identity_id).await?;
    tx.commit().await?;
    Ok(removed)
}

/// Refuses an account this client does not own.
///
/// A provider may only change what it provisioned. Otherwise one tenant's misconfigured provider could
/// disable a person somebody added by hand, and the audit trail would show the provider's own token doing it.
async fn ours(
    conn: &mut PgConnection,
    client: &Client,
    identity_id: Uuid,
) -> Result<(), ScimRefusal> {
    let found: Option<(bool, Option<Uuid>)> = sqlx::query_as(
        "SELECT m.scim_managed, m.scim_client_id FROM dam_global.tenant_members m \
         WHERE m.tenant_id = $1 AND m.identity_id = $2",
    )
    .bind(client.tenant_id)
    .bind(identity_id)
    .fetch_optional(&mut *conn)
    .await?;

    match found {
        // Not a member of this tenant, or no such identity: one answer, because a provider learning which is
        // learning about another customer's directory.
        None => Err(ScimRefusal::NoSuchUser),
        Some((managed, owner)) if managed && owner == Some(client.id) => Ok(()),
        Some(_) => Err(ScimRefusal::NotOurs),
    }
}
