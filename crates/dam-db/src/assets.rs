//! Reading assets under an access predicate.
//!
//! The one rule this module exists to keep: **the predicate goes in the query, and the count comes from the
//! same query as the rows**. §7 gives the reason — pagination counts alone disclose the existence of assets a
//! caller cannot see, and a post-filter returns the correct rows while leaking through `total`. Nothing here
//! takes a page of rows and filters it afterwards.
//!
//! ## Tier is derived here, once
//!
//! `AssetTier::of(class, restore_state)` carries a trap the schema warns about twice: an **expired** restore of
//! an archived object is archived again, not restored. Deriving it server-side means the UI cannot reimplement
//! that rule slightly differently and leave a download button enabled until somebody presses it.

use crate::Error;
use dam_core::policy::AccessPredicate;
use dam_core::{AssetTier, ProvenanceState, RestoreState, RightsState, StorageClass};
use sqlx::{Postgres, QueryBuilder, Row as _};
use uuid::Uuid;

/// The columns a grid cell draws.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub id: Uuid,
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub tier: AssetTier,
    pub rights_state: RightsState,
    pub provenance_state: ProvenanceState,
    pub tag_confidence: Option<f32>,
}

/// One page, with the total the same predicate produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub items: Vec<Summary>,
    pub total: i64,
    pub offset: i64,
}

/// How assets are ordered.
///
/// A closed set rather than a caller-supplied string: an order-by built from a query parameter is an
/// injection hole, and one that is validated against a list is the same list written twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    /// Newest first. What a grid opens on.
    #[default]
    Newest,
    Oldest,
    FilenameAsc,
    FilenameDesc,
    LargestFirst,
}

impl Order {
    /// The SQL fragment, with `assets.id` as the tie-break.
    ///
    /// The tie-break is not decoration: `created_at` is not unique, and an offset walk over a
    /// non-deterministic order silently skips and repeats rows between pages. A virtualised grid scrolling
    /// back over a page it has already seen would show different assets, which reads as data corruption.
    fn fragment(self) -> &'static str {
        match self {
            Self::Newest => " ORDER BY assets.created_at DESC, assets.id DESC",
            Self::Oldest => " ORDER BY assets.created_at ASC, assets.id ASC",
            Self::FilenameAsc => " ORDER BY assets.filename ASC, assets.id ASC",
            Self::FilenameDesc => " ORDER BY assets.filename DESC, assets.id DESC",
            Self::LargestFirst => " ORDER BY assets.bytes DESC, assets.id DESC",
        }
    }
}

/// The largest page a caller can ask for.
///
/// A virtualised grid asks for the window it is about to draw, which is tens of rows. A caller asking for
/// fifty thousand is either mistaken or exporting, and an export is a bulk operation with its own endpoint.
pub const MAX_LIMIT: i64 = 500;

/// One page of assets the caller may see, plus the matching total.
///
/// `total` is counted under the same predicate in the same statement as the rows. Two statements would be
/// two snapshots, and a concurrent upload between them makes `total` disagree with what the page contains —
/// which for a virtualised grid means a scrollbar that does not match its own contents.
pub async fn page<'e, E>(
    executor: E,
    predicate: &AccessPredicate,
    order: Order,
    offset: i64,
    limit: i64,
) -> Result<Page, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let offset = offset.max(0);
    let limit = limit.clamp(1, MAX_LIMIT);

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id, assets.filename, assets.mime, assets.bytes, assets.width, assets.height, \
                assets.rights_state, assets.provenance_state, \
                placement.storage_class, placement.restore_state, \
                count(*) OVER () AS total \
         FROM assets ",
    );
    builder.push(WARMEST_PLACEMENT);
    builder.push(" WHERE ");
    crate::access::push_asset_filter(&mut builder, predicate)?;
    builder.push(order.fragment());
    builder.push(" LIMIT ");
    builder.push_bind(limit);
    builder.push(" OFFSET ");
    builder.push_bind(offset);

    let rows = builder.build().fetch_all(executor).await?;

    // From the window function, so it is the count of what the predicate matched rather than of what this
    // page returned — and from the *same* statement as the rows, because two statements are two snapshots
    // and a concurrent upload between them makes the total disagree with the page it describes.
    let total = rows
        .first()
        .map(|row| row.get::<i64, _>("total"))
        .unwrap_or(0);

    let items = rows
        .iter()
        .map(|row| {
            Ok(Summary {
                id: row.get("id"),
                filename: row.get("filename"),
                mime: row.get("mime"),
                bytes: row.get("bytes"),
                width: row.get("width"),
                height: row.get("height"),
                tier: tier_of(row)?,
                rights_state: parse_rights(&row.get::<String, _>("rights_state"))?,
                provenance_state: parse_provenance(&row.get::<String, _>("provenance_state"))?,
                // Not selected: it comes from the enrichment tables, and a join per row on a grid page is
                // the kind of cost that only shows up at a hundred thousand assets. Populated by 4.x.
                tag_confidence: None,
            })
        })
        .collect::<Result<Vec<Summary>, Error>>()?;

    Ok(Page {
        items,
        total,
        offset,
    })
}

/// The one placement a tier is derived from: the **warmest** present copy.
///
/// A lateral rather than a plain `LEFT JOIN`, and that is a correctness matter rather than a style one. An
/// asset replicated across two pools has two `object_placements` rows — the primary key is
/// `(object_key, pool_id)` — so a plain join returns it twice, which double-counts it in the window and puts
/// it in the grid twice. `LIMIT 1` inside the lateral makes one row per asset structural.
///
/// Warmest, because the tier answers "can this be fetched now" and a Standard copy answers yes however many
/// archived copies sit beside it. Ordering by the class alone would let a Deep Archive replica of a hot
/// object report `Archive` and disable a download that would have worked. `object_key` breaks the remaining
/// tie so the choice is deterministic — an order that varies between statements makes a tier flicker between
/// two page loads with nothing having changed.
const WARMEST_PLACEMENT: &str = "LEFT JOIN LATERAL ( \
        SELECT p.storage_class, p.restore_state \
        FROM object_placements p \
        WHERE p.asset_id = assets.id AND p.state = 'present' \
        ORDER BY CASE p.storage_class \
                     WHEN 'STANDARD' THEN 0 \
                     WHEN 'STANDARD_IA' THEN 1 \
                     WHEN 'ONEZONE_IA' THEN 1 \
                     WHEN 'INTELLIGENT_TIERING' THEN 1 \
                     WHEN 'GLACIER_IR' THEN 1 \
                     ELSE 2 \
                 END, \
                 CASE p.restore_state \
                     WHEN 'available' THEN 0 \
                     WHEN 'ongoing' THEN 1 \
                     WHEN 'requested' THEN 1 \
                     ELSE 2 \
                 END, \
                 p.object_key \
        LIMIT 1 \
    ) placement ON true";

/// The tier for a row carrying `storage_class` and `restore_state`.
///
/// One function, so the list and the detail cannot derive it differently — a badge that changes when a panel
/// opens is a bug nobody reports as one.
fn tier_of(row: &sqlx::postgres::PgRow) -> Result<AssetTier, Error> {
    let class = match row.get::<Option<String>, _>("storage_class") {
        // No placement means the bytes are not in object storage yet — a freshly finalised upload. `Standard`
        // is the honest reading: nothing is archived, so nothing needs a restore.
        None => StorageClass::Standard,
        Some(raw) => raw.parse().map_err(|_| {
            Error::Inconsistent(format!("object_placements.storage_class holds {raw:?}"))
        })?,
    };
    let restore = match row.get::<Option<String>, _>("restore_state") {
        None => RestoreState::None,
        Some(raw) => raw.parse().map_err(|_| {
            Error::Inconsistent(format!("object_placements.restore_state holds {raw:?}"))
        })?,
    };
    Ok(AssetTier::of(class, restore))
}

/// Everything the detail panel needs about one asset.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    pub summary: Summary,
    /// The validated metadata, as stored.
    pub values: serde_json::Value,
    /// Probed technical facts.
    pub technical: serde_json::Value,
    pub duration_ms: Option<i64>,
    pub page_count: Option<i32>,
    pub color_space: Option<String>,
    pub has_alpha: Option<bool>,
    pub content_hash: String,
    pub status: String,
    pub enrichment_state: String,
    pub legal_hold: bool,
    pub release_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub version_no: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// One asset, or `None` when the caller may not see it.
///
/// `None` covers "does not exist" and "exists but is not yours" alike, and the caller answers both with a
/// 404: a 403 on an asset in another group confirms the asset exists, which is the disclosure the group
/// scoping was for.
pub async fn detail<'e, E>(
    executor: E,
    predicate: &AccessPredicate,
    asset_id: Uuid,
) -> Result<Option<Detail>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    // The table is not aliased, because `push_asset_filter` writes `assets.` — an alias here would mean
    // rewriting the renderer's output, and a filter assembled by string surgery is a filter one edit away
    // from not applying at all.
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id, assets.filename, assets.mime, assets.bytes, assets.width, assets.height, \
                assets.rights_state, assets.provenance_state, assets.duration_ms, assets.page_count, \
                assets.color_space, assets.has_alpha, assets.content_hash, assets.status, \
                assets.enrichment_state, assets.legal_hold, assets.release_at, assets.expires_at, \
                assets.version_no, assets.created_at, assets.updated_at, \
                coalesce(m.values, '{}'::jsonb) AS values, \
                coalesce(m.technical, '{}'::jsonb) AS technical, \
                placement.storage_class, placement.restore_state \
         FROM assets \
         LEFT JOIN asset_metadata m ON m.asset_id = assets.id ",
    );
    builder.push(WARMEST_PLACEMENT);
    builder.push(" WHERE assets.id = ");
    builder.push_bind(asset_id);
    builder.push(" AND ");
    // The same predicate the list uses, in the query rather than checked afterwards. A post-check would be
    // correct here — a single row is not a count — but writing it differently from the list is how the two
    // drift, and §12's argument is about exactly that.
    crate::access::push_asset_filter(&mut builder, predicate)?;

    let Some(row) = builder.build().fetch_optional(executor).await? else {
        return Ok(None);
    };
    let tier = tier_of(&row)?;

    Ok(Some(Detail {
        summary: Summary {
            id: row.get("id"),
            filename: row.get("filename"),
            mime: row.get("mime"),
            bytes: row.get("bytes"),
            width: row.get("width"),
            height: row.get("height"),
            tier,
            rights_state: parse_rights(&row.get::<String, _>("rights_state"))?,
            provenance_state: parse_provenance(&row.get::<String, _>("provenance_state"))?,
            tag_confidence: None,
        },
        values: row.get("values"),
        technical: row.get("technical"),
        duration_ms: row.get("duration_ms"),
        page_count: row.get("page_count"),
        color_space: row.get("color_space"),
        has_alpha: row.get("has_alpha"),
        content_hash: row.get("content_hash"),
        status: row.get("status"),
        enrichment_state: row.get("enrichment_state"),
        legal_hold: row.get("legal_hold"),
        release_at: row.get("release_at"),
        expires_at: row.get("expires_at"),
        version_no: row.get("version_no"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

/// The ids from `candidates` the caller may see, in the order given.
///
/// The hydration step for a Tantivy search: the index ranks, Postgres authorises. The order is preserved
/// because it *is* the ranking — sorting by anything else here throws away the work the ranker did.
///
/// A stale index is permissive (see `dam_search::document`), so an id the index returned may name an asset
/// the caller cannot see. This is what makes that harmless.
pub async fn visible_among<'e, E>(
    executor: E,
    predicate: &AccessPredicate,
    candidates: &[Uuid],
) -> Result<Vec<Uuid>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT assets.id FROM assets WHERE assets.id = ANY(");
    builder.push_bind(candidates.to_vec());
    builder.push(") AND ");
    crate::access::push_asset_filter(&mut builder, predicate)?;

    let permitted: std::collections::HashSet<Uuid> = builder
        .build_query_scalar::<Uuid>()
        .fetch_all(executor)
        .await?
        .into_iter()
        .collect();

    Ok(candidates
        .iter()
        .copied()
        .filter(|id| permitted.contains(id))
        .collect())
}

fn parse_rights(raw: &str) -> Result<RightsState, Error> {
    raw.parse()
        .map_err(|_| Error::Inconsistent(format!("assets.rights_state holds {raw:?}")))
}

fn parse_provenance(raw: &str) -> Result<ProvenanceState, Error> {
    raw.parse()
        .map_err(|_| Error::Inconsistent(format!("assets.provenance_state holds {raw:?}")))
}
