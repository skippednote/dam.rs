//! Insights over HTTP (M6c).
//!
//! ## One request, because it is one screen
//!
//! The chart, the two lists, the storage breakdown and the contributors arrive together, exactly as the
//! dashboard's three parts do. Five endpoints would be five round trips for a page that cannot render usefully
//! without all of them, and would let the chart disagree with the list beneath it.
//!
//! ## `Read`, not `Manage`
//!
//! Every number here is already narrowed to what this caller can see — see `dam_db::insights` — so it discloses
//! nothing they could not count for themselves off the grid. Gating it behind `Manage` would mean a curator
//! cannot see how their own library is used, which is the opposite of what an insights screen is for.
//!
//! ## The export is the same query, not a second one
//!
//! Each CSV is one of the reports on the screen, produced by the same function with the same predicate. A
//! separate export query is how a file comes to disagree with the page that offered it.
//!
//! ## What is deliberately absent
//!
//! **A library-wide total.** §7 says a count is a disclosure; "3,000 downloads, of which you may see 12" tells a
//! scoped reader exactly how much they cannot reach.
//!
//! **Per-person download counts.** The rights ledger answers "who took this" on the asset itself, where the
//! question belongs. The same number on a leaderboard is a surveillance feature, and these counts are scoped to
//! the reader anyway, so it would not even be an accurate one.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// How many of each list the screen draws.
const LIST_ROWS: i64 = 20;
/// How many rows an export carries. More than the screen, because a spreadsheet is where somebody works
/// through a long list — bounded by `insights::MAX_ROWS` regardless.
const EXPORT_ROWS: i64 = dam_db::insights::MAX_ROWS;
const DEFAULT_DAYS: i64 = 30;

pub struct InsightsState {
    pub global: PgPool,
}

impl std::fmt::Debug for InsightsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InsightsState").finish_non_exhaustive()
    }
}

pub fn router(state: InsightsState) -> Router {
    Router::new()
        .route("/insights", get(insights))
        .route("/insights/export.csv", get(export))
        .with_state(Arc::new(state))
}

/// One day on the chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DayView {
    pub day: chrono::NaiveDate,
    pub uploads: i64,
    /// From the rights ledger, so a download taken through a share link is counted. The activity feed cannot
    /// hold those — its actor is an identity and a share token is not one.
    pub downloads: i64,
    pub edits: i64,
    pub comments: i64,
    pub shares: i64,
}

/// An asset with a number against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AssetCountView {
    pub asset_id: Uuid,
    pub filename: String,
    pub mime: String,
    pub count: i64,
    /// Absent in the never-downloaded list, where there is no last time by definition.
    pub last_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// What the library holds, by media class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ClassView {
    /// `image`, `video`, `audio`, `document` or `other`.
    pub class: String,
    pub assets: i64,
    pub bytes: i64,
}

/// A person and what they did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ContributorView {
    pub person: crate::comments::PersonView,
    pub uploads: i64,
    pub edits: i64,
    pub comments: i64,
}

/// Everything the Insights screen draws.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Insights {
    /// The window these numbers cover, in days, after clamping. Echoed so a screen can say what it is showing
    /// rather than what was asked for — a request for ten years comes back as a year.
    pub days: i64,
    pub series: Vec<DayView>,
    pub most_downloaded: Vec<AssetCountView>,
    /// Never downloaded *ever*, oldest first — not "not in this window".
    pub never_downloaded: Vec<AssetCountView>,
    /// How many there are altogether, of which `never_downloaded` is the oldest few.
    ///
    /// Sent because the list is capped and this one's cap misleads: twenty rows of assets nobody has ever
    /// taken reads as "we have twenty unused assets", and on a real library that was twenty of a far larger
    /// number. A most-downloaded top-20 explains its own cap; this does not.
    pub never_downloaded_total: i64,
    pub by_class: Vec<ClassView>,
    pub contributors: Vec<ContributorView>,
}

#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
pub struct Window {
    /// Days to cover, ending today. Clamped to `[1, 366]`.
    #[serde(default = "default_days")]
    pub days: i64,
}

const fn default_days() -> i64 {
    DEFAULT_DAYS
}

#[utoipa::path(
    get,
    path = "/insights",
    params(Window),
    responses(
        (status = 200, body = Insights),
        (status = 403, description = "The credential holds no read scope"),
    ),
    tag = "insights",
)]
pub async fn insights(
    State(state): State<Arc<InsightsState>>,
    headers: HeaderMap,
    Query(window): Query<Window>,
) -> Result<Json<Insights>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let days = window.days.clamp(1, dam_db::insights::MAX_DAYS);

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // One transaction for all five, so the chart describes the same instant as the lists under it.
    let series = dam_db::insights::series(conn.executor(), &caller.predicate, days).await?;
    let top =
        dam_db::insights::most_downloaded(conn.executor(), &caller.predicate, days, LIST_ROWS)
            .await?;
    let unused =
        dam_db::insights::never_downloaded(conn.executor(), &caller.predicate, LIST_ROWS).await?;
    let unused_total =
        dam_db::insights::never_downloaded_count(conn.executor(), &caller.predicate).await?;
    let classes = dam_db::insights::by_class(conn.executor(), &caller.predicate).await?;
    let people =
        dam_db::insights::contributors(conn.executor(), &caller.predicate, days, LIST_ROWS).await?;
    conn.commit().await?;

    Ok(Json(Insights {
        days,
        series: series.into_iter().map(day).collect(),
        most_downloaded: top.into_iter().map(asset_count).collect(),
        never_downloaded: unused.into_iter().map(asset_count).collect(),
        never_downloaded_total: unused_total,
        by_class: classes
            .into_iter()
            .map(|one| ClassView {
                class: one.class,
                assets: one.assets,
                bytes: one.bytes,
            })
            .collect(),
        contributors: named(&state, people).await?,
    }))
}

/// Which report an export is of.
#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
pub struct ExportParams {
    /// `activity`, `most-downloaded`, `never-downloaded`, `storage` or `contributors`.
    pub report: Report,
    #[serde(default = "default_days")]
    pub days: i64,
}

/// The reports that can be exported.
///
/// A closed set rather than a free string: the name selects a query, and a name this build does not know has to
/// be a refusal that lists the ones it does rather than an empty file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Report {
    Activity,
    MostDownloaded,
    NeverDownloaded,
    Storage,
    Contributors,
}

impl Report {
    const fn filename(self) -> &'static str {
        match self {
            Self::Activity => "activity.csv",
            Self::MostDownloaded => "most-downloaded.csv",
            Self::NeverDownloaded => "never-downloaded.csv",
            Self::Storage => "storage-by-class.csv",
            Self::Contributors => "contributors.csv",
        }
    }
}

#[utoipa::path(
    get,
    path = "/insights/export.csv",
    params(ExportParams),
    responses(
        (status = 200, description = "text/csv", content_type = "text/csv"),
        (status = 422, description = "Not a report this build knows"),
    ),
    tag = "insights",
)]
pub async fn export(
    State(state): State<Arc<InsightsState>>,
    headers: HeaderMap,
    Query(params): Query<ExportParams>,
) -> Result<([(axum::http::HeaderName, String); 2], String), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let days = params.days.clamp(1, dam_db::insights::MAX_DAYS);

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // The same functions the screen calls, with the same predicate. A second query written for the export is
    // how a file comes to disagree with the page that offered it.
    let body = match params.report {
        Report::Activity => {
            let rows = dam_db::insights::series(conn.executor(), &caller.predicate, days).await?;
            let mut csv = String::from("day,uploads,downloads,edits,comments,shares\n");
            for row in rows {
                csv.push_str(&format!(
                    "{},{},{},{},{},{}\n",
                    row.day, row.uploads, row.downloads, row.edits, row.comments, row.shares
                ));
            }
            csv
        }
        Report::MostDownloaded => {
            let rows = dam_db::insights::most_downloaded(
                conn.executor(),
                &caller.predicate,
                days,
                EXPORT_ROWS,
            )
            .await?;
            asset_csv(&rows, true)
        }
        Report::NeverDownloaded => {
            let rows =
                dam_db::insights::never_downloaded(conn.executor(), &caller.predicate, EXPORT_ROWS)
                    .await?;
            asset_csv(&rows, false)
        }
        Report::Storage => {
            let rows = dam_db::insights::by_class(conn.executor(), &caller.predicate).await?;
            let mut csv = String::from("class,assets,bytes\n");
            for row in rows {
                csv.push_str(&format!("{},{},{}\n", row.class, row.assets, row.bytes));
            }
            csv
        }
        Report::Contributors => {
            let rows = dam_db::insights::contributors(
                conn.executor(),
                &caller.predicate,
                days,
                EXPORT_ROWS,
            )
            .await?;
            conn.commit().await?;
            // Names resolved from the control plane, which is a different pool — so this arm commits first
            // rather than holding a tenant transaction open across a second connection's round trip.
            let people = named(&state, rows).await?;
            let mut csv = String::from("person,email,uploads,edits,comments\n");
            for row in people {
                csv.push_str(&format!(
                    "{},{},{},{},{}\n",
                    crate::csv_export::cell(&row.person.name),
                    crate::csv_export::cell(&row.person.email),
                    row.uploads,
                    row.edits,
                    row.comments
                ));
            }
            return Ok((crate::csv_export::headers(params.report.filename()), csv));
        }
    };
    conn.commit().await?;

    Ok((crate::csv_export::headers(params.report.filename()), body))
}

/// Asset rows as CSV. `taken` decides whether the count and last-taken columns are meaningful.
fn asset_csv(rows: &[dam_db::insights::AssetCount], taken: bool) -> String {
    let mut csv = String::from(if taken {
        "filename,mime,downloads,last_downloaded_at,asset_id\n"
    } else {
        "filename,mime,asset_id\n"
    });
    for row in rows {
        if taken {
            csv.push_str(&format!(
                "{},{},{},{},{}\n",
                crate::csv_export::cell(&row.filename),
                crate::csv_export::cell(&row.mime),
                row.count,
                row.last_at.map(|at| at.to_rfc3339()).unwrap_or_default(),
                row.asset_id
            ));
        } else {
            csv.push_str(&format!(
                "{},{},{}\n",
                crate::csv_export::cell(&row.filename),
                crate::csv_export::cell(&row.mime),
                row.asset_id
            ));
        }
    }
    csv
}

const fn day(row: dam_db::insights::Day) -> DayView {
    DayView {
        day: row.day,
        uploads: row.uploads,
        downloads: row.downloads,
        edits: row.edits,
        comments: row.comments,
        shares: row.shares,
    }
}

fn asset_count(row: dam_db::insights::AssetCount) -> AssetCountView {
    AssetCountView {
        asset_id: row.asset_id,
        filename: row.filename,
        mime: row.mime,
        count: row.count,
        last_at: row.last_at,
    }
}

/// Resolves contributor identities to names, in one lookup.
///
/// A contributor whose identity no longer resolves is dropped rather than shown as a uuid. The other lists here
/// are about assets and a missing name is cosmetic; this list *is* the names, and a row reading
/// `a1b2c3d4-… uploaded 40` is worse than one fewer row.
async fn named(
    state: &InsightsState,
    rows: Vec<dam_db::insights::Contributor>,
) -> Result<Vec<ContributorView>, Failure> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.identity_id).collect();
    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            people
                .iter()
                .find(|person| person.id == row.identity_id)
                .map(|person| ContributorView {
                    person: crate::comments::PersonView {
                        id: person.id,
                        name: person.display_name.clone(),
                        email: person.email.clone(),
                    },
                    uploads: row.uploads,
                    edits: row.edits,
                    comments: row.comments,
                })
        })
        .collect())
}
