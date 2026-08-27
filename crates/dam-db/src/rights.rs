//! Loading and caching effective rights (2.8, GAPS G4).
//!
//! `dam_core::rights_eval` does the calculation. This loads its inputs and caches its verdicts, which is
//! the half that makes the D12 chokepoint affordable: intersecting every attached licence's scopes, every
//! attached release, and consumed usage against caps is far too much to do inside a signed-URL request,
//! and hopeless per row in a search result set.
//!
//! ## The cache is a cache, and the verdict is recomputed when it matters
//!
//! [`evaluate`] computes fresh and writes through. [`cached`] reads and returns nothing when the row has
//! passed its `expires_at`, so a stale `allowed` is never served. That expiry is exact rather than a
//! polling interval — `dam_core::rights_eval` reports the earliest instant a verdict could change on its
//! own, which is the only way an `allowed` becomes `denied` without an input changing.
//!
//! A miss is a recompute, not a denial. Failing closed on a cold cache would make every first download of
//! the day fail, and people would learn to retry rather than to read the error.
//!
//! ## `assets.rights_state` is denormalised and never authoritative
//!
//! Search and list endpoints need "is this usable" without a five-table join per row, so the verdict for
//! the tenant's default channel and territory is mirrored onto the asset. The download path does **not**
//! read it: a download names its own channel and territory, and the mirrored value answers a different
//! question.

use crate::Error;
use chrono::{DateTime, Utc};
use dam_core::rights::RightsState;
use dam_core::rights_eval::{self, Consumed, Evaluation, Inputs, License, Release, Scope, Usage};
use uuid::Uuid;

/// The channel and territory `assets.rights_state` is mirrored for.
///
/// A tenant-wide default so a list endpoint has something to show. `WORLD` is deliberately strict: it is
/// satisfied only by a grant that carves nothing out, so the mirrored badge errs toward warning rather
/// than reassuring.
pub const DEFAULT_CHANNEL: &str = "web";
pub const DEFAULT_TERRITORY: &str = "WORLD";

/// Loads everything the calculation needs for one asset.
///
/// Four queries rather than one join: the shapes are unrelated (licences have scopes, releases do not,
/// usage aggregates) and a single query would return the cross product — a licence with three scopes and
/// an asset with four releases becomes twelve rows to de-duplicate in Rust.
pub async fn inputs_for(pool: &sqlx::PgPool, asset_id: Uuid) -> Result<Inputs, Error> {
    let mut conn = pool.acquire().await?;
    inputs_for_on(&mut conn, asset_id).await
}

/// The same read, on a connection.
///
/// A caller inside a `TenantConn` transaction cannot hand over a pool — the pool's `search_path` is not the
/// transaction's — so every pool-taking function here has this variant. The MCP server is the caller that made
/// it necessary: it serves whichever tenant the key belongs to, so there is no one pinned pool to pass.
pub async fn inputs_for_on(conn: &mut sqlx::PgConnection, asset_id: Uuid) -> Result<Inputs, Error> {
    let legal_hold: Option<bool> =
        sqlx::query_scalar("SELECT legal_hold FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(legal_hold) = legal_hold else {
        return Err(Error::Core(dam_core::Error::NotFound {
            kind: dam_core::ResourceKind::Asset,
            id: asset_id.to_string(),
        }));
    };

    let license_rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            bool,
            i32,
            bool,
            bool,
            bool,
        ),
    >(
        "SELECT l.id, l.name, l.starts_at, l.ends_at, l.perpetual, l.renewal_notice_days, \
                l.ai_training_allowed, l.ai_generation_allowed, l.ai_processing_allowed \
         FROM licenses l \
         JOIN asset_licenses al ON al.license_id = l.id \
         WHERE al.asset_id = $1 ORDER BY l.id",
    )
    .bind(asset_id)
    .fetch_all(&mut *conn)
    .await?;

    let scope_rows = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            Vec<String>,
            Vec<String>,
            Vec<String>,
            Vec<String>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<i64>,
            Option<i64>,
            bool,
            bool,
        ),
    >(
        "SELECT s.id, s.license_id, s.territories, s.excluded_territories, s.channels, \
                s.excluded_channels, s.starts_at, s.ends_at, s.max_impressions, s.max_downloads, \
                s.allow_modification, s.allow_crop \
         FROM license_scopes s \
         JOIN asset_licenses al ON al.license_id = s.license_id \
         WHERE al.asset_id = $1 ORDER BY s.id",
    )
    .bind(asset_id)
    .fetch_all(&mut *conn)
    .await?;

    let licenses: Vec<License> = license_rows
        .into_iter()
        .map(|row| {
            let (
                id,
                name,
                starts_at,
                ends_at,
                perpetual,
                renewal_notice_days,
                ai_training_allowed,
                ai_generation_allowed,
                ai_processing_allowed,
            ) = row;
            License {
                id,
                name,
                starts_at,
                ends_at,
                perpetual,
                renewal_notice_days: i64::from(renewal_notice_days),
                ai_training_allowed,
                ai_generation_allowed,
                ai_processing_allowed,
                scopes: scope_rows
                    .iter()
                    .filter(|s| s.1 == id)
                    .map(|s| Scope {
                        id: s.0,
                        territories: s.2.clone(),
                        excluded_territories: s.3.clone(),
                        channels: s.4.clone(),
                        excluded_channels: s.5.clone(),
                        starts_at: s.6,
                        ends_at: s.7,
                        max_impressions: s.8,
                        max_downloads: s.9,
                        allow_modification: s.10,
                        allow_crop: s.11,
                    })
                    .collect(),
            }
        })
        .collect();

    let release_rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Vec<String>,
            Vec<String>,
            bool,
            bool,
            String,
        ),
    >(
        "SELECT r.id, r.kind, r.subject_name, r.starts_at, r.expires_at, r.territories, \
                r.channels, r.subject_is_minor, r.guardian_consent, r.status \
         FROM releases r \
         JOIN asset_releases ar ON ar.release_id = r.id \
         WHERE ar.asset_id = $1 ORDER BY r.id",
    )
    .bind(asset_id)
    .fetch_all(&mut *conn)
    .await?;

    let releases = release_rows
        .into_iter()
        .map(|row| Release {
            id: row.0,
            kind: row.1,
            subject_name: row.2,
            starts_at: row.3,
            expires_at: row.4,
            territories: row.5,
            channels: row.6,
            subject_is_minor: row.7,
            guardian_consent: row.8,
            status: row.9,
        })
        .collect();

    // Summed per scope. An append-only ledger is the right storage — a counter cannot be audited or
    // corrected — but the calculation wants totals, so the aggregation happens here.
    let consumed_rows = sqlx::query_as::<_, (Uuid, i64, i64)>(
        "SELECT license_scope_id, \
                coalesce(sum(impressions), 0)::bigint, coalesce(sum(downloads), 0)::bigint \
         FROM rights_usage \
         WHERE asset_id = $1 AND license_scope_id IS NOT NULL \
         GROUP BY license_scope_id",
    )
    .bind(asset_id)
    .fetch_all(&mut *conn)
    .await?;

    Ok(Inputs {
        licenses,
        releases,
        consumed: consumed_rows
            .into_iter()
            .map(|(id, impressions, downloads)| {
                (
                    id,
                    Consumed {
                        impressions,
                        downloads,
                    },
                )
            })
            .collect(),
        legal_hold,
    })
}

/// Computes the verdict for one usage and writes it through to the cache.
pub async fn evaluate(
    pool: &sqlx::PgPool,
    asset_id: Uuid,
    usage: &Usage,
    now: DateTime<Utc>,
) -> Result<Evaluation, Error> {
    let mut conn = pool.acquire().await?;
    evaluate_on(&mut conn, asset_id, usage, now).await
}

/// The same evaluation, on a connection — see [`inputs_for_on`] for why every one of these has a pair.
///
/// Note what it still does: it *writes* the verdict to the cache. A read-only-looking call that stores is
/// deliberate — the cache is what delivery reads on the hot path — and inside a caller's transaction that write
/// is theirs to commit or roll back.
pub async fn evaluate_on(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    usage: &Usage,
    now: DateTime<Utc>,
) -> Result<Evaluation, Error> {
    let inputs = inputs_for_on(&mut *conn, asset_id).await?;
    let evaluation = rights_eval::evaluate(&inputs, usage, now);
    store_on(&mut *conn, asset_id, usage, &evaluation).await?;
    Ok(evaluation)
}

/// Reads a cached verdict, or `None` if there is none or it has expired.
///
/// The expiry check is in the query. Reading the row and comparing in Rust would be equivalent right up
/// to the point where a caller forgot, and what they would get is a stale `allowed`.
pub async fn cached(
    pool: &sqlx::PgPool,
    asset_id: Uuid,
    usage: &Usage,
    now: DateTime<Utc>,
) -> Result<Option<CachedVerdict>, Error> {
    let mut conn = pool.acquire().await?;
    cached_on(&mut conn, asset_id, usage, now).await
}

/// [`cached`], against a caller's connection.
///
/// Delivery needs this: it resolves its tenant from the signed claim and reads through a `TenantConn`,
/// so a function that acquires its own connection from a shared pool would read the wrong schema —
/// `search_path` is set on the transaction, not on the pool.
pub async fn cached_on(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    usage: &Usage,
    now: DateTime<Utc>,
) -> Result<Option<CachedVerdict>, Error> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            serde_json::Value,
            Option<i64>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT verdict, reasons, impressions_remaining, expires_at \
         FROM rights_evaluations \
         WHERE asset_id = $1 AND channel = $2 AND territory = $3 \
           AND (expires_at IS NULL OR expires_at > $4)",
    )
    .bind(asset_id)
    .bind(&usage.channel)
    .bind(&usage.territory)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;

    let Some((verdict, reasons, impressions_remaining, expires_at)) = row else {
        return Ok(None);
    };
    Ok(Some(CachedVerdict {
        verdict: parse_verdict(&verdict)?,
        reasons,
        impressions_remaining,
        expires_at,
    }))
}

/// A verdict as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedVerdict {
    pub verdict: RightsState,
    pub reasons: serde_json::Value,
    pub impressions_remaining: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// The verdict for a usage, from the cache if it is fresh and by computing it if not.
///
/// What the delivery path calls. A miss recomputes rather than denying: failing closed on a cold cache
/// would make the first download of the day fail for every asset, and people would learn to retry instead
/// of to read the error.
pub async fn effective(
    pool: &sqlx::PgPool,
    asset_id: Uuid,
    usage: &Usage,
    now: DateTime<Utc>,
) -> Result<RightsState, Error> {
    let mut conn = pool.acquire().await?;
    effective_on(&mut conn, asset_id, usage, now).await
}

/// [`effective`], against a caller's connection.
///
/// The cache read and the evaluation that fills it on a miss run on the *same* connection, which is
/// what makes this usable inside a tenant transaction. It also means a miss writes
/// `rights_evaluations` inside the caller's transaction rather than in its own — so the row becomes
/// visible when that transaction commits instead of immediately. That is the right trade for delivery:
/// a verdict is re-derived per request anyway, and a cache row that appears slightly later is cheaper
/// than a verdict read from the wrong schema.
pub async fn effective_on(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    usage: &Usage,
    now: DateTime<Utc>,
) -> Result<RightsState, Error> {
    if let Some(hit) = cached_on(&mut *conn, asset_id, usage, now).await? {
        return Ok(hit.verdict);
    }
    Ok(evaluate_on(&mut *conn, asset_id, usage, now).await?.verdict)
}

/// Writes a verdict into the cache, and mirrors the default usage onto the asset.
async fn store_on(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    usage: &Usage,
    evaluation: &Evaluation,
) -> Result<(), Error> {
    let reasons = serde_json::to_value(
        evaluation
            .reasons
            .iter()
            .map(|r| {
                serde_json::json!({
                    "code": r.code,
                    "detail": r.detail,
                    "subject": r.subject,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| serde_json::json!([]));

    // No transaction of its own: the caller owns one. Two rows written here have to land or not land together,
    // and on a connection that is the caller's transaction rather than a nested one.
    sqlx::query(
        "INSERT INTO rights_evaluations \
         (asset_id, channel, territory, verdict, reasons, impressions_remaining, computed_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, now(), $7) \
         ON CONFLICT (asset_id, channel, territory) DO UPDATE SET \
             verdict = excluded.verdict, reasons = excluded.reasons, \
             impressions_remaining = excluded.impressions_remaining, \
             computed_at = excluded.computed_at, expires_at = excluded.expires_at",
    )
    .bind(asset_id)
    .bind(&usage.channel)
    .bind(&usage.territory)
    .bind(evaluation.verdict.as_str())
    .bind(&reasons)
    .bind(evaluation.impressions_remaining)
    .bind(evaluation.expires_at)
    .execute(&mut *conn)
    .await?;

    // Only the default usage is mirrored. Mirroring every channel would make the last evaluated one win,
    // and a list badge would then flip depending on which download happened most recently.
    if usage.channel == DEFAULT_CHANNEL && usage.territory == DEFAULT_TERRITORY {
        sqlx::query(
            "UPDATE assets SET rights_state = $2, rights_evaluated_at = now(), \
                    ai_processing_allowed = $3, earliest_rights_expiry = $4 WHERE id = $1",
        )
        .bind(asset_id)
        .bind(evaluation.verdict.as_str())
        .bind(evaluation.ai_processing_allowed)
        .bind(evaluation.expires_at)
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}

/// Drops every cached verdict for an asset.
///
/// Called when an input changes — a licence edited, a release withdrawn, usage recorded. Invalidating
/// rather than recomputing keeps the write path cheap and moves the cost to the next read, which is also
/// the first moment the new verdict is actually needed.
pub async fn invalidate(pool: &sqlx::PgPool, asset_id: Uuid) -> Result<u64, Error> {
    let mut tx = pool.begin().await?;
    let dropped = sqlx::query("DELETE FROM rights_evaluations WHERE asset_id = $1")
        .bind(asset_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // Back to `unknown`, not left at the old value. A stale badge saying `allowed` after a licence was
    // revoked is worse than one saying "not yet evaluated".
    sqlx::query(
        "UPDATE assets SET rights_state = 'unknown', rights_evaluated_at = NULL, \
                ai_processing_allowed = NULL, earliest_rights_expiry = NULL WHERE id = $1",
    )
    .bind(asset_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(dropped)
}

/// Every asset whose cached verdict has expired, for the worker to recompute.
pub async fn stale(
    pool: &sqlx::PgPool,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<(Uuid, Usage)>, Error> {
    let rows = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT asset_id, channel, territory FROM rights_evaluations \
         WHERE expires_at IS NOT NULL AND expires_at <= $1 \
         ORDER BY expires_at LIMIT $2",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(asset_id, channel, territory)| (asset_id, Usage { channel, territory }))
        .collect())
}

fn parse_verdict(raw: &str) -> Result<RightsState, Error> {
    raw.parse().map_err(|_| {
        Error::Inconsistent(format!(
            "rights_evaluations.verdict holds {raw:?}, which the CHECK constraint should have refused"
        ))
    })
}
