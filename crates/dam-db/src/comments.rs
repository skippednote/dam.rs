//! Comments on assets: public and private, routed to people, carrying a status (Q.6).
//!
//! ## Two gates, in this order, always
//!
//! A comment read passes through the caller's asset predicate *first* and its own visibility second. The order is
//! the security property: "find the comments addressed to me, then check their assets" would disclose the
//! existence of assets through the comments hanging off them, which is the same class of leak §7 describes for
//! pagination counts. So every query here starts from the visible-assets set.
//!
//! ## Private means the author and the people named
//!
//! Not "and administrators". Whether a tenant admin may read a private comment is a question about what the
//! product promises, and it is in `NEEDS-REVIEW.md` rather than decided here — the strict rule is the one that can
//! be relaxed later, because adding a reader is additive and un-disclosing is impossible.
//!
//! A recipient who loses access to the asset stops seeing the comment, because the asset gate is outside the
//! visibility gate. Being addressed in a note is not a grant.
//!
//! ## Naming somebody does not widen what they can see
//!
//! Recipients are *routing*. A public comment may name people — "look at this" — and that changes who is notified,
//! not who may read. On a private comment the same list is also the visibility set, which is the one case where
//! the two coincide.
//!
//! ## One level of threading
//!
//! A reply to a reply is refused. Arbitrary depth makes every read a recursive query and every screen an
//! indentation problem, and the shape nobody asked for is the one that is hardest to remove later.

use crate::Error;
use dam_core::query::Planned;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// The longest a comment may be. Mirrors the column's own bound.
pub const MAX_BODY_CHARS: usize = 10_000;

/// Who may read a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Anybody who can see the asset.
    Public,
    /// The author and the named recipients. See the module docs.
    Private,
}

impl Visibility {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "public" => Some(Self::Public),
            "private" => Some(Self::Private),
            _ => None,
        }
    }
}

/// What a comment currently says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    Resolved,
    Approved,
    ChangesRequested,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "open" => Some(Self::Open),
            "resolved" => Some(Self::Resolved),
            "approved" => Some(Self::Approved),
            "changes_requested" => Some(Self::ChangesRequested),
            _ => None,
        }
    }
}

/// Why a comment operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum CommentRefusal {
    /// No such asset, or not one this caller may see. One refusal for both, as everywhere else.
    #[error("no asset {0}")]
    UnknownAsset(Uuid),

    /// No such comment, or not one this caller may read. Same reasoning.
    #[error("no comment {0}")]
    UnknownComment(Uuid),

    #[error("a comment needs between 1 and {MAX_BODY_CHARS} characters; this one has {0}")]
    BadLength(usize),

    #[error("a reply cannot itself be replied to; reply to the comment it is under")]
    TooDeep,

    /// Editing or deleting somebody else's comment.
    #[error("only the author can change the words of a comment")]
    NotYours,

    /// A private comment addressed to nobody would be a note only its author could ever read.
    #[error("a private comment needs at least one recipient, or nobody but you will see it")]
    PrivateWithNoRecipients,

    #[error(transparent)]
    Database(#[from] Error),
}

/// A comment to post.
#[derive(Debug, Clone)]
pub struct NewComment {
    pub asset_id: Uuid,
    pub author_id: Uuid,
    pub body: String,
    pub visibility: Visibility,
    /// Who this is for. Required for a private comment; routing only for a public one.
    pub recipients: Vec<Uuid>,
    /// The comment this replies to, if any.
    pub parent_id: Option<Uuid>,
}

/// A comment as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub author_id: Uuid,
    pub body: String,
    pub visibility: Visibility,
    pub status: Status,
    pub parent_id: Option<Uuid>,
    pub recipients: Vec<Uuid>,
    pub status_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Set when the words changed after posting, so a screen can say so rather than silently showing different
    /// text than whoever replied to it read.
    pub edited_at: Option<chrono::DateTime<chrono::Utc>>,
}

type Row = (
    Uuid,
    Uuid,
    Uuid,
    String,
    String,
    String,
    Option<Uuid>,
    Option<Uuid>,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
);

const COLUMNS: &str = "c.id, c.asset_id, c.author_id, c.body, c.visibility, c.status, c.parent_id, \
                       c.status_by, c.created_at, c.edited_at";

fn comment(row: Row, recipients: Vec<Uuid>) -> Result<Comment, Error> {
    let (
        id,
        asset_id,
        author_id,
        body,
        visibility,
        status,
        parent_id,
        status_by,
        created_at,
        edited_at,
    ) = row;
    Ok(Comment {
        id,
        asset_id,
        author_id,
        body,
        // A column value the CHECK constraint already restricts, so an unreadable one means the database and this
        // module disagree — which is a bug to surface rather than a value to guess at.
        visibility: Visibility::parse(&visibility)
            .ok_or_else(|| Error::Migrate(format!("unknown comment visibility {visibility:?}")))?,
        status: Status::parse(&status)
            .ok_or_else(|| Error::Migrate(format!("unknown comment status {status:?}")))?,
        parent_id,
        recipients,
        status_by,
        created_at,
        edited_at,
    })
}

/// Pushes the visible-assets set for `planned` as a CTE named `visible`.
///
/// Every read here begins with this. Factored out because it is the gate that must not be forgotten, and because
/// four call sites writing the same fragment is four chances for one of them to write it differently.
fn push_visible_assets(
    builder: &mut QueryBuilder<Postgres>,
    planned: &Planned,
) -> Result<(), Error> {
    builder.push(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(builder, planned)?;
    builder.push(") ");
    Ok(())
}

/// Pushes the readability condition for one caller: public, or theirs, or addressed to them.
fn push_readable(builder: &mut QueryBuilder<Postgres>, reader: Uuid) {
    builder.push("(c.visibility = 'public' OR c.author_id = ");
    builder.push_bind(reader);
    builder.push(
        " OR EXISTS (SELECT 1 FROM asset_comment_recipients r \
           WHERE r.comment_id = c.id AND r.identity_id = ",
    );
    builder.push_bind(reader);
    builder.push("))");
}

/// Posts a comment.
pub async fn post(
    conn: &mut sqlx::PgConnection,
    spec: NewComment,
    planned: &Planned,
) -> Result<Comment, CommentRefusal> {
    let length = spec.body.chars().count();
    if length == 0 || length > MAX_BODY_CHARS {
        return Err(CommentRefusal::BadLength(length));
    }
    // Checked before anything is written, because a private note nobody but its author can read is not what
    // anybody meant by "private" — it is a comment that failed silently.
    if spec.visibility == Visibility::Private && spec.recipients.is_empty() {
        return Err(CommentRefusal::PrivateWithNoRecipients);
    }

    require_visible_asset(&mut *conn, spec.asset_id, planned).await?;

    // A reply has to be to a comment this caller can read, on this asset, and not itself a reply.
    if let Some(parent_id) = spec.parent_id {
        let parent = read(&mut *conn, parent_id, spec.author_id, planned).await?;
        if parent.parent_id.is_some() {
            return Err(CommentRefusal::TooDeep);
        }
        if parent.asset_id != spec.asset_id {
            // The parent is readable but belongs to a different asset. `UnknownComment` rather than a distinct
            // refusal: from here it is the same mistake, and a more specific message would confirm that the id
            // exists somewhere the caller was not asking about.
            return Err(CommentRefusal::UnknownComment(parent_id));
        }
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO asset_comments (id, asset_id, author_id, body, visibility, parent_id) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(spec.asset_id)
    .bind(spec.author_id)
    .bind(&spec.body)
    .bind(spec.visibility.as_str())
    .bind(spec.parent_id)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    for identity in &spec.recipients {
        sqlx::query(
            "INSERT INTO asset_comment_recipients (comment_id, identity_id) VALUES ($1, $2) \
             ON CONFLICT (comment_id, identity_id) DO NOTHING",
        )
        .bind(id)
        .bind(identity)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    }

    read(&mut *conn, id, spec.author_id, planned).await
}

/// Every comment on one asset this caller may read, oldest first, replies after their parent.
pub async fn on_asset(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    reader: Uuid,
    planned: &Planned,
) -> Result<Vec<Comment>, CommentRefusal> {
    require_visible_asset(&mut *conn, asset_id, planned).await?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible_assets(&mut builder, planned)?;
    builder.push("SELECT ");
    builder.push(COLUMNS);
    // The join is belt to `require_visible_asset`'s braces, and unobservable because that check has already
    // refused an asset the caller cannot see — removing it changes no test outcome. Both exist because they do
    // different things: the check produces a *refusal*, while the join would produce an empty list, and "no such
    // asset" is the honest answer to a request naming one. Documented rather than left looking tested.
    builder
        .push(" FROM asset_comments c JOIN visible ON visible.id = c.asset_id WHERE c.asset_id = ");
    builder.push_bind(asset_id);
    builder.push(" AND ");
    push_readable(&mut builder, reader);
    // Threads together: a reply sorts under its parent's id, and a top-level comment under its own. Then by time
    // within each thread, so a conversation reads in the order it happened.
    builder.push(" ORDER BY coalesce(c.parent_id, c.id), c.created_at, c.id");

    let rows: Vec<Row> = builder
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;

    // Recipients for the whole page in one read rather than one per comment.
    let ids: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
    let routed: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT comment_id, identity_id FROM asset_comment_recipients \
         WHERE comment_id = ANY($1) ORDER BY added_at, identity_id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row.0;
        let recipients = routed
            .iter()
            .filter(|(comment_id, _)| *comment_id == id)
            .map(|(_, identity)| *identity)
            .collect();
        out.push(comment(row, recipients).map_err(CommentRefusal::Database)?);
    }
    Ok(out)
}

/// One comment, refusing what this caller may not read.
pub async fn read(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    reader: Uuid,
    planned: &Planned,
) -> Result<Comment, CommentRefusal> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible_assets(&mut builder, planned)?;
    builder.push("SELECT ");
    builder.push(COLUMNS);
    builder.push(" FROM asset_comments c JOIN visible ON visible.id = c.asset_id WHERE c.id = ");
    builder.push_bind(id);
    builder.push(" AND ");
    push_readable(&mut builder, reader);

    let row: Option<Row> = builder
        .build_query_as()
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?;
    let row = row.ok_or(CommentRefusal::UnknownComment(id))?;

    let recipients: Vec<Uuid> = sqlx::query_scalar(
        "SELECT identity_id FROM asset_comment_recipients WHERE comment_id = $1 \
         ORDER BY added_at, identity_id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;

    comment(row, recipients).map_err(CommentRefusal::Database)
}

/// Rewrites a comment's words. The author only.
pub async fn amend(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    editor: Uuid,
    body: &str,
    planned: &Planned,
) -> Result<Comment, CommentRefusal> {
    let length = body.chars().count();
    if length == 0 || length > MAX_BODY_CHARS {
        return Err(CommentRefusal::BadLength(length));
    }
    // Readable first, so somebody who cannot see the comment gets "no such comment" rather than "not yours" —
    // which would confirm it exists.
    let existing = read(&mut *conn, id, editor, planned).await?;
    if existing.author_id != editor {
        return Err(CommentRefusal::NotYours);
    }

    sqlx::query(
        "UPDATE asset_comments SET body = $2, edited_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(body)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    read(&mut *conn, id, editor, planned).await
}

/// Moves a comment's status, recording who did it.
///
/// Any reader may set it, not only the author: `approved` is somebody *else's* verdict on what the comment asked
/// for, and a status only its author could move would be a status that could never mean approval. The name is
/// recorded because an approval nobody owns is worth nothing in an audit.
pub async fn set_status(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    actor: Uuid,
    status: Status,
    planned: &Planned,
) -> Result<Comment, CommentRefusal> {
    read(&mut *conn, id, actor, planned).await?;

    sqlx::query(
        "UPDATE asset_comments SET status = $2, status_by = $3, status_at = now(), \
         updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(actor)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    read(&mut *conn, id, actor, planned).await
}

/// Deletes a comment and its replies. The author only.
///
/// Replies go with it, by the parent's `ON DELETE CASCADE`. That is the honest outcome: a reply to a comment that
/// no longer exists is a fragment of a conversation with the question removed, and leaving those behind reads as
/// corruption rather than as a deletion.
pub async fn remove(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    actor: Uuid,
    planned: &Planned,
) -> Result<(), CommentRefusal> {
    let existing = read(&mut *conn, id, actor, planned).await?;
    if existing.author_id != actor {
        return Err(CommentRefusal::NotYours);
    }
    sqlx::query("DELETE FROM asset_comments WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    Ok(())
}

/// Refuses an asset the caller may not see.
async fn require_visible_asset(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    planned: &Planned,
) -> Result<(), CommentRefusal> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE assets.id = ",
    );
    builder.push_bind(asset_id);
    builder.push(" AND ");
    crate::query_sql::push_where(&mut builder, planned)?;

    let found: Option<Uuid> = builder
        .build_query_scalar()
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?;
    found
        .map(|_| ())
        .ok_or(CommentRefusal::UnknownAsset(asset_id))
}
