//! Which pages use which assets (M3d·4, §11.4).
//!
//! `connector_asset_refs` turns every connected site into a usage index — "this asset appears on 12 pages of
//! site X" — and three things depend on it: usage reporting, takedown impact before an asset is pulled, and a
//! pin-hot signal for the lifecycle engine, since an asset live on a production site is a poor tiering
//! candidate whatever its download history says.
//!
//! ## The site reports its own usage, so it is advisory — and staleness is the whole design
//!
//! 0004 says so of `usage_sample`: "Populated by the connector, so it is advisory rather than authoritative."
//! That is fine for a report and dangerous for a pin. A site that stops reporting — decommissioned, broken
//! module, a token nobody renewed — looks exactly like a site that stopped using the asset. If a reference
//! pinned forever, one abandoned integration would hold a library in Standard indefinitely; if it never
//! pinned, a live page would cause a restore storm the first time somebody thawed the original.
//!
//! So a reference pins only while it is **fresh**: refreshed inside [`STALE_AFTER`], on an active connector,
//! actually in use. [`pinning`] is the one query the lifecycle engine asks, and it carries every one of those
//! conditions.
//!
//! ## Two kinds of stale, and they mean different things
//!
//! **Version drift** is `synced_version_no` behind `assets.version_no`: the site is rendering an older version
//! and its sync worker has not caught up. **Refresh overdue** is `synced_at` older than [`STALE_AFTER`]: the
//! site has not told us anything lately. The first is a job to run; the second is a site to go and look at.
//! Both are derived on read rather than stored, for the reason `crate::proofing` gives about its outcome — a
//! stored flag is a second source of truth that drifts from the timestamps under it.
//!
//! The `state` column's CHECK still permits `'stale'` and nothing here ever writes it. Deliberate: the column
//! records what somebody *asserted* — `expired`, `unpublished`, `orphaned` — and staleness is computed.
//!
//! ## A reference is keyed on the remote entity, not on the asset
//!
//! `(connector_id, remote_entity_type, remote_entity_id)`, because that is what the remote can identify. An
//! entity that switches which asset it shows updates `asset_id` on the same row rather than leaving two.
//!
//! Which means the index only ever grows unless somebody tells it what went away — so [`sweep_absent`] exists:
//! the site sends its complete list for an entity type and everything else becomes `orphaned`. Without it, a
//! deleted Drupal node pins its asset hot forever and a takedown report over-counts.

use crate::Error;
use chrono::{DateTime, Duration, Utc};
use sqlx::{Postgres, QueryBuilder, Row as _};
use std::collections::HashMap;
use uuid::Uuid;

/// How long a reference keeps counting as live.
///
/// Thirty days. A connected site reports on its own schedule — a cron, a cache warm, an editor saving a node —
/// so the window has to be much longer than any of those and still short enough that an abandoned integration
/// releases its pins within a billing cycle or two.
pub const STALE_AFTER: Duration = Duration::days(30);

/// What the remote is telling us about one of its entities.
#[derive(Debug, Clone)]
pub struct NewRef<'a> {
    pub asset_id: Uuid,
    /// `media` for Drupal. The remote's own vocabulary, because it is what the remote can report and what an
    /// operator will recognise in a URL.
    pub remote_entity_type: &'a str,
    pub remote_entity_id: &'a str,
    pub remote_uuid: Option<&'a str>,
    /// Where an operator can go and look at it. The single most useful field in a takedown report.
    pub remote_url: Option<&'a str>,
    /// How many places downstream the entity is actually used — pages, not entities.
    pub usage_count: i32,
    /// `[{url, title}]`. A sample rather than a list: a media entity on four hundred pages does not need four
    /// hundred rows to tell an operator it matters.
    pub usage_sample: serde_json::Value,
    /// Which version the site is rendering, when it knows.
    pub synced_version_no: Option<i32>,
}

/// One reference, as a report reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub connector_id: Uuid,
    pub connector_label: String,
    pub asset_id: Uuid,
    pub remote_entity_type: String,
    pub remote_entity_id: String,
    pub remote_url: Option<String>,
    pub usage_count: i32,
    pub usage_sample: serde_json::Value,
    pub synced_version_no: Option<i32>,
    pub synced_at: Option<DateTime<Utc>>,
    /// `linked`, `expired`, `unpublished` or `orphaned` — what somebody asserted. Never `stale`.
    pub state: String,
    /// The site is rendering a version older than the current one. A job to run.
    pub version_drifted: bool,
    /// The site has not reported inside [`STALE_AFTER`]. A site to go and look at — and the reason this
    /// reference no longer pins.
    pub refresh_overdue: bool,
}

/// What a report pass changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reported {
    pub written: u64,
}

/// What pulling an asset would affect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Impact {
    /// Distinct connected sites.
    pub sites: i64,
    /// Remote entities — media rows, in Drupal's terms.
    pub entities: i64,
    /// Places those entities are used, summed from what each site reported. The number an operator actually
    /// weighs, and the softest of the three: it is the site's own count.
    pub pages: i64,
}

/// Records what a site is using.
///
/// One statement per reference rather than one for the batch, because the upsert has to merge per row and a
/// site reporting four hundred entities is doing so in the background. Correctness over a round trip saved.
pub async fn report(
    conn: &mut sqlx::PgConnection,
    connector_id: Uuid,
    refs: &[NewRef<'_>],
    now: DateTime<Utc>,
) -> Result<Reported, Error> {
    let mut written = 0u64;
    for one in refs {
        let affected = sqlx::query(
            "INSERT INTO connector_asset_refs \
             (connector_id, asset_id, remote_entity_type, remote_entity_id, remote_uuid, \
              remote_url, usage_count, usage_sample, synced_version_no, synced_at, state) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'linked') \
             ON CONFLICT (connector_id, remote_entity_type, remote_entity_id) DO UPDATE SET \
                asset_id = excluded.asset_id, \
                remote_uuid = excluded.remote_uuid, \
                remote_url = excluded.remote_url, \
                usage_count = excluded.usage_count, \
                usage_sample = excluded.usage_sample, \
                synced_version_no = excluded.synced_version_no, \
                synced_at = excluded.synced_at, \
                -- Back to `linked`: a site reporting an entity is a site saying it is in use again, which is
                -- what un-orphans one that was swept away and then came back.
                state = 'linked', \
                updated_at = now()",
        )
        .bind(connector_id)
        .bind(one.asset_id)
        .bind(one.remote_entity_type)
        .bind(one.remote_entity_id)
        .bind(one.remote_uuid)
        .bind(one.remote_url)
        .bind(one.usage_count.max(0))
        .bind(&one.usage_sample)
        .bind(one.synced_version_no)
        .bind(now)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        written += affected;
    }
    Ok(Reported { written })
}

/// Marks everything of one entity type that the site did *not* report as orphaned.
///
/// The other half of a full sync. Without it the index only grows: a deleted Drupal node keeps pinning its
/// asset hot and keeps appearing in takedown reports, and nothing in the system can tell that it is gone.
///
/// Scoped to one entity type, because a site may sync its media rows without knowing anything about a type
/// some other module registered. Sweeping across types would let one integration orphan another's rows.
///
/// Orphaned rather than deleted: an operator asking "why did this stop being pinned" needs to see that it was
/// once used, and a deleted row answers nothing. The row is small and the history is the point.
pub async fn sweep_absent(
    conn: &mut sqlx::PgConnection,
    connector_id: Uuid,
    remote_entity_type: &str,
    seen: &[&str],
) -> Result<u64, Error> {
    let owned: Vec<String> = seen.iter().map(|id| (*id).to_owned()).collect();
    Ok(sqlx::query(
        "UPDATE connector_asset_refs SET state = 'orphaned', updated_at = now() \
         WHERE connector_id = $1 AND remote_entity_type = $2 \
           AND NOT (remote_entity_id = ANY($3)) AND state <> 'orphaned'",
    )
    .bind(connector_id)
    .bind(remote_entity_type)
    .bind(&owned)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Records that a site's rendering of an asset is out of date, or gone.
///
/// For the states a *site* cannot report about itself: damrs expiring a licence, or unpublishing an asset. The
/// site will learn through the webhook outbox; this is what the report says in the meantime.
pub async fn set_state(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    state: &str,
) -> Result<u64, Error> {
    Ok(sqlx::query(
        "UPDATE connector_asset_refs SET state = $2, updated_at = now() \
         WHERE asset_id = $1 AND state NOT IN ('orphaned', $2)",
    )
    .bind(asset_id)
    .bind(state)
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Every reference to one asset, across every site.
pub async fn for_asset(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Vec<Reference>, Error> {
    let rows = sqlx::query(REFERENCES_FOR_ASSET)
        .bind(asset_id)
        .bind(now - STALE_AFTER)
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter().map(reference_of).collect()
}

/// Every reference one site holds, most-used first.
pub async fn for_connector(
    conn: &mut sqlx::PgConnection,
    connector_id: Uuid,
    limit: i64,
    now: DateTime<Utc>,
) -> Result<Vec<Reference>, Error> {
    let rows = sqlx::query(REFERENCES_FOR_CONNECTOR)
        .bind(connector_id)
        .bind(now - STALE_AFTER)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter().map(reference_of).collect()
}

/// What pulling each of `asset_ids` would affect.
///
/// One query for the batch, because the caller is a bulk-delete preview asking about a selection. An asset with
/// no references is absent from the map rather than present with zeroes — the caller wants "which of these are
/// in use", and a map full of zeroes makes that a filter rather than a lookup.
///
/// Counts only what is *live*: linked, in use, on an active connector, refreshed inside the window. A takedown
/// report that counted orphaned rows would tell an operator a page exists that does not.
pub async fn impact(
    conn: &mut sqlx::PgConnection,
    asset_ids: &[Uuid],
    now: DateTime<Utc>,
) -> Result<HashMap<Uuid, Impact>, Error> {
    if asset_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT r.asset_id, \
                count(DISTINCT r.connector_id) AS sites, \
                count(*) AS entities, \
                coalesce(sum(r.usage_count), 0)::bigint AS pages \
         FROM connector_asset_refs r \
         JOIN connectors c ON c.id = r.connector_id \
         WHERE r.asset_id = ANY(",
    );
    builder.push_bind(asset_ids.to_vec());
    builder.push(
        ") AND r.state = 'linked' AND r.usage_count > 0 \
           AND c.status IN ('active', 'error') \
           AND r.synced_at IS NOT NULL AND r.synced_at > ",
    );
    builder.push_bind(now - STALE_AFTER);
    builder.push(" GROUP BY r.asset_id");

    let rows = builder.build().fetch_all(&mut *conn).await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get::<Uuid, _>("asset_id")?,
                Impact {
                    sites: row.try_get("sites")?,
                    entities: row.try_get("entities")?,
                    pages: row.try_get("pages")?,
                },
            ))
        })
        .collect()
}

// The pin-hot signal §11.4 describes is **not** here. It lives in `crate::tiering::candidates`, alongside the
// three pin sources that were already in that query — legal hold, a `pin_hot` collection, the manual column —
// because a fourth place deciding whether something is pinned would let a dry-run plan read from SQL disagree
// with what actually moves. See that query's comments for why each condition of it is load-bearing.
//
// [`impact`] above is the read surface for the same fact, and it is a different question: "what would pulling
// this affect", asked by a person about to delete something, rather than "may this move", asked by a planner.

/// The two reads, spelled in full.
///
/// Two whole statements rather than a shared prefix built with `format!`: sqlx takes a static string, and
/// composing SQL out of runtime pieces is a door this codebase keeps shut even when both halves are its own
/// literals. `crate::connectors` makes the same trade for the same reason.
///
/// `version_drifted` and `refresh_overdue` are computed here rather than stored, so they cannot disagree
/// with the timestamps under them. `$2` is the staleness horizon the caller passes.
const REFERENCES_FOR_ASSET: &str = "SELECT r.connector_id, c.label AS connector_label, r.asset_id, \
                              r.remote_entity_type, r.remote_entity_id, r.remote_url, \
                              r.usage_count, r.usage_sample, r.synced_version_no, r.synced_at, \
                              r.state, \
                              (r.synced_version_no IS NOT NULL AND a.version_no IS NOT NULL \
                               AND r.synced_version_no < a.version_no) AS version_drifted, \
                              (r.synced_at IS NULL OR r.synced_at <= $2) AS refresh_overdue \
                       FROM connector_asset_refs r \
                       JOIN connectors c ON c.id = r.connector_id \
                       LEFT JOIN assets a ON a.id = r.asset_id \
                       WHERE r.asset_id = $1 \
                       ORDER BY r.usage_count DESC, c.label, r.remote_entity_id";

const REFERENCES_FOR_CONNECTOR: &str = "SELECT r.connector_id, c.label AS connector_label, r.asset_id, \
                              r.remote_entity_type, r.remote_entity_id, r.remote_url, \
                              r.usage_count, r.usage_sample, r.synced_version_no, r.synced_at, \
                              r.state, \
                              (r.synced_version_no IS NOT NULL AND a.version_no IS NOT NULL \
                               AND r.synced_version_no < a.version_no) AS version_drifted, \
                              (r.synced_at IS NULL OR r.synced_at <= $2) AS refresh_overdue \
                       FROM connector_asset_refs r \
                       JOIN connectors c ON c.id = r.connector_id \
                       LEFT JOIN assets a ON a.id = r.asset_id \
                          WHERE r.connector_id = $1 \
                          ORDER BY r.usage_count DESC, r.remote_entity_id LIMIT $3";

fn reference_of(row: sqlx::postgres::PgRow) -> Result<Reference, Error> {
    Ok(Reference {
        connector_id: row.try_get("connector_id")?,
        connector_label: row.try_get("connector_label")?,
        asset_id: row.try_get("asset_id")?,
        remote_entity_type: row.try_get("remote_entity_type")?,
        remote_entity_id: row.try_get("remote_entity_id")?,
        remote_url: row.try_get("remote_url")?,
        usage_count: row.try_get("usage_count")?,
        usage_sample: row.try_get("usage_sample")?,
        synced_version_no: row.try_get("synced_version_no")?,
        synced_at: row.try_get("synced_at")?,
        state: row.try_get("state")?,
        version_drifted: row.try_get("version_drifted")?,
        refresh_overdue: row.try_get("refresh_overdue")?,
    })
}
