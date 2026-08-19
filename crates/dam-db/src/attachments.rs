//! Attached documents: the paperwork that goes with an asset (Q.9).
//!
//! A model release, a licence, a contract. See migration 0022 for why an attachment is an ordinary `assets` row
//! marked as belonging to another rather than a table of its own — briefly: it gets the whole ingest path for free,
//! at the cost of one rule about what the library shows, and that rule already existed for versions.
//!
//! ## Attaching is joining, exactly as adding a version is
//!
//! The document is uploaded through the ordinary route and then attached. Nothing here writes bytes.
//!
//! ## Paperwork is as visible as the asset it belongs to
//!
//! A release names a person and carries their signature, so it is arguably more sensitive than the photograph it
//! licenses. It is nonetheless readable by whoever can read the asset, for one reason: the paperwork exists to
//! answer "may we use this", and a rights question somebody cannot check is a rights question they will answer by
//! guessing. Restricting paperwork to a narrower audience than the asset is a coherent alternative and a decision
//! for somebody else — it is recorded in TASKS.md rather than taken here.

use crate::Error;
use dam_core::policy::AccessPredicate;
use uuid::Uuid;

/// What kind of paperwork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Release,
    Licence,
    Contract,
    Permit,
    Other,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Licence => "licence",
            Self::Contract => "contract",
            Self::Permit => "permit",
            Self::Other => "other",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "release" => Some(Self::Release),
            "licence" => Some(Self::Licence),
            "contract" => Some(Self::Contract),
            "permit" => Some(Self::Permit),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// Why an attachment operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentRefusal {
    #[error("no asset {0}")]
    UnknownAsset(Uuid),

    /// Attaching something that is already paperwork for something else.
    ///
    /// Refused rather than re-pointed: paperwork about paperwork would make "not part of the library" a chain to
    /// walk rather than a column to check, and nobody asked for it.
    #[error("asset {0} is already attached to something else")]
    AlreadyAttached(Uuid),

    /// Attaching a document to a document.
    #[error("paperwork cannot have paperwork of its own")]
    ParentIsAttachment,

    /// Attaching something that is a version of something.
    ///
    /// A row cannot be both a superseded version and a release form: the two mean different things about why it is
    /// absent from the library, and a screen would have to guess which.
    #[error("asset {0} is a superseded version, not a document")]
    IsAVersion(Uuid),

    #[error(transparent)]
    Database(#[from] Error),
}

/// One attached document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub asset_id: Uuid,
    pub attached_to: Uuid,
    pub kind: Kind,
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
    pub uploaded_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Attaches an already-uploaded asset to another as paperwork.
pub async fn attach(
    conn: &mut sqlx::PgConnection,
    parent: Uuid,
    document: Uuid,
    kind: Kind,
    predicate: &AccessPredicate,
) -> Result<Vec<Attachment>, AttachmentRefusal> {
    // Both rows visible to this caller, checked together: attaching is a relationship, and a caller must not be
    // able to make one out of a row they cannot see in either direction.
    let visible = crate::assets::visible_among(&mut *conn, predicate, &[parent, document]).await?;
    if !visible.contains(&parent) {
        return Err(AttachmentRefusal::UnknownAsset(parent));
    }
    if !visible.contains(&document) {
        return Err(AttachmentRefusal::UnknownAsset(document));
    }

    let state: Vec<(Uuid, Option<Uuid>, bool)> =
        sqlx::query_as("SELECT id, attached_to, is_current FROM assets WHERE id = ANY($1)")
            .bind(vec![parent, document])
            .fetch_all(&mut *conn)
            .await
            .map_err(Error::from)?;
    let find = |id: Uuid| state.iter().find(|row| row.0 == id).copied();

    let Some((_, parent_attached, _)) = find(parent) else {
        return Err(AttachmentRefusal::UnknownAsset(parent));
    };
    if parent_attached.is_some() {
        return Err(AttachmentRefusal::ParentIsAttachment);
    }

    let Some((_, document_attached, document_current)) = find(document) else {
        return Err(AttachmentRefusal::UnknownAsset(document));
    };
    if let Some(existing) = document_attached {
        // Already somebody's paperwork. Named as such rather than silently moved: a release form attached to two
        // assets by accident is a rights mistake, and re-pointing it would hide the first one.
        if existing != parent {
            return Err(AttachmentRefusal::AlreadyAttached(document));
        }
    }
    if !document_current {
        return Err(AttachmentRefusal::IsAVersion(document));
    }

    sqlx::query(
        "UPDATE assets SET attached_to = $2, attachment_kind = $3, updated_at = now() WHERE id = $1",
    )
    .bind(document)
    .bind(parent)
    .bind(kind.as_str())
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    on_asset(&mut *conn, parent, predicate).await
}

/// Detaches a document, returning it to being an ordinary asset.
///
/// Not a delete: the bytes and the row stay, and the document reappears in the library. Deleting it would make
/// "detach" a destructive verb, and somebody correcting a mis-attachment does not want that.
pub async fn detach(
    conn: &mut sqlx::PgConnection,
    document: Uuid,
    predicate: &AccessPredicate,
) -> Result<(), AttachmentRefusal> {
    let visible = crate::assets::visible_among(&mut *conn, predicate, &[document]).await?;
    if visible.is_empty() {
        return Err(AttachmentRefusal::UnknownAsset(document));
    }
    sqlx::query(
        "UPDATE assets SET attached_to = NULL, attachment_kind = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(document)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(())
}

/// Everything attached to one asset, newest first.
pub async fn on_asset(
    conn: &mut sqlx::PgConnection,
    parent: Uuid,
    predicate: &AccessPredicate,
) -> Result<Vec<Attachment>, AttachmentRefusal> {
    let visible = crate::assets::visible_among(&mut *conn, predicate, &[parent]).await?;
    if visible.is_empty() {
        return Err(AttachmentRefusal::UnknownAsset(parent));
    }

    // The predicate applies to the documents too, not only the parent: an attachment is an asset row and can be in
    // a different asset group, so listing every child unfiltered would leak past the caller's scope through one.
    let mut builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
        "SELECT a.id, a.attached_to, a.attachment_kind, a.filename, a.mime, a.bytes, \
                a.uploaded_by, a.created_at \
         FROM assets a WHERE a.attached_to = ",
    );
    builder.push_bind(parent);
    builder.push(" AND a.id IN (SELECT assets.id FROM assets ");
    builder.push("LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ");
    crate::access::push_asset_filter(&mut builder, predicate)?;
    builder.push(") ORDER BY a.created_at DESC, a.id DESC");

    type Row = (
        Uuid,
        Option<Uuid>,
        Option<String>,
        String,
        String,
        i64,
        Option<Uuid>,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> = builder
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;

    rows.into_iter()
        .map(
            |(asset_id, attached_to, kind, filename, mime, bytes, uploaded_by, created_at)| {
                Ok(Attachment {
                    asset_id,
                    // Both are guaranteed by the column constraint added in 0022 — `attached_to` because the query
                    // filters on it, `kind` because the constraint says the pair travels together. An unreadable value
                    // means the database and this module disagree, which is a bug to surface rather than guess at.
                    attached_to: attached_to.ok_or_else(|| {
                        Error::Migrate(format!("attachment {asset_id} has no parent"))
                    })?,
                    kind: kind.as_deref().and_then(Kind::parse).ok_or_else(|| {
                        Error::Migrate(format!("attachment {asset_id} has no kind"))
                    })?,
                    filename,
                    mime,
                    bytes,
                    uploaded_by,
                    created_at,
                })
            },
        )
        .collect::<Result<Vec<Attachment>, Error>>()
        .map_err(AttachmentRefusal::Database)
}

/// Which of `asset_ids` have paperwork, for the has-attachment facet.
///
/// One query for a page rather than one per asset, and it applies the caller's predicate to the *documents*: an
/// asset whose only release form is outside the caller's scope has none as far as they can tell, and reporting
/// otherwise would disclose that something exists.
pub async fn which_have(
    conn: &mut sqlx::PgConnection,
    asset_ids: &[Uuid],
    predicate: &AccessPredicate,
) -> Result<Vec<Uuid>, AttachmentRefusal> {
    if asset_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
        "SELECT DISTINCT a.attached_to FROM assets a WHERE a.attached_to = ANY(",
    );
    builder.push_bind(asset_ids.to_vec());
    builder.push(") AND a.id IN (SELECT assets.id FROM assets ");
    builder.push("LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ");
    crate::access::push_asset_filter(&mut builder, predicate)?;
    builder.push(")");

    let found: Vec<Option<Uuid>> = builder
        .build_query_scalar()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;
    Ok(found.into_iter().flatten().collect())
}
