//! Insights: what the library is used for (M6c).
//!
//! ## Every number here is the reader's own, and that is not a rounding error
//!
//! §7 says a count is a disclosure, and an analytics screen is almost entirely counts. So every query below
//! starts from the caller's visible-assets set, exactly as the dashboard and the activity feed do. The
//! consequence is worth stating plainly rather than discovering: **two people legitimately see different
//! graphs**, and a scoped curator's "1,240 downloads" is 1,240 downloads *of assets they can reach*. The
//! alternative — a library-wide total with the reader's own slice beneath it — tells them precisely how much
//! they cannot see, which is the disclosure §7 is about.
//!
//! It also means these numbers cannot be used as a performance measure. "Ada uploaded forty" is forty *of the
//! ones you can see*, and a number that changes with the reader is not a number anybody should build a review
//! process on. [`contributors`] says so in its own docs and the screen says it out loud.
//!
//! ## Downloads come from the ledger, not from the feed
//!
//! `rights_usage` is written for every download including one taken through a share link; `events` is written
//! only for a download by somebody with an identity, because `events.actor_id` is an identity and a share token
//! is not one. A "most downloaded" list built on `events` would quietly omit exactly the downloads a rights
//! manager most wants to see — the ones taken by people outside the tenant.
//!
//! Only `source = 'download'` rows count. The same table holds connector usage reports and manual print-run
//! entries, which answer "where is this in use" rather than "how often was it taken", and summing them together
//! would produce a number that is neither.
//!
//! ## A day with no activity is a row, not a gap
//!
//! [`series`] generates the date spine and left-joins onto it. A chart drawn from only the days that had events
//! has no holes in it — it just draws a straight line between the two sides of a quiet week, which is a lie
//! about the shape rather than an absence of data.
//!
//! ## A day is a day in the database's timezone
//!
//! `date_trunc` and the date spine both resolve in the session timezone, which is `Etc/UTC` on a damrs
//! database. That is the assumption rather than a choice made here: a deployment whose Postgres runs on a
//! local timezone would silently move every day boundary, so an event at 23:30 would land on a different day
//! than the same event does in every other query in this codebase. Stated because it is invisible until two
//! deployments disagree about a Tuesday.
//!
//! ## This is not the audit log and not the fleet rollup
//!
//! `audit_log` is hash-chained and proves a governance action happened. `dam_global.tenant_usage_daily` is
//! fleet metering the operator reads for cost attribution, and it deliberately crosses no tenant boundary.
//! This module is the third thing: a tenant's own read of its own activity, scoped to the reader.

use crate::Error;
use chrono::{DateTime, NaiveDate, Utc};
use dam_core::policy::AccessPredicate;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// How many days a window may cover.
///
/// A year. Not a technical limit — `events` is partitioned by month and the spine is cheap — but a bound on
/// what one request may ask for, so a caller cannot turn a chart into a scan of every partition ever created.
pub const MAX_DAYS: i64 = 366;

/// How many rows a top-N list returns at most.
pub const MAX_ROWS: i64 = 200;

/// One day of activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    pub day: NaiveDate,
    pub uploads: i64,
    /// From the ledger, so a share-link download counts. See the module docs.
    pub downloads: i64,
    pub edits: i64,
    pub comments: i64,
    pub shares: i64,
}

/// One row of an asset-with-a-count query, before it becomes an [`AssetCount`].
type AssetCountRow = (Uuid, String, String, i64, Option<DateTime<Utc>>);

/// An asset with a number against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetCount {
    pub asset_id: Uuid,
    pub filename: String,
    pub mime: String,
    pub count: i64,
    /// When it was last taken. Absent in a list of things never taken at all.
    pub last_at: Option<DateTime<Utc>>,
}

/// What the library holds, by media class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassTotal {
    /// `image`, `video`, `audio`, `document` or `other` — the same classes conversions are keyed by.
    pub class: String,
    pub assets: i64,
    pub bytes: i64,
}

/// A person and what they did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributor {
    /// The identity. Resolved to a name by the API layer, as a comment thread's author is.
    pub identity_id: Uuid,
    pub uploads: i64,
    pub edits: i64,
    pub comments: i64,
}

/// The visible-assets CTE every query here starts from.
///
/// `LIBRARY_ROWS` for the same reason the grid uses it: the asset count on an analytics page has to agree with
/// the number of rows the grid shows, and a library with three versions of one asset has one of that asset in
/// it. The activity *counts* deliberately do not filter that way — an edit to a version that has since been
/// superseded still happened — but they are counted against the current row's visibility, which is the only
/// question this module can answer.
fn push_visible(
    builder: &mut QueryBuilder<Postgres>,
    predicate: &AccessPredicate,
) -> Result<(), Error> {
    builder.push(
        "WITH visible AS (SELECT assets.id, assets.filename, assets.mime, assets.bytes, \
                                 assets.created_at FROM assets WHERE ",
    );
    crate::access::push_asset_filter(builder, predicate)?;
    builder.push(crate::versions::LIBRARY_ROWS);
    builder.push(") ");
    Ok(())
}

fn window(days: i64) -> i64 {
    days.clamp(1, MAX_DAYS)
}

fn rows(limit: i64) -> i64 {
    limit.clamp(1, MAX_ROWS)
}

/// Activity per day for the last `days` days, ending today.
///
/// One statement rather than five, for the reason `events::summary` gives: they are drawn on one chart, and five
/// statements are five snapshots of a moving table.
pub async fn series(
    conn: &mut sqlx::PgConnection,
    predicate: &AccessPredicate,
    days: i64,
) -> Result<Vec<Day>, Error> {
    let days = window(days);
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible(&mut builder, predicate)?;
    // The spine first, left-joined twice: once onto the feed and once onto the ledger. A quiet day is a row of
    // zeroes rather than a missing row.
    builder.push(
        "SELECT spine.day::date, \
                coalesce(activity.uploads, 0), \
                coalesce(taken.downloads, 0), \
                coalesce(activity.edits, 0), \
                coalesce(activity.comments, 0), \
                coalesce(activity.shares, 0) \
         FROM generate_series(current_date - (",
    );
    builder.push_bind(days - 1);
    builder.push(
        " || ' days')::interval, current_date, interval '1 day') AS spine(day) \
         LEFT JOIN ( \
             SELECT date_trunc('day', e.occurred_at) AS day, \
                    count(*) FILTER (WHERE e.kind = 'upload')  AS uploads, \
                    count(*) FILTER (WHERE e.kind = 'edit')    AS edits, \
                    count(*) FILTER (WHERE e.kind = 'comment') AS comments, \
                    count(*) FILTER (WHERE e.kind = 'share')   AS shares \
             FROM events e JOIN visible v ON v.id = e.asset_id \
             WHERE e.occurred_at >= current_date - (",
    );
    builder.push_bind(days - 1);
    builder.push(
        " || ' days')::interval \
             GROUP BY 1 \
         ) AS activity ON activity.day = spine.day \
         LEFT JOIN ( \
             SELECT date_trunc('day', u.recorded_at) AS day, count(*) AS downloads \
             FROM rights_usage u JOIN visible v ON v.id = u.asset_id \
             WHERE u.source = 'download' AND u.recorded_at >= current_date - (",
    );
    builder.push_bind(days - 1);
    builder.push(
        " || ' days')::interval \
             GROUP BY 1 \
         ) AS taken ON taken.day = spine.day \
         ORDER BY spine.day",
    );

    let fetched: Vec<(NaiveDate, i64, i64, i64, i64, i64)> =
        builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(fetched
        .into_iter()
        .map(|(day, uploads, downloads, edits, comments, shares)| Day {
            day,
            uploads,
            downloads,
            edits,
            comments,
            shares,
        })
        .collect())
}

/// The most-downloaded assets in the window, most first.
pub async fn most_downloaded(
    conn: &mut sqlx::PgConnection,
    predicate: &AccessPredicate,
    days: i64,
    limit: i64,
) -> Result<Vec<AssetCount>, Error> {
    let days = window(days);
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible(&mut builder, predicate)?;
    builder.push(
        "SELECT v.id, v.filename, v.mime, count(*), max(u.recorded_at) \
         FROM visible v JOIN rights_usage u ON u.asset_id = v.id \
         WHERE u.source = 'download' AND u.recorded_at >= current_date - (",
    );
    builder.push_bind(days - 1);
    builder.push(
        " || ' days')::interval \
         GROUP BY v.id, v.filename, v.mime \
         ORDER BY count(*) DESC, max(u.recorded_at) DESC, v.id \
         LIMIT ",
    );
    builder.push_bind(rows(limit));

    let fetched: Vec<AssetCountRow> = builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(fetched
        .into_iter()
        .map(|(asset_id, filename, mime, count, last_at)| AssetCount {
            asset_id,
            filename,
            mime,
            count,
            last_at,
        })
        .collect())
}

/// Assets nobody has ever downloaded, oldest first.
///
/// Ever, not "in the window" — the question this list answers is "what are we paying to store that nobody has
/// used", and an asset downloaded once two years ago has a different answer from one downloaded never. Ordered
/// oldest-first because age is the whole signal: an asset uploaded yesterday and not yet downloaded is not a
/// finding.
pub async fn never_downloaded(
    conn: &mut sqlx::PgConnection,
    predicate: &AccessPredicate,
    limit: i64,
) -> Result<Vec<AssetCount>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible(&mut builder, predicate)?;
    builder.push(
        "SELECT v.id, v.filename, v.mime, 0::bigint, NULL::timestamptz \
         FROM visible v \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM rights_usage u \
             WHERE u.asset_id = v.id AND u.source = 'download' \
         ) \
         ORDER BY v.created_at, v.id \
         LIMIT ",
    );
    builder.push_bind(rows(limit));

    let fetched: Vec<AssetCountRow> = builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(fetched
        .into_iter()
        .map(|(asset_id, filename, mime, count, last_at)| AssetCount {
            asset_id,
            filename,
            mime,
            count,
            last_at,
        })
        .collect())
}

/// How many there are in total.
///
/// Its own query, and it earns the round trip. A capped list of twenty unused assets reads as "you have twenty
/// unused assets" — on the dev library that was twenty of a much larger number, which is the difference between
/// a tidy-up and a storage problem. A top-20 of *most* downloaded explains its own cap; a list of things nobody
/// uses does not.
pub async fn never_downloaded_count(
    conn: &mut sqlx::PgConnection,
    predicate: &AccessPredicate,
) -> Result<i64, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible(&mut builder, predicate)?;
    builder.push(
        "SELECT count(*) FROM visible v \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM rights_usage u \
             WHERE u.asset_id = v.id AND u.source = 'download' \
         )",
    );
    Ok(builder.build_query_scalar().fetch_one(&mut *conn).await?)
}

/// What the library holds, by media class, largest first.
///
/// The class rather than the mime type: forty rows of `image/jpeg`, `image/png`, `image/tiff` is a table, and
/// "images: 41,000, 3.1 TiB" is an answer. `conversions::class_of` decides the mapping, and this repeats its
/// `CASE` in SQL rather than reading every row into Rust to classify it.
pub async fn by_class(
    conn: &mut sqlx::PgConnection,
    predicate: &AccessPredicate,
) -> Result<Vec<ClassTotal>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible(&mut builder, predicate)?;
    builder.push(
        "SELECT CASE \
                    WHEN v.mime LIKE 'image/%' THEN 'image' \
                    WHEN v.mime LIKE 'video/%' THEN 'video' \
                    WHEN v.mime LIKE 'audio/%' THEN 'audio' \
                    WHEN v.mime = 'application/pdf' \
                      OR v.mime LIKE 'application/vnd.openxmlformats%' \
                      OR v.mime LIKE 'application/msword%' THEN 'document' \
                    ELSE 'other' \
                END AS class, \
                count(*), coalesce(sum(v.bytes), 0)::bigint \
         FROM visible v GROUP BY 1 ORDER BY coalesce(sum(v.bytes), 0) DESC, 1",
    );

    let fetched: Vec<(String, i64, i64)> = builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(fetched
        .into_iter()
        .map(|(class, assets, bytes)| ClassTotal {
            class,
            assets,
            bytes,
        })
        .collect())
}

/// Who did what in the window, busiest first.
///
/// **Not a performance measure**, and the API layer's docs and the screen both say so. Every count is scoped to
/// the reader's visible assets, so Ada's upload count is different for every person who looks at it — which is
/// correct for a disclosure rule and useless for a review process. Building one on this would compare people by
/// how much of their work the reader happens to be allowed to see.
///
/// Downloads are absent here deliberately. A person's download history is the one number on this screen that
/// reads as surveillance rather than as activity, and the rights ledger already answers "who took this" per
/// asset, to somebody looking at that asset, which is where the question belongs.
pub async fn contributors(
    conn: &mut sqlx::PgConnection,
    predicate: &AccessPredicate,
    days: i64,
    limit: i64,
) -> Result<Vec<Contributor>, Error> {
    let days = window(days);
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("");
    push_visible(&mut builder, predicate)?;
    builder.push(
        "SELECT e.actor_id, \
                count(*) FILTER (WHERE e.kind = 'upload'), \
                count(*) FILTER (WHERE e.kind = 'edit'), \
                count(*) FILTER (WHERE e.kind = 'comment') \
         FROM events e JOIN visible v ON v.id = e.asset_id \
         WHERE e.actor_id IS NOT NULL AND e.actor_kind = 'user' \
           AND e.occurred_at >= current_date - (",
    );
    builder.push_bind(days - 1);
    builder.push(
        " || ' days')::interval \
         GROUP BY e.actor_id \
         HAVING count(*) FILTER (WHERE e.kind IN ('upload', 'edit', 'comment')) > 0 \
         ORDER BY count(*) FILTER (WHERE e.kind IN ('upload', 'edit', 'comment')) DESC, e.actor_id \
         LIMIT ",
    );
    builder.push_bind(rows(limit));

    let fetched: Vec<(Uuid, i64, i64, i64)> =
        builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(fetched
        .into_iter()
        .map(|(identity_id, uploads, edits, comments)| Contributor {
            identity_id,
            uploads,
            edits,
            comments,
        })
        .collect())
}
