//! Metadata types: which of the tenant's fields apply to which kind of asset (Q.1).
//!
//! ## A selection, not a second vocabulary
//!
//! `field_defs` remains the one place a key has a kind, a validation rule and an order — that invariant is
//! what [`crate::fields`]'s schema-admin refusals exist to protect, and a per-type copy of a definition would
//! reintroduce exactly the divergence they prevent. A metadata type says *which* of those fields apply to an
//! asset and in what order. So `description` shared by the image and video types is one definition reached two
//! ways, and a value written under either is readable under both.
//!
//! That boundary is also why `dam_core::fields::validate` needs no change at all: it takes a slice of
//! definitions, and choosing the slice is this module's job.
//!
//! ## Resolution always ends somewhere
//!
//! A field list is what the metadata form enumerates, so a resolution that returns nothing does not lose
//! data — it *hides* it, with nothing to alarm on. The chain therefore has no dead end:
//!
//! 1. the asset's own type, if it has one;
//! 2. otherwise the tenant's default type;
//! 3. otherwise — a tenant that has defined no types at all — the whole vocabulary, which is precisely the
//!    behaviour from before types existed.
//!
//! Step 3 is what makes the migration a no-op for every existing tenant: opting in is defining a type.

use crate::Error;
use dam_core::fields::FieldDef;
use uuid::Uuid;

/// Why a metadata-type edit was refused.
#[derive(Debug, thiserror::Error)]
pub enum TypeRefusal {
    #[error("a metadata type with the key `{0}` already exists")]
    DuplicateKey(String),

    #[error("no field is defined with the key `{0}`")]
    UnknownField(String),

    #[error("no metadata type {0} exists")]
    UnknownType(Uuid),

    #[error(transparent)]
    Database(#[from] Error),
}

/// A metadata type to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewType {
    pub key: String,
    pub label: String,
    /// The media classes this type is the natural choice for: `image`, `video`, `audio`, `document`,
    /// `archive`. Empty means "only when named explicitly".
    pub applies_to: Vec<String>,
    pub is_default: bool,
    /// The fields this type includes, in the order they should appear.
    pub field_keys: Vec<String>,
}

/// A metadata type as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataType {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub applies_to: Vec<String>,
    pub is_default: bool,
    pub display_order: i32,
    /// The type's fields, in its own order.
    pub field_keys: Vec<String>,
}

/// The media class a mime type belongs to, in the vocabulary `applies_to` uses.
///
/// Coarse on purpose. The classes exist so ingest can pick a type without being told, and a taxonomy finer
/// than the one an administrator would think in ("is a TIFF an image?") would make that choice unpredictable.
pub fn media_class(mime: &str) -> &'static str {
    let (top, sub) = mime.split_once('/').unwrap_or((mime, ""));
    match top {
        "image" => "image",
        "video" => "video",
        "audio" => "audio",
        "text" => "document",
        "application" => match sub {
            "zip" | "x-tar" | "gzip" | "x-7z-compressed" | "x-rar-compressed" => "archive",
            _ => "document",
        },
        _ => "document",
    }
}

/// Defines a metadata type with its field list.
pub async fn define(pool: &sqlx::PgPool, spec: NewType) -> Result<MetadataType, TypeRefusal> {
    let mut tx = pool.begin().await.map_err(Error::from)?;

    let clash: Option<i32> = sqlx::query_scalar("SELECT 1 FROM metadata_types WHERE key = $1")
        .bind(&spec.key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::from)?;
    if clash.is_some() {
        return Err(TypeRefusal::DuplicateKey(spec.key));
    }

    // Checked by name before the insert rather than left to the foreign key: this refusal reaches an
    // administrator assembling a form, and a constraint-violation string is not something they can act on.
    for key in &spec.field_keys {
        let known: Option<i32> = sqlx::query_scalar("SELECT 1 FROM field_defs WHERE key = $1")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::from)?;
        if known.is_none() {
            return Err(TypeRefusal::UnknownField(key.clone()));
        }
    }

    let id = Uuid::new_v4();
    // The default is cleared first when this type claims it, because the partial unique index would
    // otherwise refuse the insert — and "make this the default" is an instruction to move it, not a
    // question about what the current one is.
    if spec.is_default {
        sqlx::query(
            "UPDATE metadata_types SET is_default = false, updated_at = now() WHERE is_default",
        )
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
    }
    sqlx::query(
        "INSERT INTO metadata_types (id, key, label, applies_to, is_default, display_order) \
         VALUES ($1, $2, $3, $4, $5, \
                 coalesce((SELECT max(display_order) + 1 FROM metadata_types), 0))",
    )
    .bind(id)
    .bind(&spec.key)
    .bind(&spec.label)
    .bind(&spec.applies_to)
    .bind(spec.is_default)
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;

    write_members(&mut tx, id, &spec.field_keys).await?;
    tx.commit().await.map_err(Error::from)?;

    load(pool, id).await
}

/// Replaces a type's field list, in the given order.
pub async fn set_fields(
    pool: &sqlx::PgPool,
    id: Uuid,
    field_keys: &[String],
) -> Result<MetadataType, TypeRefusal> {
    let mut tx = pool.begin().await.map_err(Error::from)?;
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM metadata_types WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::from)?;
    if exists.is_none() {
        return Err(TypeRefusal::UnknownType(id));
    }
    for key in field_keys {
        let known: Option<i32> = sqlx::query_scalar("SELECT 1 FROM field_defs WHERE key = $1")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::from)?;
        if known.is_none() {
            return Err(TypeRefusal::UnknownField(key.clone()));
        }
    }
    sqlx::query("DELETE FROM metadata_type_fields WHERE metadata_type_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
    write_members(&mut tx, id, field_keys).await?;
    tx.commit().await.map_err(Error::from)?;
    load(pool, id).await
}

/// Makes `id` the tenant's fallback type, moving the flag off whatever held it.
pub async fn set_default(pool: &sqlx::PgPool, id: Uuid) -> Result<(), TypeRefusal> {
    let mut tx = pool.begin().await.map_err(Error::from)?;
    // Cleared then set, in one transaction: the partial unique index makes the two-statement order the only
    // one that works, and a reader either sees the old default or the new one.
    sqlx::query(
        "UPDATE metadata_types SET is_default = false, updated_at = now() WHERE is_default",
    )
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;
    let touched = sqlx::query(
        "UPDATE metadata_types SET is_default = true, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;
    if touched.rows_affected() == 0 {
        return Err(TypeRefusal::UnknownType(id));
    }
    tx.commit().await.map_err(Error::from)?;
    Ok(())
}

/// Removes a type. Assets referencing it fall back rather than being blocked or orphaned.
pub async fn remove(pool: &sqlx::PgPool, id: Uuid) -> Result<(), TypeRefusal> {
    // `ON DELETE SET NULL` on `assets.metadata_type_id` does the clearing. Deliberate: removing a type is an
    // administrative decision about the schema and must not be refused because a hundred thousand assets
    // happen to reference it — they fall back, and the fallback is visible rather than empty.
    let touched = sqlx::query("DELETE FROM metadata_types WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(Error::from)?;
    if touched.rows_affected() == 0 {
        return Err(TypeRefusal::UnknownType(id));
    }
    Ok(())
}

/// Assigns (or clears) an asset's metadata type.
pub async fn assign(
    pool: &sqlx::PgPool,
    asset_id: Uuid,
    metadata_type_id: Option<Uuid>,
) -> Result<(), TypeRefusal> {
    sqlx::query("UPDATE assets SET metadata_type_id = $2, updated_at = now() WHERE id = $1")
        .bind(asset_id)
        .bind(metadata_type_id)
        .execute(pool)
        .await
        .map_err(Error::from)?;
    Ok(())
}

/// Every type, in display order.
pub async fn list(pool: &sqlx::PgPool) -> Result<Vec<MetadataType>, TypeRefusal> {
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM metadata_types ORDER BY display_order, key")
            .fetch_all(pool)
            .await
            .map_err(Error::from)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(load(pool, id).await?);
    }
    Ok(out)
}

/// One type by key.
pub async fn by_key(pool: &sqlx::PgPool, key: &str) -> Result<Option<MetadataType>, TypeRefusal> {
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM metadata_types WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(Error::from)?;
    match id {
        Some(id) => Ok(Some(load(pool, id).await?)),
        None => Ok(None),
    }
}

/// The type an asset of this mime type should get, or `None` when the tenant has defined no types.
///
/// Prefers a type claiming the mime's media class, then the default. Ingest calls this so nobody has to set a
/// type by hand for the ordinary case — and a media class nobody anticipated still lands somewhere.
pub async fn for_mime(
    pool: &sqlx::PgPool,
    mime: &str,
) -> Result<Option<MetadataType>, TypeRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    for_mime_on(&mut conn, mime).await
}

/// [`for_mime`] on a caller's connection, so ingest resolves inside its own transaction.
pub async fn for_mime_on(
    conn: &mut sqlx::PgConnection,
    mime: &str,
) -> Result<Option<MetadataType>, TypeRefusal> {
    let class = media_class(mime);
    let id: Option<Uuid> = sqlx::query_scalar(
        // One query, ordered by specificity: a class match beats the default, and `display_order` decides
        // between two types claiming the same class so the answer is stable rather than whichever row the
        // planner reached first.
        "SELECT id FROM metadata_types \
         WHERE $1 = ANY(applies_to) OR is_default \
         ORDER BY ($1 = ANY(applies_to)) DESC, display_order, key \
         LIMIT 1",
    )
    .bind(class)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    match id {
        Some(id) => Ok(Some(load_on(&mut *conn, id).await?)),
        None => Ok(None),
    }
}

/// The field definitions that apply to `asset_id`, in the order its type puts them.
///
/// See the module docs for the resolution chain and why it cannot end in nothing.
pub async fn fields_for(pool: &sqlx::PgPool, asset_id: Uuid) -> Result<Vec<FieldDef>, TypeRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    fields_for_on(&mut conn, asset_id).await
}

/// [`fields_for`] on a caller's connection, so it joins a tenant-scoped transaction.
///
/// This is the variant validation uses: reading the asset, resolving its field list, validating and writing
/// have to be one transaction, or a type reassignment lands between the resolve and the write and the payload
/// is checked against a form that no longer applies.
pub async fn fields_for_on(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
) -> Result<Vec<FieldDef>, TypeRefusal> {
    let assigned: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT metadata_type_id FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(Error::from)?;

    let type_id = match assigned.flatten() {
        Some(id) => Some(id),
        None => sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM metadata_types WHERE is_default ORDER BY display_order, key LIMIT 1",
        )
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?,
    };

    let Some(type_id) = type_id else {
        // No type on the asset and no default: either the tenant has not opted in, or it defined types
        // without nominating a fallback. Both mean "the whole vocabulary", which is the pre-types behaviour
        // and never hides a stored value.
        return Ok(crate::fields::load(&mut *conn).await?);
    };

    let ordered: Vec<String> = sqlx::query_scalar(
        "SELECT field_key FROM metadata_type_fields WHERE metadata_type_id = $1 \
         ORDER BY display_order, field_key",
    )
    .bind(type_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;

    // Filtered from the loaded vocabulary rather than re-read per key: `fields::load` is the one place a row
    // becomes a `FieldDef`, and a second parse here is a second place for the two to disagree.
    let vocabulary = crate::fields::load(&mut *conn).await?;
    Ok(ordered
        .into_iter()
        .filter_map(|key| vocabulary.iter().find(|def| def.key == key).cloned())
        .collect())
}

async fn load(pool: &sqlx::PgPool, id: Uuid) -> Result<MetadataType, TypeRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    load_on(&mut conn, id).await
}

async fn load_on(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<MetadataType, TypeRefusal> {
    let row: Option<(Uuid, String, String, Vec<String>, bool, i32)> = sqlx::query_as(
        "SELECT id, key, label, applies_to, is_default, display_order \
         FROM metadata_types WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    let Some((id, key, label, applies_to, is_default, display_order)) = row else {
        return Err(TypeRefusal::UnknownType(id));
    };
    let field_keys: Vec<String> = sqlx::query_scalar(
        "SELECT field_key FROM metadata_type_fields WHERE metadata_type_id = $1 \
         ORDER BY display_order, field_key",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(MetadataType {
        id,
        key,
        label,
        applies_to,
        is_default,
        display_order,
        field_keys,
    })
}

async fn write_members(
    tx: &mut sqlx::PgConnection,
    id: Uuid,
    field_keys: &[String],
) -> Result<(), TypeRefusal> {
    if field_keys.is_empty() {
        return Ok(());
    }
    // One statement with ordinality, so the list's order is the stored order and a failure leaves none of it
    // rather than a prefix.
    sqlx::query(
        "INSERT INTO metadata_type_fields (metadata_type_id, field_key, display_order) \
         SELECT $1, member.key, member.ord - 1 \
         FROM unnest($2::text[]) WITH ORDINALITY AS member(key, ord)",
    )
    .bind(id)
    .bind(field_keys)
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::media_class;

    /// Every class, directly.
    ///
    /// A pure-function test rather than only the integration one, because a class error is invisible through
    /// `for_mime`: if the wrong class still lands on the default type, the assertion passes and the mapping is
    /// wrong. Mutation testing found exactly that — `image/svg+xml` was "verified" by a fallback.
    #[test]
    fn every_mime_lands_in_the_class_an_administrator_would_expect() {
        for (mime, class) in [
            ("image/jpeg", "image"),
            ("image/png", "image"),
            ("image/svg+xml", "image"),
            ("image/tiff", "image"),
            ("video/mp4", "video"),
            ("video/quicktime", "video"),
            ("audio/mpeg", "audio"),
            ("audio/wav", "audio"),
            ("text/plain", "document"),
            ("text/csv", "document"),
            ("application/pdf", "document"),
            ("application/msword", "document"),
            ("application/zip", "archive"),
            ("application/x-tar", "archive"),
            ("application/gzip", "archive"),
            ("application/x-7z-compressed", "archive"),
            ("application/x-rar-compressed", "archive"),
        ] {
            assert_eq!(media_class(mime), class, "{mime}");
        }

        // A mime with no subtype, and one from a family nobody anticipated. Both have to land somewhere:
        // `for_mime` uses the class to pick a form, and a class of "" would match no type and silently take
        // the default even where a document type exists.
        assert_eq!(media_class("model/gltf-binary"), "document");
        assert_eq!(media_class("application"), "document");
        assert_eq!(media_class(""), "document");
    }
}
