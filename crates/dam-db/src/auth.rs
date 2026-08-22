//! API-key authentication and grant loading.
//!
//! Nothing here is invented: `api_keys`, `tenant_members` and `roles` already prescribe the model. A key
//! identifies a tenant and an identity; the identity's `role_names` resolve to rows in that tenant's
//! `roles` table; those compile to the [`dam_core::policy::AccessPredicate`] from 0.10.
//!
//! ## Why the hash is a fast digest and not a password hash
//!
//! An API key here is 256 bits from the OS CSPRNG. Guessing it is not a threat model, so argon2 or bcrypt
//! would buy nothing and cost a deliberate ~100ms on **every request**. The stored hash is BLAKE3, which
//! makes authentication a single indexed lookup against the `UNIQUE (key_hash)` index that already exists
//! — no prefix scan, no per-candidate verification.
//!
//! The corollary is that the hash is unsalted, which would be wrong for a password: a salt defends against
//! precomputation over a *dictionary*, and there is no dictionary for 256 random bits.
//!
//! ## Scopes narrow, never widen
//!
//! `api_keys.scopes` restricts a key below what its owner can do — that is what makes a key safe to paste
//! into a CI job. It intersects with the identity's permissions and never adds to them; a union would let
//! anyone escalate their own privileges by writing a broader scope on a key they issue themselves.

use crate::Error;
use chrono::{DateTime, Duration, Utc};
use dam_core::{
    TenantSlug,
    policy::{Grant, Grants},
};
use sqlx::PgPool;
use uuid::Uuid;

/// How stale `last_used_at` must be before authentication rewrites it.
///
/// The column exists for key hygiene — finding credentials nobody uses. Writing it on every request turns
/// every read-only endpoint into a write and costs a row of WAL per API call, which is a price nobody
/// chose. An hour's resolution answers "is this key still in use" just as well.
const LAST_USED_RESOLUTION: Duration = Duration::hours(1);

/// A freshly generated key, held only long enough to show the caller once.
///
/// The plaintext is never stored, so this type is the only place it exists. It is deliberately not
/// `Clone` or `Debug`-printable in full.
pub struct ApiKey {
    plaintext: String,
    prefix: String,
    hash: String,
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The prefix only. A key that reaches a log is a key that must be rotated, and `{:?}` on a struct
        // is how that happens.
        f.debug_struct("ApiKey")
            .field("prefix", &self.prefix)
            .field("plaintext", &"[REDACTED]")
            .finish()
    }
}

impl ApiKey {
    /// Number of random bytes behind a key. 32 bytes = 256 bits.
    const SECRET_BYTES: usize = 32;

    /// Generates a key.
    ///
    /// Shaped `damrs_<hex>` so a secret scanner has something to match on — a leaked key that looks like
    /// arbitrary base64 is a leaked key nobody notices.
    pub fn generate() -> Self {
        // `rand::random` over the thread RNG: ChaCha-based and `CryptoRng`, which is the bar a
        // credential needs. Not `expect`-ing on a fallible OS call keeps this infallible, which matters
        // because a key-issuing endpoint that can fail for an unexplainable reason is a support ticket.
        let bytes: [u8; Self::SECRET_BYTES] = rand::random();
        let secret = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let plaintext = format!("damrs_{secret}");
        Self {
            prefix: plaintext[..14].to_owned(),
            hash: Self::hash_of(&plaintext),
            plaintext,
        }
    }

    /// The digest stored in `api_keys.key_hash`.
    ///
    /// Domain-separated, so a hash from this table can never collide with a content hash computed
    /// elsewhere in the system — the two live in different columns today and that is exactly the kind of
    /// thing that changes.
    pub fn hash_of(plaintext: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"damrs-api-key-v1\0");
        hasher.update(plaintext.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Shown to the caller once, at creation.
    pub fn plaintext(&self) -> &str {
        &self.plaintext
    }

    /// Stored so a key can be identified in a UI without revealing it.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Consumes the key, yielding the plaintext to hand over exactly once.
    pub fn into_plaintext(self) -> String {
        self.plaintext
    }
}

/// Who a request is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    pub tenant_id: Uuid,
    /// The slug, which is also the schema name after `t_` — so this is what `TenantConn` needs.
    pub tenant_slug: TenantSlug,
    /// `None` for a key issued to no particular person, e.g. a machine integration.
    pub identity_id: Option<Uuid>,
    pub api_key_id: Uuid,
    /// The key's scopes. Empty means unscoped.
    pub scopes: Vec<String>,
}

/// Authenticates a presented key.
///
/// `Ok(None)` covers every reason a key does not work — unknown, revoked, expired, or belonging to a tenant
/// that is not active. They are deliberately indistinguishable to the caller: telling a prober which of their
/// guesses had the right *shape* hands them the cheap half of the search. A suspended tenant's user gets the
/// same answer as somebody guessing keys, which is correct — the place to explain a suspension is the billing
/// page, not an API that has just refused a credential.
pub async fn authenticate(
    global: &PgPool,
    presented: &str,
) -> Result<Option<Authenticated>, Error> {
    // Hashed before it reaches the query, so the plaintext never appears in a statement, a query log, or
    // an error message.
    let hash = ApiKey::hash_of(presented);

    // An inner join on `tenants`, not a lookup then a fetch: a key whose tenant has been deleted must not
    // authenticate, and relying on the foreign key's cascade to have run is the wrong place to be
    // trusting. Expiry and revocation are in the WHERE clause for the same reason — a caller that had to
    // remember to check them is a caller that will forget.
    //
    // **And the tenant must be `active`, not merely present.** The join alone proved existence, so a
    // `suspended` tenant's keys kept working — suspending a tenant for non-payment or abuse did not cut off
    // its API access, which is the one thing suspension is for. `provisioning` is refused because the
    // schema may not exist yet, `deprovisioning` because it is being torn down, and `migration_failed`
    // because its schema is at an unknown version and every later query would fail with "relation does not
    // exist" from inside a handler. `damd` already filtered on `active` when resolving its delivery tenant;
    // this makes authentication agree with it.
    //
    // Found by a surviving mutation: turning the join into a `LEFT JOIN` broke nothing, which meant nothing
    // asserted the tenant side at all. That mutation still survives and is now *equivalent* rather than
    // undetected — with a left join `t.status` is NULL for a missing tenant, and `NULL = 'active'` is not
    // true, so the row is excluded either way. The status check subsumes the join's protection; the inner
    // join stays because it says what is meant.
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            String,
            Option<Uuid>,
            Vec<String>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT k.id, k.tenant_id, t.slug, k.identity_id, k.scopes, k.last_used_at \
         FROM dam_global.api_keys k \
         JOIN dam_global.tenants t ON t.id = k.tenant_id \
         WHERE k.key_hash = $1 \
           AND t.status = 'active' \
           AND k.revoked_at IS NULL \
           AND (k.expires_at IS NULL OR k.expires_at > now())",
    )
    .bind(&hash)
    .fetch_optional(global)
    .await?;

    let Some((api_key_id, tenant_id, slug, identity_id, scopes, last_used_at)) = row else {
        return Ok(None);
    };

    // Only when it is already stale — see LAST_USED_RESOLUTION.
    if last_used_at.is_none_or(|at| Utc::now() - at > LAST_USED_RESOLUTION) {
        sqlx::query("UPDATE dam_global.api_keys SET last_used_at = now() WHERE id = $1")
            .bind(api_key_id)
            .execute(global)
            .await?;
    }

    let tenant_slug = TenantSlug::new(&slug)?;
    Ok(Some(Authenticated {
        tenant_id,
        tenant_slug,
        identity_id,
        api_key_id,
        scopes,
    }))
}

/// Loads the grants an identity holds in a tenant, narrowed by the key's scopes.
///
/// `global` supplies the membership, `tenant` the role definitions — they live in different schemas, and
/// the split is the D2 boundary rather than an accident.
///
/// The tenant side is any [`sqlx::PgExecutor`], which in a request is a [`crate::TenantConn`]'s
/// connection: the unqualified `FROM roles` below resolves through that transaction's `search_path`, so
/// passing a pool whose search_path is the *global* schema would read the wrong table — or, worse, no
/// table and therefore no grants, which fails closed but looks like a permissions bug. A per-tenant pool
/// would avoid that and reintroduce the thousand-pools problem §5.2 exists to prevent.
pub async fn grants_for<'e, E>(
    global: &PgPool,
    tenant: E,
    tenant_id: Uuid,
    identity_id: Uuid,
    scopes: &[&str],
) -> Result<Grants, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let membership = sqlx::query_as::<_, (Vec<String>, bool)>(
        "SELECT role_names, is_tenant_admin FROM dam_global.tenant_members \
         WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .fetch_optional(global)
    .await?;

    let Some((role_names, is_tenant_admin)) = membership else {
        // Not a member. No grants at all — not even read.
        return Ok(Grants::from(vec![]));
    };

    let mut grants: Vec<Grant> = Vec::new();

    if is_tenant_admin {
        // A shortcut on the membership rather than a role row, so it has to be synthesised. It bypasses
        // group scoping and release windows (ABAC 5) — and nothing else: expiry, legal hold and
        // `rights_state` are enforced by `policy::evaluate`, which this loader cannot influence.
        grants.push(Grant {
            permissions: vec![
                "asset:read".to_owned(),
                "asset:download".to_owned(),
                "asset:manage".to_owned(),
            ],
            asset_group_ids: vec![],
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        });
    }

    if !role_names.is_empty() {
        // A membership can name a role that has since been deleted. Those simply contribute nothing:
        // failing the whole request would lock a user out over an administrator's tidy-up.
        let rows = sqlx::query_as::<
            _,
            (
                Vec<String>,
                Vec<Uuid>,
                bool,
                Option<DateTime<Utc>>,
                Option<DateTime<Utc>>,
                bool,
            ),
        >(
            "SELECT permissions, asset_group_ids, all_asset_groups, valid_from, valid_until, \
                    requires_eula \
             FROM roles WHERE key = ANY($1)",
        )
        .bind(&role_names)
        .fetch_all(tenant)
        .await?;

        for (
            permissions,
            asset_group_ids,
            all_asset_groups,
            valid_from,
            valid_until,
            requires_eula,
        ) in rows
        {
            grants.push(Grant {
                permissions,
                asset_group_ids,
                all_asset_groups,
                valid_from,
                valid_until,
                requires_eula,
                // Resolved from the caller's acceptance record by the API layer; a role only says an
                // acceptance is required. Defaulting to `false` keeps the gate closed until something
                // actively opens it.
                eula_accepted: false,
            });
        }
    }

    // Scopes intersect. Applied after the roles are assembled so a scope can only remove.
    if !scopes.is_empty() {
        for grant in &mut grants {
            grant
                .permissions
                .retain(|permission| scopes.contains(&permission.as_str()));
        }
        // A grant left with no permissions contributes nothing but would still widen the group union for
        // any *other* action, so it is dropped entirely.
        grants.retain(|grant| !grant.permissions.is_empty());
    }

    Ok(Grants::from(grants))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debug_impl_never_prints_the_secret() {
        // A key in a log is a key that has to be rotated, and `{:?}` on a request context is how it gets
        // there.
        let key = ApiKey::generate();
        let rendered = format!("{key:?}");
        assert!(!rendered.contains(&key.plaintext[14..]), "got {rendered}");
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn the_hash_is_domain_separated() {
        // Without the prefix, an API-key hash and a content hash of the same bytes would be equal — and
        // the two live in different columns today, which is exactly the kind of thing that changes.
        let plaintext = "damrs_deadbeef";
        assert_ne!(
            ApiKey::hash_of(plaintext),
            blake3::hash(plaintext.as_bytes()).to_hex().to_string()
        );
    }

    #[test]
    fn the_prefix_is_long_enough_to_identify_and_short_enough_to_be_useless() {
        let key = ApiKey::generate();
        // `damrs_` plus eight hex characters: enough to tell two keys apart in a list, and 224 bits short
        // of being the key.
        assert_eq!(key.prefix().len(), 14);
        assert!(key.plaintext().len() - key.prefix().len() > 50);
    }
}
