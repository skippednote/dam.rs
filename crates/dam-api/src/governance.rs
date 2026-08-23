//! Legal hold, and the tamper-evident record of it (G10, §Q).
//!
//! ## Two absences that only make sense together
//!
//! `assets.legal_hold` has been read since migration 0001 and written by nothing. The rights gate refuses to
//! deliver a held asset, the tiering scan refuses to move one, the purge view excludes one — and there has
//! never been a way to put a hold on. `audit_log` has carried a hash chain since 0007 with nothing writing to
//! it. Shipping either alone would be half a feature: a hold nobody can prove was placed, or a chain with
//! nothing worth chaining.
//!
//! ## The reason is not a column
//!
//! There is no `legal_hold_reason` on `assets`, and this endpoint does not add one. A hold's reason is never
//! wanted on its own — the question is always *who* placed it, *when*, and why, and a column answers only the
//! last third of that while overwriting itself on every change. The audit entry is the whole answer, and
//! `GET /audit?target_kind=asset&target_id=…` is how a screen asks for it.
//!
//! A reason is required in both directions, and lifting is the direction that needs it more. "Someone lifted
//! the litigation hold on this asset" with no sentence attached is the audit row that makes an auditor
//! distrust the rest of the log.
//!
//! ## A no-op does not get an entry
//!
//! Placing a hold that is already placed changes nothing, and writing an entry for it would fill the record
//! with rows that describe no decision. A log where most entries are re-assertions is a log nobody reads to
//! the end, so the handler compares first and reports `changed: false`.
//!
//! ## Exporting is a POST because it writes
//!
//! An export appends the `audit.exported` entry that says a copy was taken. Behind GET that entry would be
//! written by a link preview, a browser prefetch and every uptime probe — which is both noise and a false
//! trail of people who never asked for the data. Verification, which reads only, stays a GET.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::assets;
use dam_db::audit::{self, ActorKind, NewEntry};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

/// How many entries one page of the log carries.
const PAGE_ROWS: i64 = 50;
/// The default size of an extract, well under `audit::EXPORT_LIMIT`.
const EXPORT_ROWS: i64 = 1_000;

pub struct GovernanceState {
    pub global: PgPool,
}

impl std::fmt::Debug for GovernanceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GovernanceState").finish_non_exhaustive()
    }
}

pub fn router(state: GovernanceState) -> Router {
    Router::new()
        .route("/assets/{asset_id}/legal-hold", put(set_legal_hold))
        .route("/audit", get(list))
        .route("/audit/verify", get(verify))
        .route("/audit/export", post(export))
        .with_state(Arc::new(state))
}

/// Place or lift a hold.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LegalHoldBody {
    /// `true` places the hold, `false` lifts it. Not a verb in the path, because the two directions have the
    /// same permission, the same audit shape and the same idempotency — and splitting them into
    /// `/hold` and `/release` would be two handlers that had to stay identical.
    pub held: bool,
    /// Why. Required in both directions; see the module note.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LegalHoldView {
    pub asset_id: Uuid,
    pub held: bool,
    /// `false` when the asset was already in the requested state, in which case no audit entry was written.
    pub changed: bool,
    /// The sequence number of the entry recording this change, when there was one. Returned so a caller can
    /// cite it — an operator who has just placed a hold under instruction needs the reference, and going to
    /// look for it afterwards is how the wrong row gets cited.
    pub audit_seq: Option<i64>,
}

/// Places or lifts a legal hold.
#[utoipa::path(
    put,
    path = "/assets/{asset_id}/legal-hold",
    params(("asset_id" = Uuid, Path, description = "The asset")),
    request_body = LegalHoldBody,
    responses(
        (status = 200, body = LegalHoldView),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "No such asset, or not one this caller may see"),
        (status = 422, description = "No reason given"),
    ),
    tag = "governance",
)]
pub async fn set_legal_hold(
    State(state): State<Arc<GovernanceState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(body): Json<LegalHoldBody>,
) -> Result<Json<LegalHoldView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(Failure::Unprocessable(
            "a legal hold needs a reason, in both directions".to_owned(),
        ));
    }

    // One transaction over the read, the write and the audit entry. The entry has to commit or roll back with
    // the change it describes: a hold placed with no record is unprovable, and a record of a hold that was
    // not placed is worse.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // The predicate first, and 404 rather than 403 for an asset this caller cannot see — the same rule the
    // rest of the asset surface follows, because the gap between those two answers is an existence oracle.
    let existing = assets::detail(conn.executor(), &caller.predicate, asset_id)
        .await?
        .ok_or(Failure::NotFound)?;

    if existing.legal_hold == body.held {
        conn.commit().await?;
        return Ok(Json(LegalHoldView {
            asset_id,
            held: body.held,
            changed: false,
            audit_seq: None,
        }));
    }

    sqlx::query("UPDATE assets SET legal_hold = $1 WHERE id = $2")
        .bind(body.held)
        .bind(asset_id)
        .execute(conn.executor())
        .await
        .map_err(dam_db::Error::from)?;

    let action = if body.held {
        audit::Action::LegalHoldPlaced
    } else {
        audit::Action::LegalHoldLifted
    };
    let entry = NewEntry {
        action,
        // A key with no identity behind it has no grants, so `authorize` has already refused one — but the
        // audit row must name a person or say that it cannot, never guess.
        actor_kind: if caller.identity_id.is_some() {
            ActorKind::User
        } else {
            ActorKind::ApiKey
        },
        actor_id: caller.identity_id,
        target_kind: "asset".to_owned(),
        target_id: Some(asset_id.to_string()),
        payload: serde_json::json!({
            "reason": reason,
            "filename": existing.summary.filename,
            "api_key_id": caller.api_key_id,
        }),
    };
    let recorded = audit::record(conn.executor(), entry).await?;

    conn.commit().await?;
    Ok(Json(LegalHoldView {
        asset_id,
        held: body.held,
        changed: true,
        audit_seq: Some(recorded.seq),
    }))
}

/// One entry, as a screen and an auditor both read it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EntryView {
    pub seq: i64,
    /// The timestamp in the exact form the hash covers — six fractional digits, always.
    ///
    /// A `DateTime` here would serialise through chrono's `AutoSi`, which drops the fraction when the
    /// microseconds are zero. Roughly one entry in a million lands on that, and the extract would carry an
    /// `at` that does not reproduce the digest printed beside it — so an auditor following the published
    /// formula would report a tampered record. Still RFC 3339, so a screen parses it unchanged.
    pub at: String,
    pub actor_id: Option<Uuid>,
    pub actor_kind: String,
    pub action: String,
    pub target_kind: String,
    pub target_id: Option<String>,
    pub payload: serde_json::Value,
    /// Both hashes travel with the row, because an extract that carried only the digest could not be walked
    /// and an extract that carried only the link could not be checked.
    pub prev_hash: Option<String>,
    pub hash: String,
}

impl From<audit::Entry> for EntryView {
    fn from(entry: audit::Entry) -> Self {
        Self {
            seq: entry.seq,
            at: dam_core::audit::canonical_time(entry.at),
            actor_id: entry.actor_id,
            actor_kind: entry.actor_kind,
            action: entry.action,
            target_kind: entry.target_kind,
            target_id: entry.target_id,
            payload: entry.payload,
            prev_hash: entry.prev_hash,
            hash: entry.hash,
        }
    }
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct PageQuery {
    pub action: Option<String>,
    pub actor_id: Option<Uuid>,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    /// Keyset cursor: the page runs backwards from here, exclusive.
    pub before_seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PageView {
    pub entries: Vec<EntryView>,
    /// Pass back as `before_seq` for the next page. `None` at the end of the log.
    pub next_before_seq: Option<i64>,
}

/// Reads the governance record, newest first.
#[utoipa::path(
    get,
    path = "/audit",
    params(PageQuery),
    responses(
        (status = 200, body = PageView),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "governance",
)]
pub async fn list(
    State(state): State<Arc<GovernanceState>>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<PageView>, Failure> {
    // `Manage`, the same gate as the other tenant-configuration surfaces. The audit log is not asset-scoped,
    // so it is not filtered by the caller's predicate — and that is the reason it takes the strongest gate
    // this model has rather than a narrower permission string nothing would grant. See `ai.rs` for the same
    // decision and the note about why a new permission string would be a gate no existing role could pass.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    let filter = audit::Filter {
        action: query.action,
        actor_id: query.actor_id,
        target_kind: query.target_kind,
        target_id: query.target_id,
        before_seq: query.before_seq,
    };
    let entries = audit::page(conn.executor(), &filter, PAGE_ROWS).await?;
    conn.commit().await?;

    // The cursor is only offered when the page filled: a short page is the end of the log, and offering a
    // cursor there sends a client round again for nothing.
    let next_before_seq = if i64::try_from(entries.len()).unwrap_or(0) == PAGE_ROWS {
        entries.last().map(|entry| entry.seq)
    } else {
        None
    };

    Ok(Json(PageView {
        entries: entries.into_iter().map(EntryView::from).collect(),
        next_before_seq,
    }))
}

#[derive(Debug, Clone, Copy, Deserialize, IntoParams)]
pub struct FromQuery {
    /// Where to start. Defaults to the beginning of the chain.
    #[serde(default)]
    pub from_seq: i64,
    /// How many entries to take. Only read by the export.
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VerificationView {
    pub intact: bool,
    pub checked: u64,
    pub from_seq: i64,
    pub through_seq: Option<i64>,
    /// Present only when the chain broke. `kind` is `altered` or `unlinked`: the first says a row's own
    /// columns no longer produce its digest, the second says the row before it is not the row it names.
    /// An investigator needs the difference — one points at a record, the other at a gap.
    pub failure: Option<FailureView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FailureView {
    pub kind: String,
    pub seq: i64,
    pub detail: String,
}

/// Walks the chain and reports the first inconsistency.
#[utoipa::path(
    get,
    path = "/audit/verify",
    params(FromQuery),
    responses(
        (status = 200, body = VerificationView, description = "The report. A broken chain is a 200 with `intact: false` — the request succeeded, and the answer is bad news."),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "governance",
)]
pub async fn verify(
    State(state): State<Arc<GovernanceState>>,
    headers: HeaderMap,
    Query(query): Query<FromQuery>,
) -> Result<Json<VerificationView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let result = audit::verify(conn.executor(), query.from_seq.max(0)).await?;
    conn.commit().await?;

    // A failed verification is not a failed request. A 500 would be indistinguishable from the database being
    // down, which is exactly the wrong thing to be ambiguous about: "we cannot tell you" and "the record has
    // been altered" are different sentences and only one of them is an emergency.
    let failure = result.first_break.as_ref().map(|breakage| match breakage {
        audit::Break::Altered {
            seq,
            stored,
            recomputed,
        } => FailureView {
            kind: "altered".to_owned(),
            seq: *seq,
            detail: format!("stored hash {stored}, recomputed {recomputed}"),
        },
        audit::Break::Unlinked {
            seq,
            claimed_prev,
            actual_prev,
        } => FailureView {
            kind: "unlinked".to_owned(),
            seq: *seq,
            detail: format!(
                "names predecessor {}, but the entry before it hashes to {}",
                claimed_prev.as_deref().unwrap_or("nothing"),
                actual_prev.as_deref().unwrap_or("nothing")
            ),
        },
    });

    Ok(Json(VerificationView {
        intact: result.is_intact(),
        checked: result.checked,
        from_seq: result.from_seq,
        through_seq: result.through_seq,
        failure,
    }))
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ExtractView {
    pub entries: Vec<EntryView>,
    /// The hash the first entry links back to, so the extract's own head can be checked rather than trusted.
    /// `None` when the extract starts at the beginning of the chain.
    pub anchor: Option<String>,
    /// The entry recording that this export happened. Not a member of `entries`.
    pub recorded_as: EntryView,
    /// How the hashes were computed, so the extract can be re-verified without this codebase.
    pub chain_version: u8,
}

/// Takes a re-verifiable extract, and records that it was taken.
#[utoipa::path(
    post,
    path = "/audit/export",
    params(FromQuery),
    responses(
        (status = 200, body = ExtractView),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "governance",
)]
pub async fn export(
    State(state): State<Arc<GovernanceState>>,
    headers: HeaderMap,
    Query(query): Query<FromQuery>,
) -> Result<Json<ExtractView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    let extract = audit::export(
        conn.executor(),
        query.from_seq.max(0),
        query.limit.unwrap_or(EXPORT_ROWS),
        caller.identity_id,
        if caller.identity_id.is_some() {
            ActorKind::User
        } else {
            ActorKind::ApiKey
        },
    )
    .await?;
    conn.commit().await?;

    Ok(Json(ExtractView {
        entries: extract.entries.into_iter().map(EntryView::from).collect(),
        anchor: extract.anchor,
        recorded_as: EntryView::from(extract.recorded_as),
        chain_version: dam_core::audit::CHAIN_VERSION,
    }))
}
