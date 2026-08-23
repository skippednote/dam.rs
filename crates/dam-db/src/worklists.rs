//! The admin worklists: the queries that answer "what needs attention" (Q.20, Q.2c·3).
//!
//! Every one of these is a question about data damrs already holds, which is why this file is SQL and not a
//! new table. Nothing is recorded, nothing is enqueued, and there is no state to fall out of date: a worklist
//! is a view of the library's own gaps, so an asset leaves one the moment somebody fixes the thing.
//!
//! ## Why not the query IR
//!
//! Six of the eight could be expressed as search clauses, and it would be the wrong place for them. "A
//! required field of this asset's *resolved* metadata type is absent" is a three-way join with a fallback
//! chain — the asset's type, else the tenant default, else every field — and nobody types that into a search
//! box. Putting it in the IR would also put it in saved searches, asset-group predicates and the MCP surface,
//! where its cost and its meaning are much harder to reason about. A worklist is administration; it stays here.
//!
//! ## Every count runs through the caller's predicate
//!
//! A worklist is a to-do list, and a to-do list that counts work the reader cannot see is worse than none: it
//! sends somebody looking for an asset that 404s. So the counts are per-caller, which also means two people
//! legitimately see different numbers — and the screen says so rather than presenting them as the library's.
//!
//! ## One statement for the counts
//!
//! Eight scalar subqueries in one round trip rather than eight queries. The numbers then describe the same
//! instant, which matters here more than it looks: a page showing "12 uncategorised" beside "12 missing
//! metadata" invites the reading that they are the same twelve, and two snapshots would make that unknowable.

use crate::Error;
use dam_core::policy::AccessPredicate;
use sqlx::{Postgres, QueryBuilder, Row};

/// Every worklist is about assets in active circulation.
///
/// An archived asset is one somebody deliberately took out of circulation, so asking anybody to file it, caption
/// it or render a thumbnail for it is busywork — and a worklist full of busywork is a worklist nobody opens. The
/// API suite caught this: archiving the expired asset took it off the exposure list and left it on the filing
/// list, which is two answers to "has this been dealt with".
///
/// Applied once here rather than repeated in each clause, so a ninth worklist cannot forget it. Deleted rows are
/// already gone — the access filter drops them.
const ACTIVE_ONLY: &str = " AND assets.status = 'active'";

/// How far ahead the *scheduled* expiry list looks.
///
/// Thirty days, and deliberately a different mechanism from the licence one below. `assets.expires_at` is a
/// retention date somebody set on the asset — "take this down after the campaign" — and thirty days is enough
/// notice to decide whether to extend it.
///
/// Licence expiry is **not** this. A licence's notice window is `licenses.renewal_notice_days`, per licence,
/// because a contract that takes ninety days to renew needs ninety days' warning; `dam_core::rights_eval`
/// applies it and stores the verdict in `assets.rights_state`. So the two expiry worklists below read two
/// different columns on purpose, and both say which in their explanation.
pub const EXPIRY_HORIZON_DAYS: i64 = 30;

/// The worklists, in the order a screen should show them.
///
/// Ordered by how actionable each one is rather than by how many rows it has: an expired licence is a legal
/// exposure and an unenriched asset is a nicety, and a list sorted by count would put them the other way round
/// on most libraries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Worklist {
    /// `expires_at` has passed and the asset is still active — so it is still being served.
    Expired,
    /// The rights evaluation says a licence term is inside its own notice window.
    ///
    /// Reads `assets.rights_state`, which is the column the grid badge renders. That is the whole point of
    /// reading it rather than computing licence dates here: the first version of this module asked
    /// `assets.expires_at` instead and reported nothing while the grid showed three assets badged
    /// "Expiring" — one question with two answers, and the wrong one was the one about a contract.
    RightsExpiring,
    /// The rights evaluation says the intended use is not permitted.
    ///
    /// Narrower and more actionable than [`Self::NoLicence`]: paperwork exists and says no, rather than not
    /// existing at all. Same column as the badge, for the same reason.
    RightsDenied,
    /// `expires_at` falls inside [`EXPIRY_HORIZON_DAYS`].
    ExpiringSoon,
    /// No licence at all. Not "rights unknown", which is a computed state: this is the absence of paperwork.
    NoLicence,
    /// A required field of the asset's resolved metadata type is empty.
    MissingRequired,
    /// In no category. The worklist Q.2c·3 asked for.
    Uncategorised,
    /// `release_at` is in the future, so the asset is embargoed and invisible to most readers.
    Embargoed,
    /// Enrichment failed. Distinct from `pending`: pending is a queue, failed is a thing that stopped.
    EnrichmentFailed,
    /// No thumbnail derivative, so this asset is a grey placeholder in every grid it appears in.
    NoThumbnail,
}

impl Worklist {
    /// Every worklist, in display order.
    ///
    /// Deliberately *not* including `enrichment_state = 'needs_review'`, which is the biggest queue on an
    /// AI-enabled tenant. It has the review screen, where the work can actually be done; a worklist row would
    /// be a second front door to the same queue that cannot act on it, and two counts of one thing eventually
    /// disagree.
    #[must_use]
    pub const fn all() -> [Self; 10] {
        [
            Self::Expired,
            Self::RightsExpiring,
            Self::RightsDenied,
            Self::ExpiringSoon,
            Self::NoLicence,
            Self::MissingRequired,
            Self::Uncategorised,
            Self::Embargoed,
            Self::EnrichmentFailed,
            Self::NoThumbnail,
        ]
    }

    /// The stable machine name, used in the URL.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::RightsExpiring => "rights-expiring",
            Self::RightsDenied => "rights-denied",
            Self::ExpiringSoon => "expiring-soon",
            Self::NoLicence => "no-licence",
            Self::MissingRequired => "missing-required",
            Self::Uncategorised => "uncategorised",
            Self::Embargoed => "embargoed",
            Self::EnrichmentFailed => "enrichment-failed",
            Self::NoThumbnail => "no-thumbnail",
        }
    }

    /// Parses a key from a URL. `None` rather than a default, so a typo is a 404 and not a different list.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::all().into_iter().find(|one| one.key() == key)
    }

    /// The SQL condition, as a clause that can be `AND`ed onto an access filter.
    ///
    /// Every branch is a literal: nothing here interpolates a caller's input, which is what lets
    /// [`crate::assets::page_narrowed`] push the string verbatim.
    const fn clause(self) -> &'static str {
        match self {
            // Still active is the point of this one — an expired asset that was archived has been dealt
            // with — and it comes from [`ACTIVE_ONLY`], which every worklist gets. Stated here because
            // "expired but archived" is the case somebody will come looking for.
            Self::Expired => "(assets.expires_at IS NOT NULL AND assets.expires_at <= now())",
            // The stored verdict, not a recomputation. `rights_eval` needs the licences, the releases, the
            // usage and each licence's own notice window; duplicating that in SQL would be a second rights
            // engine that disagrees with the first one the day either changes.
            Self::RightsExpiring => "(assets.rights_state = 'expiring')",
            Self::RightsDenied => "(assets.rights_state = 'denied')",
            Self::ExpiringSoon => {
                "(assets.expires_at IS NOT NULL AND assets.expires_at > now() \
                  AND assets.expires_at <= now() + interval '30 days')"
            }
            Self::NoLicence => {
                "NOT EXISTS (SELECT 1 FROM asset_licenses al WHERE al.asset_id = assets.id)"
            }
            // The fallback chain in SQL: the asset's own type, else the tenant default, else — when the tenant
            // has neither — every required field, because that is what `metadata_types` says resolution does.
            // Written as an EXISTS over the required fields rather than a join, so an asset with four missing
            // fields is one row in the worklist and not four.
            Self::MissingRequired => {
                "EXISTS ( \
                   SELECT 1 FROM field_defs f \
                   WHERE f.required \
                     AND ( \
                       coalesce(assets.metadata_type_id, \
                                (SELECT d.id FROM metadata_types d WHERE d.is_default)) IS NULL \
                       OR EXISTS ( \
                         SELECT 1 FROM metadata_type_fields mtf \
                         WHERE mtf.metadata_type_id = coalesce(assets.metadata_type_id, \
                                   (SELECT d.id FROM metadata_types d WHERE d.is_default)) \
                           AND mtf.field_key = f.key) \
                     ) \
                     AND NOT EXISTS ( \
                       SELECT 1 FROM asset_metadata m \
                       WHERE m.asset_id = assets.id \
                         AND m.values ? f.key \
                         AND m.values -> f.key NOT IN ('null'::jsonb, '\"\"'::jsonb, '[]'::jsonb)) \
                 )"
            }
            // Three conditions, and each one is load-bearing. **Confirmed** tags only, because a suggested
            // tag is a machine's guess awaiting review and an asset filed by nobody is exactly what this list
            // is for. **Category** taxonomies only, because a vocabulary is a label set for tagging rather
            // than a filing tree — `categories::uncategorised` refuses a non-tree taxonomy for the same
            // reason. Any category tree counts: one is filing, and demanding all of them would put an asset
            // on this list forever on a tenant with four trees.
            Self::Uncategorised => {
                "NOT EXISTS (SELECT 1 FROM asset_tags at \
                             JOIN taxonomy_terms t ON t.id = at.term_id \
                             JOIN taxonomies tx ON tx.id = t.taxonomy_id \
                             WHERE at.asset_id = assets.id \
                               AND at.state = 'confirmed' \
                               AND tx.kind = 'category')"
            }
            Self::Embargoed => "(assets.release_at IS NOT NULL AND assets.release_at > now())",
            Self::EnrichmentFailed => "(assets.enrichment_state = 'failed')",
            // By role rather than by op hash: a *stale* thumbnail from an older profile still draws, so
            // demanding the current recipe would fill this list with assets nobody needs to touch. What is
            // being surfaced is the asset that has none at all — the grey square in the grid.
            Self::NoThumbnail => {
                "NOT EXISTS (SELECT 1 FROM derivatives d \
                             WHERE d.asset_id = assets.id AND d.role = 'thumbnail')"
            }
        }
    }
}

/// Every worklist's size for this caller, in one statement.
pub async fn counts<'e, E>(
    executor: E,
    predicate: &AccessPredicate,
) -> Result<Vec<(Worklist, i64)>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("SELECT ");
    for (index, worklist) in Worklist::all().into_iter().enumerate() {
        if index > 0 {
            builder.push(", ");
        }
        builder.push("(SELECT count(*) FROM assets ");
        builder.push(" WHERE ");
        crate::access::push_asset_filter(&mut builder, predicate)?;
        // The same rows the grid calls the library: current versions, and nothing that is paperwork hanging
        // off something else. Without it a worklist counts three versions of one asset as three jobs, and a
        // release form with no category as a category to fill in.
        builder.push(crate::versions::LIBRARY_ROWS);
        builder.push(ACTIVE_ONLY);
        builder.push(" AND ");
        builder.push(worklist.clause());
        builder.push(") AS c");
        builder.push(index.to_string());
    }

    let row = builder.build().fetch_one(executor).await?;
    Ok(Worklist::all()
        .into_iter()
        .enumerate()
        .map(|(index, worklist)| {
            let count: i64 = row.get(format!("c{index}").as_str());
            (worklist, count)
        })
        .collect())
}

/// A page of one worklist, in the same row shape as the grid.
pub async fn page<'e, E>(
    executor: E,
    predicate: &AccessPredicate,
    worklist: Worklist,
    order: crate::assets::Order,
    offset: i64,
    limit: i64,
) -> Result<crate::assets::Page, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    // The clause and the active-only rule together, so a page shows exactly the rows its count counted.
    let narrowing = format!("{}{}", worklist.clause(), ACTIVE_ONLY);
    crate::assets::page_narrowed(executor, predicate, Some(&narrowing), order, offset, limit).await
}
