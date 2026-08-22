//! Ratings, favourites and watches (Q.5).
//!
//! Three things a person does *to* an asset rather than to its metadata. They share a shape — (asset, person) —
//! and almost nothing else, which is why they are three tables and one module: the rules that matter are the
//! same for all three, and the reads are completely different.
//!
//! ## Every write goes through the caller's own predicate
//!
//! Rating an asset is not a metadata edit, so it is tempting to treat it as harmless and skip the check. It is
//! not harmless: an endpoint that accepts a rating for any id is an existence oracle, and one that accepts a
//! *favourite* for any id lets somebody build a private list of assets they cannot see and watch it fill in as
//! their access changes. So every write resolves the asset under [`Planned`] first, and an asset the caller
//! cannot see is refused with [`EngagementRefusal::UnknownAsset`] — the same answer as one that does not exist,
//! because a different answer is the disclosure.
//!
//! ## Every read goes through it too, including the lists
//!
//! "My favourites" is the interesting case. The rows belong to the caller, so it looks like a query that needs no
//! filtering — but access can be taken away after a favourite is made, and an unfiltered list would keep
//! returning the asset afterwards. §7's rule is about counts and existence, not about who owns the row.
//!
//! ## What is aggregated, and what is not
//!
//! A rating average and a favourite count are *about the asset*, so a caller who can see the asset can see them.
//! Who rated or favourited it is never returned: it is not needed to render anything, and "seven people
//! favourited this, and here they are" is a different disclosure from "seven people did". Watches have no public
//! count at all — how many colleagues are watching a file is closer to a fact about the colleagues than about the
//! file, and nothing on any screen needs it. See `DECISIONS.md`.
//!
//! ## Clearing is deleting
//!
//! No zero star, no `favourite = false`. "No opinion" and "thinks it is bad" must not share a representation, or
//! an average silently counts absences.

use crate::Error;
use dam_core::query::Planned;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// Why an engagement write or read was refused.
#[derive(Debug, thiserror::Error)]
pub enum EngagementRefusal {
    /// No such asset — *or* not one this caller may see. Deliberately the same refusal for both: see the
    /// module docs.
    #[error("no asset {0}")]
    UnknownAsset(Uuid),

    #[error("a rating is 1 to 5 stars; {0} is not")]
    OutOfRange(i16),

    #[error(transparent)]
    Database(#[from] Error),
}

/// The engagement facts for one asset, as the caller may see them.
#[derive(Debug, Clone, PartialEq)]
pub struct Engagement {
    pub asset_id: Uuid,
    /// The mean of every rating, or `None` when nobody has rated it.
    ///
    /// Not defaulted to zero: "unrated" and "rated badly by everyone" are different facts, and a screen that
    /// showed them the same way would be lying about one of them.
    pub average_stars: Option<f64>,
    pub rating_count: i64,
    pub favourite_count: i64,
    /// This caller's own rating, if they have one.
    pub my_stars: Option<i16>,
    pub is_favourite: bool,
    pub is_watched: bool,
}

/// Resolves `asset_id` under the caller's predicate, refusing what they may not see.
///
/// Every public function here starts with this. Factored out rather than inlined six times because it is the one
/// check that must not be forgotten, and a named function is harder to omit than a fragment of SQL.
async fn visible(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    planned: &Planned,
) -> Result<(), EngagementRefusal> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE assets.id = ",
    );
    builder.push_bind(asset_id);
    // ANDed with the caller's compiled predicate, never instead of it: the id is the caller's input and the
    // predicate is the system's, so both have to hold.
    builder.push(" AND ");
    crate::query_sql::push_where(&mut builder, planned)?;

    let found: Option<Uuid> = builder
        .build_query_scalar()
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?;
    found
        .map(|_| ())
        .ok_or(EngagementRefusal::UnknownAsset(asset_id))
}

/// Records or replaces this caller's rating, returning the asset's engagement afterwards.
///
/// Upsert rather than insert-or-fail: changing your mind about a rating is the ordinary case, and making the
/// client discover whether it is a create or an update would put a race in every star widget.
pub async fn rate(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    identity_id: Uuid,
    stars: i16,
    planned: &Planned,
) -> Result<Engagement, EngagementRefusal> {
    // Range first, and before the visibility check: it is a fact about the request rather than about the caller's
    // access, and answering "1 to 5" for an asset they cannot see discloses nothing.
    if !(1..=5).contains(&stars) {
        return Err(EngagementRefusal::OutOfRange(stars));
    }
    visible(&mut *conn, asset_id, planned).await?;

    sqlx::query(
        "INSERT INTO asset_ratings (asset_id, identity_id, stars) VALUES ($1, $2, $3) \
         ON CONFLICT (asset_id, identity_id) DO UPDATE SET stars = excluded.stars, updated_at = now()",
    )
    .bind(asset_id)
    .bind(identity_id)
    .bind(stars)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    one(&mut *conn, asset_id, identity_id, planned).await
}

/// Removes this caller's rating. Removing one that is not there is not an error.
pub async fn unrate(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    identity_id: Uuid,
    planned: &Planned,
) -> Result<Engagement, EngagementRefusal> {
    visible(&mut *conn, asset_id, planned).await?;
    sqlx::query("DELETE FROM asset_ratings WHERE asset_id = $1 AND identity_id = $2")
        .bind(asset_id)
        .bind(identity_id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    one(&mut *conn, asset_id, identity_id, planned).await
}

/// Adds this asset to the caller's favourites. Idempotent.
pub async fn favourite(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    identity_id: Uuid,
    planned: &Planned,
) -> Result<Engagement, EngagementRefusal> {
    visible(&mut *conn, asset_id, planned).await?;
    // `DO NOTHING` rather than an update: there is nothing to change, and re-favouriting must not move the
    // `created_at` that orders the caller's own list.
    sqlx::query(
        "INSERT INTO asset_favourites (identity_id, asset_id) VALUES ($1, $2) \
         ON CONFLICT (identity_id, asset_id) DO NOTHING",
    )
    .bind(identity_id)
    .bind(asset_id)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    one(&mut *conn, asset_id, identity_id, planned).await
}

/// Removes it from the caller's favourites. Idempotent.
pub async fn unfavourite(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    identity_id: Uuid,
    planned: &Planned,
) -> Result<Engagement, EngagementRefusal> {
    visible(&mut *conn, asset_id, planned).await?;
    sqlx::query("DELETE FROM asset_favourites WHERE identity_id = $1 AND asset_id = $2")
        .bind(identity_id)
        .bind(asset_id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    one(&mut *conn, asset_id, identity_id, planned).await
}

/// Starts watching. Idempotent.
pub async fn watch(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    identity_id: Uuid,
    planned: &Planned,
) -> Result<Engagement, EngagementRefusal> {
    visible(&mut *conn, asset_id, planned).await?;
    sqlx::query(
        "INSERT INTO asset_watches (identity_id, asset_id) VALUES ($1, $2) \
         ON CONFLICT (identity_id, asset_id) DO NOTHING",
    )
    .bind(identity_id)
    .bind(asset_id)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    one(&mut *conn, asset_id, identity_id, planned).await
}

/// Stops watching. Idempotent.
pub async fn unwatch(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    identity_id: Uuid,
    planned: &Planned,
) -> Result<Engagement, EngagementRefusal> {
    visible(&mut *conn, asset_id, planned).await?;
    sqlx::query("DELETE FROM asset_watches WHERE identity_id = $1 AND asset_id = $2")
        .bind(identity_id)
        .bind(asset_id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    one(&mut *conn, asset_id, identity_id, planned).await
}

/// One asset's engagement, refusing what the caller may not see.
pub async fn one(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    identity_id: Uuid,
    planned: &Planned,
) -> Result<Engagement, EngagementRefusal> {
    let mut found = many(&mut *conn, &[asset_id], identity_id, planned).await?;
    // `many` silently drops what the caller cannot see, which is right for a page of thumbnails and wrong for a
    // request naming one asset: there the caller asked a question and deserves the refusal.
    found.pop().ok_or(EngagementRefusal::UnknownAsset(asset_id))
}

/// Engagement for a page of assets, in the order given, omitting any the caller may not see.
///
/// One query rather than one per asset: a grid asks for fifty at a time, and the round trips would dominate.
/// Assets nobody has touched still come back, with zero counts and no average, because "no ratings" is an answer
/// a screen has to render and a missing row would make the client guess. That shape needs no special case: the
/// aggregates over an empty set are exactly it.
pub async fn many(
    conn: &mut sqlx::PgConnection,
    asset_ids: &[Uuid],
    identity_id: Uuid,
    planned: &Planned,
) -> Result<Vec<Engagement>, EngagementRefusal> {
    if asset_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE assets.id = ANY(",
    );
    builder.push_bind(asset_ids.to_vec());
    builder.push(") AND ");
    crate::query_sql::push_where(&mut builder, planned)?;
    builder.push(
        ") SELECT visible.id, \
                  (SELECT avg(stars)::float8 FROM asset_ratings r WHERE r.asset_id = visible.id), \
                  (SELECT count(*) FROM asset_ratings r WHERE r.asset_id = visible.id), \
                  (SELECT count(*) FROM asset_favourites f WHERE f.asset_id = visible.id), \
                  (SELECT stars FROM asset_ratings r WHERE r.asset_id = visible.id \
                   AND r.identity_id = ",
    );
    builder.push_bind(identity_id);
    builder.push(
        "), \
                  EXISTS (SELECT 1 FROM asset_favourites f WHERE f.asset_id = visible.id \
                   AND f.identity_id = ",
    );
    builder.push_bind(identity_id);
    builder.push(
        "), \
                  EXISTS (SELECT 1 FROM asset_watches w WHERE w.asset_id = visible.id \
                   AND w.identity_id = ",
    );
    builder.push_bind(identity_id);
    builder.push(") FROM visible");

    type Row = (Uuid, Option<f64>, i64, i64, Option<i16>, bool, bool);
    let rows: Vec<Row> = builder
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;

    // Back into the caller's order. The query returns whatever order the planner likes, and a grid that asked for
    // fifty ids in a particular order has to be able to zip the answers against them.
    let mut ordered: Vec<Engagement> = Vec::with_capacity(rows.len());
    for wanted in asset_ids {
        if let Some(row) = rows.iter().find(|row| &row.0 == wanted) {
            let (asset_id, average_stars, rating_count, favourite_count, my_stars, fav, watched) =
                *row;
            ordered.push(Engagement {
                asset_id,
                average_stars,
                rating_count,
                favourite_count,
                my_stars,
                is_favourite: fav,
                is_watched: watched,
            });
        }
    }
    Ok(ordered)
}

/// What kind of private list to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Favourites,
    Watches,
}

impl List {
    fn table(self) -> &'static str {
        match self {
            Self::Favourites => "asset_favourites",
            Self::Watches => "asset_watches",
        }
    }
}

/// One page of the caller's own favourites or watches, newest first, and the total.
///
/// Filtered by the predicate even though every row belongs to the caller: access can be withdrawn after the row
/// is made, and a list that kept returning the asset afterwards would be a durable record of something the
/// caller is no longer allowed to know exists.
pub async fn mine(
    conn: &mut sqlx::PgConnection,
    which: List,
    identity_id: Uuid,
    planned: &Planned,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<Uuid>), EngagementRefusal> {
    // The table name is a literal from this module's own enum, never a caller's string.
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id, mine.created_at FROM assets \
         JOIN ",
    );
    builder.push(which.table());
    builder.push(" mine ON mine.asset_id = assets.id AND mine.identity_id = ");
    builder.push_bind(identity_id);
    builder.push(" LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ");
    crate::query_sql::push_where(&mut builder, planned)?;
    builder.push(") SELECT id FROM visible ORDER BY created_at DESC, id LIMIT ");
    builder.push_bind(limit.clamp(0, 500));
    builder.push(" OFFSET ");
    builder.push_bind(offset.max(0));

    let ids: Vec<Uuid> = builder
        .build_query_scalar()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;

    // Counted separately rather than from `ids.len()`, because the page is limited and the total is not — and the
    // total has to come from the same predicate or it would disclose the size of what is filtered out.
    let mut counter: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT count(*) FROM assets JOIN ");
    counter.push(which.table());
    counter.push(" mine ON mine.asset_id = assets.id AND mine.identity_id = ");
    counter.push_bind(identity_id);
    counter.push(" LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ");
    crate::query_sql::push_where(&mut counter, planned)?;
    let total: i64 = counter
        .build_query_scalar()
        .fetch_one(&mut *conn)
        .await
        .map_err(Error::from)?;

    Ok((total, ids))
}

/// Who is watching an asset.
///
/// For whatever eventually sends notifications (M6), and takes no predicate on purpose: the sender is not a
/// caller and has no grants of its own. **It must re-check each watcher's access before telling them anything** —
/// a watch made while somebody could see an asset says nothing about whether they still can, and this function
/// answers the question it was asked rather than a stale version of a different one.
pub async fn watchers(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
) -> Result<Vec<Uuid>, EngagementRefusal> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT identity_id FROM asset_watches WHERE asset_id = $1 ORDER BY created_at, identity_id",
    )
    .bind(asset_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(ids)
}
