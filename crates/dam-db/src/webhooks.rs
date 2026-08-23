//! The webhook outbox: subscriptions, the transactional enqueue, and the delivery claim (Q.20c, §11).
//!
//! The schema has been here since migration 0004 and nothing has ever written to it. Its comments already make
//! the three arguments that shape this module, and they are worth restating because each one rules out the
//! obvious implementation:
//!
//! **A transactional outbox, not a call after the commit.** [`enqueue`] takes a connection, so the row lands in
//! the *same* transaction as the change it describes. Emitting from application code after a commit loses
//! events on a crash; emitting before one announces changes that may roll back. Both failures are invisible
//! until a customer's CMS is out of step with the library and nobody can say when it happened.
//!
//! **Ordering is per asset, and it is a correctness property.** An `asset.version_created` delivered after
//! `asset.expired` republishes an expired asset — the withdrawal is undone by a message that was already in
//! flight. So [`claim`] hands out at most one in-flight delivery per `(subscription, asset)`, and the next one
//! for that asset waits. Deliveries for *different* assets, and for different subscriptions, run concurrently.
//!
//! Ordered by `seq`, a sequence added in 0035, and not by `created_at`. `now()` is the *transaction*
//! timestamp, so two events enqueued in one transaction — which is precisely how "publish this version and
//! expire the old one" arrives — get identical timestamps and the tie-break falls to a random uuid. Exact
//! within a transaction; best-effort across concurrent ones, because a sequence is allocated at insert rather
//! than at commit, and a cross-transaction race over one asset has no correct order to preserve anyway.
//!
//! **A dead endpoint must not accumulate an unbounded outbox.** Attempts back off, `max_attempts` ends in
//! `dead` rather than in a retry forever, and a subscription that keeps failing is disabled with a reason
//! somebody can read and undo.
//!
//! ## What is *not* here
//!
//! No HTTP. This module decides what should be sent and in what order; `dam_connect` signs and sends it. The
//! split is what lets the ordering and retry rules be tested against a real database without a real server,
//! and the signing be tested without either.

use crate::Error;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// How many consecutive failures disable a subscription.
///
/// Eight attempts each, so a subscription reaching this has failed for days rather than minutes — long past a
/// deploy or a certificate renewal. Disabling sooner would turn an ordinary restart into an integration
/// outage; later would leave an outbox growing against an endpoint that is never coming back.
pub const FAILURES_BEFORE_DISABLE: i32 = 5;

/// The backoff schedule, in seconds, indexed by attempt.
///
/// Explicit rather than computed, because the shape matters more than the formula: the first two retries are
/// fast, since most failures are a deploy or a blip and a delivery that arrives within a minute is invisible
/// to the customer. After that it stretches to hours, so a genuinely dead endpoint is polled a handful of
/// times a day rather than continuously. The last value repeats for any attempt past the end.
const BACKOFF_SECONDS: [i64; 8] = [10, 30, 120, 600, 1_800, 7_200, 21_600, 43_200];

/// What a subscription asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub id: Uuid,
    pub connector_id: Option<Uuid>,
    pub url: String,
    /// The event kinds wanted. **Empty means all of them**, which is the schema's default and the useful
    /// one: a client that has not thought about filtering wants everything rather than nothing.
    pub event_kinds: Vec<String>,
    pub active: bool,
    /// Why the system disabled it, when it did. `None` while healthy.
    pub disabled_reason: Option<String>,
    pub consecutive_failures: i32,
    pub created_at: DateTime<Utc>,
}

/// A subscription to create.
#[derive(Debug, Clone)]
pub struct NewSubscription<'a> {
    pub connector_id: Option<Uuid>,
    pub url: &'a str,
    /// The HMAC key. Held in the row because the *sender* needs it on every delivery; there is no
    /// asymmetric option that would let this be a public value.
    pub secret: &'a str,
    pub event_kinds: &'a [String],
}

/// One queued delivery, as the dispatcher needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub id: Uuid,
    pub subscription_id: Uuid,
    pub url: String,
    /// The signing key for this delivery's subscription, read in the same statement so the dispatcher does
    /// not make a second round trip per event.
    pub secret: String,
    pub event_kind: String,
    pub asset_id: Option<Uuid>,
    pub payload: Value,
    /// Attempts *before* this one. Zero on a first delivery, which is what the backoff indexes on.
    pub attempts: i32,
    pub max_attempts: i32,
}

/// The columns [`subscriptions`] reads back, in order.
///
/// A named alias rather than an inline tuple: the shape appears in the query, the binding and the mapping, and
/// three copies of a nine-element tuple is where a column gets silently transposed.
type SubscriptionRow = (
    Uuid,
    Option<Uuid>,
    String,
    Vec<String>,
    bool,
    Option<String>,
    i32,
    DateTime<Utc>,
);

/// Every subscription, healthy or not.
pub async fn subscriptions(conn: &mut sqlx::PgConnection) -> Result<Vec<Subscription>, Error> {
    let rows: Vec<SubscriptionRow> = sqlx::query_as(
        "SELECT id, connector_id, url, event_kinds, active, disabled_reason, \
                consecutive_failures, created_at \
         FROM webhook_subscriptions ORDER BY created_at",
    )
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                connector_id,
                url,
                event_kinds,
                active,
                disabled_reason,
                consecutive_failures,
                created_at,
            )| Subscription {
                id,
                connector_id,
                url,
                event_kinds,
                active,
                disabled_reason,
                consecutive_failures,
                created_at,
            },
        )
        .collect())
}

/// Registers a subscription.
pub async fn subscribe(
    conn: &mut sqlx::PgConnection,
    new: &NewSubscription<'_>,
) -> Result<Uuid, Error> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO webhook_subscriptions (id, connector_id, url, secret, event_kinds) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4) RETURNING id",
    )
    .bind(new.connector_id)
    .bind(new.url)
    .bind(new.secret)
    .bind(new.event_kinds)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// Removes a subscription, and its queued deliveries with it by cascade.
///
/// A hard delete, unlike a taxonomy term. The difference is what the row means: a term is referenced by
/// assets a customer tagged years ago, while a subscription is a live configuration for one endpoint. Nothing
/// outside this table points at it, and a soft-deleted one would need excluding from every query here.
pub async fn unsubscribe(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<bool, Error> {
    let deleted = sqlx::query("DELETE FROM webhook_subscriptions WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}

/// Re-enables a subscription the system disabled, and forgives its failure count.
///
/// The count is reset deliberately: leaving it would disable the subscription again after one more failure,
/// which makes "enable" look broken to somebody who has just fixed their endpoint.
pub async fn reactivate(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE webhook_subscriptions \
         SET active = true, disabled_reason = NULL, consecutive_failures = 0, updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Queues one event to every subscription that wants it.
///
/// **Call this inside the transaction that made the change.** That is the whole point of an outbox, and it is
/// why the signature takes a connection rather than a pool: a caller holding a pool would be committing the
/// change and the announcement separately, so a crash between them loses the event and a rollback after them
/// announces something that never happened.
///
/// Returns how many deliveries were queued, which is zero on a tenant with no subscriptions — the common case,
/// and the reason this is a single `INSERT … SELECT` rather than a read followed by writes.
pub async fn enqueue(
    conn: &mut sqlx::PgConnection,
    event_kind: &str,
    asset_id: Option<Uuid>,
    payload: &Value,
) -> Result<u64, Error> {
    // One statement: the subscription filter and the insert together. `event_kinds = '{}'` is "everything",
    // which is the schema's default, so the predicate has to admit an empty array rather than matching on it.
    let queued = sqlx::query(
        "INSERT INTO webhook_deliveries (id, subscription_id, event_kind, asset_id, payload) \
         SELECT gen_random_uuid(), s.id, $1, $2, $3 \
         FROM webhook_subscriptions s \
         WHERE s.active AND (cardinality(s.event_kinds) = 0 OR $1 = ANY(s.event_kinds))",
    )
    .bind(event_kind)
    .bind(asset_id)
    .bind(payload)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(queued)
}

/// The columns [`claim`] reads back, in order.
type ClaimedRow = (
    Uuid,
    Uuid,
    String,
    String,
    String,
    Option<Uuid>,
    Value,
    i32,
    i32,
);

/// Claims up to `limit` deliveries that are due, respecting per-asset order.
///
/// The ordering guarantee is the whole difficulty. Two properties have to hold at once:
///
/// - At most one delivery in flight per `(subscription, asset)`. Otherwise two events for one asset race, and
///   the CMS applies them in whichever order the network settles — which is how an expired asset gets
///   republished by a version event that was already on its way.
/// - Two workers must not claim the same row. `FOR UPDATE SKIP LOCKED` handles that, and it is also what
///   makes the first property hold under concurrency: the check for an in-flight sibling and the claim are one
///   statement, so a second worker cannot pass the check between another's check and its write.
///
/// Rows with a `NULL` asset are ordered against each other per subscription, not globally: an event about no
/// particular asset (a settings change, a taxonomy edit) has no asset whose history it could reorder, but two
/// of them to one endpoint should still arrive in the order they happened.
pub async fn claim(conn: &mut sqlx::PgConnection, limit: i64) -> Result<Vec<Delivery>, Error> {
    let rows: Vec<ClaimedRow> = sqlx::query_as(
        "WITH due AS ( \
                 SELECT d.id \
                 FROM webhook_deliveries d \
                 JOIN webhook_subscriptions s ON s.id = d.subscription_id \
                 WHERE d.state IN ('pending', 'failed') \
                   AND d.next_attempt_at <= now() \
                   AND s.active \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM webhook_deliveries other \
                     WHERE other.subscription_id = d.subscription_id \
                       AND other.asset_id IS NOT DISTINCT FROM d.asset_id \
                       AND other.state = 'delivering') \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM webhook_deliveries earlier \
                     WHERE earlier.subscription_id = d.subscription_id \
                       AND earlier.asset_id IS NOT DISTINCT FROM d.asset_id \
                       AND earlier.state IN ('pending', 'failed') \
                       AND earlier.seq < d.seq) \
                 ORDER BY d.next_attempt_at, d.seq \
                 LIMIT $1 \
                 FOR UPDATE OF d SKIP LOCKED \
             ) \
             UPDATE webhook_deliveries d \
             SET state = 'delivering', attempts = d.attempts + 1 \
             FROM due, webhook_subscriptions s \
             WHERE d.id = due.id AND s.id = d.subscription_id \
             RETURNING d.id, d.subscription_id, s.url, s.secret, d.event_kind, d.asset_id, \
                       d.payload, d.attempts - 1, d.max_attempts",
    )
    .bind(limit.max(1))
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                subscription_id,
                url,
                secret,
                event_kind,
                asset_id,
                payload,
                attempts,
                max_attempts,
            )| Delivery {
                id,
                subscription_id,
                url,
                secret,
                event_kind,
                asset_id,
                payload,
                attempts,
                max_attempts,
            },
        )
        .collect())
}

/// Records a delivery that the endpoint accepted.
///
/// Also clears the subscription's failure count, because "consecutive" is the whole meaning of that column: a
/// subscription that fails twice, succeeds, then fails twice more is not on its way to being disabled.
pub async fn delivered(conn: &mut sqlx::PgConnection, id: Uuid, status: i32) -> Result<(), Error> {
    sqlx::query(
        "UPDATE webhook_deliveries \
         SET state = 'delivered', delivered_at = now(), response_status = $2, last_error = NULL \
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "UPDATE webhook_subscriptions s \
         SET consecutive_failures = 0, updated_at = now() \
         FROM webhook_deliveries d \
         WHERE d.id = $1 AND s.id = d.subscription_id AND s.consecutive_failures > 0",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// What happened to a subscription after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterFailure {
    /// It will be tried again, at the time returned.
    Retrying,
    /// This delivery has run out of attempts and is now in the dead-letter queue.
    DeadLettered,
    /// The delivery is dead *and* the subscription has been disabled.
    SubscriptionDisabled,
}

/// Records a failed attempt: backs the delivery off, or dead-letters it, and counts against the subscription.
///
/// `status` is the HTTP status where there was one — a 500 is a different diagnosis from a DNS failure, and an
/// operator reading the row needs to be able to tell them apart, so the absence is preserved rather than
/// written as a zero.
pub async fn failed(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    status: Option<i32>,
    error: &str,
) -> Result<AfterFailure, Error> {
    // Truncated: `last_error` is shown in a UI and a server that answers with a whole HTML error page would
    // otherwise put kilobytes of markup in a table read on every page load.
    let error = error.chars().take(500).collect::<String>();

    let row: Option<(i32, i32)> =
        sqlx::query_as("SELECT attempts, max_attempts FROM webhook_deliveries WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((attempts, max_attempts)) = row else {
        // Deleted while in flight, which happens when somebody removes the subscription mid-delivery. Not an
        // error: there is nothing left to record against.
        return Ok(AfterFailure::DeadLettered);
    };

    let exhausted = attempts >= max_attempts;
    let backoff = BACKOFF_SECONDS
        .get(usize::try_from(attempts.max(0)).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or_else(|| BACKOFF_SECONDS[BACKOFF_SECONDS.len() - 1]);

    sqlx::query(
        "UPDATE webhook_deliveries \
         SET state = CASE WHEN $2 THEN 'dead' ELSE 'failed' END, \
             response_status = $3, \
             last_error = $4, \
             next_attempt_at = now() + make_interval(secs => $5) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(exhausted)
    .bind(status)
    .bind(&error)
    .bind(
        // Rounded to a float for `make_interval`, which takes no integer seconds argument. The values are
        // whole seconds, so nothing is lost.
        #[expect(clippy::cast_precision_loss, reason = "whole seconds, all under a day")]
        {
            backoff as f64
        },
    )
    .execute(&mut *conn)
    .await?;

    // Counted per *subscription*, not per delivery: the question being answered is "is this endpoint alive",
    // and one poisonous payload that fails eight times should not disable an endpoint that is otherwise fine.
    // So the count moves only when a delivery dies.
    if !exhausted {
        return Ok(AfterFailure::Retrying);
    }

    let disabled: Option<bool> = sqlx::query_scalar(
        "UPDATE webhook_subscriptions s \
         SET consecutive_failures = s.consecutive_failures + 1, \
             active = CASE WHEN s.consecutive_failures + 1 >= $2 THEN false ELSE s.active END, \
             disabled_reason = CASE WHEN s.consecutive_failures + 1 >= $2 \
                 THEN 'disabled automatically after ' || (s.consecutive_failures + 1) \
                      || ' deliveries were abandoned; the last error was: ' || $3 \
                 ELSE s.disabled_reason END, \
             updated_at = now() \
         FROM webhook_deliveries d \
         WHERE d.id = $1 AND s.id = d.subscription_id \
         RETURNING NOT s.active",
    )
    .bind(id)
    .bind(FAILURES_BEFORE_DISABLE)
    .bind(&error)
    .fetch_optional(&mut *conn)
    .await?;

    if disabled == Some(true) {
        Ok(AfterFailure::SubscriptionDisabled)
    } else {
        Ok(AfterFailure::DeadLettered)
    }
}

/// Returns a delivery to the queue without counting an attempt against it.
///
/// For a worker that is shutting down or lost its claim: the delivery was never attempted, so counting it
/// would spend one of the eight tries on a deploy. Bounded by the same claim rules, so an unclaimed row simply
/// becomes claimable again.
pub async fn release(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<(), Error> {
    sqlx::query(
        "UPDATE webhook_deliveries \
         SET state = 'pending', attempts = greatest(attempts - 1, 0) \
         WHERE id = $1 AND state = 'delivering'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Returns deliveries stuck in `delivering` past `stale_after_seconds` to the queue.
///
/// A worker killed mid-delivery leaves a row claimed forever, and because ordering is per asset that one row
/// blocks every later event for the same asset — a silent halt of one asset's stream rather than a visible
/// error. The attempt is *kept*, unlike [`release`]: the request may well have reached the endpoint, so the
/// conservative reading is that it was tried.
pub async fn reclaim_stalled(
    conn: &mut sqlx::PgConnection,
    stale_after_seconds: i64,
) -> Result<u64, Error> {
    let reclaimed = sqlx::query(
        "UPDATE webhook_deliveries \
         SET state = 'failed', last_error = 'the worker delivering this stopped without reporting' \
         WHERE state = 'delivering' \
           AND created_at < now() - make_interval(secs => $1)",
    )
    .bind(
        #[expect(clippy::cast_precision_loss, reason = "a timeout in seconds")]
        {
            stale_after_seconds as f64
        },
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(reclaimed)
}

/// The columns [`log`] reads back, in order.
type LogRow = (
    Uuid,
    Uuid,
    String,
    Option<Uuid>,
    String,
    i32,
    Option<i32>,
    Option<String>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
);

/// One row of the delivery log, for an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryLog {
    pub id: Uuid,
    pub subscription_id: Uuid,
    pub event_kind: String,
    pub asset_id: Option<Uuid>,
    pub state: String,
    pub attempts: i32,
    pub response_status: Option<i32>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub next_attempt_at: DateTime<Utc>,
}

/// The most recent deliveries for one subscription, newest first.
///
/// No payload. It is the largest column, it is the one an operator least often needs, and a log endpoint that
/// returned every payload would be the cheapest way to read a tenant's whole change history in one request.
pub async fn log(
    conn: &mut sqlx::PgConnection,
    subscription_id: Uuid,
    limit: i64,
) -> Result<Vec<DeliveryLog>, Error> {
    let rows: Vec<LogRow> = sqlx::query_as(
        "SELECT id, subscription_id, event_kind, asset_id, state, attempts, response_status, \
                last_error, created_at, delivered_at, next_attempt_at \
         FROM webhook_deliveries WHERE subscription_id = $1 \
         ORDER BY seq DESC LIMIT $2",
    )
    .bind(subscription_id)
    .bind(limit.clamp(1, 500))
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                subscription_id,
                event_kind,
                asset_id,
                state,
                attempts,
                response_status,
                last_error,
                created_at,
                delivered_at,
                next_attempt_at,
            )| DeliveryLog {
                id,
                subscription_id,
                event_kind,
                asset_id,
                state,
                attempts,
                response_status,
                last_error,
                created_at,
                delivered_at,
                next_attempt_at,
            },
        )
        .collect())
}

/// Re-queues a dead delivery for one more round of attempts.
///
/// The attempt count is reset rather than incremented: an operator retrying a dead-lettered event has usually
/// just fixed the endpoint, and handing them a single attempt before it dies again would make the button
/// useless. Only `dead` rows, so this cannot be used to jump the queue for something already in flight.
pub async fn revive(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE webhook_deliveries \
         SET state = 'pending', attempts = 0, next_attempt_at = now(), last_error = NULL \
         WHERE id = $1 AND state = 'dead'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}
