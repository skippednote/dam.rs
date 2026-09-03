//! Per-tenant encryption keys (G10·3b).
//!
//! `dam_global.encryption_keys` has existed since `0002_enterprise.sql` and, until this module, nothing read
//! or wrote it — a schema describing a capability the code did not have, the same shape `audit_log`'s hash
//! columns and `assets.legal_hold` had before G10·1.
//!
//! ## What a key here is, and what it is not
//!
//! `key_ref` is an ARN or a URI. **Never key material.** The row says which key a KMS should be asked to use;
//! it does not say what that key is, and a database backup of this table therefore discloses nothing that
//! could decrypt anything. That is the whole reason the column is a reference rather than a secret, and it is
//! why this module has no `Secret` in it anywhere.
//!
//! ## Resolution falls back, deliberately
//!
//! [`resolve`] answers "which key should this tenant's *blob* writes use" with the tenant's own active key if
//! it has one, and otherwise `None` — leaving the caller to use whatever the deployment configured. A
//! deployment with one key for every tenant is the correct implementation of a single-tenant or dedicated
//! install, and was never wrong; what it cannot claim is BYOK on a *shared* install, which is an RFP
//! pass/fail question and is why the distinction is worth being exact about.
//!
//! Falling back rather than failing is the right direction here and the argument is worth stating, because
//! the opposite direction is usually correct in this codebase. A missing key is not a permission that might
//! wrongly be granted — it is a *stronger* key that might wrongly be skipped. Refusing the write would take a
//! deployment that encrypts correctly today and stop it working the moment this table exists and is empty,
//! which is every deployment at the moment of upgrade.
//!
//! ## Only one key is active per tenant and purpose
//!
//! Enforced by a partial unique index in the schema rather than here, so two rows cannot both be active even
//! if two processes race. `rotating`, `retired` and `revoked` rows stay: a retired key must be retained as
//! long as anything it encrypted still exists, so retirement is not deletion.

use crate::Error;
use chrono::{DateTime, Utc};
use sqlx::Row;
use uuid::Uuid;

/// What a key is for.
///
/// Four values, matching the schema's CHECK. They are at very different stages: `blob` is wired, and the other
/// three are resolvable but have no consumer yet — recorded as variants rather than omitted, because the
/// column already admits them and a resolver that silently could not express one would be a second schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Object-storage encryption — today `storage.sse_kms_key_id`, one key for the deployment.
    Blob,
    /// The identity a C2PA manifest is signed with.
    ///
    /// Per-tenant has consequences well beyond storage: it is what a consumer verifies a provenance claim
    /// against, so changing it changes what an already-published assertion means to somebody outside.
    C2paSigning,
    /// Column-level encryption. Not implemented anywhere: there is no field encryption to key.
    Field,
    /// Backup encryption — today the bucket's and the managed database's own.
    Backup,
}

impl Purpose {
    /// The string the schema's CHECK admits.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::C2paSigning => "c2pa_signing",
            Self::Field => "field",
            Self::Backup => "backup",
        }
    }

    /// Reads a purpose back, or nothing.
    ///
    /// `None` rather than a default: a row whose purpose this build does not understand must not be treated
    /// as some other purpose's key, which is how the wrong key gets used for the right-looking reason.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "blob" => Some(Self::Blob),
            "c2pa_signing" => Some(Self::C2paSigning),
            "field" => Some(Self::Field),
            "backup" => Some(Self::Backup),
            _ => None,
        }
    }
}

/// Where a key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    AwsKms,
    GcpKms,
    AzureKv,
    Vault,
    /// A key held by the deployment itself rather than a managed service.
    Local,
}

impl Provider {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwsKms => "aws_kms",
            Self::GcpKms => "gcp_kms",
            Self::AzureKv => "azure_kv",
            Self::Vault => "vault",
            Self::Local => "local",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "aws_kms" => Some(Self::AwsKms),
            "gcp_kms" => Some(Self::GcpKms),
            "azure_kv" => Some(Self::AzureKv),
            "vault" => Some(Self::Vault),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

/// The lifecycle a key is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Rotating,
    Retired,
    Revoked,
}

impl State {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Rotating => "rotating",
            Self::Retired => "retired",
            Self::Revoked => "revoked",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "rotating" => Some(Self::Rotating),
            "retired" => Some(Self::Retired),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// One row: a reference to a key, never the key.
#[derive(Debug, Clone)]
pub struct EncryptionKey {
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub purpose: Purpose,
    pub provider: Provider,
    /// An ARN or URI. Never key material — see the module docs.
    pub key_ref: String,
    pub customer_managed: bool,
    pub state: State,
    pub activated_at: DateTime<Utc>,
    pub rotated_at: Option<DateTime<Utc>>,
    pub retired_at: Option<DateTime<Utc>>,
}

/// What to record for a new key.
#[derive(Debug, Clone)]
pub struct NewKey<'a> {
    pub tenant_id: Option<Uuid>,
    pub purpose: Purpose,
    pub provider: Provider,
    pub key_ref: &'a str,
    pub customer_managed: bool,
}

fn row_to_key(row: &sqlx::postgres::PgRow) -> Result<EncryptionKey, Error> {
    let purpose: String = row.try_get("purpose")?;
    let provider: String = row.try_get("provider")?;
    let state: String = row.try_get("state")?;
    Ok(EncryptionKey {
        id: row.try_get("id")?,
        tenant_id: row.try_get("tenant_id")?,
        // An unreadable value is an error rather than a default: see `Purpose::parse`.
        purpose: Purpose::parse(&purpose).ok_or_else(|| {
            Error::Inconsistent(format!("encryption_keys.purpose holds {purpose:?}"))
        })?,
        provider: Provider::parse(&provider).ok_or_else(|| {
            Error::Inconsistent(format!("encryption_keys.provider holds {provider:?}"))
        })?,
        key_ref: row.try_get("key_ref")?,
        customer_managed: row.try_get("customer_managed")?,
        state: State::parse(&state)
            .ok_or_else(|| Error::Inconsistent(format!("encryption_keys.state holds {state:?}")))?,
        activated_at: row.try_get("activated_at")?,
        rotated_at: row.try_get("rotated_at")?,
        retired_at: row.try_get("retired_at")?,
    })
}

/// The key a tenant's writes for this purpose should use, if it has one of its own.
///
/// `None` means "use whatever the deployment configured" — see the module docs on why this falls back rather
/// than refusing. Only an `active` key is returned: a `rotating` one is not yet the write key, and `retired`
/// and `revoked` ones exist to decrypt what they already encrypted.
///
/// # Errors
/// Any database failure, or a row whose enum columns this build cannot read.
pub async fn resolve(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    purpose: Purpose,
) -> Result<Option<EncryptionKey>, Error> {
    // The column list is written out rather than interpolated: `sqlx::query` refuses a dynamic string, which
    // is the guard that stops a `format!` from ever growing a caller-supplied fragment.
    let row = sqlx::query(
        "SELECT id, tenant_id, purpose, provider, key_ref, customer_managed, state, \
                activated_at, rotated_at, retired_at \
         FROM dam_global.encryption_keys \
         WHERE tenant_id = $1 AND purpose = $2 AND state = 'active'",
    )
    .bind(tenant_id)
    .bind(purpose.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_key).transpose()
}

/// Every key recorded for a tenant, newest first, whatever its state.
///
/// Retired and revoked rows are included on purpose: the question an operator asks of this list is usually
/// "what did we encrypt this with last year", and a list of only the live key cannot answer it.
///
/// # Errors
/// Any database failure, or a row whose enum columns this build cannot read.
pub async fn for_tenant(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
) -> Result<Vec<EncryptionKey>, Error> {
    let rows = sqlx::query(
        "SELECT id, tenant_id, purpose, provider, key_ref, customer_managed, state, \
                activated_at, rotated_at, retired_at \
         FROM dam_global.encryption_keys \
         WHERE tenant_id = $1 ORDER BY activated_at DESC",
    )
    .bind(tenant_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(row_to_key).collect()
}

/// Records a key and makes it the active one for its tenant and purpose.
///
/// The previous active key is moved to `retired` in the same statement pair, inside the caller's transaction,
/// because the schema's partial unique index refuses two active rows — and doing it in two round trips would
/// leave a window where a concurrent writer sees none.
///
/// # Errors
/// Any database failure.
pub async fn activate(
    conn: &mut sqlx::PgConnection,
    new: &NewKey<'_>,
) -> Result<EncryptionKey, Error> {
    // `IS NOT DISTINCT FROM`, not `=`: a deployment-wide key has a NULL tenant, and `tenant_id = NULL` is
    // never true, so `=` would fail to retire the previous deployment key and leave two active — which the
    // partial unique index does not catch either, because it treats NULLs as distinct. Nothing creates a
    // NULL-tenant key today, so this is latent, but the wrong operator here is a silent double-active key.
    sqlx::query(
        "UPDATE dam_global.encryption_keys SET state = 'retired', retired_at = now() \
         WHERE tenant_id IS NOT DISTINCT FROM $1 AND purpose = $2 AND state = 'active'",
    )
    .bind(new.tenant_id)
    .bind(new.purpose.as_str())
    .execute(&mut *conn)
    .await?;

    let row = sqlx::query(
        "INSERT INTO dam_global.encryption_keys \
         (id, tenant_id, purpose, provider, key_ref, customer_managed, state) \
         VALUES ($1, $2, $3, $4, $5, $6, 'active') \
         RETURNING id, tenant_id, purpose, provider, key_ref, customer_managed, state, \
                   activated_at, rotated_at, retired_at",
    )
    .bind(Uuid::new_v4())
    .bind(new.tenant_id)
    .bind(new.purpose.as_str())
    .bind(new.provider.as_str())
    .bind(new.key_ref)
    .bind(new.customer_managed)
    .fetch_one(&mut *conn)
    .await?;
    row_to_key(&row)
}

/// Marks a key revoked, so nothing new is written with it.
///
/// Revocation is not deletion and not retirement: a revoked key is one that must not be *used*, where a
/// retired one was merely superseded. Both are kept, because whatever they encrypted still needs them.
///
/// # Errors
/// Any database failure.
pub async fn revoke(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<bool, Error> {
    let done = sqlx::query(
        "UPDATE dam_global.encryption_keys SET state = 'revoked', retired_at = now() \
         WHERE id = $1 AND state <> 'revoked'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The KMS key a write to this pool should carry, for this tenant.
///
/// Three sources, most specific first, and the order is the whole design:
///
/// 1. **The tenant's own active `blob` key.** Customer-managed, rotatable and revocable by the customer —
///    this is what BYOK means and what an RFP is asking about.
/// 2. **The pool's `kms_key_ref`.** Infrastructure: "objects in this bucket are encrypted with this",
///    configured by whoever runs the deployment.
/// 3. **Nothing**, leaving the caller to use `storage.sse_kms_key_id` — the process-wide key that was the
///    only answer before this existed, and which remains correct for a single-tenant or dedicated install.
///
/// Never fails closed. See the module docs: a missing key is a *stronger* key that might wrongly be skipped,
/// not a permission that might wrongly be granted, and refusing would stop every correctly-encrypting
/// deployment from writing the moment this table went live.
///
/// # Errors
/// Any database failure, or a key row this build cannot read.
pub async fn key_for_write(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    pool_id: Uuid,
) -> Result<Option<String>, Error> {
    if let Some(key) = resolve(&mut *conn, tenant_id, Purpose::Blob).await? {
        return Ok(Some(key.key_ref));
    }
    let pool_key: Option<Option<String>> =
        sqlx::query_scalar("SELECT kms_key_ref FROM dam_global.storage_pools WHERE id = $1")
            .bind(pool_id)
            .fetch_optional(&mut *conn)
            .await?;
    // Blank is treated as absent, matching `S3Store::with_sse_kms` — otherwise the two disagree and an empty
    // column sends an empty key id, failing every write with an error naming the key rather than its absence.
    Ok(pool_key
        .flatten()
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty()))
}
