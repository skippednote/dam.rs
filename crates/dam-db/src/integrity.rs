//! Checking that the database's account of the object store is true.
//!
//! Every other module here trusts `object_placements`: the delivery path resolves a key from it, the
//! tiering sweep moves what it names, metering bills the bytes it claims. Nothing has ever asked the
//! store whether those rows are still right, and a row that is wrong is not visibly wrong — an asset
//! whose object has gone reads as `active`, reports its recorded size, and answers a download with a
//! 404. The library says the bytes are there and only the download disagrees.
//!
//! Found by losing some. A load run filled the disk under a single-node SeaweedFS, the container was
//! killed and restarted, and the writes from the last three minutes before it died did not survive:
//! 608 objects gone outright and around 80 present but unreadable. Postgres had flushed its rows and
//! kept every one of them. Nothing in the system noticed, because the columns that would have said so
//! — `state` with `missing` and `corrupt` in its CHECK, `remote_checksum`, `last_verified_at` — had
//! been in the schema since the first migration with no writer anywhere. `PlacementState` even
//! documents `Corrupt` as the state that "needs a scrub".
//!
//! **Size and the store's own checksum, not the content hash.** `checksum` is blake3 over the bytes
//! and computing it means downloading them, which is egress on the whole library to answer a question
//! a `HEAD` mostly answers. `remote_checksum` is for what the backend itself reports, and comparing
//! *that* across runs is what detects an object replaced underneath us — a different question from
//! whether the bytes match their hash, and the only one that is free. So the first pass over a
//! placement records the store's checksum and the ones after it compare.
//!
//! **A verdict is re-derived every pass, never accumulated.** A `missing` placement whose object comes
//! back — a restore, a re-replication, an operator putting it back — returns to `present` on the next
//! pass. Latching the bad state would mean the scrub could only ever report worse news, and an
//! operator who fixed something would have no way to see that they had.

use chrono::{DateTime, Utc};
use dam_core::PlacementState;
use sqlx::Row as _;
use uuid::Uuid;

use crate::Error;

/// One placement the scrub has to ask the store about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkable {
    pub object_key: String,
    pub pool_id: Uuid,
    /// Which asset this belongs to, for the report. `None` on a derivative's placement.
    pub asset_id: Option<Uuid>,
    pub derivative_id: Option<Uuid>,
    /// What the row claims the object weighs.
    pub size_bytes: i64,
    /// The backend's own checksum as of the last pass, when there was one.
    pub remote_checksum: Option<String>,
    pub state: PlacementState,
}

/// What one pass concluded about one placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The object is there and agrees with the row.
    Present,
    /// The store says there is no such object.
    Missing,
    /// The object is there and disagrees with the row.
    Corrupt,
}

impl Verdict {
    #[must_use]
    pub fn state(self) -> PlacementState {
        match self {
            Self::Present => PlacementState::Present,
            Self::Missing => PlacementState::Missing,
            Self::Corrupt => PlacementState::Corrupt,
        }
    }

    /// Whether this verdict is a finding an operator has to act on.
    #[must_use]
    pub fn is_finding(self) -> bool {
        !matches!(self, Self::Present)
    }
}

/// How many placements sit in each state right now.
///
/// The report the scrub exists to make possible. Counted rather than derived from one pass's return
/// value, because the useful number is the standing total across every pass — a run that checked two
/// hundred placements and found nothing says nothing about the six hundred a previous run flagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Standing {
    pub present: i64,
    pub missing: i64,
    pub corrupt: i64,
    /// Placements no pass has ever reached.
    pub unverified: i64,
}

impl Standing {
    #[must_use]
    pub fn findings(self) -> i64 {
        self.missing + self.corrupt
    }
}

/// The placements least recently checked, never-checked first.
///
/// `uploading` and `deleting` are excluded because neither is a claim about a stored object:
/// `metering` documents the first as "not stored yet", and the second is a row on its way out, so a
/// `HEAD` that misses it is the system working. `transitioning` is excluded for a different reason —
/// a class change in flight is exactly when a backend may answer inconsistently, and a scrub that
/// reported that as corruption would cry wolf on its own tiering sweep.
///
/// Ordered `NULLS FIRST` so a library that has never been scrubbed is worked through front to back
/// rather than sampled, and by key after that so the order is total and a run is reproducible.
pub async fn due(conn: &mut sqlx::PgConnection, limit: i64) -> Result<Vec<Checkable>, Error> {
    let rows = sqlx::query(
        "SELECT object_key, pool_id, asset_id, derivative_id, size_bytes, \
                remote_checksum, state \
         FROM object_placements \
         WHERE state IN ('present', 'missing', 'corrupt') \
         ORDER BY last_verified_at ASC NULLS FIRST, object_key \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter().map(checkable_of).collect()
}

fn checkable_of(row: sqlx::postgres::PgRow) -> Result<Checkable, Error> {
    let state: String = row.try_get("state")?;
    Ok(Checkable {
        object_key: row.try_get("object_key")?,
        pool_id: row.try_get("pool_id")?,
        asset_id: row.try_get("asset_id")?,
        derivative_id: row.try_get("derivative_id")?,
        size_bytes: row.try_get("size_bytes")?,
        remote_checksum: row.try_get("remote_checksum")?,
        state: state
            .parse()
            .map_err(|_| Error::Inconsistent(format!("placement state holds {state:?}")))?,
    })
}

/// Writes one verdict, stamping `last_verified_at` whatever it was.
///
/// The stamp is the point of the pass even when the answer is "still fine": without it the ordering in
/// [`due`] cannot advance and the scrub would re-check the same first page forever.
///
/// `remote_checksum` is only ever written, never cleared — a backend that stops reporting one (or a
/// driver that cannot, which `Capabilities` allows) must not erase the value a previous pass recorded
/// and with it the ability to notice a replacement.
pub async fn record(
    conn: &mut sqlx::PgConnection,
    object_key: &str,
    verdict: Verdict,
    remote_checksum: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE object_placements \
         SET state = $2, \
             last_verified_at = $3, \
             remote_checksum = coalesce($4, remote_checksum) \
         WHERE object_key = $1",
    )
    .bind(object_key)
    .bind(verdict.state().as_str())
    .bind(now)
    .bind(remote_checksum)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The standing total per state.
pub async fn standing(conn: &mut sqlx::PgConnection) -> Result<Standing, Error> {
    let row = sqlx::query(
        "SELECT count(*) FILTER (WHERE state = 'present') AS present, \
                count(*) FILTER (WHERE state = 'missing') AS missing, \
                count(*) FILTER (WHERE state = 'corrupt') AS corrupt, \
                count(*) FILTER (WHERE last_verified_at IS NULL) AS unverified \
         FROM object_placements",
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(Standing {
        present: row.try_get("present")?,
        missing: row.try_get("missing")?,
        corrupt: row.try_get("corrupt")?,
        unverified: row.try_get("unverified")?,
    })
}

/// Every placement currently flagged, for an operator who needs the list rather than the count.
///
/// Ordered by state then key so `corrupt` — the one that may still be recoverable from a backup, and
/// the one that will not announce itself by 404ing — reads before `missing`.
pub async fn findings(conn: &mut sqlx::PgConnection, limit: i64) -> Result<Vec<Checkable>, Error> {
    let rows = sqlx::query(
        "SELECT object_key, pool_id, asset_id, derivative_id, size_bytes, \
                remote_checksum, state \
         FROM object_placements \
         WHERE state IN ('corrupt', 'missing') \
         ORDER BY state, object_key \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter().map(checkable_of).collect()
}
