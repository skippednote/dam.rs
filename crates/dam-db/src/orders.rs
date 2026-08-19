//! Orders: a request for assets, and somebody else's authorisation (Q.13a).
//!
//! ## An order grants nothing by itself
//!
//! Approval is a *decision*, not a grant. What hands over bytes is a share link created at fulfilment, which is
//! the machinery that already answers who may take what: a token, an optional passcode, an expiry, a download
//! cap, revocation, and rights re-evaluated on every delivery. See the migration on why the delegating design —
//! approval granting the requester a download right — was not taken.
//!
//! ## You cannot order what you cannot see
//!
//! Items are filtered by the requester's own predicate at submission. Not a new rule: it is the Read gate, and
//! the alternative would let somebody enumerate the library by ordering ids and reading the refusals.
//!
//! ## An approver cannot approve what they cannot see
//!
//! [`approve`] refuses when any item is outside the approver's scope, and says how many. An approver is agreeing
//! to hand over specific assets, and agreeing to hand over something they cannot inspect is not a decision — it
//! is a signature on a blank page.
//!
//! ## `expired` is not a state
//!
//! An expiry is a timestamp passing, not something anybody performs. A stored `expired` would need a sweeper to
//! keep it true and would be wrong between sweeps, so [`Order::is_expired`] derives it. One source of truth.

use crate::Error;
use dam_core::policy::AccessPredicate;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// Why an order operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum OrderRefusal {
    /// No such order, or not one this caller may see.
    #[error("no order {0}")]
    Unknown(Uuid),

    /// The order is not in a state where this makes sense.
    ///
    /// Carries both states, because "cannot approve" without saying what it *is* leaves an approver refreshing
    /// a screen. The commonest case is two approvers opening the same queue.
    #[error("order {0} is {1}, so it cannot be {2}")]
    WrongState(String, String, &'static str),

    /// An order with nothing in it.
    ///
    /// Refused at submission rather than accepted and puzzled over later: an empty order is always a mistake in
    /// the client, and an approver receiving one has nothing to decide about.
    #[error("an order needs at least one asset")]
    Empty,

    /// Every asset asked for is outside the requester's scope.
    ///
    /// Distinct from [`Self::Empty`] so the client can say something true. An order for ten assets none of which
    /// the requester may see is not an empty request — it is a request for things that, to them, do not exist.
    #[error("none of those assets exist for this caller")]
    NothingVisible,

    /// The approver cannot see some of what they are being asked to approve.
    #[error("{0} of the assets in this order are outside your scope, so you cannot judge it")]
    Unjudgeable(usize),

    /// Somebody other than the requester tried to withdraw it.
    #[error("only the person who asked for an order may cancel it")]
    NotYours,

    #[error(transparent)]
    Database(#[from] Error),
}

/// One order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub id: Uuid,
    /// Human-quotable, per tenant: `ORD-000123`.
    pub reference: String,
    pub requested_by: Uuid,
    pub purpose: String,
    pub channel: Option<String>,
    pub territory: Option<String>,
    pub conversion_key: Option<String>,
    pub include_metadata: bool,
    pub recipients: Vec<String>,
    pub state: String,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
    pub decision_note: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub share_link_id: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// The assets asked for, in the order they were asked for.
    pub items: Vec<Item>,
}

impl Order {
    /// Whether the pickup window has closed.
    ///
    /// Derived, never stored — see the module docs. An expired order is still `ready` in the column, and every
    /// read that cares asks this rather than trusting a state that would need sweeping to stay true.
    pub fn is_expired(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }

    /// Whether the requester decided their own order.
    ///
    /// Reported rather than prevented. An order exists to record somebody's authorisation, and a person who
    /// holds the permission to approve does not need an order at all — so a self-approval is either a tenant
    /// where that is the normal path, or something a reader of the trail should be able to see. Refusing it here
    /// would be inventing a policy; recording it makes the policy checkable.
    pub fn self_approved(&self) -> bool {
        self.decided_by == Some(self.requested_by)
    }
}

/// One asset in an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub asset_id: Uuid,
    /// The name as asked for, so an order reads sensibly after a rename or a deletion.
    pub filename: String,
}

/// An order to place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    pub requested_by: Uuid,
    pub purpose: String,
    pub channel: Option<String>,
    pub territory: Option<String>,
    pub conversion_key: Option<String>,
    pub include_metadata: bool,
    pub recipients: Vec<String>,
    pub asset_ids: Vec<Uuid>,
}

/// Places an order for whichever of `asset_ids` the requester may see.
///
/// Silently narrowing rather than refusing a partly-visible request is deliberate: the requester is choosing from
/// a grid that already showed them only what they may see, so an invisible id means the world changed under them
/// or a client sent something stale. Refusing the whole order would lose the nine assets they can have because of
/// the one they cannot — and telling them *which* one was invisible is the enumeration this filter prevents.
pub async fn place(
    conn: &mut sqlx::PgConnection,
    new: &NewOrder,
    predicate: &AccessPredicate,
) -> Result<Order, OrderRefusal> {
    if new.asset_ids.is_empty() {
        return Err(OrderRefusal::Empty);
    }
    let visible = crate::assets::visible_among(&mut *conn, predicate, &new.asset_ids).await?;
    if visible.is_empty() {
        return Err(OrderRefusal::NothingVisible);
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders \
         (id, requested_by, purpose, channel, territory, conversion_key, include_metadata, recipients) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(new.requested_by)
    .bind(new.purpose.trim())
    .bind(new.channel.as_deref())
    .bind(new.territory.as_deref())
    .bind(new.conversion_key.as_deref())
    .bind(new.include_metadata)
    .bind(&new.recipients)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    // The filename is copied in, so the order still reads after a rename. Ordered by the caller's own list
    // rather than by id, because the sequence somebody assembled is information.
    for asset_id in new.asset_ids.iter().filter(|id| visible.contains(id)) {
        sqlx::query(
            "INSERT INTO order_items (order_id, asset_id, filename) \
             SELECT $1, id, filename FROM assets WHERE id = $2 \
             ON CONFLICT (order_id, asset_id) DO NOTHING",
        )
        .bind(id)
        .bind(asset_id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    }

    read(&mut *conn, id).await?.ok_or(OrderRefusal::Unknown(id))
}

/// Approves an order, opening a pickup window of `window_days`.
///
/// Refuses if the approver cannot see every item — see the module docs. Does *not* create the share: that is
/// fulfilment's job, and the gap between `approved` and `ready` is exactly the difference between a decision
/// having been made and the bytes being reachable.
pub async fn approve(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    approver: Uuid,
    note: Option<&str>,
    predicate: &AccessPredicate,
    window_days: i64,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Order, OrderRefusal> {
    let order = read(&mut *conn, id)
        .await?
        .ok_or(OrderRefusal::Unknown(id))?;
    if order.state != "submitted" {
        return Err(OrderRefusal::WrongState(
            order.reference,
            order.state,
            "approved",
        ));
    }

    let ids: Vec<Uuid> = order.items.iter().map(|item| item.asset_id).collect();
    let visible = crate::assets::visible_among(&mut *conn, predicate, &ids).await?;
    let unseen = ids.iter().filter(|id| !visible.contains(id)).count();
    if unseen > 0 {
        return Err(OrderRefusal::Unjudgeable(unseen));
    }

    sqlx::query(
        "UPDATE orders SET state = 'approved', decided_by = $2, decided_at = $3, decision_note = $4, \
                expires_at = $3 + make_interval(days => $5::int), updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(approver)
    .bind(now)
    .bind(note)
    .bind(i32::try_from(window_days.clamp(1, 365)).unwrap_or(14))
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    read(&mut *conn, id).await?.ok_or(OrderRefusal::Unknown(id))
}

/// Refuses an order.
///
/// No visibility requirement: saying no to something you cannot see is a defensible answer, and requiring an
/// approver to see an asset before refusing it would leave orders nobody can close.
pub async fn reject(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    approver: Uuid,
    note: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Order, OrderRefusal> {
    let order = read(&mut *conn, id)
        .await?
        .ok_or(OrderRefusal::Unknown(id))?;
    if order.state != "submitted" {
        return Err(OrderRefusal::WrongState(
            order.reference,
            order.state,
            "rejected",
        ));
    }
    sqlx::query(
        "UPDATE orders SET state = 'rejected', decided_by = $2, decided_at = $3, decision_note = $4, \
                updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(approver)
    .bind(now)
    .bind(note)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    read(&mut *conn, id).await?.ok_or(OrderRefusal::Unknown(id))
}

/// Withdraws an order, which only its requester may do, and only before a decision.
///
/// After a decision there is nothing to cancel: an approval is somebody else's recorded act, and letting the
/// requester erase it would remove the trail the order exists to keep. A pickup they no longer want is a share
/// to revoke, which is a different verb on a different object.
pub async fn cancel(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    requester: Uuid,
) -> Result<Order, OrderRefusal> {
    let order = read(&mut *conn, id)
        .await?
        .ok_or(OrderRefusal::Unknown(id))?;
    if order.requested_by != requester {
        return Err(OrderRefusal::NotYours);
    }
    if order.state != "submitted" {
        return Err(OrderRefusal::WrongState(
            order.reference,
            order.state,
            "cancelled",
        ));
    }
    sqlx::query("UPDATE orders SET state = 'cancelled', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    read(&mut *conn, id).await?.ok_or(OrderRefusal::Unknown(id))
}

/// Attaches the pickup share, moving an approved order to `ready`.
///
/// Fulfilment's one write. Separate from [`approve`] because the two can fail independently — a decision that
/// stands while packaging is retried is the useful arrangement, and rolling back an approval because a zip
/// failed would make an approver decide twice.
pub async fn mark_ready(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    share_link_id: Uuid,
) -> Result<Order, OrderRefusal> {
    let order = read(&mut *conn, id)
        .await?
        .ok_or(OrderRefusal::Unknown(id))?;
    if order.state != "approved" {
        return Err(OrderRefusal::WrongState(
            order.reference,
            order.state,
            "made ready",
        ));
    }
    sqlx::query(
        "UPDATE orders SET state = 'ready', share_link_id = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(share_link_id)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    read(&mut *conn, id).await?.ok_or(OrderRefusal::Unknown(id))
}

/// Records that the pickup was used.
///
/// Idempotent: a recipient downloading twice is one collection, and the second call is not an error. The share's
/// own download cap is what limits how much they may take.
pub async fn mark_collected(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<(), Error> {
    sqlx::query(
        "UPDATE orders SET state = 'collected', updated_at = now() \
         WHERE id = $1 AND state IN ('ready', 'collected')",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One order with its items, or `None`.
pub async fn read(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Option<Order>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(LIST_ORDERS);
    builder.push(" WHERE o.id = ");
    builder.push_bind(id);
    let row: Option<Row> = builder.build_query_as().fetch_optional(&mut *conn).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let items = items_for(&mut *conn, id).await?;
    Ok(Some(into_order(row, items)))
}

/// The orders one person asked for, newest first.
pub async fn placed_by(
    conn: &mut sqlx::PgConnection,
    requester: Uuid,
    limit: i64,
) -> Result<Vec<Order>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(LIST_ORDERS);
    builder.push(" WHERE o.requested_by = ");
    builder.push_bind(requester);
    builder.push(" ORDER BY o.created_at DESC LIMIT ");
    builder.push_bind(limit.clamp(1, MAX_ROWS));
    let rows: Vec<Row> = builder.build_query_as().fetch_all(&mut *conn).await?;
    with_items(&mut *conn, rows).await
}

/// The orders waiting for a decision, oldest first.
///
/// Oldest first, unlike everything else here: a queue is worked through, and the thing that has been waiting
/// longest is the thing to do next.
pub async fn awaiting_decision(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<Order>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(LIST_ORDERS);
    builder.push(" WHERE o.state = 'submitted' ORDER BY o.created_at LIMIT ");
    builder.push_bind(limit.clamp(1, MAX_ROWS));
    let rows: Vec<Row> = builder.build_query_as().fetch_all(&mut *conn).await?;
    with_items(&mut *conn, rows).await
}

/// The order a share link belongs to, if any.
///
/// The pickup path's one read: a share token arrives, and this says whether it is an order's pickup and which.
pub async fn for_share(
    conn: &mut sqlx::PgConnection,
    share_link_id: Uuid,
) -> Result<Option<Order>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(LIST_ORDERS);
    builder.push(" WHERE o.share_link_id = ");
    builder.push_bind(share_link_id);
    let row: Option<Row> = builder.build_query_as().fetch_optional(&mut *conn).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id = row.0;
    let items = items_for(&mut *conn, id).await?;
    Ok(Some(into_order(row, items)))
}

async fn items_for(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Vec<Item>, Error> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT asset_id, filename FROM order_items WHERE order_id = $1 ORDER BY filename, asset_id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(asset_id, filename)| Item { asset_id, filename })
        .collect())
}

/// Items for many orders, in one query rather than one per order.
async fn with_items(conn: &mut sqlx::PgConnection, rows: Vec<Row>) -> Result<Vec<Order>, Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
    let all: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT order_id, asset_id, filename FROM order_items \
         WHERE order_id = ANY($1) ORDER BY filename, asset_id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let id = row.0;
            let items = all
                .iter()
                .filter(|(order_id, _, _)| *order_id == id)
                .map(|(_, asset_id, filename)| Item {
                    asset_id: *asset_id,
                    filename: filename.clone(),
                })
                .collect();
            into_order(row, items)
        })
        .collect())
}

type Row = (
    Uuid,
    String,
    Uuid,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Vec<String>,
    String,
    Option<Uuid>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<Uuid>,
    chrono::DateTime<chrono::Utc>,
);

/// Every column [`Row`] reads, in its order.
///
/// One constant, because the tuple type and the select list have to agree and a mismatch between them is a
/// runtime decode error rather than a compile one. Sharing the text means there is one place to change.
const LIST_ORDERS: &str = "SELECT o.id, o.reference, o.requested_by, o.purpose, o.channel, o.territory, \
                           o.conversion_key, o.include_metadata, o.recipients, o.state, o.decided_by, \
                           o.decided_at, o.decision_note, o.expires_at, o.share_link_id, o.created_at \
                           FROM orders o";

fn into_order(row: Row, items: Vec<Item>) -> Order {
    let (
        id,
        reference,
        requested_by,
        purpose,
        channel,
        territory,
        conversion_key,
        include_metadata,
        recipients,
        state,
        decided_by,
        decided_at,
        decision_note,
        expires_at,
        share_link_id,
        created_at,
    ) = row;
    Order {
        id,
        reference,
        requested_by,
        purpose,
        channel,
        territory,
        conversion_key,
        include_metadata,
        recipients,
        state,
        decided_by,
        decided_at,
        decision_note,
        expires_at,
        share_link_id,
        created_at,
        items,
    }
}

/// How many orders one list returns.
const MAX_ROWS: i64 = 200;
