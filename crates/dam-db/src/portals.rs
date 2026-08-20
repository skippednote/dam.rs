//! Named, branded shares of a set (Q.14).
//!
//! The storage is one table; what is worth defending is the relationship between it and `share_links`. A portal
//! is *content and presentation*; the share link pointing at it is *access*. Everything about who may look and
//! who may take stays in the share machinery that 0001 and 3.4 built, so:
//!
//! - creating a portal creates its share link in the same transaction — a portal with no link is unreachable and
//!   a link with no portal renders nothing, and neither should be able to exist alone;
//! - retiring a portal revokes the link in the same transaction, because a URL that was handed out must stop
//!   working, and stopping *one* of the two would leave the other half live;
//! - nothing here checks a passcode, an expiry or a download cap. Those questions have one answer, in
//!   `crate::shares`, and a second one here would be the divergence §12 warns about.
//!
//! ## Exactly one source, and the code does not choose
//!
//! A collection, a saved search, or a media class. The database refuses two (`portals_one_source`), so this
//! module can read the row and dispatch — rather than defaulting when it finds two, which is how a portal ends up
//! showing something nobody configured.

use crate::Error;
use chrono::{DateTime, Utc};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// Why a portal could not be written.
#[derive(Debug, thiserror::Error)]
pub enum PortalRefusal {
    #[error("no portal {0}")]
    Unknown(Uuid),

    /// A field the database refuses — the slug's shape, the accent colour, the one-source rule.
    ///
    /// Carries the constraint's name. The CHECKs are the specification; restating them as Rust branches would be
    /// a second copy to drift from the first.
    #[error("{0}")]
    Invalid(String),

    /// That slug is taken. Its own variant because the fix is a different name rather than a different value.
    #[error("the name `{0}` is already taken by another portal")]
    Taken(String),

    #[error(transparent)]
    Database(#[from] Error),
}

/// Which layout a portal wears.
///
/// Presentation, never permission — see the migration's note. `Video` and `Channel` exist because a library
/// whose whole point is video should not have to curate a collection to have a portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Standard,
    Brand,
    Video,
    Channel,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Brand => "brand",
            Self::Video => "video",
            Self::Channel => "channel",
        }
    }

    /// Parses a stored value. `None` for anything else, which a caller refuses rather than approximates: a kind
    /// this build does not know is a newer migration read by an older binary, and guessing a layout would show a
    /// tenant a portal they did not design.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "standard" => Some(Self::Standard),
            "brand" => Some(Self::Brand),
            "video" => Some(Self::Video),
            "channel" => Some(Self::Channel),
            _ => None,
        }
    }
}

/// Where a portal's assets come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Collection(Uuid),
    SavedSearch(Uuid),
    /// Everything of one media class — what makes a video or channel portal possible without curation.
    MediaClass(String),
}

/// One portal, as everything but the share machinery sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portal {
    pub id: Uuid,
    /// The slug. Resolves as a URL only when [`Self::is_public`] — a slug is guessable and a token is not.
    pub key: String,
    pub title: String,
    pub intro: String,
    pub kind: String,
    pub source: Source,
    pub logo_asset_id: Option<Uuid>,
    pub accent: String,
    pub is_public: bool,
    pub allow_search: bool,
    pub created_at: DateTime<Utc>,
    pub retired_at: Option<DateTime<Utc>>,
}

impl Portal {
    /// The layout, when this build knows it.
    pub fn kind(&self) -> Option<Kind> {
        Kind::parse(&self.kind)
    }

    /// Whether this portal still answers.
    pub fn is_live(&self) -> bool {
        self.retired_at.is_none()
    }
}

/// A portal to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPortal {
    pub key: String,
    pub title: String,
    pub intro: String,
    pub kind: Kind,
    pub source: Source,
    pub logo_asset_id: Option<Uuid>,
    pub accent: String,
    pub is_public: bool,
    pub allow_search: bool,
}

/// A portal and the token that reaches it.
#[derive(Debug, Clone)]
pub struct Created {
    pub portal: Portal,
    /// The share link's id, for revocation.
    pub share_id: Uuid,
    /// The token, readable exactly once — it is stored as a digest.
    pub token: String,
}

const SELECT: &str = "SELECT id, key, title, intro, kind, collection_id, saved_search_id, media_class, \
                             logo_asset_id, accent, is_public, allow_search, created_at, retired_at \
                      FROM portals";

/// The row as the columns come back.
type Row = (
    Uuid,
    String,
    String,
    String,
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<String>,
    Option<Uuid>,
    String,
    bool,
    bool,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

fn into_portal(row: Row) -> Result<Portal, Error> {
    let (
        id,
        key,
        title,
        intro,
        kind,
        collection_id,
        saved_search_id,
        media_class,
        logo_asset_id,
        accent,
        is_public,
        allow_search,
        created_at,
        retired_at,
    ) = row;

    // The CHECK guarantees exactly one, so a row with none is a database somebody has edited by hand — reported
    // rather than defaulted, because every default here would show somebody a set they did not configure.
    let source = match (collection_id, saved_search_id, media_class) {
        (Some(id), None, None) => Source::Collection(id),
        (None, Some(id), None) => Source::SavedSearch(id),
        (None, None, Some(class)) => Source::MediaClass(class),
        _ => {
            return Err(Error::Inconsistent(format!(
                "portal {id} does not have exactly one source"
            )));
        }
    };

    Ok(Portal {
        id,
        key,
        title,
        intro,
        kind,
        source,
        logo_asset_id,
        accent,
        is_public,
        allow_search,
        created_at,
        retired_at,
    })
}

/// Creates a portal and the share link that reaches it, in one transaction.
///
/// The two together or neither: a portal with no link cannot be visited, and a link with no portal renders
/// nothing. `spec` carries the access half — expiry, passcode, download cap, whether originals may be taken —
/// because that is the share's business and this module does not duplicate it.
pub async fn create(
    conn: &mut sqlx::PgConnection,
    new: &NewPortal,
    spec: &crate::shares::ShareSpec<'_>,
    created_by: Option<Uuid>,
) -> Result<Created, PortalRefusal> {
    let id = Uuid::now_v7();
    let (collection_id, saved_search_id, media_class) = match &new.source {
        Source::Collection(id) => (Some(*id), None, None),
        Source::SavedSearch(id) => (None, Some(*id), None),
        Source::MediaClass(class) => (None, None, Some(class.clone())),
    };

    sqlx::query(
        "INSERT INTO portals \
            (id, key, title, intro, kind, collection_id, saved_search_id, media_class, \
             logo_asset_id, accent, is_public, allow_search, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(id)
    .bind(new.key.trim())
    .bind(new.title.trim())
    .bind(&new.intro)
    .bind(new.kind.as_str())
    .bind(collection_id)
    .bind(saved_search_id)
    .bind(media_class.as_deref())
    .bind(new.logo_asset_id)
    .bind(new.accent.trim().to_lowercase())
    .bind(new.is_public)
    .bind(new.allow_search)
    .bind(created_by)
    .execute(&mut *conn)
    .await
    .map_err(|error| constraint_or_database(error, &new.key))?;

    // The share, pointed at the portal. `kind = 'portal'` is what makes the portal page render a set rather
    // than one asset — the same shape Q.13d used for an order pickup.
    let share = crate::shares::create_on(
        &mut *conn,
        &crate::shares::ShareSpec {
            kind: "portal",
            target_id: Some(id),
            ..spec.clone()
        },
    )
    .await
    .map_err(PortalRefusal::Database)?;

    let portal = read(&mut *conn, id)
        .await?
        .ok_or(PortalRefusal::Unknown(id))?;
    Ok(Created {
        portal,
        share_id: share.id,
        token: share.token().to_owned(),
    })
}

/// One portal by id.
pub async fn read(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Option<Portal>, Error> {
    // `QueryBuilder` rather than `format!`: sqlx refuses a dynamically built string without an explicit audit,
    // and the refusal is right — it marks every site where an injection would have to be reasoned about.
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    builder.push(" WHERE id = ");
    builder.push_bind(id);
    let row: Option<Row> = builder.build_query_as().fetch_optional(&mut *conn).await?;
    row.map(into_portal).transpose()
}

/// One *public, live* portal by slug.
///
/// The lookup a slug visit does, and it is deliberately narrower than [`read`]: a private portal is reachable
/// only by its token, and a retired one is reachable not at all. Returning it here and refusing later would put
/// the decision in two places.
pub async fn by_public_key(
    conn: &mut sqlx::PgConnection,
    key: &str,
) -> Result<Option<Portal>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    builder.push(" WHERE key = ");
    builder.push_bind(key.trim().to_lowercase());
    builder.push(" AND is_public AND retired_at IS NULL");
    let row: Option<Row> = builder.build_query_as().fetch_optional(&mut *conn).await?;
    row.map(into_portal).transpose()
}

/// Every portal, retired ones included — somebody has to be able to see what they retired.
pub async fn all(conn: &mut sqlx::PgConnection) -> Result<Vec<Portal>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(SELECT);
    builder.push(" ORDER BY retired_at IS NOT NULL, created_at DESC");
    let rows: Vec<Row> = builder.build_query_as().fetch_all(&mut *conn).await?;
    rows.into_iter().map(into_portal).collect()
}

/// The live token for a portal, if it has one.
pub async fn share_of(
    conn: &mut sqlx::PgConnection,
    portal_id: Uuid,
) -> Result<Option<Uuid>, Error> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM share_links \
          WHERE kind = 'portal' AND target_id = $1 AND revoked_at IS NULL \
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(portal_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// What a portal shows and says. Never what it permits — that is the share's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    pub title: String,
    pub intro: String,
    pub kind: Kind,
    pub logo_asset_id: Option<Uuid>,
    pub accent: String,
    pub is_public: bool,
    pub allow_search: bool,
}

/// Changes how a portal looks and reads.
///
/// Deliberately cannot change the *source*: a portal that swapped its set would show a different library to
/// everyone holding the old URL, which is a new portal wearing an old name. Retire it and make another.
pub async fn present(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    presentation: &Presentation,
) -> Result<Portal, PortalRefusal> {
    let updated = sqlx::query(
        "UPDATE portals \
            SET title = $2, intro = $3, kind = $4, logo_asset_id = $5, accent = $6, \
                is_public = $7, allow_search = $8, updated_at = now() \
          WHERE id = $1 AND retired_at IS NULL",
    )
    .bind(id)
    .bind(presentation.title.trim())
    .bind(&presentation.intro)
    .bind(presentation.kind.as_str())
    .bind(presentation.logo_asset_id)
    .bind(presentation.accent.trim().to_lowercase())
    .bind(presentation.is_public)
    .bind(presentation.allow_search)
    .execute(&mut *conn)
    .await
    .map_err(|error| constraint_or_database(error, ""))?;
    if updated.rows_affected() == 0 {
        // Either it does not exist or it is retired, and the two collapse on purpose: a retired portal is not a
        // thing to edit, and saying which would invite an attempt to un-retire by editing.
        return Err(PortalRefusal::Unknown(id));
    }
    read(&mut *conn, id)
        .await?
        .ok_or(PortalRefusal::Unknown(id))
}

/// Retires a portal and revokes the link that reaches it.
///
/// Both, in one transaction. Retiring the portal alone would leave a live token rendering a retired portal;
/// revoking the link alone would leave a portal nobody can reach and nothing saying why.
pub async fn retire(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Portal, PortalRefusal> {
    let updated = sqlx::query(
        "UPDATE portals SET retired_at = now(), updated_at = now() \
          WHERE id = $1 AND retired_at IS NULL",
    )
    .bind(id)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    if updated.rows_affected() == 0 {
        return Err(PortalRefusal::Unknown(id));
    }
    sqlx::query(
        "UPDATE share_links SET revoked_at = now() \
          WHERE kind = 'portal' AND target_id = $1 AND revoked_at IS NULL",
    )
    .bind(id)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    read(&mut *conn, id)
        .await?
        .ok_or(PortalRefusal::Unknown(id))
}

/// Maps a database error onto a refusal, keeping the constraint's name.
fn constraint_or_database(error: sqlx::Error, key: &str) -> PortalRefusal {
    if let sqlx::Error::Database(ref database) = error {
        if database.is_unique_violation() {
            return PortalRefusal::Taken(key.to_owned());
        }
        if let Some(constraint) = database.constraint() {
            return PortalRefusal::Invalid(constraint.to_owned());
        }
    }
    PortalRefusal::Database(Error::from(error))
}

/// One asset a portal shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub asset_id: Uuid,
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
}

/// The assets a collection-backed portal lists.
///
/// ## No access predicate, and that is the design
///
/// A portal's visitor has no account and therefore no predicate. What stands in for one is the tenant's own act
/// of putting an asset in the collection: somebody with Manage decided this asset is published. That decision is
/// the visibility boundary, and it is why this API accepts only a collection — a live query would move the
/// decision from a person to a rule (see NEEDS-REVIEW.md).
///
/// Distribution is a separate question and stays where it always is: every preview and every download is
/// rights-checked per asset at the delivery chokepoint. So an unlicensed asset in a published collection is
/// *listed* and cannot be *taken*, which is the same answer the order pickup gives.
///
/// ## `LIBRARY_ROWS`, for the same reason as everywhere else
///
/// Current versions only, and nothing that is paperwork attached to something else. A portal showing three
/// versions of one photograph, or a model release beside the photograph it belongs to, is a portal nobody would
/// send to a client.
pub async fn members(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
    search: Option<&str>,
    media_class: Option<&str>,
    limit: i64,
) -> Result<Vec<Member>, Error> {
    let mut builder = member_query(
        "SELECT assets.id, assets.filename, assets.mime, assets.bytes",
        collection_id,
        search,
        media_class,
    );
    builder.push(" ORDER BY collection_items.position, assets.filename LIMIT ");
    builder.push_bind(limit);
    let rows: Vec<(Uuid, String, String, i64)> = builder.build_query_as().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(asset_id, filename, mime, bytes)| Member {
            asset_id,
            filename,
            mime,
            bytes,
        })
        .collect())
}

/// How many assets the set holds, after any search.
///
/// Counted with the same predicate as the rows, in a separate statement only because the page is capped: a count
/// from a different query is how a portal tells a visitor there are two hundred assets and shows them twelve for
/// a different reason than the cap.
pub async fn member_count(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
    search: Option<&str>,
    media_class: Option<&str>,
) -> Result<i64, Error> {
    let mut builder = member_query("SELECT count(*)", collection_id, search, media_class);
    Ok(builder.build_query_scalar().fetch_one(pool).await?)
}

/// The shared `FROM`/`WHERE` of both reads, so the count and the rows cannot disagree.
fn member_query(
    select: &str,
    collection_id: Uuid,
    search: Option<&str>,
    media_class: Option<&str>,
) -> QueryBuilder<Postgres> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(select);
    builder.push(
        " FROM collection_items \
          JOIN assets ON assets.id = collection_items.asset_id \
          LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id \
          WHERE collection_items.collection_id = ",
    );
    builder.push_bind(collection_id);
    builder.push(" AND assets.deleted_at IS NULL");
    builder.push(crate::versions::LIBRARY_ROWS);

    if let Some(class) = media_class {
        // Prefix-matched on the mime's type, so `video` covers every codec without a table of them.
        builder.push(" AND assets.mime LIKE ");
        builder.push_bind(format!("{class}/%"));
    }
    if let Some(term) = search {
        // Filename or any metadata value. Not the search index: a portal's set is small and bounded, and going
        // through Tantivy would mean an anonymous visitor's query reaching an index whose documents carry group
        // ids they have no predicate for. `ILIKE` over the set is both simpler and narrower.
        builder.push(" AND (assets.filename ILIKE ");
        builder.push_bind(format!("%{term}%"));
        builder.push(" OR coalesce(asset_metadata.values, '{}'::jsonb)::text ILIKE ");
        builder.push_bind(format!("%{term}%"));
        builder.push(")");
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_kind_is_refused_rather_than_guessed() {
        assert_eq!(Kind::parse("brand"), Some(Kind::Brand));
        // A newer migration read by an older binary. Guessing a layout would show a tenant a portal they did
        // not design.
        assert_eq!(Kind::parse("microsite"), None);
    }

    #[test]
    fn a_row_with_two_sources_is_reported_rather_than_resolved() {
        // The CHECK makes this unreachable through the API; a hand-edited database is the case this covers, and
        // picking one of the two silently is how a portal shows a set nobody configured.
        let row = (
            Uuid::nil(),
            "press".to_owned(),
            "Press".to_owned(),
            String::new(),
            "standard".to_owned(),
            Some(Uuid::nil()),
            Some(Uuid::nil()),
            None,
            None,
            "#2563eb".to_owned(),
            true,
            true,
            Utc::now(),
            None,
        );
        assert!(into_portal(row).is_err());
    }
}
