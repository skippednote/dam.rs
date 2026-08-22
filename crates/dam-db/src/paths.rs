//! Paths: triggered notifications (3.6, GAPS G9).
//!
//! A path is "tell me when X happens": a licence expiring in 30 days, an asset added to a group, a restore
//! becoming ready. The schema calls `path_firings` a ledger that "exists purely for idempotency", and that is
//! the whole difficulty.
//!
//! ## The digest key decides whether a sweep notifies once or thirty times
//!
//! `path_firings_dedupe_idx` is `UNIQUE (path_id, digest_key)`, so the key *is* the deduplication. A daily
//! "expiring in 30 days" sweep sees the same asset on all thirty days, so the key must be stable across the
//! whole window.
//!
//! Keying on today's date fires thirty times. Keying on the **thing being notified about** — the expiry
//! instant — fires once. That is the difference between a useful warning and thirty emails somebody filters to
//! trash, after which they miss the real one.
//!
//! ## Idempotent under retry means at-least-once plus a provider key
//!
//! TASKS.md asks for delivery that is idempotent under retry, and there is no local-only way to get it. A
//! worker that sends and then crashes before recording `sent` leaves a `queued` row: retrying may duplicate,
//! and not retrying may lose. Insert-then-send is at-least-once; send-then-insert is at-most-once.
//!
//! For a notification, at-least-once is the right side to fail on — a duplicate is an annoyance and a silent
//! miss is the failure the path existed to prevent. So the firing is recorded first, and [`Firing::digest_key`]
//! is handed to the provider as *its* idempotency key, which is where the duplicate actually gets collapsed.
//! Claiming that the ledger alone makes delivery idempotent would be false.

use crate::Error;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// A path definition, as far as firing cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    pub id: Uuid,
    pub name: String,
    pub trigger_kind: String,
    pub lead_days: Option<i32>,
    pub channels: Vec<String>,
    pub digest_window: Option<Duration>,
    pub throttle_per_asset: Option<Duration>,
}

/// A recorded firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    pub id: Uuid,
    pub path_id: Uuid,
    pub asset_id: Option<Uuid>,
    pub digest_key: String,
    pub state: String,
    pub fired_at: DateTime<Utc>,
}

/// Whether a firing was recorded or suppressed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    /// Newly recorded; the caller should deliver it.
    Recorded(Firing),
    /// This exact thing has already been notified. The commonest outcome of a daily sweep, and not an error.
    AlreadyFired(Firing),
    /// Inside this asset's throttle window for this path.
    Throttled { until: DateTime<Utc> },
}

/// What is being notified about.
///
/// The variants exist because they produce *different* digest keys, and the key is the deduplication — so
/// getting the shape wrong is what turns one warning into thirty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// Something that will happen at a known instant: a licence ending, a release lapsing.
    ///
    /// Keyed on that instant, so a sweep that sees the same asset every day for a month fires once. Keying on
    /// the sweep's own date is the mistake.
    Deadline { at: DateTime<Utc> },
    /// Something that happened once, identified by the event.
    ///
    /// Keyed on the event id, so a replayed event is recognised.
    Event { id: Uuid },
    /// A recurring state worth reporting periodically — "this asset still has no AI disclosure".
    ///
    /// Keyed on a **bucket** of the current time, so it repeats at the digest cadence rather than either
    /// once ever or once per sweep.
    Recurring { bucket: DateTime<Utc> },
}

/// Builds the digest key for a firing.
///
/// Deterministic and stable: two callers describing the same notification must produce the same key, or the
/// unique index deduplicates nothing.
pub fn digest_key(path: &Path, asset_id: Option<Uuid>, subject: &Subject) -> String {
    // The asset is part of the key, because a path fires per asset and two assets expiring on the same day are
    // two notifications.
    let asset = asset_id.map_or_else(|| "-".to_owned(), |id| id.to_string());
    match subject {
        // The instant, at second resolution. Not the sweep date — see the module docs.
        Subject::Deadline { at } => {
            format!("{}|{asset}|deadline:{}", path.trigger_kind, at.timestamp())
        }
        Subject::Event { id } => format!("{}|{asset}|event:{id}", path.trigger_kind),
        Subject::Recurring { bucket } => {
            format!(
                "{}|{asset}|window:{}",
                path.trigger_kind,
                bucket.timestamp()
            )
        }
    }
}

/// The digest bucket `now` falls into for a window.
///
/// Truncated to a multiple of the window from the epoch, so every caller inside one window computes the same
/// bucket without coordinating. A window of `None` buckets to `now` itself, which means no collapsing.
pub fn digest_bucket(now: DateTime<Utc>, window: Option<Duration>) -> DateTime<Utc> {
    match window {
        Some(window) if window.num_seconds() > 0 => {
            let seconds = window.num_seconds();
            let truncated = now.timestamp() - now.timestamp().rem_euclid(seconds);
            DateTime::from_timestamp(truncated, 0).unwrap_or(now)
        }
        _ => now,
    }
}

/// Records a firing, deduplicating against the ledger and honouring the throttle.
///
/// The insert is the deduplication: `ON CONFLICT DO NOTHING` against the unique index means a concurrent
/// sweep on another worker cannot produce a second notification for the same subject.
pub async fn fire(
    pool: &sqlx::PgPool,
    path: &Path,
    asset_id: Option<Uuid>,
    subject: &Subject,
    recipient_count: i32,
    now: DateTime<Utc>,
) -> Result<FireOutcome, Error> {
    // The throttle is checked before the insert, because a throttled firing must leave *no* ledger row: a
    // suppressed row would occupy the digest key and stop the notification firing later when it should.
    if let Some(throttle) = path.throttle_per_asset
        && let Some(asset_id) = asset_id
    {
        let since = now - throttle;
        let recent: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT max(fired_at) FROM path_firings \
             WHERE path_id = $1 AND asset_id = $2 AND fired_at > $3 AND state <> 'suppressed'",
        )
        .bind(path.id)
        .bind(asset_id)
        .bind(since)
        .fetch_one(pool)
        .await?;
        if let Some(last) = recent {
            return Ok(FireOutcome::Throttled {
                until: last + throttle,
            });
        }
    }

    let key = digest_key(path, asset_id, subject);
    let id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO path_firings \
         (id, path_id, asset_id, digest_key, recipient_count, state, fired_at) \
         VALUES ($1, $2, $3, $4, $5, 'queued', $6) \
         ON CONFLICT (path_id, digest_key) DO NOTHING",
    )
    .bind(id)
    .bind(path.id)
    .bind(asset_id)
    .bind(&key)
    .bind(recipient_count)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();

    let existing = by_key(pool, path.id, &key).await?.ok_or_else(|| {
        Error::Inconsistent(format!(
            "firing {key} for path {} vanished immediately after being recorded",
            path.id
        ))
    })?;

    if inserted > 0 {
        // Counters on the path itself, for the admin UI. Updated only on a genuinely new firing, so a
        // deduplicated sweep does not inflate `fire_count` — which is the number somebody uses to judge
        // whether a path is too noisy.
        sqlx::query(
            "UPDATE paths SET last_fired_at = $2, fire_count = fire_count + 1, updated_at = $2 \
             WHERE id = $1",
        )
        .bind(path.id)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(FireOutcome::Recorded(existing))
    } else {
        Ok(FireOutcome::AlreadyFired(existing))
    }
}

/// A firing by its key.
pub async fn by_key(
    pool: &sqlx::PgPool,
    path_id: Uuid,
    digest_key: &str,
) -> Result<Option<Firing>, Error> {
    let row = sqlx::query_as::<_, FiringRow>(
        "SELECT id, path_id, asset_id, digest_key, state, fired_at FROM path_firings \
         WHERE path_id = $1 AND digest_key = $2",
    )
    .bind(path_id)
    .bind(digest_key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(into_firing))
}

/// The queued firings due for delivery, oldest first.
///
/// **This does not claim exclusively, and the name used to say otherwise.** Rows stay `queued` until a delivery
/// result is recorded, which is deliberate — an in-progress state would need a crash-recovery sweep to move
/// abandoned rows back, and the provider idempotency key already covers the duplicate a retry causes. The
/// consequence is that two workers polling at the same time both get the same firings.
///
/// It carried `FOR UPDATE SKIP LOCKED` and a comment claiming that let several workers drain without
/// overlapping. That was wrong in a way worth recording: the statement runs on a pool, so it is its own
/// transaction, and the row locks are released the instant it returns. `SKIP LOCKED` protects nothing between
/// two *calls* — only within one transaction a caller holds open. Found by a surviving mutation: deleting the
/// clause changed no behaviour at all, because it never had any.
///
/// So the clause is gone rather than left as false reassurance, and the duplicate is stated. When the
/// notification worker is written it must be idempotent per `(path_id, digest_key)` at the *provider*, which is
/// what `digest_key` exists for — not assume this function handed it exclusive work.
pub async fn due_for_delivery(pool: &sqlx::PgPool, limit: i64) -> Result<Vec<Firing>, Error> {
    let rows = sqlx::query_as::<_, FiringRow>(
        "SELECT id, path_id, asset_id, digest_key, state, fired_at FROM path_firings \
         WHERE state = 'queued' ORDER BY fired_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(into_firing).collect())
}

/// Marks a firing delivered.
pub async fn mark_sent(pool: &sqlx::PgPool, id: Uuid) -> Result<bool, Error> {
    let updated =
        sqlx::query("UPDATE path_firings SET state = 'sent' WHERE id = $1 AND state = 'queued'")
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(updated > 0)
}

/// Records a delivery failure, keeping the reason.
///
/// The row stays in the ledger rather than being deleted, so the digest key stays claimed. Deleting it would
/// let the next sweep fire the same notification again — turning a provider outage into a flood once it
/// recovered.
pub async fn mark_failed(pool: &sqlx::PgPool, id: Uuid, reason: &str) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE path_firings SET state = 'failed', last_error = $2 WHERE id = $1 AND state = 'queued'",
    )
    .bind(id)
    .bind(reason)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Loads the enabled paths for a trigger.
pub async fn enabled_for<'e, E>(executor: E, trigger_kind: &str) -> Result<Vec<Path>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            Option<i32>,
            Vec<String>,
            Option<sqlx::postgres::types::PgInterval>,
            Option<sqlx::postgres::types::PgInterval>,
        ),
    >(
        "SELECT id, name, trigger_kind, lead_days, channels, digest_window, throttle_per_asset \
         FROM paths WHERE enabled AND trigger_kind = $1 ORDER BY lead_days DESC NULLS LAST, name",
    )
    .bind(trigger_kind)
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, name, trigger_kind, lead_days, channels, digest_window, throttle)| Path {
                id,
                name,
                trigger_kind,
                lead_days,
                channels,
                digest_window: digest_window.map(interval_to_duration),
                throttle_per_asset: throttle.map(interval_to_duration),
            },
        )
        .collect())
}

/// A Postgres `interval` as a `Duration`.
///
/// Months are approximated at 30 days. That is wrong in general and harmless here: these intervals are digest
/// and throttle windows, measured in hours or days, and a path configured in months is asking for "about a
/// month" rather than a calendar boundary.
fn interval_to_duration(interval: sqlx::postgres::types::PgInterval) -> Duration {
    Duration::microseconds(interval.microseconds)
        + Duration::days(i64::from(interval.days))
        + Duration::days(i64::from(interval.months) * 30)
}

type FiringRow = (Uuid, Uuid, Option<Uuid>, String, String, DateTime<Utc>);

fn into_firing(row: FiringRow) -> Firing {
    let (id, path_id, asset_id, digest_key, state, fired_at) = row;
    Firing {
        id,
        path_id,
        asset_id,
        digest_key,
        state,
        fired_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn path() -> Path {
        Path {
            id: Uuid::from_u128(1),
            name: "licence expiring".to_owned(),
            trigger_kind: "license_expiring".to_owned(),
            lead_days: Some(30),
            channels: vec!["email".to_owned()],
            digest_window: None,
            throttle_per_asset: None,
        }
    }

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap()
    }

    #[test]
    fn a_deadline_key_is_stable_across_the_whole_sweep_window() {
        // The property the ledger exists for. A daily "expiring in 30 days" sweep sees the same asset on all
        // thirty days; keying on the sweep's own date fires thirty times, and the recipient filters the path to
        // trash and then misses the real warning.
        let asset = Some(Uuid::from_u128(7));
        let expiry = at(30, 12);

        let day_one = digest_key(&path(), asset, &Subject::Deadline { at: expiry });
        let day_thirty = digest_key(&path(), asset, &Subject::Deadline { at: expiry });
        assert_eq!(
            day_one, day_thirty,
            "the key must depend on the deadline, not on when the sweep ran"
        );
    }

    #[test]
    fn two_assets_expiring_together_are_two_notifications() {
        let expiry = at(30, 12);
        let a = digest_key(
            &path(),
            Some(Uuid::from_u128(1)),
            &Subject::Deadline { at: expiry },
        );
        let b = digest_key(
            &path(),
            Some(Uuid::from_u128(2)),
            &Subject::Deadline { at: expiry },
        );
        assert_ne!(a, b);
    }

    #[test]
    fn a_different_deadline_on_the_same_asset_is_a_new_notification() {
        // A renewed licence has a new end date, and the new expiry deserves its own warning — otherwise
        // renewing an asset once silences it forever.
        let asset = Some(Uuid::from_u128(7));
        let first = digest_key(&path(), asset, &Subject::Deadline { at: at(30, 12) });
        let renewed = digest_key(&path(), asset, &Subject::Deadline { at: at(31, 12) });
        assert_ne!(first, renewed);
    }

    #[test]
    fn the_trigger_kind_is_part_of_the_key() {
        // Two paths on different triggers can concern the same asset and the same instant — a licence and a
        // release both lapsing on the same day. They are separate notifications.
        let asset = Some(Uuid::from_u128(7));
        let deadline = Subject::Deadline { at: at(30, 12) };
        let licence = digest_key(&path(), asset, &deadline);
        let release = digest_key(
            &Path {
                trigger_kind: "release_expiring".to_owned(),
                ..path()
            },
            asset,
            &deadline,
        );
        assert_ne!(licence, release);
    }

    #[test]
    fn a_digest_window_buckets_every_caller_the_same_way() {
        // Truncated from the epoch rather than measured from a start time, so two workers inside one window
        // compute the same bucket without coordinating — which is what makes the dedupe work across processes.
        let window = Some(Duration::hours(6));
        let early = digest_bucket(at(18, 1), window);
        let late = digest_bucket(at(18, 5), window);
        assert_eq!(
            early, late,
            "01:00 and 05:00 fall in the same six-hour bucket"
        );

        let next = digest_bucket(at(18, 7), window);
        assert_ne!(next, early, "07:00 is the next bucket");
    }

    #[test]
    fn no_digest_window_means_no_collapsing() {
        let a = digest_bucket(at(18, 1), None);
        let b = digest_bucket(at(18, 2), None);
        assert_ne!(a, b);
    }

    #[test]
    fn a_zero_window_does_not_divide_by_zero() {
        // A path configured with a zero interval is a configuration mistake, and it must not panic in a
        // scheduler that runs every minute.
        let now = at(18, 1);
        assert_eq!(digest_bucket(now, Some(Duration::zero())), now);
    }

    #[test]
    fn an_event_key_recognises_a_replay() {
        let event = Uuid::from_u128(99);
        let first = digest_key(&path(), None, &Subject::Event { id: event });
        let replay = digest_key(&path(), None, &Subject::Event { id: event });
        assert_eq!(first, replay);
    }
}
