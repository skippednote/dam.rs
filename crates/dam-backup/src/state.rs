//! Writing `dr_state`, which nothing did.
//!
//! Two functions, and the separation is the whole point of the table. [`record_backup`] moves
//! `last_backup_at`; [`record_drill`] moves `last_verified_restore_at`. Nothing moves both, because §17's
//! argument is that "the gap between 'we take backups' and 'we have restored one' is where DR plans fail" —
//! and a backup that quietly updated the verification column would close that gap on paper while widening it
//! in fact.

use chrono::{DateTime, Utc};
use dam_core::TenantSlug;

/// Records that a backup was taken. Deliberately does **not** touch `last_verified_restore_at`.
pub async fn record_backup(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    at: DateTime<Utc>,
    bytes: u64,
) -> Result<(), dam_db::Error> {
    // Upserted, because a tenant provisioned before this existed has no row and the first backup should not
    // fail on that.
    sqlx::query(
        "INSERT INTO dam_global.dr_state (tenant_id, last_backup_at, notes, updated_at) \
         SELECT id, $2, $3, $2 FROM dam_global.tenants WHERE slug = $1 \
         ON CONFLICT (tenant_id) DO UPDATE SET \
             last_backup_at = excluded.last_backup_at, \
             notes = excluded.notes, \
             updated_at = excluded.updated_at",
    )
    .bind(slug.as_str())
    .bind(at)
    .bind(format!("last dump {bytes} bytes"))
    .execute(global)
    .await?;
    Ok(())
}

/// Records that a restore actually happened, with how long it took.
///
/// The duration is measured rather than configured: §17 says the published RTO must be defensible, and a
/// number somebody typed into a config file is not.
pub async fn record_drill(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    at: DateTime<Utc>,
    duration_seconds: i64,
) -> Result<(), dam_db::Error> {
    sqlx::query(
        "INSERT INTO dam_global.dr_state \
             (tenant_id, last_verified_restore_at, verified_restore_duration_s, updated_at) \
         SELECT id, $2, $3, $2 FROM dam_global.tenants WHERE slug = $1 \
         ON CONFLICT (tenant_id) DO UPDATE SET \
             last_verified_restore_at = excluded.last_verified_restore_at, \
             verified_restore_duration_s = excluded.verified_restore_duration_s, \
             updated_at = excluded.updated_at",
    )
    .bind(slug.as_str())
    .bind(at)
    .bind(i32::try_from(duration_seconds).unwrap_or(i32::MAX))
    .execute(global)
    .await?;
    Ok(())
}

/// The four columns the report reads, named so the tuple is not four anonymous options.
type ReportRow = (
    String,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<i32>,
);

/// One row of the DR report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub slug: String,
    pub last_backup_at: Option<DateTime<Utc>>,
    pub last_verified_restore_at: Option<DateTime<Utc>>,
    pub verified_restore_duration_s: Option<i32>,
}

/// Every tenant, unverified first.
///
/// The order is the report: §17 says the list of tenants whose restore has never been verified "should be
/// short", and a report that buries them among the healthy ones is a report nobody reads to the end. Tenants
/// with no `dr_state` row at all are included — a missing row means no backup has ever been taken, which is
/// the most urgent line on the page and the easiest one to omit by joining the wrong way round.
pub async fn report(global: &sqlx::PgPool) -> Result<Vec<Row>, dam_db::Error> {
    let rows: Vec<ReportRow> = sqlx::query_as(
            "SELECT t.slug, d.last_backup_at, d.last_verified_restore_at, d.verified_restore_duration_s \
             FROM dam_global.tenants t \
             LEFT JOIN dam_global.dr_state d ON d.tenant_id = t.id \
             WHERE t.status <> 'deleted' \
             ORDER BY d.last_verified_restore_at ASC NULLS FIRST, t.slug",
    )
    .fetch_all(global)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(slug, last_backup_at, last_verified_restore_at, verified_restore_duration_s)| Row {
                slug,
                last_backup_at,
                last_verified_restore_at,
                verified_restore_duration_s,
            },
        )
        .collect())
}
