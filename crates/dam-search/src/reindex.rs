//! Rebuilding a tenant's index from Postgres (2.6's operational half).
//!
//! Postgres is the record; the index is derived. So there has to be one command that regenerates the
//! derived thing from the record, and it has to be the *same* document builder the incremental path uses —
//! two builders is how a reindex silently changes what search returns.
//!
//! ## Soft-deleted assets are indexed, with the flag set
//!
//! Not skipped. [`crate::query::render`] excludes `deleted` on every query, so an indexed tombstone is
//! invisible to search; and a restore then needs no reindex, because the document is already there and only
//! the flag has to flip. Skipping them would make undelete a reindex, which for a large tenant is minutes
//! of stale search.
//!
//! ## Cursored, not `LIMIT/OFFSET`
//!
//! Keyed on `assets.id`, so a run over a library that is being written to cannot skip or repeat a row —
//! an OFFSET walk over a table taking inserts does both.

use crate::document::AssetDocument;
use crate::pool::IndexPool;
use crate::schema::IndexSchema;
use crate::{Error, Result};
use dam_core::TenantSlug;
use uuid::Uuid;

/// Rows fetched per round trip. Large enough that a big library is not thousands of queries, small enough
/// that the batch and its documents fit comfortably in memory.
pub const DEFAULT_BATCH: usize = 500;

/// What a reindex did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub indexed: usize,
    /// How many of those were soft-deleted tombstones. Reported because "indexed 40,000" over a library of
    /// 12,000 live assets is otherwise a mystery.
    pub tombstones: usize,
}

/// Rebuilds `tenant`'s index from the tenant schema `pool` is scoped to.
///
/// Replaces the index wholesale: every document is deleted and rewritten in one commit, so a reader either
/// sees the old index or the new one. Deleting first and committing per batch would leave search returning
/// a fraction of the library for the duration, which on a large tenant is worse than a stale index.
pub async fn tenant(
    pool: &sqlx::PgPool,
    indexes: &IndexPool,
    tenant: &TenantSlug,
    schema: &IndexSchema,
    batch: usize,
) -> Result<Stats> {
    let batch = i64::try_from(batch.max(1)).unwrap_or(i64::from(u16::MAX));
    let writer = indexes.writer(tenant, schema).await?;
    let mut guard = writer.lock().await;
    guard.delete_all_documents()?;

    let mut stats = Stats::default();
    let mut after: Option<Uuid> = None;
    loop {
        let rows = sqlx::query_as::<_, Row>(
            "SELECT a.id, a.filename, a.deleted_at IS NOT NULL AS deleted, \
                    coalesce(m.values, '{}'::jsonb) AS values, \
                    coalesce(array_agg(gm.group_id) FILTER (WHERE gm.group_id IS NOT NULL), '{}') AS groups \
             FROM assets a \
             LEFT JOIN asset_metadata m ON m.asset_id = a.id \
             LEFT JOIN asset_group_members gm ON gm.asset_id = a.id \
             WHERE ($1::uuid IS NULL OR a.id > $1) \
             GROUP BY a.id, a.filename, a.deleted_at, m.values \
             ORDER BY a.id LIMIT $2",
        )
        .bind(after)
        .bind(batch)
        .fetch_all(pool)
        .await
        .map_err(|e| Error::Tantivy(format!("reading assets to index: {e}")))?;

        if rows.is_empty() {
            break;
        }
        after = rows.last().map(|row| row.0);

        for (asset_id, filename, deleted, values, groups) in rows {
            if deleted {
                stats.tombstones += 1;
            }
            let document = AssetDocument {
                asset_id,
                filename,
                deleted,
                group_ids: groups,
                // A `values` that is not an object is a row that contradicts its own column type. Indexed
                // as empty rather than refused: one malformed row must not stop a tenant's whole reindex,
                // and the asset is still findable by filename.
                values: values.as_object().cloned().unwrap_or_default(),
            };
            guard.add_document(document.to_tantivy(schema))?;
            stats.indexed += 1;
        }
    }

    guard.commit()?;
    Ok(stats)
}

type Row = (Uuid, String, bool, serde_json::Value, Vec<Uuid>);
