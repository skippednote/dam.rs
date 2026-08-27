//! Writing a person's metadata edit, in one place.
//!
//! Three routes reach this: the single-asset `PATCH`, the bulk executor applying one operation across a
//! selection, and — the reason this module exists now rather than later — a migration transfer writing
//! the metadata it read from a source.
//!
//! Before this there were two copies and they had already drifted. The bulk executor's version is
//! documented as merging "exactly as the single-asset PATCH endpoint does", and it did not: it omitted
//! `enrichment::forget_provenance`, so bulk-editing a field a model had written left it still marked as
//! machine output. Every AI disclosure built on that marking then said a model wrote a value a person had
//! replaced — which is wrong in the direction that makes people stop believing the marking at all.
//!
//! The comment claiming parity is what kept it invisible: a reader checking whether the two agreed found a
//! sentence saying they did. So the fix is not "add the missing call to the second copy" — it is having one
//! copy, and this module is it.

use serde_json::Value;
use uuid::Uuid;

use crate::Error;

/// What a merge changed, for the caller's event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The document as stored, for a route that echoes it back.
    pub values: Value,
    /// The keys the caller touched, in the order given.
    ///
    /// The *keys*, never the values. A consumer needs to know whether the field it renders was touched,
    /// which the keys answer; the values would put a tenant's metadata into a delivery log and into
    /// whatever the receiver writes request bodies to. A receiver that wants them can read the asset with
    /// its own credential and get what it is allowed to see.
    pub edited: Vec<String>,
}

/// Merges `values` into one asset's metadata and records that a person did it.
///
/// A present `null` clears the field; an absent key is left alone. That asymmetry is the whole contract of
/// a patch: absent means "no opinion", and a caller wanting to clear says so with `null`.
///
/// Three writes, and each is load-bearing:
///
/// 1. the merged document,
/// 2. the provenance for the edited keys, dropped — see the module docs for the bug this was missing from,
/// 3. the asset's own `updated_at`, because a metadata edit is otherwise invisible to anything watching
///    the asset, and the reindex queue and the connector both key off it.
///
/// The outbox row is deliberately *not* here. Each route names itself in the event it emits, and an event
/// that misreported which one an edit came from would be worse than three call sites that each say so.
///
/// Takes a connection rather than a pool: every caller is already inside a transaction, and the four
/// writes have to land together or an asset ends up reindexed against metadata it does not have.
pub async fn merge(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    values: impl IntoIterator<Item = (String, Value)>,
) -> Result<Merged, Error> {
    let stored: Option<Value> =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await?;

    let mut merged = stored
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut edited = Vec::new();
    for (key, value) in values {
        edited.push(key.clone());
        if value.is_null() {
            merged.remove(&key);
        } else {
            merged.insert(key, value);
        }
    }
    let document = Value::Object(merged);

    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2) \
         ON CONFLICT (asset_id) DO UPDATE SET values = excluded.values, updated_at = now()",
    )
    .bind(asset_id)
    .bind(&document)
    .execute(&mut *conn)
    .await?;

    crate::enrichment::forget_provenance(&mut *conn, asset_id, &edited).await?;

    sqlx::query("UPDATE assets SET updated_at = now() WHERE id = $1")
        .bind(asset_id)
        .execute(&mut *conn)
        .await?;

    Ok(Merged {
        values: document,
        edited,
    })
}
