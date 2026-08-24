//! Asking the store whether the database is telling the truth.
//!
//! `dam_db::integrity` explains why this exists and what the two checksum columns are for. This is the
//! half that talks to the backend.
//!
//! **A `HEAD` per placement, and nothing downloaded.** The store trait says so in as many words —
//! `ObjectState::checksum` exists so "the integrity scrub verify without paying egress to
//! re-download". Verifying the content hash would mean pulling the whole library through the network
//! every pass, which is a cost an operator would turn off, and a check that gets turned off detects
//! nothing.
//!
//! **A store that cannot be reached is not a finding.** The distinction the whole feature turns on: an
//! object the backend says is absent is news, and an object the backend could not answer about is
//! weather. Recording the second as `missing` would fill the report with noise on every network blip
//! and teach an operator to disbelieve it — the same mistake `finalise` made in the other direction,
//! where one unreachable `HEAD` was read as "the object is gone" and retired a finished upload.
//!
//! **And one byte, because `HEAD` can be right about an object that cannot be read.** Measured on the
//! load run: of 689 damaged objects, 609 were gone outright and `HEAD` said so, while around 80 were
//! still listed at exactly their recorded size and served nothing — an asset reporting 331,390 bytes
//! whose download returned zero. A metadata-only pass calls that healthy, and on a backend that
//! reports no checksum on `HEAD` — which SeaweedFS does not — there is nothing else in the response
//! left to disagree with. So a placement that passes the metadata checks is asked for its first byte:
//! one request and one byte of egress per placement per pass.
//!
//! **What that probe does and does not catch, measured rather than assumed.** Only a probe that
//! *succeeds and returns nothing* is treated as a finding, because that is the one answer no working
//! backend can give: a byte was requested, the store said yes, and there was no byte. A probe that
//! **errors** is weather, and is left alone — and on SeaweedFS that is precisely how those 80 objects
//! fail, so this pass does not flag them. It is the right trade in one direction and a real gap in the
//! other: a single failed read cannot be told apart from a blip, and calling it corruption is how a
//! report becomes noise. Distinguishing them needs history — the same probe failing across several
//! passes — which is a column and a decision this slice does not have. Until then the honest statement
//! is that the scrub detects a missing object reliably and an unreadable one only when the backend
//! answers rather than errors.

use chrono::{DateTime, Utc};
use dam_core::TenantSlug;
use dam_db::integrity::{self, Verdict};
use dam_store::{BlobStore, ByteRange, GetOutcome, Key};

use crate::Result;

/// How many placements one pass checks.
///
/// A window rather than the whole table, because a pass holds no transaction and a library is not
/// bounded. `due` orders never-verified first and oldest after, so consecutive passes walk the whole
/// library rather than re-checking the same page — the window sets how long a full cycle takes, not
/// whether one happens. Five thousand `HEAD`s is pennies at S3's request pricing and minutes of
/// wall-clock; a deployment large enough to care wants a shorter cadence or a wider window, and both
/// are one number.
pub const WINDOW: i64 = 5_000;

/// How long between scrubs.
///
/// Daily, matching the tiering sweep. The failure this detects is not one an hourly pass would catch
/// meaningfully sooner — bytes do not go missing on a schedule — and the report is something an
/// operator reads in the morning.
pub const SCRUB_EVERY: chrono::Duration = chrono::Duration::hours(24);

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scrubbed {
    /// Placements checked and found to agree with the store.
    pub verified: usize,
    /// Placements the store says are not there.
    pub missing: usize,
    /// Placements present and disagreeing with the row.
    pub corrupt: usize,
    /// Placements the store could not answer about. Not a finding, and deliberately not recorded.
    pub unreachable: usize,
}

impl Scrubbed {
    #[must_use]
    pub fn findings(&self) -> usize {
        self.missing + self.corrupt
    }
}

/// Checks one window of this tenant's placements against the store.
pub async fn scrub(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &TenantSlug,
    now: DateTime<Utc>,
) -> Result<Scrubbed> {
    // Read the window and let the connection go. A pass is thousands of round trips to a remote
    // backend, and holding a transaction across them would pin a connection for minutes against a
    // table every request reads — the reason `tiering::one_policy` does the same.
    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let window = integrity::due(conn.executor(), WINDOW).await?;
    conn.commit().await?;

    let mut scrubbed = Scrubbed::default();
    let mut verdicts = Vec::with_capacity(window.len());

    for placement in window {
        let key = match Key::new(placement.object_key.clone()) {
            Ok(key) => key,
            Err(_) => {
                // A key the store layer will not accept is a database problem, not a storage one, and
                // it is the one row shape a `HEAD` cannot be asked about. Loud, and skipped.
                tracing::error!(
                    key = %placement.object_key,
                    "object_placements holds a key the store cannot parse",
                );
                scrubbed.unreachable += 1;
                continue;
            }
        };

        match store.head(&key).await {
            Ok(state) => {
                let (verdict, checksum) = verify(store, &key, &placement, &state).await;
                match verdict {
                    Verdict::Present => scrubbed.verified += 1,
                    Verdict::Corrupt => {
                        scrubbed.corrupt += 1;
                        tracing::warn!(
                            key = %placement.object_key,
                            recorded = placement.size_bytes,
                            found = state.size,
                            "placement disagrees with the object it names",
                        );
                    }
                    Verdict::Missing => scrubbed.missing += 1,
                }
                verdicts.push((placement.object_key, verdict, checksum));
            }
            Err(dam_store::Error::NotFound(_)) => {
                scrubbed.missing += 1;
                tracing::warn!(
                    key = %placement.object_key,
                    asset = ?placement.asset_id,
                    "the object a placement names is not in the store",
                );
                verdicts.push((placement.object_key, Verdict::Missing, None));
            }
            // An archived object with no live restore is exactly where the lifecycle engine put it.
            // `head` answers for it, so this is defensive rather than expected, and it is not a finding.
            Err(dam_store::Error::NotRestored { .. }) => {
                scrubbed.verified += 1;
                verdicts.push((placement.object_key, Verdict::Present, None));
            }
            Err(error) => {
                // Weather, not news. See the module docs.
                scrubbed.unreachable += 1;
                tracing::debug!(
                    key = %placement.object_key,
                    %error,
                    "the store could not be asked about a placement; leaving its state alone",
                );
            }
        }
    }

    // One transaction for the writes, taken after the network work rather than around it.
    if !verdicts.is_empty() {
        let mut conn = dam_db::TenantConn::begin(global, slug).await?;
        for (key, verdict, checksum) in &verdicts {
            integrity::record(conn.executor(), key, *verdict, checksum.as_deref(), now).await?;
        }
        conn.commit().await?;
    }

    Ok(scrubbed)
}

/// The verdict for one placement the store answered for, and the checksum worth remembering.
///
/// Size first, because every backend reports it and a truncated object is the failure a killed writer
/// actually produces — the load run that motivated this left around eighty objects whose `HEAD`
/// succeeded and whose bytes read as empty.
///
/// The checksum comparison only fires when both sides have one. A first pass has nothing to compare
/// against and records instead, which is why a fresh deployment reports no corruption on day one and
/// can from day two.
async fn verify(
    store: &dyn BlobStore,
    key: &Key,
    placement: &dam_db::integrity::Checkable,
    state: &dam_store::ObjectState,
) -> (Verdict, Option<String>) {
    let recorded = u64::try_from(placement.size_bytes).unwrap_or(0);
    if state.size != recorded {
        return (Verdict::Corrupt, state.checksum.clone());
    }

    if let (Some(before), Some(now)) = (&placement.remote_checksum, &state.checksum)
        && before != now
    {
        return (Verdict::Corrupt, Some(now.clone()));
    }

    // A zero-byte object is a legitimate thing to store and there is no first byte to ask for, so the
    // probe is skipped rather than reporting every empty object as damaged.
    if recorded == 0 {
        return (Verdict::Present, state.checksum.clone());
    }

    match store.get(key, Some(ByteRange::new(0, Some(0)))).await {
        Ok(GetOutcome::Bytes(bytes)) if bytes.is_empty() => {
            tracing::warn!(
                key = %key.as_str(),
                recorded,
                "the store lists an object at its recorded size and serves none of it",
            );
            (Verdict::Corrupt, state.checksum.clone())
        }
        Ok(_) => (Verdict::Present, state.checksum.clone()),
        // Weather. The metadata agreed, and one unreadable probe is not enough to overturn that —
        // leaving the verdict alone costs a pass, and getting it wrong costs the report's credibility.
        Err(error) => {
            tracing::debug!(
                key = %key.as_str(),
                %error,
                "the first-byte probe could not be answered; keeping the metadata verdict",
            );
            (Verdict::Present, state.checksum.clone())
        }
    }
}
