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
    /// Joined from `assets`, so a screen can show a curated order as pictures rather than as a column of
    /// uuids. A member whose asset row is gone does not appear at all — the join is inner deliberately: a
    /// dangling membership is nothing a curator can act on and nothing a portal should publish.
    pub filename: String,
    pub mime: String,
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
    conn: &mut sqlx::PgConnection,
    collection_id: Uuid,
    asset_id: Uuid,
    added_by: Option<Uuid>,
) -> Result<(), Error> {
    // `position` is computed in the same statement as the insert, so two concurrent adds cannot both take the
    // same slot, and `ON CONFLICT DO NOTHING` makes the whole thing idempotent.
    //
    // **The caller's transaction is the boundary.** This opened its own from a pool in the first version,
    // which read as self-contained and made the function unreachable: a tenant table needs the tenant's
    // `search_path`, that lives on a `TenantConn`'s connection, and a pool handed these queries the
    // `dam_global` path instead. The same reason `restores` and `provenance` were dead code.
    sqlx::query(
        "INSERT INTO collection_items (collection_id, asset_id, position, added_by) \
         SELECT $1, $2, coalesce(max(position) + 1, 0), $3 \
         FROM collection_items WHERE collection_id = $1 \
         ON CONFLICT (collection_id, asset_id) DO NOTHING",
    )
    .bind(collection_id)
    .bind(asset_id)
    .bind(added_by)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Removes an asset and closes the gap it left.
///
/// The renumber is what keeps positions dense. Leaving a hole would be cheaper and would make the
/// density invariant unstateable, so the next bug in this area would have nothing to assert against.
pub async fn remove(
    conn: &mut sqlx::PgConnection,
    collection_id: Uuid,
    asset_id: Uuid,
) -> Result<bool, Error> {
    let deleted =
        sqlx::query("DELETE FROM collection_items WHERE collection_id = $1 AND asset_id = $2")
            .bind(collection_id)
            .bind(asset_id)
            .execute(&mut *conn)
            .await?
            .rows_affected();

    if deleted > 0 {
        renumber(&mut *conn, collection_id).await?;
    }
    Ok(deleted > 0)
}

/// Moves an asset to `to_position`, shifting everything between.
///
/// Clamped rather than refused for an out-of-range target: a drag-and-drop UI reporting "position 47 of
/// 30" is a rounding difference between what the client and server think the list is, and refusing the
/// drop loses the user's action over an off-by-one.
pub async fn move_item(
    conn: &mut sqlx::PgConnection,
    collection_id: Uuid,
    asset_id: Uuid,
    to_position: i32,
) -> Result<(), Error> {
    let current: Option<i32> = sqlx::query_scalar(
        "SELECT position FROM collection_items WHERE collection_id = $1 AND asset_id = $2",
    )
    .bind(collection_id)
    .bind(asset_id)
    .fetch_optional(&mut *conn)
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
            .fetch_one(&mut *conn)
            .await?;
    let last = i32::try_from(count.saturating_sub(1)).unwrap_or(i32::MAX);
    let target = to_position.clamp(0, last);
    if target == current {
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
    .execute(&mut *conn)
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
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query(
            "UPDATE collection_items SET position = position - 1 \
             WHERE collection_id = $1 AND position > $2 AND position <= $3",
        )
        .bind(collection_id)
        .bind(current)
        .bind(target)
        .execute(&mut *conn)
        .await?;
    }

    sqlx::query(
        "UPDATE collection_items SET position = $3 WHERE collection_id = $1 AND asset_id = $2",
    )
    .bind(collection_id)
    .bind(asset_id)
    .bind(target)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// The collection's assets in curated order.
///
/// Ordered by `(position, asset_id)`. The tiebreak is not decoration: without it two rows that somehow
/// share a position order differently between reads, and a customer's presentation reshuffles itself.
pub async fn items(conn: &mut sqlx::PgConnection, collection_id: Uuid) -> Result<Vec<Item>, Error> {
    let rows = sqlx::query_as::<_, (Uuid, i32, String, String)>(
        "SELECT i.asset_id, i.position, a.filename, a.mime \
         FROM collection_items i JOIN assets a ON a.id = i.asset_id \
         WHERE i.collection_id = $1 AND a.status <> 'deleted' \
         ORDER BY i.position, i.asset_id",
    )
    .bind(collection_id)
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(asset_id, position, filename, mime)| Item {
            asset_id,
            position,
            filename,
            mime,
        })
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
pub async fn pins(
    conn: &mut sqlx::PgConnection,
    asset_ids: &[Uuid],
) -> Result<HashMap<Uuid, Pin>, Error> {
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
    .fetch_all(&mut *conn)
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
async fn renumber(conn: &mut sqlx::PgConnection, collection_id: Uuid) -> Result<(), Error> {
    sqlx::query(
        "UPDATE collection_items ci SET position = ranked.rank \
         FROM (SELECT asset_id, \
                      (row_number() OVER (ORDER BY position, asset_id) - 1)::int AS rank \
               FROM collection_items WHERE collection_id = $1) ranked \
         WHERE ci.collection_id = $1 AND ci.asset_id = ranked.asset_id \
           AND ci.position <> ranked.rank",
    )
    .bind(collection_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ─── the collections themselves (Q.14b) ─────────────────────────────────────
//
// Membership, ordering and pinning have existed since 2.3. Making a collection has not — so every function
// above operated on rows that only a test could create, and a portal, which publishes a collection, could not
// be set up by the person who wanted one.

/// One collection, as an administrator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: Uuid,
    /// The stable name a portal references. Immutable after creation — see [`rename`].
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub visibility: String,
    /// Whether membership blocks tiering (§6.4).
    pub pin_hot: bool,
    pub item_count: i64,
}

/// What a new collection needs.
#[derive(Debug, Clone)]
pub struct NewCollection<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub description: Option<&'a str>,
    pub visibility: &'a str,
    pub pin_hot: bool,
    pub owner_id: Option<Uuid>,
}

/// Every collection, with its size.
///
/// The count comes from a lateral rather than a `GROUP BY` join, so a collection with no members still appears.
/// An empty collection is the normal state of a newly created one, and a list that hid them would make "did
/// that save?" unanswerable.
pub async fn all(conn: &mut sqlx::PgConnection) -> Result<Vec<Collection>, Error> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, Option<String>, String, bool, i64)>(
        "SELECT c.id, c.key, c.label, c.description, c.visibility, c.pin_hot, \
                coalesce(n.count, 0) \
         FROM collections c \
         LEFT JOIN LATERAL ( \
             SELECT count(*) AS count FROM collection_items ci WHERE ci.collection_id = c.id \
         ) n ON true \
         ORDER BY c.label",
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, key, label, description, visibility, pin_hot, item_count)| Collection {
                id,
                key,
                label,
                description,
                visibility,
                pin_hot,
                item_count,
            },
        )
        .collect())
}

/// One collection by its key.
pub async fn by_key(conn: &mut sqlx::PgConnection, key: &str) -> Result<Option<Collection>, Error> {
    Ok(all(conn)
        .await?
        .into_iter()
        .find(|collection| collection.key == key))
}

/// Creates a collection.
///
/// A taken key is a [`Error::Unsupported`] naming it rather than a constraint violation, because "key already
/// exists" is something the person typing can fix and a 500 is not.
pub async fn create(conn: &mut sqlx::PgConnection, new: &NewCollection<'_>) -> Result<Uuid, Error> {
    if !matches!(new.visibility, "private" | "shared" | "public") {
        return Err(Error::Unsupported(format!(
            "{:?} is not a collection visibility; use private, shared or public",
            new.visibility
        )));
    }
    let id = Uuid::now_v7();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO collections (id, key, label, description, visibility, pin_hot, owner_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (key) DO NOTHING \
         RETURNING id",
    )
    .bind(id)
    .bind(new.key)
    .bind(new.label)
    .bind(new.description)
    .bind(new.visibility)
    .bind(new.pin_hot)
    .bind(new.owner_id)
    .fetch_optional(&mut *conn)
    .await?;

    inserted.ok_or_else(|| {
        Error::Unsupported(format!(
            "a collection with the key {:?} already exists",
            new.key
        ))
    })
}

/// Changes a collection's label, description, visibility or pinning.
///
/// **Not its key.** A portal references a collection by key, so renaming one would silently repoint or break
/// every portal built on it — and the label is the thing anybody actually wanted to change. Stated here
/// because "rename" is exactly what somebody would expect to move the key.
pub async fn rename(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    label: &str,
    description: Option<&str>,
    visibility: &str,
    pin_hot: bool,
) -> Result<bool, Error> {
    if !matches!(visibility, "private" | "shared" | "public") {
        return Err(Error::Unsupported(format!(
            "{visibility:?} is not a collection visibility; use private, shared or public"
        )));
    }
    let updated = sqlx::query(
        "UPDATE collections \
         SET label = $2, description = $3, visibility = $4, pin_hot = $5, updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(label)
    .bind(description)
    .bind(visibility)
    .bind(pin_hot)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Deletes a collection. Membership goes with it by cascade; the assets do not.
///
/// Refused while a portal publishes it, because a portal whose collection vanished is a public page that
/// serves nothing with no explanation — and the fix an operator wants is to delete the portal first, which
/// they can only choose if they are told.
pub async fn delete(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<bool, Error> {
    // Propagated, never defaulted. The first version ended this with `unwrap_or(0)`, which looks like
    // graceful degradation and is the opposite of it twice over: a guard that reads "no portals" when it
    // could not ask is a guard that permits the delete it exists to refuse, and a failed statement inside the
    // *caller's* transaction aborts that transaction — so the swallowed error resurfaced on the next
    // statement as "current transaction is aborted", pointing at the wrong line entirely.
    //
    // It also had the column name wrong (`deleted_at`; portals retire via `retired_at`), which is exactly the
    // class of mistake `unwrap_or` hides.
    let portals: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM portals WHERE collection_id = $1 AND retired_at IS NULL",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if portals > 0 {
        return Err(Error::Unsupported(format!(
            "{portals} portal(s) publish this collection; delete or repoint them first"
        )));
    }

    let deleted = sqlx::query("DELETE FROM collections WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}
