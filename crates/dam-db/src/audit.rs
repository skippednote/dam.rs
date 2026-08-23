//! The tamper-evident governance record (G10).
//!
//! `audit_log` has existed since migration 0007 — with its hash-chain formula in a comment, its rules
//! refusing UPDATE and DELETE, and its four indexes — and nothing has ever written a row to it. This is the
//! writer, the verifier, and the export.
//!
//! ## Why it is not `events`
//!
//! `events` answers "what has been happening", is partitioned for volume, and is written on every download.
//! This answers "prove that this governance decision was taken, by that person, and has not been edited
//! since". `dam_db::events` documents the split from the other side; the consequence here is that a write to
//! this table is expensive on purpose and belongs only on the actions a customer would subpoena.
//!
//! ## The chain has to be serialised, or it forks
//!
//! Every entry hashes its predecessor's hash, so two transactions that read the same tail and both insert
//! produce two rows claiming the same `prev_hash`. Nothing fails at the time. The damage surfaces at
//! verification — months later, in front of an auditor — as a broken chain, with no way left to tell which
//! branch was the real history.
//!
//! So [`record`] takes a per-tenant advisory lock covering the read of the tail and the insert. Per tenant
//! rather than global because the chains are per schema and one customer's governance write has no business
//! blocking another's. Held to the end of the transaction rather than released early because releasing it
//! before the insert commits is the same race with a smaller window.
//!
//! **And [`record`] opens that transaction itself rather than trusting the caller to be inside one.**
//! `pg_advisory_xact_lock` outside a transaction is taken and released by the statement that calls it, which
//! is a lock in the shape of a no-op — the same silent failure `crate::tenant_conn` exists to prevent for
//! `SET LOCAL`, and undetectable by the code that depends on it. `Connection::begin` issues a `BEGIN` when
//! there is no transaction and a `SAVEPOINT` when there is, and an advisory transaction lock taken inside a
//! savepoint is still held until the top-level transaction ends. Either way the lock spans the read and the
//! write, and the entry commits or rolls back with the action it records.
//!
//! The cost is real and worth stating: an audit write inside a long transaction serialises that tenant's
//! other audit writes behind it. That is affordable only because these are governance-rate events. It would
//! be untenable at request rate, which is the concrete reason the download feed is a different table.
//!
//! ## `seq` and `at` are supplied, not defaulted
//!
//! Both columns have defaults — `GENERATED ALWAYS AS IDENTITY` and `now()` — and this path uses neither,
//! because the hash covers both and only Rust computes the hash. Letting the database assign them would mean
//! hashing values we could not yet see, and the repair — insert, read back, update the hash — is exactly the
//! UPDATE the table refuses. So the sequence is drawn with `nextval` and the clock is read in the same
//! statement, then the insert overrides both.
//!
//! `clock_timestamp()` rather than `now()`: `now()` is transaction-start time, and a transaction that waited
//! on the advisory lock started earlier than the one that overtook it, which would write timestamps that run
//! backwards against the sequence. Read after the lock, the wall clock is monotonic in chain order.
//!
//! **A gap in `seq` is therefore not evidence of tampering.** `nextval` is deliberately non-transactional, so
//! a rolled-back governance action consumes a number and leaves a hole. Verification chains on `prev_hash`
//! for that reason and never on sequence contiguity — a verifier that counted numbers would report every
//! failed request as a deleted record.
//!
//! ## The payload is hashed as the database will store it
//!
//! `jsonb` is a normalising type, and the differences are small enough to be missed and fatal to a digest.
//! Negative zero is the demonstrable one: serde_json renders the f64 as `-0.0` and `jsonb` reads it back as
//! `0.0`. A very large number goes in as `1.2345678901234568e22` and comes out as its full decimal expansion,
//! which happens to re-parse to the same f64 — so that case survives by luck rather than by design, and
//! relying on the difference between the two is relying on a property of `numeric` output nobody documented
//! for this purpose.
//!
//! So the same statement that draws the sequence casts the payload through `jsonb` and hands it back, and
//! what gets hashed is what gets stored. Without it a row can be unverifiable from the instant it is written,
//! which is tamper evidence for an entry nobody touched — the failure that teaches a reader to distrust the
//! alarm.
//!
//! ## An export is itself auditable
//!
//! Taking a copy of the governance record out of the system is a governance action, so [`export`] appends an
//! entry describing what it exported. The exported extract cannot contain the entry that records it, which is
//! the correct shape: the log says a copy was taken, and the copy does not get to say otherwise.

use crate::Error;
use chrono::{DateTime, Utc};
use dam_core::audit::Link;
use sqlx::Connection as _;
use sqlx::PgConnection;
use sqlx::Row as _;
use uuid::Uuid;

/// How many rows [`verify`] holds in memory at once.
///
/// Verification walks the whole chain, which for a long-lived tenant is larger than a response. Batched so
/// the work is bounded and streamable rather than proportional to the log.
const VERIFY_BATCH: i64 = 1_000;

/// The largest extract [`export`] will produce in one call.
///
/// An export is meant to be re-verified offline, which means the caller needs the whole range or a documented
/// cursor into it — silently returning the first page of a request for everything would be an extract that
/// verifies as a broken chain.
pub const EXPORT_LIMIT: i64 = 10_000;

/// Who took the action.
///
/// `support` is separate from `system` because §Q's point is that support access to customer data is itself
/// auditable, and folding it into `system` hides precisely the rows a customer most wants to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    User,
    ApiKey,
    Connector,
    System,
    Support,
}

impl ActorKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKey => "api_key",
            Self::Connector => "connector",
            Self::System => "system",
            Self::Support => "support",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "api_key" => Some(Self::ApiKey),
            "connector" => Some(Self::Connector),
            "system" => Some(Self::System),
            "support" => Some(Self::Support),
            _ => None,
        }
    }
}

/// What was done.
///
/// A closed set for what this code writes, over a column that stays free text — the same arrangement as
/// `events::Kind`, and for the same reason: a later subsystem must be able to record a governance action
/// without a migration, but everything written here has to be a string the export and the screen can phrase.
/// An unrecognised action read back is surfaced as itself rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    LegalHoldPlaced,
    LegalHoldLifted,
    RetentionChanged,
    ErasureRequested,
    ErasureCompleted,
    ErasureRefused,
    ConnectorRegistered,
    ConnectorRotated,
    ConnectorRevoked,
    IdentityProvisioned,
    IdentityDeprovisioned,
    IdentityReactivated,
    RoleGranted,
    RoleRevoked,
    KeyIssued,
    KeyRevoked,
    AuditExported,
    SupportAccess,
}

impl Action {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegalHoldPlaced => "legal_hold.placed",
            Self::LegalHoldLifted => "legal_hold.lifted",
            Self::RetentionChanged => "retention.changed",
            Self::ErasureRequested => "erasure.requested",
            Self::ErasureCompleted => "erasure.completed",
            Self::ErasureRefused => "erasure.refused",
            Self::ConnectorRegistered => "connector.registered",
            Self::ConnectorRotated => "connector.rotated",
            Self::ConnectorRevoked => "connector.revoked",
            Self::IdentityProvisioned => "identity.provisioned",
            Self::IdentityDeprovisioned => "identity.deprovisioned",
            Self::IdentityReactivated => "identity.reactivated",
            Self::RoleGranted => "role.granted",
            Self::RoleRevoked => "role.revoked",
            Self::KeyIssued => "key.issued",
            Self::KeyRevoked => "key.revoked",
            Self::AuditExported => "audit.exported",
            Self::SupportAccess => "support.access",
        }
    }
}

/// An entry waiting to be chained.
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub action: Action,
    pub actor_kind: ActorKind,
    /// `None` for a system action with no person behind it. Never invented: a handler that cannot name the
    /// actor must say so rather than attribute the action to whoever happens to be convenient.
    pub actor_id: Option<Uuid>,
    pub target_kind: String,
    pub target_id: Option<String>,
    pub payload: serde_json::Value,
}

impl NewEntry {
    /// An action taken by a person.
    #[must_use]
    pub fn by(action: Action, actor_id: Uuid, target_kind: impl Into<String>) -> Self {
        Self {
            action,
            actor_kind: ActorKind::User,
            actor_id: Some(actor_id),
            target_kind: target_kind.into(),
            target_id: None,
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    /// An action taken by the system with nobody behind it — a scheduled retention pass, a lifecycle move.
    #[must_use]
    pub fn by_system(action: Action, target_kind: impl Into<String>) -> Self {
        Self {
            action,
            actor_kind: ActorKind::System,
            actor_id: None,
            target_kind: target_kind.into(),
            target_id: None,
            payload: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    #[must_use]
    pub fn on(mut self, target_id: impl Into<String>) -> Self {
        self.target_id = Some(target_id.into());
        self
    }

    #[must_use]
    pub fn as_kind(mut self, kind: ActorKind) -> Self {
        self.actor_kind = kind;
        self
    }

    #[must_use]
    pub fn with(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }
}

/// A chained entry, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub seq: i64,
    pub at: DateTime<Utc>,
    pub actor_id: Option<Uuid>,
    pub actor_kind: String,
    pub action: String,
    pub target_kind: String,
    pub target_id: Option<String>,
    pub payload: serde_json::Value,
    pub prev_hash: Option<String>,
    pub hash: String,
}

impl Entry {
    /// Recompute this entry's hash from its own columns.
    ///
    /// The whole verification is this function plus a walk: if the recomputed digest differs from the stored
    /// one the row was altered, and if it matches but the next row's `prev_hash` does not, a row went
    /// missing between them.
    #[must_use]
    pub fn recomputed_hash(&self) -> String {
        dam_core::audit::hash(&Link {
            seq: self.seq,
            at: self.at,
            actor_id: self.actor_id,
            actor_kind: &self.actor_kind,
            action: &self.action,
            target_kind: &self.target_kind,
            target_id: self.target_id.as_deref(),
            payload: &self.payload,
            prev_hash: self.prev_hash.as_deref(),
        })
    }
}

/// Append one entry, chained to the current tail.
pub async fn record(conn: &mut PgConnection, entry: NewEntry) -> Result<Entry, Error> {
    // A real `BEGIN` if the caller is not in a transaction, a `SAVEPOINT` if they are — see the module note
    // on why the lock below is worthless without one.
    let mut tx = conn.begin().await?;

    // The chain lock, for the rest of the transaction. Keyed on the table's own OID, which resolves through
    // the caller's `search_path` — the thing `TenantConn` exists to guarantee — and is unique across the
    // cluster, so one tenant's governance write can never block another's. A hashed schema name would be the
    // obvious alternative and would collide sometimes, which is a coupling nobody would ever diagnose.
    sqlx::query("SELECT pg_advisory_xact_lock('audit_log'::regclass::oid::bigint)")
        .execute(&mut *tx)
        .await?;

    // One statement for the four things only the database can answer: the next sequence value, the clock
    // after the lock, the payload as `jsonb` will store it, and the hash at the end of the chain.
    let prepared = sqlx::query(
        "SELECT nextval(pg_get_serial_sequence('audit_log', 'seq')) AS seq, \
                clock_timestamp() AS at, \
                $1::jsonb AS payload, \
                (SELECT hash FROM audit_log ORDER BY seq DESC LIMIT 1) AS prev_hash",
    )
    .bind(&entry.payload)
    .fetch_one(&mut *tx)
    .await?;

    let seq: i64 = prepared.try_get("seq")?;
    let at: DateTime<Utc> = prepared.try_get("at")?;
    let payload: serde_json::Value = prepared.try_get("payload")?;
    let prev_hash: Option<String> = prepared.try_get("prev_hash")?;

    let stored = Entry {
        seq,
        at,
        actor_id: entry.actor_id,
        actor_kind: entry.actor_kind.as_str().to_owned(),
        action: entry.action.as_str().to_owned(),
        target_kind: entry.target_kind,
        target_id: entry.target_id,
        payload,
        prev_hash,
        hash: String::new(),
    };
    let hash = stored.recomputed_hash();

    sqlx::query(
        "INSERT INTO audit_log \
             (seq, at, actor_id, actor_kind, action, target_kind, target_id, payload, prev_hash, hash) \
         OVERRIDING SYSTEM VALUE \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(stored.seq)
    .bind(stored.at)
    .bind(stored.actor_id)
    .bind(&stored.actor_kind)
    .bind(&stored.action)
    .bind(&stored.target_kind)
    .bind(&stored.target_id)
    .bind(&stored.payload)
    .bind(&stored.prev_hash)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Entry { hash, ..stored })
}

/// Which entries to read.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub action: Option<String>,
    pub actor_id: Option<Uuid>,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    /// Keyset pagination: the page runs backwards from here, exclusive.
    pub before_seq: Option<i64>,
}

/// One page of the log, newest first.
pub async fn page(
    conn: &mut PgConnection,
    filter: &Filter,
    limit: i64,
) -> Result<Vec<Entry>, Error> {
    let rows = sqlx::query(
        "SELECT seq, at, actor_id, actor_kind, action, target_kind, target_id, payload, prev_hash, hash \
         FROM audit_log \
         WHERE ($1::text IS NULL OR action = $1) \
           AND ($2::uuid IS NULL OR actor_id = $2) \
           AND ($3::text IS NULL OR target_kind = $3) \
           AND ($4::text IS NULL OR target_id = $4) \
           AND ($5::bigint IS NULL OR seq < $5) \
         ORDER BY seq DESC \
         LIMIT $6",
    )
    .bind(&filter.action)
    .bind(filter.actor_id)
    .bind(&filter.target_kind)
    .bind(&filter.target_id)
    .bind(filter.before_seq)
    .bind(limit.clamp(1, EXPORT_LIMIT))
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter().map(row_to_entry).collect()
}

/// Where a chain stops being consistent with itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Break {
    /// The row's columns do not produce the hash stored beside them: this row was altered.
    Altered {
        seq: i64,
        stored: String,
        recomputed: String,
    },
    /// The row's own hash checks out, but the predecessor it names is not the row before it: something was
    /// removed from, or inserted into, the gap.
    Unlinked {
        seq: i64,
        claimed_prev: Option<String>,
        actual_prev: Option<String>,
    },
}

impl Break {
    #[must_use]
    pub fn seq(&self) -> i64 {
        match self {
            Self::Altered { seq, .. } | Self::Unlinked { seq, .. } => *seq,
        }
    }
}

/// The result of walking the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub checked: u64,
    pub from_seq: i64,
    /// The last sequence number reached — the break's, if there was one.
    pub through_seq: Option<i64>,
    /// `None` means every row from `from_seq` onward hashes and links correctly.
    pub first_break: Option<Break>,
}

impl Verification {
    #[must_use]
    pub fn is_intact(&self) -> bool {
        self.first_break.is_none()
    }
}

/// Walk the chain from `from_seq` and stop at the first inconsistency.
///
/// Stops rather than collecting every break, because one alteration invalidates the link to everything after
/// it: a list of subsequent failures would be one finding reported a thousand times, and the sequence number
/// that matters is the first.
///
/// Starting part-way along still checks the link into `from_seq`, by reading the row before it. Skipping that
/// would let a caller choose a start that hides the break.
pub async fn verify(conn: &mut PgConnection, from_seq: i64) -> Result<Verification, Error> {
    let mut expected_prev: Option<String> =
        sqlx::query_scalar("SELECT hash FROM audit_log WHERE seq < $1 ORDER BY seq DESC LIMIT 1")
            .bind(from_seq)
            .fetch_optional(&mut *conn)
            .await?
            .flatten();

    let mut cursor = from_seq;
    let mut checked: u64 = 0;
    let mut through: Option<i64> = None;

    loop {
        let rows = sqlx::query(
            "SELECT seq, at, actor_id, actor_kind, action, target_kind, target_id, payload, prev_hash, hash \
             FROM audit_log WHERE seq >= $1 ORDER BY seq ASC LIMIT $2",
        )
        .bind(cursor)
        .bind(VERIFY_BATCH)
        .fetch_all(&mut *conn)
        .await?;

        if rows.is_empty() {
            return Ok(Verification {
                checked,
                from_seq,
                through_seq: through,
                first_break: None,
            });
        }

        let batch = rows.len();
        for row in rows {
            let entry = row_to_entry(row)?;
            through = Some(entry.seq);
            checked = checked.saturating_add(1);

            let recomputed = entry.recomputed_hash();
            if recomputed != entry.hash {
                return Ok(Verification {
                    checked,
                    from_seq,
                    through_seq: through,
                    first_break: Some(Break::Altered {
                        seq: entry.seq,
                        stored: entry.hash,
                        recomputed,
                    }),
                });
            }
            if entry.prev_hash != expected_prev {
                return Ok(Verification {
                    checked,
                    from_seq,
                    through_seq: through,
                    first_break: Some(Break::Unlinked {
                        seq: entry.seq,
                        claimed_prev: entry.prev_hash,
                        actual_prev: expected_prev,
                    }),
                });
            }
            cursor = entry.seq.saturating_add(1);
            expected_prev = Some(entry.hash);
        }

        if i64::try_from(batch).unwrap_or(VERIFY_BATCH) < VERIFY_BATCH {
            return Ok(Verification {
                checked,
                from_seq,
                through_seq: through,
                first_break: None,
            });
        }
    }
}

/// A contiguous extract, oldest first, together with the entry recording that it was taken.
#[derive(Debug, Clone)]
pub struct Extract {
    pub entries: Vec<Entry>,
    /// The `audit.exported` entry appended by this call. Not a member of `entries`: an extract cannot
    /// contain the record of its own creation.
    pub recorded_as: Entry,
    /// The hash the extract's first entry links back to, so a verifier can check the extract's own head
    /// rather than having to trust it. `None` when the extract starts at the beginning of the chain.
    pub anchor: Option<String>,
}

/// Take a re-verifiable extract of the chain, and record that it happened.
///
/// Oldest first, unlike [`page`], because an extract is verified by walking forward and a reversed one would
/// have to be re-sorted before it meant anything.
pub async fn export(
    conn: &mut PgConnection,
    from_seq: i64,
    limit: i64,
    actor: Option<Uuid>,
    actor_kind: ActorKind,
) -> Result<Extract, Error> {
    let limit = limit.clamp(1, EXPORT_LIMIT);
    let anchor: Option<String> =
        sqlx::query_scalar("SELECT hash FROM audit_log WHERE seq < $1 ORDER BY seq DESC LIMIT 1")
            .bind(from_seq)
            .fetch_optional(&mut *conn)
            .await?
            .flatten();

    let rows = sqlx::query(
        "SELECT seq, at, actor_id, actor_kind, action, target_kind, target_id, payload, prev_hash, hash \
         FROM audit_log WHERE seq >= $1 ORDER BY seq ASC LIMIT $2",
    )
    .bind(from_seq)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;

    let entries: Vec<Entry> = rows
        .into_iter()
        .map(row_to_entry)
        .collect::<Result<Vec<_>, _>>()?;

    let recorded_as = record(
        conn,
        NewEntry {
            action: Action::AuditExported,
            actor_kind,
            actor_id: actor,
            target_kind: "audit_log".to_owned(),
            target_id: None,
            payload: serde_json::json!({
                "from_seq": from_seq,
                "entries": entries.len(),
                // The range actually taken, not the range asked for: an export that hit the limit and an
                // export that reached the end of the log are different facts about what was handed over.
                "through_seq": entries.last().map(|entry| entry.seq),
                "truncated": i64::try_from(entries.len()).unwrap_or(limit) >= limit,
            }),
        },
    )
    .await?;

    Ok(Extract {
        entries,
        recorded_as,
        anchor,
    })
}

fn row_to_entry(row: sqlx::postgres::PgRow) -> Result<Entry, Error> {
    Ok(Entry {
        seq: row.try_get("seq")?,
        at: row.try_get("at")?,
        actor_id: row.try_get("actor_id")?,
        actor_kind: row.try_get("actor_kind")?,
        action: row.try_get("action")?,
        target_kind: row.try_get("target_kind")?,
        target_id: row.try_get("target_id")?,
        payload: row.try_get("payload")?,
        prev_hash: row.try_get("prev_hash")?,
        hash: row.try_get("hash")?,
    })
}
