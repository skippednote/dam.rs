//! Collections: membership, ordering, and the pin that keeps assets out of cold storage (2.3).
//!
//! Two things here are more than CRUD.
//!
//! ## Order has to be stable
//!
//! `collection_items.position` is `int NOT NULL DEFAULT 0` with no uniqueness, so inserting without
//! managing it leaves every row at 0 and the order is whatever the planner returns. A curated collection
//! is usually a presentation or a brand portal page; one that reorders itself between page loads is worse
//! than one with no order at all, because it looks like a bug in the customer's own work.
//!
//! Positions are therefore kept **dense** — `0..n-1`, no gaps — and every read orders by
//! `(position, asset_id)` so even a corrupt tie is deterministic. Dense rather than sparse (gaps of 1000,
//! rebalanced when they run out) because a sparse scheme still needs the rebalance, and meanwhile the
//! invariant is unstateable: with dense positions "is this collection well-ordered" is a query.
//!
//! The cost is that inserting at the front rewrites the collection's positions. That is one UPDATE, and
//! it is bounded by the collection size rather than the library size.
//!
//! ## `pin_hot` is a union, not a flag
//!
//! §6.4 makes collection membership a block on tiering, and the lifecycle engine already takes
//! `pinned`/`pin_reason` per candidate. The subtlety is that an asset can be in several collections: it
//! is pinned if **any** of them is `pin_hot`, so removing it from one pinned collection must not unpin it
//! while another still holds it. Computing the pin per-collection and letting the last writer win is the
//! bug this module exists to avoid — and its symptom is a master silently tiered to Glacier while a live
//! portal page still links to it.

use crate::Error;
use std::collections::HashMap;
use uuid::Uuid;

/// An asset in a collection, in curated order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub asset_id: Uuid,
    pub position: i32,
}

/// Why an asset may not be tiered.
///
/// Carries the collection so [`crate::Error`] is not the only thing an operator sees when a tiering plan
/// skips an object. "pinned" with no reason is a plan nobody can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub asset_id: Uuid,
    /// Every `pin_hot` collection holding this asset, by key, sorted.
    ///
    /// All of them, not the first: an operator deciding whether to unpin needs to know there are three
    /// collections to deal with, not one.
    pub collections: Vec<String>,
}

impl Pin {
    /// A reason string for `lifecycle::Candidate::pin_reason`.
    pub fn reason(&self) -> String {
        format!("pin_hot collection(s): {}", self.collections.join(", "))
    }
}

/// Adds an asset to a collection at the end of the curated order.
///
/// Idempotent. Re-adding an asset already present leaves its position alone rather than moving it to the
/// end — a retried request must not silently reorder somebody's curation.
pub async fn add(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
    asset_id: Uuid,
    added_by: Option<Uuid>,
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;

    // `position` is computed inside the transaction, so two concurrent adds cannot both take the same
    // slot. The `ON CONFLICT DO NOTHING` then makes the whole thing idempotent.
    sqlx::query(
        "INSERT INTO collection_items (collection_id, asset_id, position, added_by) \
         SELECT $1, $2, coalesce(max(position) + 1, 0), $3 \
         FROM collection_items WHERE collection_id = $1 \
         ON CONFLICT (collection_id, asset_id) DO NOTHING",
    )
    .bind(collection_id)
    .bind(asset_id)
    .bind(added_by)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Removes an asset and closes the gap it left.
///
/// The renumber is what keeps positions dense. Leaving a hole would be cheaper and would make the
/// density invariant unstateable, so the next bug in this area would have nothing to assert against.
pub async fn remove(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
    asset_id: Uuid,
) -> Result<bool, Error> {
    let mut tx = pool.begin().await?;
    let deleted =
        sqlx::query("DELETE FROM collection_items WHERE collection_id = $1 AND asset_id = $2")
            .bind(collection_id)
            .bind(asset_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    if deleted > 0 {
        renumber(&mut tx, collection_id).await?;
    }
    tx.commit().await?;
    Ok(deleted > 0)
}

/// Moves an asset to `to_position`, shifting everything between.
///
/// Clamped rather than refused for an out-of-range target: a drag-and-drop UI reporting "position 47 of
/// 30" is a rounding difference between what the client and server think the list is, and refusing the
/// drop loses the user's action over an off-by-one.
pub async fn move_item(
    pool: &sqlx::PgPool,
    collection_id: Uuid,
    asset_id: Uuid,
    to_position: i32,
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;

    let current: Option<i32> = sqlx::query_scalar(
        "SELECT position FROM collection_items WHERE collection_id = $1 AND asset_id = $2",
    )
    .bind(collection_id)
    .bind(asset_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        // Not an inconsistency: reordering something a concurrent request just removed is ordinary,
        // and the caller should see a 404 rather than a 500.
        return Err(Error::Core(dam_core::Error::NotFound {
            kind: dam_core::ResourceKind::Asset,
            id: format!("{asset_id} is not in collection {collection_id}"),
        }));
    };

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE collection_id = $1")
            .bind(collection_id)
            .fetch_one(&mut *tx)
            .await?;
    let last = i32::try_from(count.saturating_sub(1)).unwrap_or(i32::MAX);
    let target = to_position.clamp(0, last);
    if target == current {
        tx.commit().await?;
        return Ok(());
    }

    // Park the moved row outside the range first. Without it the shift below would collide with the row
    // being moved — there is no unique index today, but relying on its absence would make adding one a
    // silent corruption rather than a constraint violation.
    sqlx::query(
        "UPDATE collection_items SET position = -1 WHERE collection_id = $1 AND asset_id = $2",
    )
    .bind(collection_id)
    .bind(asset_id)
    .execute(&mut *tx)
    .await?;

    if target < current {
        // Moving up: everything from the target down to just above the old slot shifts one later.
        sqlx::query(
            "UPDATE collection_items SET position = position + 1 \
             WHERE collection_id = $1 AND position >= $2 AND position < $3",
        )
        .bind(collection_id)
        .bind(target)
        .bind(current)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE collection_items SET position = position - 1 \
             WHERE collection_id = $1 AND position > $2 AND position <= $3",
        )
        .bind(collection_id)
        .bind(current)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "UPDATE collection_items SET position = $3 WHERE collection_id = $1 AND asset_id = $2",
    )
    .bind(collection_id)
    .bind(asset_id)
    .bind(target)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// The collection's assets in curated order.
///
/// Ordered by `(position, asset_id)`. The tiebreak is not decoration: without it two rows that somehow
/// share a position order differently between reads, and a customer's presentation reshuffles itself.
pub async fn items(pool: &sqlx::PgPool, collection_id: Uuid) -> Result<Vec<Item>, Error> {
    let rows = sqlx::query_as::<_, (Uuid, i32)>(
        "SELECT asset_id, position FROM collection_items WHERE collection_id = $1 \
         ORDER BY position, asset_id",
    )
    .bind(collection_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(asset_id, position)| Item { asset_id, position })
        .collect())
}

/// Which of `asset_ids` may not be tiered, and why.
///
/// One query for the whole batch, because the caller is the lifecycle worker walking thousands of
/// placements — a query per candidate would make the tiering pass cost more than the storage it saves.
///
/// Deleted assets are absent even when they sit in a `pin_hot` collection. The pin exists to keep
/// something reachable for people, and nobody is reaching a deleted asset; a legal hold, which is a
/// different mechanism, still blocks tiering *and* purge.
pub async fn pins(pool: &sqlx::PgPool, asset_ids: &[Uuid]) -> Result<HashMap<Uuid, Pin>, Error> {
    if asset_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT ci.asset_id, c.key \
         FROM collection_items ci \
         JOIN collections c ON c.id = ci.collection_id \
         JOIN assets a ON a.id = ci.asset_id \
         WHERE ci.asset_id = ANY($1) AND c.pin_hot AND a.deleted_at IS NULL \
         ORDER BY ci.asset_id, c.key",
    )
    .bind(asset_ids)
    .fetch_all(pool)
    .await?;

    let mut pins: HashMap<Uuid, Pin> = HashMap::new();
    for (asset_id, key) in rows {
        pins.entry(asset_id)
            .or_insert_with(|| Pin {
                asset_id,
                collections: Vec::new(),
            })
            .collections
            .push(key);
    }
    Ok(pins)
}

/// Rewrites a collection's positions to `0..n-1` in their current order.
///
/// One statement via a window function. The alternative — reading the rows and issuing an UPDATE each —
/// is N round trips and leaves a window in which the collection is half-renumbered.
async fn renumber(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    collection_id: Uuid,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE collection_items ci SET position = ranked.rank \
         FROM (SELECT asset_id, \
                      (row_number() OVER (ORDER BY position, asset_id) - 1)::int AS rank \
               FROM collection_items WHERE collection_id = $1) ranked \
         WHERE ci.collection_id = $1 AND ci.asset_id = ranked.asset_id \
           AND ci.position <> ranked.rank",
    )
    .bind(collection_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
