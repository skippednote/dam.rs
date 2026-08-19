//! Named download formats (Q.11).
//!
//! The set a tenant offers a person downloading an asset: "Web JPEG, 2048px", "Print PNG, full size". Each one
//! has a description written for whoever is choosing, an order somebody considered, and optionally a permission
//! a role must carry to use it.
//!
//! ## Two gates, asset first
//!
//! A conversion never widens anything. The asset's own Download gate is checked by the caller's predicate
//! before this module is consulted, and [`offerable`] can only *narrow* what that allowed. The order matters
//! for the reason it always does here: asking "which formats may you use" first and "may you have this asset"
//! second would answer the second question through the shape of the first.
//!
//! ## Withdrawn, not deleted
//!
//! A delivery token carries a conversion's key, so a link already in somebody's email resolves through this
//! table. [`withdraw`] hides a format from what is offered and leaves what has been rendered resolvable;
//! nothing here deletes a row. An administrator tidying a list should not break a colleague's link.
//!
//! ## The cache key comes from the recipe
//!
//! [`Conversion::op_hash`] is `dam_media::profiles::tenant_op_hash` over the recipe columns, so a redefinition
//! is a different key and renders fresh. That is why there is no revision column — see the migration.

use crate::Error;
use dam_media::derive::{Fit, OutputFormat, Rendition};
use uuid::Uuid;

/// Why a conversion could not be written.
#[derive(Debug, thiserror::Error)]
pub enum ConversionRefusal {
    /// The key is already taken.
    ///
    /// Its own variant rather than a database error, because two administrators naming a format `web-2048`
    /// on the same afternoon is ordinary and the second one needs to be told which word to change.
    #[error("a conversion named {0} already exists")]
    DuplicateKey(String),

    /// A field the database refuses: an unusable size, quality, format, fit, background, key or permission.
    ///
    /// One variant carrying the constraint's own name. The CHECKs are the specification — restating each of
    /// them as a Rust branch would be a second copy to drift from the first.
    #[error("{0}")]
    Invalid(String),

    /// No such conversion.
    #[error("no conversion {0}")]
    Unknown(Uuid),

    #[error(transparent)]
    Database(#[from] Error),
}

/// One named download format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub description: String,
    pub media_class: String,
    pub max_width: i32,
    pub max_height: i32,
    pub format: String,
    pub quality: i32,
    pub fit: String,
    pub background: String,
    pub required_permission: Option<String>,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Conversion {
    /// The recipe, as the renderer takes it.
    ///
    /// `None` when a stored value is one this build cannot render. That is not dead code guarding against a
    /// CHECK constraint: a *future* migration widening the vocabulary — `heif`, `jxl` — leaves rows an older
    /// binary must refuse rather than approximate, and during a rolling deploy both binaries are live.
    pub fn rendition(&self) -> Option<Rendition> {
        let format = match self.format.as_str() {
            "jpeg" => OutputFormat::Jpeg,
            "png" => OutputFormat::Png,
            "webp" => OutputFormat::WebP,
            "avif" => OutputFormat::Avif,
            _ => return None,
        };
        let fit = match self.fit.as_str() {
            "contain" => Fit::Contain,
            "cover" => Fit::Cover,
            _ => return None,
        };
        let background = hex_rgb(&self.background)?;
        Some(Rendition {
            // The CHECKs bound these to 16..=20000 and 1..=100, so the casts cannot wrap. `try_into` anyway,
            // because "the database says so" is an argument that stops being true one migration later.
            width: self.max_width.try_into().ok()?,
            height: self.max_height.try_into().ok()?,
            format,
            quality: self.quality.try_into().ok()?,
            fit,
            background,
        })
    }

    /// The cache key a rendered derivative is stored under.
    ///
    /// `None` for a recipe this build cannot render — see [`Self::rendition`].
    pub fn op_hash(&self) -> Option<String> {
        self.rendition()
            .as_ref()
            .map(dam_media::profiles::tenant_op_hash)
    }

    /// Whether a caller holding `permissions` may use this format.
    ///
    /// `None` on the column means "anybody who may download the asset at all", which is the common case: most
    /// formats exist to be used. A named permission narrows it, and narrowing is all it can do — the asset's
    /// Download gate has already been passed by the time anybody asks this.
    pub fn permitted_for(&self, permissions: &[String]) -> bool {
        match &self.required_permission {
            None => true,
            Some(required) => permissions.iter().any(|held| held == required),
        }
    }
}

/// A conversion to create, or the new definition of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewConversion {
    pub key: String,
    pub label: String,
    pub description: String,
    pub media_class: String,
    pub max_width: i32,
    pub max_height: i32,
    pub format: String,
    pub quality: i32,
    pub fit: String,
    pub background: String,
    pub required_permission: Option<String>,
    pub sort_order: i32,
}

/// Creates a conversion.
pub async fn create(
    conn: &mut sqlx::PgConnection,
    new: &NewConversion,
    created_by: Option<Uuid>,
) -> Result<Conversion, ConversionRefusal> {
    let row: Option<Row> = sqlx::query_as(
        "INSERT INTO conversions \
         (id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
          background, required_permission, sort_order, created_by) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT (key) DO NOTHING \
         RETURNING id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
                   background, required_permission, is_active, sort_order, created_at",
    )
    .bind(new.key.trim())
    .bind(new.label.trim())
    .bind(new.description.trim())
    .bind(&new.media_class)
    .bind(new.max_width)
    .bind(new.max_height)
    .bind(&new.format)
    .bind(new.quality)
    .bind(&new.fit)
    .bind(&new.background)
    .bind(new.required_permission.as_deref())
    .bind(new.sort_order)
    .bind(created_by)
    .fetch_optional(&mut *conn)
    .await
    .map_err(constraint_or_database)?;

    // `DO NOTHING` returns no row for a duplicate, which is how the duplicate is detected without a second
    // query and without a race: the unique index decides, not a prior SELECT.
    row.map(into_conversion)
        .ok_or_else(|| ConversionRefusal::DuplicateKey(new.key.trim().to_owned()))
}

/// Replaces a conversion's definition.
///
/// The key is *not* replaceable. A delivery token carries it, so renaming one would strand links that were
/// valid when they were sent — and a rename is indistinguishable from withdrawing one format and creating
/// another, which is what somebody actually means when the name was wrong.
pub async fn redefine(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    new: &NewConversion,
) -> Result<Conversion, ConversionRefusal> {
    let row: Option<Row> = sqlx::query_as(
        "UPDATE conversions SET label = $2, description = $3, media_class = $4, max_width = $5, \
                max_height = $6, format = $7, quality = $8, fit = $9, background = $10, \
                required_permission = $11, sort_order = $12, updated_at = now() \
         WHERE id = $1 \
         RETURNING id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
                   background, required_permission, is_active, sort_order, created_at",
    )
    .bind(id)
    .bind(new.label.trim())
    .bind(new.description.trim())
    .bind(&new.media_class)
    .bind(new.max_width)
    .bind(new.max_height)
    .bind(&new.format)
    .bind(new.quality)
    .bind(&new.fit)
    .bind(&new.background)
    .bind(new.required_permission.as_deref())
    .bind(new.sort_order)
    .fetch_optional(&mut *conn)
    .await
    .map_err(constraint_or_database)?;

    row.map(into_conversion)
        .ok_or(ConversionRefusal::Unknown(id))
}

/// Withdraws a conversion, or restores a withdrawn one.
///
/// Never deletes. See the module docs: an already-issued delivery token names this row.
pub async fn set_active(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    active: bool,
) -> Result<Conversion, ConversionRefusal> {
    let row: Option<Row> = sqlx::query_as(
        "UPDATE conversions SET is_active = $2, updated_at = now() WHERE id = $1 \
         RETURNING id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
                   background, required_permission, is_active, sort_order, created_at",
    )
    .bind(id)
    .bind(active)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;

    row.map(into_conversion)
        .ok_or(ConversionRefusal::Unknown(id))
}

/// Every conversion, withdrawn ones included, in the order a list shows them.
///
/// For administration. What a person downloading is offered is [`offerable`], which is a narrower question.
pub async fn all(conn: &mut sqlx::PgConnection) -> Result<Vec<Conversion>, Error> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
                background, required_permission, is_active, sort_order, created_at \
         FROM conversions ORDER BY is_active DESC, sort_order, key",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(into_conversion).collect())
}

/// What to offer somebody downloading an asset of `media_class`.
///
/// Active, applicable, and permitted — in that order, and all three in one place so a caller cannot apply two
/// of them. A format the caller has no permission for is **absent**, not shown-and-refused: a list of things
/// you cannot have is a worse answer than a shorter list.
///
/// The permission filter is applied in Rust rather than SQL. `required_permission` is a single value against a
/// small held set, so the array containment operator would buy nothing and would put the rule in a second
/// language — where [`Conversion::permitted_for`] is the one the direct-request path also calls.
pub async fn offerable(
    conn: &mut sqlx::PgConnection,
    media_class: &str,
    permissions: &[String],
) -> Result<Vec<Conversion>, Error> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
                background, required_permission, is_active, sort_order, created_at \
         FROM conversions WHERE is_active AND media_class = $1 ORDER BY sort_order, key",
    )
    .bind(media_class)
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(into_conversion)
        .filter(|conversion| conversion.permitted_for(permissions))
        // A recipe this build cannot render is not offered. Offering it would put a format in a dialog that
        // the worker then refuses, which reads as a broken download rather than as a version skew.
        .filter(|conversion| conversion.rendition().is_some())
        .collect())
}

/// One conversion by the name a delivery token carries.
///
/// Withdrawn ones **included**, and that is the point of the split from [`offerable`]: a link sent last month
/// names a format that may since have been withdrawn, and the bytes it points at are still the bytes somebody
/// was promised. The caller decides whether being withdrawn matters for what it is doing.
pub async fn by_key(conn: &mut sqlx::PgConnection, key: &str) -> Result<Option<Conversion>, Error> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, key, label, description, media_class, max_width, max_height, format, quality, fit, \
                background, required_permission, is_active, sort_order, created_at \
         FROM conversions WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(into_conversion))
}

/// Which media class an asset's mime belongs to.
///
/// Here rather than in the API layer because [`offerable`] takes the class and the two must agree: a mime the
/// classifier calls `document` and the table calls `image` would silently offer nothing.
pub fn class_of(mime: &str) -> &'static str {
    match mime.split('/').next().unwrap_or("") {
        "image" => "image",
        "video" => "video",
        "audio" => "audio",
        _ => "document",
    }
}

type Row = (
    Uuid,
    String,
    String,
    String,
    String,
    i32,
    i32,
    String,
    i32,
    String,
    String,
    Option<String>,
    bool,
    i32,
    chrono::DateTime<chrono::Utc>,
);

fn into_conversion(row: Row) -> Conversion {
    let (
        id,
        key,
        label,
        description,
        media_class,
        max_width,
        max_height,
        format,
        quality,
        fit,
        background,
        required_permission,
        is_active,
        sort_order,
        created_at,
    ) = row;
    Conversion {
        id,
        key,
        label,
        description,
        media_class,
        max_width,
        max_height,
        format,
        quality,
        fit,
        background,
        required_permission,
        is_active,
        sort_order,
        created_at,
    }
}

/// Turns a CHECK-constraint violation into [`ConversionRefusal::Invalid`], carrying the constraint's name.
///
/// The constraints are the specification for what a usable recipe is. Re-deriving them as Rust branches would
/// be a second copy that drifts — and the copy that drifts is the one that lets a 0×0 rendition through.
fn constraint_or_database(error: sqlx::Error) -> ConversionRefusal {
    if let sqlx::Error::Database(db) = &error {
        // 23514 is check_violation. Matched on the code rather than on the message, which is localised.
        if db.code().as_deref() == Some("23514") {
            return ConversionRefusal::Invalid(
                db.constraint()
                    .unwrap_or("a value the database refuses")
                    .to_owned(),
            );
        }
    }
    ConversionRefusal::Database(Error::from(error))
}

/// Six lowercase hex digits to a byte triple.
fn hex_rgb(hex: &str) -> Option<[u8; 3]> {
    if hex.len() != 6 {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversion(format: &str, fit: &str, background: &str) -> Conversion {
        Conversion {
            id: Uuid::nil(),
            key: "web-2048".into(),
            label: "Web JPEG".into(),
            description: "For a web page.".into(),
            media_class: "image".into(),
            max_width: 2048,
            max_height: 2048,
            format: format.into(),
            quality: 82,
            fit: fit.into(),
            background: background.into(),
            required_permission: None,
            is_active: true,
            sort_order: 0,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_recipe_this_build_cannot_render_is_refused_rather_than_approximated() {
        // The case is a rolling deploy: a newer migration widens the format vocabulary, and this binary must
        // refuse the row rather than pick the nearest thing it knows. Approximating would deliver a JPEG to
        // somebody who asked for the format the newer binary offered them.
        assert!(conversion("jxl", "contain", "ffffff").rendition().is_none());
        assert!(
            conversion("jpeg", "letterbox", "ffffff")
                .rendition()
                .is_none()
        );
        assert!(conversion("jpeg", "contain", "white").rendition().is_none());
        assert!(conversion("jpeg", "contain", "fff").rendition().is_none());
        // And no cache key either, so nothing can be looked up or written for it.
        assert!(conversion("jxl", "contain", "ffffff").op_hash().is_none());
    }

    #[test]
    fn a_recipe_this_build_knows_becomes_the_renderers_own_type() {
        let rendition = conversion("webp", "cover", "0a0b0c")
            .rendition()
            .expect("renderable");
        assert_eq!(rendition.width, 2048);
        assert_eq!(rendition.format, OutputFormat::WebP);
        assert_eq!(rendition.fit, Fit::Cover);
        assert_eq!(rendition.background, [10, 11, 12]);
        assert_eq!(rendition.quality, 82);
    }

    #[test]
    fn an_unrestricted_conversion_is_open_and_a_restricted_one_is_not() {
        let open = conversion("jpeg", "contain", "ffffff");
        assert!(open.permitted_for(&[]), "no permission named means anybody");

        let mut print = open.clone();
        print.required_permission = Some("conversion:print".into());
        assert!(!print.permitted_for(&[]));
        assert!(!print.permitted_for(&["conversion:web".to_owned()]));
        assert!(print.permitted_for(&["conversion:print".to_owned()]));
        // Not a prefix or substring match: `conversion:print-extra` is a different permission, and treating it
        // as this one would grant a format nobody granted.
        assert!(!print.permitted_for(&["conversion:print-extra".to_owned()]));
    }

    #[test]
    fn a_mime_maps_to_the_class_the_table_uses() {
        assert_eq!(class_of("image/jpeg"), "image");
        assert_eq!(class_of("video/mp4"), "video");
        assert_eq!(class_of("audio/mpeg"), "audio");
        // Everything else is a document, including something unrecognised: the fallback has to be the class
        // with no conversions rather than the one that has them, or an unknown type gets offered an image
        // recipe it cannot be rendered through.
        assert_eq!(class_of("application/pdf"), "document");
        assert_eq!(class_of("nonsense"), "document");
        assert_eq!(class_of(""), "document");
    }
}
