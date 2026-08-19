//! Versions of an asset (Q.8).
//!
//! `assets` has carried `version_group_id`, `version_no`, `is_current` and `replaces_id` since migration 0001, and
//! nothing has ever written a second version. This is what writes them, and what reads a version history back.
//!
//! ## A version is a row, not a column
//!
//! Each version is its own `assets` row: its own bytes, its own dimensions, its own object in storage. They share a
//! `version_group_id`, and exactly one of them is `is_current`. The alternative — one row whose bytes are replaced
//! — cannot answer "give me what we shipped in March", which is the entire point of keeping versions.
//!
//! ## The unique index makes the swap atomic or impossible
//!
//! `assets_current_idx` is `UNIQUE (version_group_id) WHERE is_current AND deleted_at IS NULL`. So a new current
//! version cannot be inserted while the old one is still current: the database refuses it. That is a feature, and
//! it is why [`add`] demotes and inserts in one transaction — a caller cannot leave a group with two current
//! versions or none, because the index will not have it.
//!
//! ## Listings show current versions; a named asset is whatever was named
//!
//! This is the rule the rest of the codebase has to honour, and until now there was nothing to honour it for.
//! Browse, search and the facet counts describe *the library*, so they show one row per version group. Reading,
//! previewing or downloading an asset **by id** works whatever its version, because asking for a specific version
//! is a legitimate request and the row is still an asset the caller may see.
//!
//! Engagement and comments stay attached to the row they were made on: a rating of March's cut is a rating of
//! March's cut. Whether they should roll up to the group is a real question and a later one — see TASKS.md.

use crate::Error;
use dam_core::policy::AccessPredicate;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// Why a version operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum VersionRefusal {
    /// No such asset, or not one this caller may see.
    #[error("no asset {0}")]
    UnknownAsset(Uuid),

    /// Superseding a version that is not the current one.
    ///
    /// Refused rather than silently re-pointing the group: somebody adding a version to what they believe is the
    /// latest, when it is not, has a stale screen — and quietly making their upload current would discard whatever
    /// they had not seen.
    #[error("asset {0} is not the current version; reload and add the version to the latest one")]
    NotCurrent(Uuid),

    #[error(transparent)]
    Database(#[from] Error),
}

/// One version in a group's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub asset_id: Uuid,
    pub version_no: i32,
    pub is_current: bool,
    pub filename: String,
    pub bytes: i64,
    pub content_hash: String,
    /// The version this one replaced, when it replaced one.
    pub replaces_id: Option<Uuid>,
    pub uploaded_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// The SQL fragment that restricts a listing to current versions.
///
/// A named constant because it has to be applied in several places and missing one is invisible: every asset is
/// current until a second version exists, so a listing without this clause looks perfectly correct right up until
/// somebody adds a version and their library appears to double.
pub const CURRENT_ONLY: &str = " AND assets.is_current";

/// Adds a version to `superseding`'s group, making the new asset current.
///
/// `new_asset_id` is a row this caller has already created — through the ordinary ingest path, so it has its own
/// bytes, probe results and metadata. This joins it to a group rather than creating it, because an upload is an
/// upload: routing it through a second, parallel ingest is how the two diverge.
///
/// Demotes and promotes in one statement pair inside the caller's transaction. See the module docs on why the
/// unique index makes any other arrangement fail rather than corrupt.
pub async fn add(
    conn: &mut sqlx::PgConnection,
    superseding: Uuid,
    new_asset_id: Uuid,
    predicate: &AccessPredicate,
) -> Result<Version, VersionRefusal> {
    // Both rows have to be visible to this caller: the one being superseded, and the one doing it. Checked
    // together, so a caller cannot join an asset they can see to a group they cannot.
    let visible =
        crate::assets::visible_among(&mut *conn, predicate, &[superseding, new_asset_id]).await?;
    if !visible.contains(&superseding) {
        return Err(VersionRefusal::UnknownAsset(superseding));
    }
    if !visible.contains(&new_asset_id) {
        return Err(VersionRefusal::UnknownAsset(new_asset_id));
    }

    let existing: Option<(Uuid, i32, bool)> =
        sqlx::query_as("SELECT version_group_id, version_no, is_current FROM assets WHERE id = $1")
            .bind(superseding)
            .fetch_optional(&mut *conn)
            .await
            .map_err(Error::from)?;
    let Some((group, version_no, is_current)) = existing else {
        return Err(VersionRefusal::UnknownAsset(superseding));
    };
    if !is_current {
        return Err(VersionRefusal::NotCurrent(superseding));
    }

    // Demote first. The unique index means the other order simply fails, which is the database refusing to hold a
    // group with two current versions — but doing it in the order that works keeps the error surface to real
    // problems rather than to sequencing.
    sqlx::query("UPDATE assets SET is_current = false, updated_at = now() WHERE id = $1")
        .bind(superseding)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;

    sqlx::query(
        "UPDATE assets SET version_group_id = $2, version_no = $3, is_current = true, \
         replaces_id = $4, updated_at = now() WHERE id = $1",
    )
    .bind(new_asset_id)
    .bind(group)
    .bind(version_no + 1)
    .bind(superseding)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    history(&mut *conn, new_asset_id, predicate)
        .await?
        .into_iter()
        .find(|version| version.asset_id == new_asset_id)
        .ok_or(VersionRefusal::UnknownAsset(new_asset_id))
}

/// Every version in the group `asset_id` belongs to, newest first.
///
/// Reachable from *any* version, not only the current one: somebody looking at March's cut needs to be able to see
/// what replaced it, and requiring them to find the current version first would be requiring them to already know
/// the answer.
///
/// Soft-deleted versions are excluded — by the access filter, which is the one place that decides what "deleted"
/// removes. A deleted version is not part of a history a person can act on, and listing one would offer a download
/// the delivery chokepoint refuses.
pub async fn history(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    predicate: &AccessPredicate,
) -> Result<Vec<Version>, VersionRefusal> {
    let visible = crate::assets::visible_among(&mut *conn, predicate, &[asset_id]).await?;
    if visible.is_empty() {
        return Err(VersionRefusal::UnknownAsset(asset_id));
    }

    // The group is read from the named asset rather than passed in, so a caller cannot ask for a group they have
    // no member of — which would be a way to enumerate groups.
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT a.id, a.version_no, a.is_current, a.filename, a.bytes, a.content_hash, \
                a.replaces_id, a.uploaded_by, a.created_at \
         FROM assets a \
         WHERE a.version_group_id = (SELECT version_group_id FROM assets WHERE id = ",
    );
    builder.push_bind(asset_id);
    // The predicate again, on every row of the history: a group can in principle contain versions in different
    // asset groups, and a history that showed all of them would leak past the caller's scope through a sibling.
    // No explicit `deleted_at` clause: the access filter below already excludes soft-deleted rows, and duplicating
    // that decision here would give it two homes that can disagree. Mutation testing found the duplicate — removing
    // it changed nothing, because the predicate was doing the work.
    builder.push(") AND a.id IN (SELECT assets.id FROM assets ");
    builder.push("LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ");
    crate::access::push_asset_filter(&mut builder, predicate)?;
    builder.push(") ORDER BY a.version_no DESC");

    type Row = (
        Uuid,
        i32,
        bool,
        String,
        i64,
        String,
        Option<Uuid>,
        Option<Uuid>,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> = builder
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;

    Ok(rows
        .into_iter()
        .map(
            |(
                asset_id,
                version_no,
                is_current,
                filename,
                bytes,
                content_hash,
                replaces_id,
                uploaded_by,
                created_at,
            )| Version {
                asset_id,
                version_no,
                is_current,
                filename,
                bytes,
                content_hash,
                replaces_id,
                uploaded_by,
                created_at,
            },
        )
        .collect())
}

/// Makes an earlier version current again.
///
/// A *promotion*, not a copy: the row that was current is demoted and the named one takes its place, keeping its
/// original `version_no`. So a history reads 1, 2, 3 with 2 current, which is the truth — the alternative,
/// duplicating version 2 as version 4, would claim somebody uploaded something they did not.
pub async fn restore(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    predicate: &AccessPredicate,
) -> Result<Vec<Version>, VersionRefusal> {
    let versions = history(&mut *conn, asset_id, predicate).await?;
    let target = versions
        .iter()
        .find(|version| version.asset_id == asset_id)
        .ok_or(VersionRefusal::UnknownAsset(asset_id))?;
    if target.is_current {
        // Already current. Not an error: a double click on "make this current" is not a fault, and the caller's
        // intent is already satisfied.
        return Ok(versions);
    }

    sqlx::query(
        "UPDATE assets SET is_current = false, updated_at = now() \
         WHERE version_group_id = (SELECT version_group_id FROM assets WHERE id = $1) \
           AND is_current AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    sqlx::query("UPDATE assets SET is_current = true, updated_at = now() WHERE id = $1")
        .bind(asset_id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;

    history(&mut *conn, asset_id, predicate).await
}
