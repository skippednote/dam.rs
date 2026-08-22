//! What an asset was taken for, and by whom (Q.12).
//!
//! `rights_usage` has been the consumption ledger since migration 0005, and until now only connector reports
//! and manual entry wrote to it. This is the download half — the half its own comment named, and the half that
//! turns `license_scopes.max_downloads` from a decorative number into a cap that refuses.
//!
//! ## Recorded before the URL is minted
//!
//! An unrecorded download makes a cap under-count, which permits more than the licence allows. A recorded
//! download that then failed to mint over-counts, which permits fewer. The first is a licence breach and the
//! second is an inconvenience, so the caller records first — see the API's own note.
//!
//! ## Attributed to the scope that permitted it
//!
//! `license_scope_id` is what the evaluator sums against a cap, so a row with a null scope counts toward
//! nothing. The scope comes from the evaluation itself ([`dam_core::rights_eval::Evaluation::consuming_scope`])
//! rather than being re-derived here: two answers to "which licence permitted this" is exactly the divergence
//! §12 is about, applied to rights.
//!
//! ## Read back per asset, under the caller's predicate
//!
//! "Who has taken this, and what for" is part of understanding an asset's rights position — a person deciding
//! whether they may use it benefits from knowing it went out under a print licence last month. Scoped like
//! everything else: the ledger is read through the asset filter, so a row cannot disclose an asset.

use crate::Error;
use dam_core::policy::AccessPredicate;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// One line of the ledger, as a person reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub channel: Option<String>,
    pub territory: Option<String>,
    /// Whether the person named the use, or the API defaulted it. See migration 0024.
    pub declared: bool,
    pub downloads: i64,
    pub recorded_by: Option<Uuid>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

/// A download to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDownload {
    pub asset_id: Uuid,
    pub channel: String,
    pub territory: String,
    /// The scope the evaluation says this consumes against. `None` when nothing capped or covered it — the row
    /// is still written, because the download is a fact, but it advances no cap.
    pub license_scope_id: Option<Uuid>,
    /// True when the request named the use rather than accepting a default.
    pub declared: bool,
    pub recorded_by: Option<Uuid>,
}

/// Records one download in the ledger.
///
/// One row per download rather than an incremented counter, which is what makes the ledger auditable: a total
/// can be recomputed and a mistake can be corrected by appending, neither of which is true of a counter.
pub async fn record_download(
    conn: &mut sqlx::PgConnection,
    new: &NewDownload,
) -> Result<Uuid, Error> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO rights_usage \
         (id, asset_id, license_scope_id, channel, territory, downloads, source, declared, recorded_by) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, 1, 'download', $5, $6) RETURNING id",
    )
    .bind(new.asset_id)
    .bind(new.license_scope_id)
    .bind(&new.channel)
    .bind(&new.territory)
    .bind(new.declared)
    .bind(new.recorded_by)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// What one asset has been taken for, newest first.
///
/// Downloads only. A connector's usage report and a manual print-run entry are in the same table and answer a
/// different question — "where is this in use" rather than "who took it and what did they say it was for" —
/// and mixing them would put rows with no person and no declaration in a list about people's stated intentions.
pub async fn for_asset(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    predicate: &AccessPredicate,
    limit: i64,
) -> Result<Vec<Declaration>, Error> {
    // From the visible set, so an asset the caller cannot see has no ledger rather than an empty one.
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::access::push_asset_filter(&mut builder, predicate)?;
    builder.push(
        ") SELECT u.id, u.asset_id, u.channel, u.territory, u.declared, u.downloads, u.recorded_by, \
                  u.recorded_at \
          FROM rights_usage u JOIN visible ON visible.id = u.asset_id \
          WHERE u.asset_id = ",
    );
    builder.push_bind(asset_id);
    builder.push(" AND u.source = 'download' ORDER BY u.recorded_at DESC, u.id DESC LIMIT ");
    builder.push_bind(limit.clamp(1, MAX_ROWS));

    type Row = (
        Uuid,
        Uuid,
        Option<String>,
        Option<String>,
        bool,
        i64,
        Option<Uuid>,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> = builder.build_query_as().fetch_all(&mut *conn).await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, asset_id, channel, territory, declared, downloads, recorded_by, recorded_at)| {
                Declaration {
                    id,
                    asset_id,
                    channel,
                    territory,
                    declared,
                    downloads,
                    recorded_by,
                    recorded_at,
                }
            },
        )
        .collect())
}

/// The channels and territories this tenant's licences actually reference.
///
/// The vocabulary a person picks from, derived rather than configured. Every option here is one that can change
/// a rights answer — which is the useful property: offering "social" when no licence mentions it invites
/// somebody to declare a use nothing evaluates differently, and offering nothing at all makes the question
/// unanswerable.
///
/// Includes exclusions as well as inclusions: "worldwide except China" means `CN` is a territory somebody may
/// want to declare, and the honest answer to declaring it is a refusal with a reason rather than an absence
/// from the list.
///
/// A tenant that wants options no licence mentions needs a declared vocabulary of its own, which is a table and
/// a screen; recorded in TASKS.md rather than guessed at here.
pub async fn vocabulary(
    conn: &mut sqlx::PgConnection,
) -> Result<(Vec<String>, Vec<String>), Error> {
    let channels: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT c FROM license_scopes, unnest(channels || excluded_channels) AS c \
         WHERE c <> '' ORDER BY c",
    )
    .fetch_all(&mut *conn)
    .await?;
    let territories: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT t FROM license_scopes, unnest(territories || excluded_territories) AS t \
         WHERE t <> '' ORDER BY t",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok((channels, territories))
}

/// How much of one asset's ledger a single read returns.
const MAX_ROWS: i64 = 200;
