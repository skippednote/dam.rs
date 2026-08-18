//! The derivative cache (3.2).
//!
//! ## Keyed on the recipe, not the name
//!
//! `derivatives_op_idx` is `UNIQUE (asset_id, op_hash)`, and `op_hash` covers the size, format, quality,
//! fit, background, colour profile and rendering intent (§18.1). Looking a derivative up by **name** would
//! serve the old bytes forever after a profile was redefined — no error, nothing in a log, and a customer
//! seeing yesterday's quality setting indefinitely.
//!
//! 3.1 shipped with exactly that bug: its delivery path resolved `WHERE profile = $2`. It is fixed here,
//! and this module is the reason the fix has somewhere to live.
//!
//! ## `last_served_at` is written coarsely on purpose
//!
//! The lifecycle engine uses it to decide what is cold enough to evict. Writing it on every delivery turns
//! every read into a write and costs a row of WAL per download — on the hottest path in the system. An
//! hour's resolution answers "is this still being served" just as well, which is the same argument
//! `auth::LAST_USED_RESOLUTION` makes about API keys.

use crate::Error;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// How stale `last_served_at` must be before a delivery rewrites it.
pub const SERVED_RESOLUTION: Duration = Duration::hours(1);

/// A cached derivative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivative {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub role: String,
    pub profile: String,
    pub op_hash: String,
    pub object_key: String,
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// The column list every read here selects, as a tuple.
///
/// Named so the three readers share one shape: a fourth query with the columns in a different order would
/// map `mime` into `object_key` and the mistake would only show at delivery.
type DerivativeRow = (
    Uuid,
    Uuid,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i32>,
    Option<i32>,
);

fn into_derivative(row: DerivativeRow) -> Derivative {
    let (id, asset_id, role, profile, op_hash, object_key, mime, bytes, width, height) = row;
    Derivative {
        id,
        asset_id,
        role,
        profile,
        op_hash,
        object_key,
        mime,
        bytes,
        width,
        height,
    }
}

/// Finds the derivative for an exact recipe.
///
/// By `op_hash`, never by profile name. A profile that has been redefined has a different hash, so this
/// misses and the caller renders fresh — which is the entire point.
pub async fn by_op_hash<'e, E>(
    executor: E,
    asset_id: Uuid,
    op_hash: &str,
) -> Result<Option<Derivative>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as::<_, DerivativeRow>(
        "SELECT d.id, d.asset_id, d.role, d.profile, d.op_hash, d.object_key, d.mime, d.bytes, \
                d.width, d.height \
         FROM derivatives d JOIN assets a ON a.id = d.asset_id \
         WHERE d.asset_id = $1 AND d.op_hash = $2 AND a.deleted_at IS NULL",
    )
    .bind(asset_id)
    .bind(op_hash)
    .fetch_optional(executor)
    .await?;

    Ok(row.map(into_derivative))
}

/// What to record for a freshly rendered derivative.
#[derive(Debug, Clone)]
pub struct NewDerivative<'a> {
    pub asset_id: Uuid,
    pub role: &'a str,
    pub profile: &'a str,
    pub op_hash: &'a str,
    pub object_key: &'a str,
    pub mime: &'a str,
    pub bytes: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// How long the render took, so the lifecycle engine can tell a cheap thumbnail from an expensive
    /// export. §6.4: a 40-minute ProRes transcode is worth storing, a 400px JPEG is not.
    pub regen_cost_ms: Option<i32>,
}

/// Records a rendered derivative, returning the row that ended up in the table.
///
/// `ON CONFLICT DO NOTHING` then read back, rather than an upsert. Two workers can render the same
/// derivative concurrently — they produce byte-identical output for the same recipe, so the loser's row is
/// simply redundant. Overwriting would repoint `object_key` at a second identical object and orphan the
/// first, which the reaper has no way to know about.
///
/// ## The proxy is different, and it is a constraint rather than a choice
///
/// `derivatives_proxy_idx` is `UNIQUE (asset_id) WHERE role = 'proxy'`: an asset has **one** master proxy,
/// because D5 makes it the search-and-AI substrate rather than one rendition among many. So a *redefined*
/// proxy profile cannot coexist with the old one the way a thumbnail can — there is no room for two rows.
///
/// This refuses that case instead of silently replacing, and [`replace_proxy`] does the swap while handing
/// back the superseded object key. An upsert here would look tidier and would orphan an object on every
/// proxy redefinition, with nothing in the schema recording that the old key still exists.
pub async fn record(pool: &sqlx::PgPool, new: &NewDerivative<'_>) -> Result<Derivative, Error> {
    if new.role == "proxy"
        && let Some(existing) = current_proxy(pool, new.asset_id).await?
        && existing.op_hash != new.op_hash
    {
        return Err(Error::Unsupported(format!(
            "asset {} already has a master proxy under recipe {}; an asset has exactly one (D5), so a \
             redefined proxy must go through `replace_proxy`, which reports the object it supersedes",
            new.asset_id, existing.op_hash
        )));
    }
    sqlx::query(
        "INSERT INTO derivatives \
         (id, asset_id, role, profile, op_hash, object_key, mime, bytes, width, height, regen_cost_ms) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (asset_id, op_hash) DO NOTHING",
    )
    .bind(new.asset_id)
    .bind(new.role)
    .bind(new.profile)
    .bind(new.op_hash)
    .bind(new.object_key)
    .bind(new.mime)
    .bind(new.bytes)
    .bind(new.width)
    .bind(new.height)
    .bind(new.regen_cost_ms)
    .execute(pool)
    .await?;

    by_op_hash(pool, new.asset_id, new.op_hash)
        .await?
        .ok_or_else(|| {
            Error::Inconsistent(format!(
                "derivative {} for asset {} vanished immediately after being recorded",
                new.op_hash, new.asset_id
            ))
        })
}

/// The asset's current master proxy, if it has one.
pub async fn current_proxy<'e, E>(executor: E, asset_id: Uuid) -> Result<Option<Derivative>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query_as::<_, DerivativeRow>(
        "SELECT d.id, d.asset_id, d.role, d.profile, d.op_hash, d.object_key, d.mime, d.bytes, \
                d.width, d.height \
         FROM derivatives d JOIN assets a ON a.id = d.asset_id \
         WHERE d.asset_id = $1 AND d.role = 'proxy' AND a.deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(into_derivative))
}

/// Swaps an asset's master proxy for a newly rendered one.
///
/// Returns the object key that is now unreferenced, so the caller can schedule its deletion. Returning it
/// rather than deleting it here is deliberate: the row and the object live in different systems, and
/// deleting the object before the row is committed would leave a placement pointing at nothing. The caller
/// commits first and reclaims after.
pub async fn replace_proxy(
    pool: &sqlx::PgPool,
    new: &NewDerivative<'_>,
) -> Result<(Derivative, Option<String>), Error> {
    if new.role != "proxy" {
        return Err(Error::Unsupported(format!(
            "replace_proxy is for the master proxy; {:?} is an ordinary derivative and can simply be \
             recorded alongside its predecessors",
            new.role
        )));
    }

    let mut tx = pool.begin().await?;
    let superseded: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, op_hash, object_key FROM derivatives \
         WHERE asset_id = $1 AND role = 'proxy' FOR UPDATE",
    )
    .bind(new.asset_id)
    .fetch_optional(&mut *tx)
    .await?;

    let orphaned = match superseded {
        // Already the current recipe: nothing to do, and nothing orphaned. Idempotent, because a retried
        // render must not report an object for deletion that is still in use.
        Some((_, ref op_hash, _)) if op_hash == new.op_hash => None,
        Some((id, _, object_key)) => {
            sqlx::query(
                "UPDATE derivatives SET op_hash = $2, object_key = $3, mime = $4, bytes = $5, \
                        width = $6, height = $7, regen_cost_ms = $8, profile = $9, \
                        last_served_at = NULL \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(new.op_hash)
            .bind(new.object_key)
            .bind(new.mime)
            .bind(new.bytes)
            .bind(new.width)
            .bind(new.height)
            .bind(new.regen_cost_ms)
            .bind(new.profile)
            .execute(&mut *tx)
            .await?;
            Some(object_key)
        }
        None => {
            sqlx::query(
                "INSERT INTO derivatives \
                 (id, asset_id, role, profile, op_hash, object_key, mime, bytes, width, height, \
                  regen_cost_ms) \
                 VALUES (gen_random_uuid(), $1, 'proxy', $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(new.asset_id)
            .bind(new.profile)
            .bind(new.op_hash)
            .bind(new.object_key)
            .bind(new.mime)
            .bind(new.bytes)
            .bind(new.width)
            .bind(new.height)
            .bind(new.regen_cost_ms)
            .execute(&mut *tx)
            .await?;
            None
        }
    };
    tx.commit().await?;

    let stored = by_op_hash(pool, new.asset_id, new.op_hash)
        .await?
        .ok_or_else(|| {
            Error::Inconsistent(format!(
                "master proxy {} for asset {} vanished immediately after being written",
                new.op_hash, new.asset_id
            ))
        })?;
    Ok((stored, orphaned))
}

/// Notes that a derivative was served, at most once per [`SERVED_RESOLUTION`].
///
/// Returns whether a write happened, so a caller can assert the throttling rather than assume it. The
/// filter is in the `WHERE` clause: doing it in Rust would need a read first, which is the round trip this
/// exists to avoid.
pub async fn mark_served(
    pool: &sqlx::PgPool,
    derivative_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE derivatives SET last_served_at = $2 \
         WHERE id = $1 AND (last_served_at IS NULL OR last_served_at < $3)",
    )
    .bind(derivative_id)
    .bind(now)
    .bind(now - SERVED_RESOLUTION)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Derivatives whose recipe no longer matches any current profile definition.
///
/// The eviction list after a profile is redefined. They are not deleted here: the bytes are still valid for
/// whatever asked for them, and a URL already issued against the old recipe still resolves. Reclaiming them
/// is the lifecycle engine's job, which is the only place that weighs storage cost against regeneration
/// cost.
pub async fn superseded<'e, E>(
    executor: E,
    current_op_hashes: &[String],
    limit: i64,
) -> Result<Vec<Derivative>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    // An empty current set would make `<> ALL('{}')` true for every row and propose deleting the entire
    // cache. Refusing is the safe reading: "no profiles are defined" is a configuration failure, not an
    // instruction to evict everything.
    if current_op_hashes.is_empty() {
        return Err(Error::Unsupported(
            "refusing to list superseded derivatives against an empty profile set; every derivative \
             would qualify, which is a configuration failure rather than an eviction plan"
                .to_owned(),
        ));
    }

    let rows = sqlx::query_as::<_, DerivativeRow>(
        "SELECT d.id, d.asset_id, d.role, d.profile, d.op_hash, d.object_key, d.mime, d.bytes, \
                d.width, d.height \
         FROM derivatives d \
         WHERE d.op_hash <> ALL($1) \
         ORDER BY d.last_served_at NULLS FIRST, d.created_at LIMIT $2",
    )
    .bind(current_op_hashes)
    .bind(limit)
    .fetch_all(executor)
    .await?;

    Ok(rows.into_iter().map(into_derivative).collect())
}
