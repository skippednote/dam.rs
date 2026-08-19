//! What happened, and who did it (Q.7).
//!
//! `events` has existed since migration 0001 and nothing has ever written to it. This is the writer, and the
//! activity feed that reads it back.
//!
//! ## An event is not an audit record
//!
//! `audit_log` is hash-chained and exists to prove that a governance action happened and was not altered. This is
//! the other thing: a stream of ordinary activity, partitioned for volume, that a dashboard reads. Conflating them
//! would either put a tamper-evident chain in the path of every download, or leave the compliance record at the
//! mercy of a retention policy written for a feed.
//!
//! ## The feed is filtered by the caller's predicate, and the order matters
//!
//! An event names an asset, so a feed that showed every row would disclose the existence — and the filenames — of
//! assets the caller cannot see. Every read therefore starts from the visible-assets set, exactly as comments do.
//!
//! Events with no asset (a login, a schema change) are a separate question, and the answer here is that they are
//! *excluded* from the asset feed rather than shown to everybody. A feed is about the library; an event with no
//! asset belongs to a different screen with its own rule, and defaulting it into this one would be deciding that
//! rule by accident.
//!
//! ## Recording never fails a request
//!
//! [`record`] returns `Result` so a caller can log a failure, but no caller is expected to abort on one. An upload
//! that succeeded and then failed to write its feed entry has still succeeded, and turning that into a 500 would
//! trade a real outcome for a cosmetic one. The default partition added in 0021 is what makes this rare rather
//! than routine.

use crate::Error;
use dam_core::query::Planned;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// What kind of thing happened.
///
/// A closed set rather than the column's free text: the column is deliberately open so a future subsystem can
/// record something without a migration, but everything *this* code writes and the feed reads has to be a value
/// the UI knows how to phrase. An unknown kind read back is surfaced as itself rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An upload became an asset.
    Uploaded,
    /// Metadata was edited.
    Edited,
    /// A share link was created.
    Shared,
    /// A comment was posted.
    Commented,
    /// An original or rendition was delivered.
    Downloaded,
    /// An asset was soft-deleted.
    Deleted,
    /// An archived asset was asked for.
    RestoreRequested,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uploaded => "upload",
            Self::Edited => "edit",
            Self::Shared => "share",
            Self::Commented => "comment",
            Self::Downloaded => "download",
            Self::Deleted => "delete",
            Self::RestoreRequested => "restore",
        }
    }
}

/// Who did it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    User,
    ApiKey,
    ShareLink,
    System,
    Connector,
}

impl ActorKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKey => "api_key",
            Self::ShareLink => "share_link",
            Self::System => "system",
            Self::Connector => "connector",
        }
    }
}

/// An event to record.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub kind: Kind,
    /// The asset it concerns. `None` for something that is not about one asset — see the module docs on why those
    /// do not appear in the asset feed.
    pub asset_id: Option<Uuid>,
    pub actor_id: Option<Uuid>,
    pub actor_kind: ActorKind,
    /// Anything the phrasing needs that is not a column: a share's label, the fields an edit touched.
    pub context: serde_json::Value,
    /// Bytes moved, for a download. `None` otherwise.
    pub bytes: Option<i64>,
}

impl NewEvent {
    /// An event a person caused, about an asset.
    #[must_use]
    pub fn by(kind: Kind, asset_id: Uuid, actor_id: Uuid) -> Self {
        Self {
            kind,
            asset_id: Some(asset_id),
            actor_id: Some(actor_id),
            actor_kind: ActorKind::User,
            context: serde_json::json!({}),
            bytes: None,
        }
    }

    /// Attaches context to the event.
    #[must_use]
    pub fn with(mut self, context: serde_json::Value) -> Self {
        self.context = context;
        self
    }
}

/// One entry in the feed.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: Uuid,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    /// The stored kind, as text. Not the [`Kind`] enum: the column is open, so a row written by something this
    /// build does not know about is reported as itself rather than dropped or guessed at.
    pub kind: String,
    pub asset_id: Option<Uuid>,
    /// The asset's filename at read time, so a feed line reads as a sentence without a second query.
    pub filename: Option<String>,
    pub actor_id: Option<Uuid>,
    pub actor_kind: String,
    pub context: serde_json::Value,
    pub bytes: Option<i64>,
}

/// The largest feed page. A dashboard shows a screenful; anything more is an export.
pub const MAX_FEED: i64 = 200;

/// Records an event.
///
/// See the module docs: a caller logs a failure here rather than failing the request that caused it.
pub async fn record(conn: &mut sqlx::PgConnection, event: NewEvent) -> Result<Uuid, Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO events (id, kind, asset_id, actor_id, actor_kind, context, bytes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(event.kind.as_str())
    .bind(event.asset_id)
    .bind(event.actor_id)
    .bind(event.actor_kind.as_str())
    .bind(&event.context)
    .bind(event.bytes)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// The most recent activity on assets this caller may see, newest first.
///
/// Joined to `assets` rather than left-joined, which is what excludes both the events with no asset and the events
/// whose asset the predicate rejects. Those are the same exclusion mechanically and different decisions: see the
/// module docs.
pub async fn feed(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    limit: i64,
    kinds: &[Kind],
) -> Result<Vec<Entry>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id, assets.filename FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    builder.push(
        ") SELECT e.id, e.occurred_at, e.kind, e.asset_id, visible.filename, e.actor_id, \
                  e.actor_kind, e.context, e.bytes \
          FROM events e JOIN visible ON visible.id = e.asset_id",
    );
    if !kinds.is_empty() {
        builder.push(" WHERE e.kind = ANY(");
        builder.push_bind(
            kinds
                .iter()
                .map(|kind| kind.as_str().to_owned())
                .collect::<Vec<String>>(),
        );
        builder.push(")");
    }
    // `id` as the tie-break, because `occurred_at` is not unique — several events can share a timestamp to the
    // microsecond, as a loop recording five edits does.
    //
    // Its effect is *not* observable through this function and mutation testing says so: `feed` always returns the
    // newest N with no offset, so a plan that happened to return equal-timestamp rows in another order would still
    // satisfy every caller. It matters the moment an offset is added — an offset walk over a non-deterministic
    // order silently skips and repeats rows — and it is here now rather than being remembered then. Recorded as
    // precautionary rather than left looking tested.
    builder.push(" ORDER BY e.occurred_at DESC, e.id DESC LIMIT ");
    builder.push_bind(limit.clamp(1, MAX_FEED));

    type Row = (
        Uuid,
        chrono::DateTime<chrono::Utc>,
        String,
        Option<Uuid>,
        Option<String>,
        Option<Uuid>,
        String,
        serde_json::Value,
        Option<i64>,
    );
    let rows: Vec<Row> = builder.build_query_as().fetch_all(&mut *conn).await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, occurred_at, kind, asset_id, filename, actor_id, actor_kind, context, bytes)| {
                Entry {
                    id,
                    occurred_at,
                    kind,
                    asset_id,
                    filename,
                    actor_id,
                    actor_kind,
                    context,
                    bytes,
                }
            },
        )
        .collect())
}

/// What a dashboard counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    /// Assets the caller can see. The only total on the page that is about the library rather than about activity.
    pub assets: i64,
    pub uploads_this_week: i64,
    pub downloads_this_week: i64,
    pub comments_this_week: i64,
    /// Assets with no metadata values at all — the work queue a landing page exists to surface.
    pub without_metadata: i64,
}

/// The dashboard's counts, every one under the caller's predicate.
///
/// One statement rather than five: they are read together and rendered together, and five statements are five
/// snapshots — a number that disagrees with the one beside it looks like a bug in the page rather than in the
/// clock.
///
/// "This week" is the last seven days rather than a calendar week, because a Monday-morning dashboard showing
/// almost nothing is a dashboard people stop opening.
pub async fn summary(conn: &mut sqlx::PgConnection, planned: &Planned) -> Result<Summary, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    // Current versions only, for the same reason the grid uses: the asset count on a dashboard has to agree with
    // the number of rows the grid shows. The activity feed below deliberately does *not* filter — an event about a
    // superseded version still happened.
    builder.push(crate::versions::CURRENT_ONLY);
    builder.push(
        ") SELECT \
            (SELECT count(*) FROM visible), \
            (SELECT count(*) FROM events e JOIN visible v ON v.id = e.asset_id \
             WHERE e.kind = 'upload' AND e.occurred_at > now() - interval '7 days'), \
            (SELECT count(*) FROM events e JOIN visible v ON v.id = e.asset_id \
             WHERE e.kind = 'download' AND e.occurred_at > now() - interval '7 days'), \
            (SELECT count(*) FROM events e JOIN visible v ON v.id = e.asset_id \
             WHERE e.kind = 'comment' AND e.occurred_at > now() - interval '7 days'), \
            (SELECT count(*) FROM visible v \
             LEFT JOIN asset_metadata m ON m.asset_id = v.id \
             WHERE m.asset_id IS NULL OR m.values = '{}'::jsonb)",
    );

    let row: (i64, i64, i64, i64, i64) = builder.build_query_as().fetch_one(&mut *conn).await?;
    Ok(Summary {
        assets: row.0,
        uploads_this_week: row.1,
        downloads_this_week: row.2,
        comments_this_week: row.3,
        without_metadata: row.4,
    })
}
