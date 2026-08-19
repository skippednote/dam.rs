//! Upload profiles: what an upload arrives already knowing (Q.3).
//!
//! ## Why a row rather than parameters
//!
//! A profile answers three questions asked at three different times by three different pieces of the system:
//!
//! - the **uploader**, before any bytes move, needs the form and whether to insist on required fields;
//! - **finalise**, writing the asset row, needs the defaults and the metadata type;
//! - **enrichment**, in a worker long afterwards, needs to know whether machine tagging was permitted at all.
//!
//! Only the last of those can be answered from the asset, and only the first from the request. A profile is the
//! one place all three can read, and it is what makes an intake reproducible: re-running an import under the
//! same profile produces the same defaults.
//!
//! ## Defaults are metadata, so they are validated like metadata
//!
//! A default goes through the same validator a human's edit does — `Writer::Human`, because a profile expresses
//! an administrator's intent rather than a model's guess, and `Mode::Patch`, because a default fills fields
//! rather than claiming to be a complete record. That is what stops a profile writing a read-only field, a
//! value of the wrong kind, or a key nobody defined.
//!
//! Validated **twice**: when the profile is saved, and again when it is applied. A field definition can change
//! in between — somebody removes it, or narrows its constraints — and a default that has quietly become invalid
//! must fail where somebody can see it rather than being dropped from every upload from then on.
//!
//! ## A default is a starting point, not an override
//!
//! Applying defaults fills only keys the upload did not supply. A profile that overwrote what somebody typed
//! would silently discard their work, which is the opposite of what a default is for.

use crate::Error;
use dam_core::fields::{Mode, Rejection, Writer};
use serde_json::{Map, Value};
use uuid::Uuid;

/// Why a profile operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum ProfileRefusal {
    #[error("an upload profile with the key `{0}` already exists")]
    DuplicateKey(String),

    #[error("no upload profile {0} exists")]
    UnknownProfile(Uuid),

    /// The defaults do not validate against the tenant's schema, with a problem per field.
    #[error("the profile's default metadata is not valid: {}", summarise(.0))]
    InvalidDefaults(Vec<Rejection>),

    #[error(transparent)]
    Database(#[from] Error),
}

fn summarise(problems: &[Rejection]) -> String {
    problems
        .iter()
        .map(|problem| format!("{}: {}", problem.key, problem.code))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A profile to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProfile {
    pub key: String,
    pub label: String,
    /// The form uploads get, overriding the media-class guess. `None` lets the mime decide.
    pub metadata_type_id: Option<Uuid>,
    pub defaults: Value,
    pub require_complete: bool,
    pub ai_tags_enabled: bool,
    pub is_default: bool,
}

/// A profile as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadProfile {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub metadata_type_id: Option<Uuid>,
    pub defaults: Value,
    /// Whether the uploader makes a person fill required fields before proceeding.
    ///
    /// A client-facing rule, deliberately not enforced at finalise: bytes are already staged by then, and
    /// refusing there would strand an upload over metadata a person could have supplied. "Which assets are
    /// incomplete" is a worklist query instead — asked where an incomplete asset can actually be fixed.
    pub require_complete: bool,
    pub ai_tags_enabled: bool,
    pub is_default: bool,
    pub display_order: i32,
}

/// The columns every read selects, in order.
type ProfileRow = (
    Uuid,
    String,
    String,
    Option<Uuid>,
    Value,
    bool,
    bool,
    bool,
    i32,
);

fn profile(row: ProfileRow) -> UploadProfile {
    let (
        id,
        key,
        label,
        metadata_type_id,
        defaults,
        require_complete,
        ai_tags_enabled,
        is_default,
        display_order,
    ) = row;
    UploadProfile {
        id,
        key,
        label,
        metadata_type_id,
        defaults,
        require_complete,
        ai_tags_enabled,
        is_default,
        display_order,
    }
}

// Written out at each call site rather than composed from a constant: sqlx refuses `format!`-built SQL, and
// the guard is right — a query assembled from pieces is one nobody reads as a whole.

/// Creates a profile, validating its defaults against the tenant's schema.
pub async fn create(
    pool: &sqlx::PgPool,
    spec: NewProfile,
) -> Result<UploadProfile, ProfileRefusal> {
    let mut tx = pool.begin().await.map_err(Error::from)?;
    let created = create_on(&mut tx, spec).await?;
    tx.commit().await.map_err(Error::from)?;
    Ok(created)
}

/// [`create`] on a caller's connection, so it joins a tenant-scoped transaction.
pub async fn create_on(
    tx: &mut sqlx::PgConnection,
    spec: NewProfile,
) -> Result<UploadProfile, ProfileRefusal> {
    let clash: Option<i32> = sqlx::query_scalar("SELECT 1 FROM upload_profiles WHERE key = $1")
        .bind(&spec.key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::from)?;
    if clash.is_some() {
        return Err(ProfileRefusal::DuplicateKey(spec.key));
    }

    // Checked before the insert, so a default that cannot ever apply never becomes a stored setting: it would
    // otherwise sit there until an upload used it and then either fail every upload or be silently dropped.
    validate_defaults(&mut *tx, &spec.defaults).await?;

    let id = Uuid::new_v4();
    // The default is cleared first when this profile claims it, because the partial unique index would
    // otherwise refuse the insert — and "make this the default" is an instruction to move it.
    if spec.is_default {
        sqlx::query(
            "UPDATE upload_profiles SET is_default = false, updated_at = now() WHERE is_default",
        )
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
    }
    sqlx::query(
        "INSERT INTO upload_profiles \
             (id, key, label, metadata_type_id, defaults, require_complete, ai_tags_enabled, \
              is_default, display_order) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 coalesce((SELECT max(display_order) + 1 FROM upload_profiles), 0))",
    )
    .bind(id)
    .bind(&spec.key)
    .bind(&spec.label)
    .bind(spec.metadata_type_id)
    .bind(&spec.defaults)
    .bind(spec.require_complete)
    .bind(spec.ai_tags_enabled)
    .bind(spec.is_default)
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;

    let row: ProfileRow = sqlx::query_as(
        "SELECT id, key, label, metadata_type_id, defaults, require_complete, ai_tags_enabled, \
                is_default, display_order \
         FROM upload_profiles WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    Ok(profile(row))
}

/// Every profile, in display order.
pub async fn list(pool: &sqlx::PgPool) -> Result<Vec<UploadProfile>, ProfileRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    list_on(&mut conn).await
}

/// [`list`] on a caller's connection.
pub async fn list_on(conn: &mut sqlx::PgConnection) -> Result<Vec<UploadProfile>, ProfileRefusal> {
    let rows: Vec<ProfileRow> = sqlx::query_as(
        "SELECT id, key, label, metadata_type_id, defaults, require_complete, ai_tags_enabled, \
                is_default, display_order \
         FROM upload_profiles ORDER BY display_order, key",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(rows.into_iter().map(profile).collect())
}

/// One profile by key.
pub async fn by_key(
    pool: &sqlx::PgPool,
    key: &str,
) -> Result<Option<UploadProfile>, ProfileRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    by_key_on(&mut conn, key).await
}

/// [`by_key`] on a caller's connection.
pub async fn by_key_on(
    conn: &mut sqlx::PgConnection,
    key: &str,
) -> Result<Option<UploadProfile>, ProfileRefusal> {
    let row: Option<ProfileRow> = sqlx::query_as(
        "SELECT id, key, label, metadata_type_id, defaults, require_complete, ai_tags_enabled, \
                is_default, display_order \
         FROM upload_profiles WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(row.map(profile))
}

/// What may be amended about a profile. An omitted member is left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Amendment {
    pub label: Option<String>,
    pub metadata_type_id: Option<Option<Uuid>>,
    pub defaults: Option<Value>,
    pub require_complete: Option<bool>,
    pub ai_tags_enabled: Option<bool>,
    pub is_default: Option<bool>,
}

/// Amends a profile on a caller's connection.
///
/// Defaults are re-validated when supplied, for the same reason they are validated on create: a stored default
/// that cannot apply breaks every intake from that source, and the person who could fix it never sees why.
pub async fn amend_on(
    tx: &mut sqlx::PgConnection,
    id: Uuid,
    change: Amendment,
) -> Result<UploadProfile, ProfileRefusal> {
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM upload_profiles WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::from)?;
    if exists.is_none() {
        return Err(ProfileRefusal::UnknownProfile(id));
    }

    if let Some(defaults) = &change.defaults {
        validate_defaults(&mut *tx, defaults).await?;
    }

    // Claiming the fallback moves it, in the same transaction, because the partial unique index admits only one
    // and "make this the default" is an instruction rather than a question.
    if change.is_default == Some(true) {
        sqlx::query(
            "UPDATE upload_profiles SET is_default = false, updated_at = now() WHERE is_default",
        )
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
    }

    sqlx::query(
        "UPDATE upload_profiles SET \
             label = coalesce($2, label), \
             metadata_type_id = CASE WHEN $3 THEN $4 ELSE metadata_type_id END, \
             defaults = coalesce($5, defaults), \
             require_complete = coalesce($6, require_complete), \
             ai_tags_enabled = coalesce($7, ai_tags_enabled), \
             is_default = coalesce($8, is_default), \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(change.label.as_deref())
    .bind(change.metadata_type_id.is_some())
    .bind(change.metadata_type_id.flatten())
    .bind(change.defaults.as_ref())
    .bind(change.require_complete)
    .bind(change.ai_tags_enabled)
    .bind(change.is_default)
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;

    let row: ProfileRow = sqlx::query_as(
        "SELECT id, key, label, metadata_type_id, defaults, require_complete, ai_tags_enabled, \
                is_default, display_order \
         FROM upload_profiles WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    Ok(profile(row))
}

/// The profile an upload should be treated under.
///
/// A named profile wins. A name that no longer resolves falls back rather than failing: a session can outlive
/// an administrator's tidy-up, and refusing the upload then would strand staged bytes over a configuration
/// change nobody told the uploader about. `None` when the tenant has defined no profiles at all, which is the
/// behaviour from before profiles existed.
pub async fn for_upload(
    pool: &sqlx::PgPool,
    named: Option<Uuid>,
) -> Result<Option<UploadProfile>, ProfileRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    for_upload_on(&mut conn, named).await
}

/// [`for_upload`] on a caller's connection, so finalise resolves inside its own transaction.
pub async fn for_upload_on(
    conn: &mut sqlx::PgConnection,
    named: Option<Uuid>,
) -> Result<Option<UploadProfile>, ProfileRefusal> {
    if let Some(id) = named {
        let row: Option<ProfileRow> = sqlx::query_as(
            "SELECT id, key, label, metadata_type_id, defaults, require_complete, ai_tags_enabled, \
                    is_default, display_order \
             FROM upload_profiles WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?;
        if let Some(row) = row {
            return Ok(Some(profile(row)));
        }
    }
    let row: Option<ProfileRow> = sqlx::query_as(
        "SELECT id, key, label, metadata_type_id, defaults, require_complete, ai_tags_enabled, \
                is_default, display_order \
         FROM upload_profiles WHERE is_default ORDER BY display_order, key LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(row.map(profile))
}

/// Removes a profile. Assets that arrived under it keep everything but the reference.
pub async fn remove(pool: &sqlx::PgPool, id: Uuid) -> Result<(), ProfileRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    remove_on(&mut conn, id).await
}

/// [`remove`] on a caller's connection.
pub async fn remove_on(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<(), ProfileRefusal> {
    // `ON DELETE SET NULL` on both referencing columns does the clearing. Deliberate: removing a profile is a
    // decision about *future* intakes, and it must neither be blocked by nor destroy what already arrived.
    let touched = sqlx::query("DELETE FROM upload_profiles WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    if touched.rows_affected() == 0 {
        return Err(ProfileRefusal::UnknownProfile(id));
    }
    Ok(())
}

/// `supplied`, with the profile's defaults filling only the keys it does not already have.
///
/// Re-validated here as well as at save time — see the module docs for why. The merged result is what should be
/// written, and it is returned rather than written so the caller can put it in the same transaction as the
/// asset row.
pub async fn apply_defaults(
    pool: &sqlx::PgPool,
    profile: &UploadProfile,
    supplied: &Map<String, Value>,
) -> Result<Map<String, Value>, ProfileRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    apply_defaults_on(&mut conn, profile, supplied).await
}

/// [`apply_defaults`] on a caller's connection, so it joins the transaction that writes the asset.
pub async fn apply_defaults_on(
    conn: &mut sqlx::PgConnection,
    profile: &UploadProfile,
    supplied: &Map<String, Value>,
) -> Result<Map<String, Value>, ProfileRefusal> {
    let Some(defaults) = profile.defaults.as_object() else {
        // Not an object: nothing sensible to apply. Treated as empty rather than as an error, because the
        // column has a `{}` default and a malformed value can only come from outside this module.
        return Ok(supplied.clone());
    };
    if defaults.is_empty() {
        return Ok(supplied.clone());
    }

    validate_defaults(&mut *conn, &profile.defaults).await?;

    let mut merged = supplied.clone();
    for (key, value) in defaults {
        // Only absent keys. A default is a starting point; overwriting what the uploader supplied would
        // silently discard their work.
        merged.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Ok(merged)
}

/// Runs the tenant's own validator over a profile's defaults.
async fn validate_defaults(
    conn: &mut sqlx::PgConnection,
    defaults: &Value,
) -> Result<(), ProfileRefusal> {
    let Some(object) = defaults.as_object() else {
        return Ok(());
    };
    if object.is_empty() {
        return Ok(());
    }
    // `Mode::Patch`, because defaults fill fields rather than claiming to be a complete record — a profile
    // should not have to name every required field. `Writer::Human`, because a profile is an administrator's
    // intent, not a model's guess, so it is bound by `read_only` and not by `ai_writable`.
    match crate::fields::validate_on(&mut *conn, object, Mode::Patch, Writer::Human).await {
        Ok(_) => Ok(()),
        Err(crate::fields::ValidationOutcome::Rejected(problems)) => {
            Err(ProfileRefusal::InvalidDefaults(problems))
        }
        Err(crate::fields::ValidationOutcome::Failed(error)) => {
            Err(ProfileRefusal::Database(error))
        }
    }
}
