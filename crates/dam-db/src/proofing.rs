//! Proofing rounds: a named review over a set of assets, with a verdict per reviewer (M6b).
//!
//! ## The state is derived, every time
//!
//! A round is `changes_requested` if any reviewer said so, `approved` if every reviewer approved, and open
//! otherwise. Computed in the query rather than stored beside the verdicts, because two sources of truth for
//! one fact means one of them is eventually wrong — and it is always the copy.
//!
//! `changes_requested` wins over `approved` deliberately: if three people approved and one asked for changes,
//! the round has changes to make. Taking a majority would be inventing a governance rule nobody asked for, and
//! quietly overruling the one person who found the problem.
//!
//! ## The set is snapshotted, and this module never widens it
//!
//! There is no "add assets to an open round". 0025's argument, unchanged: an approver who agreed to forty
//! photographs must not find they agreed to four hundred. A round whose scope needs to grow is a new round.
//!
//! ## Nothing here gates anything
//!
//! Like the comment status it builds on. These functions record that people agreed; whether an unapproved asset
//! may be published is a rights question, and answering it here would put a collaboration table in the delivery
//! path.

use crate::Error;
use chrono::{DateTime, Utc};
use dam_core::policy::AccessPredicate;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// What a round has come to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Somebody has not decided yet.
    Open,
    /// Every reviewer approved.
    Approved,
    /// At least one reviewer asked for changes. Wins over any number of approvals — see the module docs.
    ChangesRequested,
    /// Withdrawn by whoever asked for it.
    Cancelled,
}

impl Outcome {
    /// Whether the round is over, either way.
    #[must_use]
    pub const fn is_closed(self) -> bool {
        matches!(
            self,
            Self::Approved | Self::ChangesRequested | Self::Cancelled
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One reviewer's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pending,
    Approved,
    ChangesRequested,
}

impl Verdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
        }
    }

    /// Parses a stored value. `None` means the database and this module disagree, which is a bug to surface
    /// rather than a value to guess at.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "changes_requested" => Some(Self::ChangesRequested),
            _ => None,
        }
    }

    /// A verdict somebody can actually give. `pending` is a starting state, not an answer.
    #[must_use]
    pub fn parse_decision(value: &str) -> Option<Self> {
        match Self::parse(value) {
            Some(Self::Pending) | None => None,
            decided => decided,
        }
    }
}

/// Why a round operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum ProofRefusal {
    /// No such round, or one over assets this caller cannot see. One refusal for both, as everywhere else.
    #[error("no round {0}")]
    UnknownRound(Uuid),

    /// A round over an asset the requester cannot see. Refused wholesale rather than silently narrowed: a
    /// review of thirty-nine photographs when somebody asked for forty is a different review.
    #[error("{0} of the assets are not ones you can see; a round cannot be narrowed silently")]
    AssetsOutOfScope(usize),

    #[error("a round needs at least one asset")]
    NoAssets,

    #[error("a round needs at least one reviewer, or nobody is being asked anything")]
    NoReviewers,

    #[error("{0} is not a reviewer on this round")]
    NotAReviewer(Uuid),

    #[error("this round is already closed; a further review is a new round")]
    AlreadyClosed,

    #[error("{0:?} is not a verdict; use approved or changes_requested")]
    NotAVerdict(String),

    #[error(transparent)]
    Database(#[from] Error),
}

/// A round to open.
#[derive(Debug, Clone)]
pub struct NewRound<'a> {
    pub title: &'a str,
    pub brief: &'a str,
    pub asset_ids: &'a [Uuid],
    pub reviewer_ids: &'a [Uuid],
    pub due_at: Option<DateTime<Utc>>,
    pub requested_by: Option<Uuid>,
    /// The round this follows, when it is a second pass. Its number is taken from that one plus one.
    pub supersedes: Option<Uuid>,
}

/// A round as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Round {
    pub id: Uuid,
    pub title: String,
    pub brief: String,
    pub number: i32,
    pub supersedes: Option<Uuid>,
    pub due_at: Option<DateTime<Utc>>,
    pub requested_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// Derived from the verdicts — see the module docs.
    pub outcome: Outcome,
    /// How many assets are still in it. Shrinks when one is deleted, by cascade.
    pub asset_count: i64,
    pub reviewers: Vec<Reviewer>,
}

/// One reviewer's standing on a round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    pub identity_id: Uuid,
    pub verdict: Verdict,
    pub note: String,
    pub decided_at: Option<DateTime<Utc>>,
}

/// Opens a round.
///
/// Every asset is checked against the requester's own scope, and the whole round is refused if any is outside
/// it. Narrowing silently would be worse than refusing: a reviewer would approve a set the requester did not
/// choose, and neither of them would know the two differed.
pub async fn open(
    conn: &mut sqlx::PgConnection,
    new: &NewRound<'_>,
    predicate: &AccessPredicate,
) -> Result<Uuid, ProofRefusal> {
    if new.asset_ids.is_empty() {
        return Err(ProofRefusal::NoAssets);
    }
    if new.reviewer_ids.is_empty() {
        return Err(ProofRefusal::NoReviewers);
    }

    let visible = crate::assets::visible_among(&mut *conn, predicate, new.asset_ids).await?;
    if visible.len() != new.asset_ids.len() {
        return Err(ProofRefusal::AssetsOutOfScope(
            new.asset_ids.len() - visible.len(),
        ));
    }

    // The number from the round being followed, so "round 3" is readable without walking the chain.
    let number: i32 = match new.supersedes {
        None => 1,
        Some(previous) => {
            let found: Option<i32> =
                sqlx::query_scalar("SELECT number FROM proof_rounds WHERE id = $1")
                    .bind(previous)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(Error::from)?;
            found.ok_or(ProofRefusal::UnknownRound(previous))? + 1
        }
    };

    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO proof_rounds \
           (id, title, brief, number, supersedes, due_at, requested_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(new.title.trim())
    .bind(new.brief.trim())
    .bind(number)
    .bind(new.supersedes)
    .bind(new.due_at)
    .bind(new.requested_by)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    // In the order given, which is the order the requester arranged them in.
    for (position, asset_id) in new.asset_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO proof_round_assets (round_id, asset_id, position) VALUES ($1, $2, $3) \
             ON CONFLICT (round_id, asset_id) DO NOTHING",
        )
        .bind(id)
        .bind(asset_id)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    }

    for reviewer in new.reviewer_ids {
        sqlx::query(
            "INSERT INTO proof_round_reviewers (round_id, identity_id) VALUES ($1, $2) \
             ON CONFLICT (round_id, identity_id) DO NOTHING",
        )
        .bind(id)
        .bind(reviewer)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    }

    Ok(id)
}

/// Records one reviewer's verdict, and closes the round if that was the last word.
///
/// The close is decided here rather than by a sweep, because the moment a round closes is the moment its last
/// verdict lands — and a job that noticed later would put a gap between "everybody has approved" and "the round
/// says so", which is exactly the window somebody screenshots.
pub async fn decide(
    conn: &mut sqlx::PgConnection,
    round_id: Uuid,
    reviewer: Uuid,
    verdict: Verdict,
    note: &str,
) -> Result<Outcome, ProofRefusal> {
    let round: Option<(Option<DateTime<Utc>>,)> =
        sqlx::query_as("SELECT closed_at FROM proof_rounds WHERE id = $1")
            .bind(round_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(Error::from)?;
    let (closed_at,) = round.ok_or(ProofRefusal::UnknownRound(round_id))?;
    if closed_at.is_some() {
        return Err(ProofRefusal::AlreadyClosed);
    }

    let updated = sqlx::query(
        "UPDATE proof_round_reviewers \
         SET verdict = $3, note = $4, decided_at = now() \
         WHERE round_id = $1 AND identity_id = $2",
    )
    .bind(round_id)
    .bind(reviewer)
    .bind(verdict.as_str())
    .bind(note.trim())
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?
    .rows_affected();
    if updated == 0 {
        return Err(ProofRefusal::NotAReviewer(reviewer));
    }

    // Closed in the same transaction as the verdict that closed it.
    let outcome = outcome_of(&mut *conn, round_id).await?;
    if outcome.is_closed() {
        sqlx::query(
            "UPDATE proof_rounds SET closed_at = now() WHERE id = $1 AND closed_at IS NULL",
        )
        .bind(round_id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    }
    Ok(outcome)
}

/// Withdraws a round.
///
/// Verdicts already given are kept. A cancelled round is part of the record of what was asked and what came
/// back, and deleting the answers would make "why was this cancelled" unanswerable.
pub async fn cancel(
    conn: &mut sqlx::PgConnection,
    round_id: Uuid,
    by: Option<Uuid>,
) -> Result<bool, ProofRefusal> {
    let updated = sqlx::query(
        "UPDATE proof_rounds SET cancelled_at = now(), cancelled_by = $2, \
                                 closed_at = coalesce(closed_at, now()) \
         WHERE id = $1 AND cancelled_at IS NULL",
    )
    .bind(round_id)
    .bind(by)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?
    .rows_affected();
    Ok(updated > 0)
}

/// The derived state of one round.
async fn outcome_of(conn: &mut sqlx::PgConnection, round_id: Uuid) -> Result<Outcome, Error> {
    let row: Option<(bool, i64, i64)> = sqlx::query_as(
        "SELECT r.cancelled_at IS NOT NULL, \
                count(v.*) FILTER (WHERE v.verdict = 'pending'), \
                count(v.*) FILTER (WHERE v.verdict = 'changes_requested') \
         FROM proof_rounds r \
         LEFT JOIN proof_round_reviewers v ON v.round_id = r.id \
         WHERE r.id = $1 GROUP BY r.id, r.cancelled_at",
    )
    .bind(round_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some((cancelled, pending, changes)) = row else {
        // No such round. The caller has already established it exists in every path that reaches here, so this
        // is the "deleted underneath us" case rather than a lookup.
        return Ok(Outcome::Cancelled);
    };
    Ok(decide_outcome(cancelled, pending, changes))
}

/// The rule, in one place so the query and the tests cannot disagree about it.
///
/// `changes_requested` beats everything but a cancellation: if three approved and one asked for changes, the
/// round has changes to make. A majority rule would be inventing governance nobody asked for, and would quietly
/// overrule the one person who found the problem.
#[must_use]
pub const fn decide_outcome(cancelled: bool, pending: i64, changes_requested: i64) -> Outcome {
    if cancelled {
        Outcome::Cancelled
    } else if changes_requested > 0 {
        Outcome::ChangesRequested
    } else if pending > 0 {
        Outcome::Open
    } else {
        Outcome::Approved
    }
}

/// One round in full, if this caller may see it.
///
/// Visible when the caller can see **every** asset in it. A round is an agreement about a specific set, so
/// showing somebody a round whose scope they can only partly see would tell them a set exists that is larger
/// than what they can read — and would invite them to approve assets they have never seen.
pub async fn read(
    conn: &mut sqlx::PgConnection,
    round_id: Uuid,
    predicate: &AccessPredicate,
) -> Result<Round, ProofRefusal> {
    let assets = asset_ids(&mut *conn, round_id).await?;
    if assets.is_empty() {
        // Either no such round or one whose assets have all been deleted. The same refusal for both, because
        // distinguishing them would confirm the round exists.
        return Err(ProofRefusal::UnknownRound(round_id));
    }
    let visible = crate::assets::visible_among(&mut *conn, predicate, &assets).await?;
    if visible.len() != assets.len() {
        return Err(ProofRefusal::UnknownRound(round_id));
    }

    let row: Option<RoundRow> = sqlx::query_as(
        "SELECT id, title, brief, number, supersedes, due_at, requested_by, created_at, \
                closed_at, cancelled_at \
         FROM proof_rounds WHERE id = $1",
    )
    .bind(round_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    let row = row.ok_or(ProofRefusal::UnknownRound(round_id))?;
    hydrate(
        &mut *conn,
        row,
        i64::try_from(assets.len()).unwrap_or(i64::MAX),
    )
    .await
}

/// Every round this caller may see, newest first.
///
/// Filtered the same way as [`read`]: a round appears only when every one of its assets is visible. The filter
/// is applied in Rust after the round list is read, because the alternative — a `NOT EXISTS` over the predicate
/// per round — is the same work with the predicate rendered twice.
pub async fn list(
    conn: &mut sqlx::PgConnection,
    predicate: &AccessPredicate,
    limit: i64,
) -> Result<Vec<Round>, ProofRefusal> {
    let rows: Vec<RoundRow> = sqlx::query_as(
        "SELECT id, title, brief, number, supersedes, due_at, requested_by, created_at, \
                closed_at, cancelled_at \
         FROM proof_rounds ORDER BY created_at DESC LIMIT $1",
    )
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let round_id = row.0;
        let assets = asset_ids(&mut *conn, round_id).await?;
        if assets.is_empty() {
            continue;
        }
        let visible = crate::assets::visible_among(&mut *conn, predicate, &assets).await?;
        if visible.len() != assets.len() {
            continue;
        }
        out.push(
            hydrate(
                &mut *conn,
                row,
                i64::try_from(assets.len()).unwrap_or(i64::MAX),
            )
            .await?,
        );
    }
    Ok(out)
}

/// The rounds waiting on one person, oldest first — the read a reviewer's own list makes.
///
/// Ordered by due date with the undated last, because a review with a deadline is the one that matters today.
pub async fn waiting_on(
    conn: &mut sqlx::PgConnection,
    reviewer: Uuid,
    predicate: &AccessPredicate,
) -> Result<Vec<Round>, ProofRefusal> {
    let rows: Vec<RoundRow> = sqlx::query_as(
        "SELECT r.id, r.title, r.brief, r.number, r.supersedes, r.due_at, r.requested_by, \
                r.created_at, r.closed_at, r.cancelled_at \
         FROM proof_rounds r \
         JOIN proof_round_reviewers v ON v.round_id = r.id \
         WHERE v.identity_id = $1 AND v.verdict = 'pending' AND r.closed_at IS NULL \
         ORDER BY r.due_at NULLS LAST, r.created_at",
    )
    .bind(reviewer)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let assets = asset_ids(&mut *conn, row.0).await?;
        if assets.is_empty() {
            continue;
        }
        let visible = crate::assets::visible_among(&mut *conn, predicate, &assets).await?;
        if visible.len() != assets.len() {
            continue;
        }
        out.push(
            hydrate(
                &mut *conn,
                row,
                i64::try_from(assets.len()).unwrap_or(i64::MAX),
            )
            .await?,
        );
    }
    Ok(out)
}

/// One asset in a round's snapshot, with enough joined from `assets` to draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub asset_id: Uuid,
    pub position: i32,
    /// Joined, so a review screen shows pictures rather than a column of uuids. The join is inner and skips
    /// `deleted`: a round is a snapshot of *what was asked about*, and an asset deleted since then is
    /// something a reviewer can neither see nor judge. `asset_count` on the round shrinks with it, which is
    /// why that field's own note says it shrinks.
    pub filename: String,
    pub mime: String,
}

/// A round's assets in snapshot order, for the caller who can see them.
///
/// Visibility is checked by [`read`] before this is called, and `read` refuses a round whose assets the
/// caller cannot *all* see — so there is no per-item filtering here. That is deliberate rather than lax: a
/// review screen that silently dropped two of eleven pictures would show an approval of a set nobody
/// reviewed, and the whole-round refusal is what makes this list safe to draw entire.
pub async fn items(
    conn: &mut sqlx::PgConnection,
    round_id: Uuid,
) -> Result<Vec<Item>, ProofRefusal> {
    let rows = sqlx::query_as::<_, (Uuid, i32, String, String)>(
        "SELECT r.asset_id, r.position, a.filename, a.mime \
         FROM proof_round_assets r JOIN assets a ON a.id = r.asset_id \
         WHERE r.round_id = $1 AND a.status <> 'deleted' \
         ORDER BY r.position, r.asset_id",
    )
    .bind(round_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;

    Ok(rows
        .into_iter()
        .map(|(asset_id, position, filename, mime)| Item {
            asset_id,
            position,
            filename,
            mime,
        })
        .collect())
}

/// The assets in a round, in the order they were put in.
pub async fn assets_in(
    conn: &mut sqlx::PgConnection,
    round_id: Uuid,
) -> Result<Vec<Uuid>, ProofRefusal> {
    asset_ids(conn, round_id).await
}

async fn asset_ids(
    conn: &mut sqlx::PgConnection,
    round_id: Uuid,
) -> Result<Vec<Uuid>, ProofRefusal> {
    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT a.asset_id FROM proof_round_assets a WHERE a.round_id = ");
    builder.push_bind(round_id);
    builder.push(" ORDER BY a.position, a.asset_id");
    Ok(builder
        .build_query_scalar()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?)
}

type RoundRow = (
    Uuid,
    String,
    String,
    i32,
    Option<Uuid>,
    Option<DateTime<Utc>>,
    Option<Uuid>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

async fn hydrate(
    conn: &mut sqlx::PgConnection,
    row: RoundRow,
    asset_count: i64,
) -> Result<Round, ProofRefusal> {
    let (
        id,
        title,
        brief,
        number,
        supersedes,
        due_at,
        requested_by,
        created_at,
        closed_at,
        cancelled_at,
    ) = row;

    let reviewer_rows: Vec<(Uuid, String, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT identity_id, verdict, note, decided_at FROM proof_round_reviewers \
         WHERE round_id = $1 ORDER BY added_at, identity_id",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;

    let mut reviewers = Vec::with_capacity(reviewer_rows.len());
    let mut pending = 0i64;
    let mut changes = 0i64;
    for (identity_id, verdict, note, decided_at) in reviewer_rows {
        let verdict = Verdict::parse(&verdict)
            .ok_or_else(|| Error::Migrate(format!("unknown proofing verdict {verdict:?}")))?;
        match verdict {
            Verdict::Pending => pending += 1,
            Verdict::ChangesRequested => changes += 1,
            Verdict::Approved => {}
        }
        reviewers.push(Reviewer {
            identity_id,
            verdict,
            note,
            decided_at,
        });
    }

    Ok(Round {
        id,
        title,
        brief,
        number,
        supersedes,
        due_at,
        requested_by,
        created_at,
        closed_at,
        // Derived from the verdicts just read, so the outcome and the reviewers cannot disagree — which is what
        // a second query for the state would allow between two statements.
        outcome: decide_outcome(cancelled_at.is_some(), pending, changes),
        asset_count,
        reviewers,
    })
}
