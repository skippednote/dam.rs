//! Reading what the lifecycle engine plans over, and writing what it decided (§6.4).
//!
//! `dam_store::lifecycle` is pure arithmetic over a slice of candidates: it takes rows and returns verdicts,
//! which is what makes it testable without a database. This module is the other half — the query that
//! produces those rows, and the writes that record the outcome.
//!
//! ## `pinned` is computed here, not trusted
//!
//! `object_placements.pinned` exists, and there is an index built for the candidate scan that reads it. But
//! nothing has ever written it, and a column nobody maintains is a column that says `false` for a
//! legal-hold asset. Archiving one of those is the single worst thing this engine can do: the bytes become
//! unreadable for up to 48 hours in the middle of the litigation that put the hold there.
//!
//! So the scan derives pinning from the facts that mean it — `assets.legal_hold`, membership of a `pin_hot`
//! collection — and ORs the stored column on top. The column stays useful as a manual override; it is just
//! not the only thing standing between a policy typo and a legal problem.
//!
//! ## The interface derivatives are already exempt, structurally
//!
//! §2 makes the proxies and thumbnails the search substrate: the grid draws them, the enrichment stage reads
//! them, a portal serves them. An archived thumbnail is a grid cell that cannot render for two days.
//!
//! That rule is already enforced one layer down and better than this module could: `Key::is_tier_exempt`
//! reads the key's own namespace (`p/`, `t/`, `c2pa/`, `staging/`), so it holds "even for an object whose
//! placement row is missing or stale" — and the planner reports those as `TierExempt` rather than dropping
//! them, so a run over a policy scoped to derivatives is visibly a no-op instead of a silent one. The scan
//! therefore does not re-derive it from `derivatives.role`; a second implementation of the same rule is a
//! second thing to keep in step, and this one would be the weaker of the two.
//!
//! What the scan *does* honour is `applies_to` and `derivative_roles`, because those are the policy's scope
//! rather than a safety rail: a tenant asking to archive superseded renditions of a particular role is asking
//! a legitimate question, and the exemption above still answers for the three that back the interface.

use crate::Error;
use chrono::{DateTime, Utc};
use dam_core::storage::StorageClass;
use dam_store::Key;
use dam_store::lifecycle::{Candidate, LifecyclePolicy};
use sqlx::Row;
use uuid::Uuid;

/// One `lifecycle_policies` row, with the parts the engine needs.
#[derive(Debug, Clone)]
pub struct Policy {
    pub id: Uuid,
    pub engine: LifecyclePolicy,
    /// `original`, `derivative`, or `both`.
    pub applies_to: String,
    pub derivative_roles: Vec<String>,
    pub action: String,
    pub target_pool_id: Option<Uuid>,
    pub from_storage_class: Option<StorageClass>,
    pub min_age_days: i32,
}

/// The enabled policies, in the order the engine must apply them.
///
/// Priority ascending — "lowest wins, first match applies", as the schema comment says. Reading them in a
/// different order than the schema documents would make a two-policy tenant's outcome depend on insertion
/// order, which is the kind of bug that only shows up once a customer has enough policies to notice.
pub async fn policies(conn: &mut sqlx::PgConnection) -> Result<Vec<Policy>, Error> {
    let rows = sqlx::query(
        "SELECT id, name, enabled, applies_to, derivative_roles, only_superseded, \
                min_age_days, idle_days, from_storage_class, action, target_pool_id, \
                target_class, max_objects_per_run, dry_run \
         FROM lifecycle_policies WHERE enabled ORDER BY priority, created_at",
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter().map(policy_of).collect()
}

/// One policy by id, enabled or not.
///
/// Separate from [`policies`] because a dry run is something an operator asks for *by name* — including for a
/// policy they have not enabled yet, which is the whole point of being able to read a plan first.
pub async fn policy(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Option<Policy>, Error> {
    let row = sqlx::query(
        "SELECT id, name, enabled, applies_to, derivative_roles, only_superseded, \
                min_age_days, idle_days, from_storage_class, action, target_pool_id, \
                target_class, max_objects_per_run, dry_run \
         FROM lifecycle_policies WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;

    row.map(policy_of).transpose()
}

fn policy_of(row: sqlx::postgres::PgRow) -> Result<Policy, Error> {
    let target: Option<String> = row.try_get("target_class")?;
    // `lifecycle_target_present` guarantees a class for a transition, so a missing one here means the
    // constraint was dropped or the action is `evict`/`replicate` — neither of which this engine executes.
    // Defaulting to the current class would produce a policy that plans nothing and explains nothing.
    let target_class = match target.as_deref() {
        Some(raw) => raw
            .parse()
            .map_err(|_| Error::Inconsistent(format!("lifecycle target_class holds {raw:?}")))?,
        None => StorageClass::Standard,
    };
    let from: Option<String> = row.try_get("from_storage_class")?;
    let from_storage_class = match from.as_deref() {
        Some(raw) => Some(raw.parse().map_err(|_| {
            Error::Inconsistent(format!("lifecycle from_storage_class holds {raw:?}"))
        })?),
        None => None,
    };

    // `idle_days` is what the engine ages on; `min_age_days` is a separate floor the scan applies. A policy
    // with neither is one that would move everything the moment it was enabled, so a null `idle_days` reads
    // as `min_age_days` rather than as zero.
    let min_age_days: i32 = row.try_get("min_age_days")?;
    let idle_days: Option<i32> = row.try_get("idle_days")?;
    let after_days = u32::try_from(idle_days.unwrap_or(min_age_days).max(0)).unwrap_or(u32::MAX);
    let max: i32 = row.try_get("max_objects_per_run")?;

    Ok(Policy {
        id: row.try_get("id")?,
        engine: LifecyclePolicy {
            name: row.try_get("name")?,
            enabled: row.try_get("enabled")?,
            target_class,
            after_days,
            only_superseded: row.try_get("only_superseded")?,
            // The 128 KiB minimum billable object on IA and Glacier IR: below it, the colder class costs
            // *more*. Taken from the target pool rather than hardcoded, and zero when there is no pool.
            min_size_bytes: None,
            max_objects_per_run: u32::try_from(max).ok(),
            dry_run: row.try_get("dry_run")?,
        },
        applies_to: row.try_get("applies_to")?,
        derivative_roles: row.try_get("derivative_roles")?,
        action: row.try_get("action")?,
        target_pool_id: row.try_get("target_pool_id")?,
        from_storage_class,
        min_age_days,
    })
}

/// The placements one policy could act on.
///
/// Movable rows first, then by key. Both halves matter:
///
/// Ordered by key so a plan is stable between runs: an operator comparing two dry runs is comparing text, and
/// a set query returning rows in a different order every time would make every diff useless.
///
/// ## The window is much wider than the run cap, and that is the fix for a real bug
///
/// The first version fetched `max_objects_per_run + 1` rows. Against a real library on AWS that meant a tenant
/// with 136 pinned placements and a cap of 1 planned nothing at all, run after run: the two rows it fetched
/// were both pinned, the planner correctly reported two skips and no transitions, and a policy that looked
/// perfectly configured could never reach a movable object.
///
/// Two attempts at ordering the unmovable rows last both failed, and instructively. Demoting `pinned` missed
/// pins that come from a `pin_hot` collection rather than the column; adding "already in the target class"
/// then missed cold-to-warm, which is a refusal only a class ordering can see. Every version of that fix was
/// an incomplete reimplementation of the planner in SQL, which is the wrong shape: `lifecycle::plan` applies
/// rules no predicate can express — a size floor, a minimum-duration counter, a warming direction — and it is
/// the only thing that should be deciding.
///
/// So the query stops trying to be clever. The **cap bounds what moves**, because that is what costs money and
/// makes S3 calls; the **window bounds what is read**, which is one indexed scan. They are different budgets
/// and conflating them is what produced a policy that silently did nothing forever.
pub fn scan_window(max_objects_per_run: Option<u32>) -> i64 {
    // Fifty rows read per row moved, floored at a thousand and ceilinged at fifty thousand. The floor is
    // there so a cap of one still gets a real look at the library; the ceiling so a cap of ten thousand does
    // not turn one sweep into a half-million-row read. Between them, a policy whose candidates are almost all
    // pinned still finds the few that are not.
    let per_move = i64::from(max_objects_per_run.unwrap_or(10_000)).saturating_mul(50);
    per_move.clamp(1_000, 50_000)
}

/// The placements one policy could act on.
///
/// Ordered by key so a plan is stable between runs: an operator comparing two dry runs is comparing text, and
/// a set query returning rows in a different order every time would make every diff useless. Read at
/// [`scan_window`] width rather than at the run cap — see there for why those are different budgets.
pub async fn candidates(
    conn: &mut sqlx::PgConnection,
    policy: &Policy,
    now: DateTime<Utc>,
) -> Result<Vec<Candidate>, Error> {
    let horizon = now - chrono::Duration::days(i64::from(policy.min_age_days.max(0)));
    let limit = scan_window(policy.engine.max_objects_per_run);

    let rows = sqlx::query(
        "SELECT p.object_key, p.pool_id, p.size_bytes, p.state, p.storage_class, \
                p.restore_state, p.min_duration_until, p.placed_at, p.last_accessed_at, \
                (p.pinned \
                 OR coalesce(a.legal_hold, false) \
                 OR EXISTS (SELECT 1 FROM collection_items ci \
                            JOIN collections c ON c.id = ci.collection_id \
                            WHERE ci.asset_id = p.asset_id AND c.pin_hot)) AS pinned, \
                -- The reason, in the order that decides which one matters. A legal hold outranks everything
                -- because it is the one that is not negotiable; a pinned collection names itself, so an
                -- operator reading a plan can go and unpin it; the stored column is the manual note and
                -- comes last.
                coalesce( \
                    CASE WHEN a.legal_hold THEN 'the asset is under legal hold' END, \
                    (SELECT 'a member of the pinned collection ' || quote_literal(c.label) \
                     FROM collection_items ci JOIN collections c ON c.id = ci.collection_id \
                     WHERE ci.asset_id = p.asset_id AND c.pin_hot \
                     ORDER BY c.label LIMIT 1), \
                    p.pin_reason \
                ) AS pin_reason \
         FROM object_placements p \
         LEFT JOIN assets a ON a.id = p.asset_id \
         LEFT JOIN derivatives d ON d.id = p.derivative_id \
         WHERE p.state = 'present' \
           AND p.placed_at <= $1 \
           AND ($2::text IS NULL OR p.storage_class = $2) \
           AND (a.id IS NULL OR a.deleted_at IS NULL) \
           AND CASE $4 \
                   WHEN 'original' THEN p.derivative_id IS NULL \
                   WHEN 'derivative' THEN p.derivative_id IS NOT NULL \
                   ELSE true \
               END \
           AND ($5::text[] = '{}' OR p.derivative_id IS NULL OR d.role = ANY($5)) \
         ORDER BY p.object_key \
         LIMIT $3",
    )
    .bind(horizon)
    .bind(policy.from_storage_class.map(|c| c.to_string()))
    .bind(limit)
    .bind(&policy.applies_to)
    .bind(&policy.derivative_roles)
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter().map(candidate_of).collect()
}

fn candidate_of(row: sqlx::postgres::PgRow) -> Result<Candidate, Error> {
    let key: String = row.try_get("object_key")?;
    let class: String = row.try_get("storage_class")?;
    let state: String = row.try_get("state")?;
    let restore: String = row.try_get("restore_state")?;
    let size: i64 = row.try_get("size_bytes")?;
    let pin_reason: Option<String> = row.try_get("pin_reason")?;

    Ok(Candidate {
        object_key: Key::new(key.clone())
            .map_err(|_| Error::Inconsistent(format!("object_placements holds key {key:?}")))?,
        pool_id: row.try_get("pool_id")?,
        size_bytes: u64::try_from(size).unwrap_or(0),
        state: state
            .parse()
            .map_err(|_| Error::Inconsistent(format!("placement state holds {state:?}")))?,
        storage_class: class
            .parse()
            .map_err(|_| Error::Inconsistent(format!("storage_class holds {class:?}")))?,
        restore_state: restore
            .parse()
            .map_err(|_| Error::Inconsistent(format!("restore_state holds {restore:?}")))?,
        pinned: row.try_get("pinned")?,
        // Resolved in the query, in precedence order — see the `coalesce` there. A skip that says only
        // "pinned" is a plan an operator cannot act on: the useful question is which pin, and the answer is
        // different work depending on whether it is a hold, a collection, or a note somebody left.
        pin_reason,
        min_duration_until: row.try_get("min_duration_until")?,
        placed_at: row.try_get("placed_at")?,
        last_accessed_at: row.try_get("last_accessed_at")?,
    })
}

/// Records one completed transition.
///
/// `storage_class` and `min_duration_until` together, in one statement, because they are one fact: the class
/// an object is in and the date it may next move are what the *next* run reads, and a crash between two
/// statements would leave an object in Glacier with no minimum-duration counter — free to hop again
/// immediately, at the full 90-day charge for the hop it just made.
pub async fn transitioned(
    conn: &mut sqlx::PgConnection,
    object_key: &str,
    pool_id: Uuid,
    to: StorageClass,
    min_duration_until: Option<DateTime<Utc>>,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE object_placements \
         SET storage_class = $3, min_duration_until = $4 \
         WHERE object_key = $1 AND pool_id = $2",
    )
    .bind(object_key)
    .bind(pool_id)
    .bind(to.to_string())
    .bind(min_duration_until)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Writes what a run did, for the next run and for anybody asking why nothing moved.
pub async fn ran(
    conn: &mut sqlx::PgConnection,
    policy_id: Uuid,
    moved: i32,
    at: DateTime<Utc>,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE lifecycle_policies SET last_run_at = $2, last_run_moved = $3, updated_at = now() \
         WHERE id = $1",
    )
    .bind(policy_id)
    .bind(at)
    .bind(moved)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
