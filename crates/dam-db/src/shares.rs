//! Share links (3.3).
//!
//! A share link is a URL handed to somebody outside the tenant: a client reviewing a shoot, an agency
//! collecting assets. It carries its own expiry, download limit, optional passcode, and revocation.
//!
//! ## Two secrets, two hashes, for opposite reasons
//!
//! The **token** is 256 bits from the OS CSPRNG. There is no dictionary for that, so guessing is not the
//! threat model and a slow hash would buy nothing while costing time on every request. It is stored as a
//! BLAKE3 digest so a database leak does not hand over every live share link — the same reasoning as
//! `auth::ApiKey`.
//!
//! The **passcode** is chosen by a person. `spring2026` *is* in a dictionary, so it gets argon2id. Using
//! BLAKE3 here because it worked for the token would make an offline attack on a leaked digest trivial.
//! Using argon2 for the token would add ~100 ms to every share view for no gain. The asymmetry is the point.
//!
//! ## Revocation has to reach an already-issued URL
//!
//! TASKS.md names this, and it is the part that constrains the design. Resolving the share token per request
//! makes revoking the *share* immediate. But a share link mints delivery tokens (3.1), and one of those is
//! valid for its own TTL — so revoking the share would leave outstanding delivery URLs working.
//!
//! The delivery claim therefore carries the share link's id, and delivery re-checks it. That is the same
//! shape as D12's rights check: the signature proves the request was issued, and the entitlement is
//! evaluated afresh at delivery. Anything else makes "revoke" mean "revoke, eventually".

use crate::Error;
use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Bytes of randomness in a share token. 32 bytes = 256 bits.
const TOKEN_BYTES: usize = 32;

/// A freshly created share link, holding the token long enough to show it once.
///
/// The plaintext token exists only here; the row stores a digest. Deliberately not `Clone`.
pub struct NewShare {
    token: String,
    pub id: Uuid,
}

impl std::fmt::Debug for NewShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A share token in a log is a share link that has to be revoked.
        f.debug_struct("NewShare")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl NewShare {
    /// The token to put in the URL. Shown once, at creation.
    pub fn token(&self) -> &str {
        &self.token
    }
}

/// What a share link permits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub id: Uuid,
    pub kind: String,
    pub target_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub max_downloads: Option<i32>,
    pub download_count: i32,
    pub allow_original: bool,
    pub requires_eula: bool,
    /// Whether a passcode must be supplied. The hash itself never leaves this module.
    pub has_passcode: bool,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl Share {
    /// Whether this share is usable at `now`, ignoring the passcode.
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none()
            && self.expires_at.is_none_or(|at| now < at)
            && self
                .max_downloads
                .is_none_or(|max| self.download_count < max)
    }
}

/// Why a share link cannot be used.
///
/// Distinguished for the caller's benefit — a recipient told "this link has expired" can ask for a new one,
/// where "not found" sends them to check the URL. That is a deliberate disclosure: a share token is 256
/// random bits, so an attacker cannot enumerate one, and the recipient is the person who needs the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ShareRefusal {
    #[error("no such share link")]
    NotFound,
    #[error("this share link has been revoked")]
    Revoked,
    #[error("this share link has expired")]
    Expired,
    #[error("this share link has reached its download limit")]
    Exhausted,
    #[error("a passcode is required")]
    PasscodeRequired,
    #[error("that passcode is not correct")]
    PasscodeWrong,
}

/// What to create.
#[derive(Debug, Clone)]
pub struct ShareSpec<'a> {
    pub kind: &'a str,
    pub target_id: Option<Uuid>,
    pub search_query: Option<serde_json::Value>,
    /// Plaintext; hashed with argon2id before it is stored.
    pub passcode: Option<&'a str>,
    pub expires_at: Option<DateTime<Utc>>,
    pub max_downloads: Option<i32>,
    pub allow_original: bool,
    pub requires_eula: bool,
    pub created_by: Option<Uuid>,
}

/// Creates a share link and returns its one-time token.
pub async fn create(pool: &sqlx::PgPool, spec: &ShareSpec<'_>) -> Result<NewShare, Error> {
    let mut conn = pool.acquire().await?;
    create_on(&mut conn, spec).await
}

/// The same creation, on a connection the caller has already scoped — see `bulk::create_on` for why.
pub async fn create_on(
    conn: &mut sqlx::PgConnection,
    spec: &ShareSpec<'_>,
) -> Result<NewShare, Error> {
    let bytes: [u8; TOKEN_BYTES] = rand::random();
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = Uuid::new_v4();

    let passcode_hash = match spec.passcode {
        Some(passcode) => Some(hash_passcode(passcode)?),
        None => None,
    };

    sqlx::query(
        "INSERT INTO share_links \
         (id, token, kind, target_id, search_query, passcode_hash, expires_at, max_downloads, \
          allow_original, requires_eula, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(token_digest(&token))
    .bind(spec.kind)
    .bind(spec.target_id)
    .bind(&spec.search_query)
    .bind(&passcode_hash)
    .bind(spec.expires_at)
    .bind(spec.max_downloads)
    .bind(spec.allow_original)
    .bind(spec.requires_eula)
    .bind(spec.created_by)
    .execute(conn)
    .await?;

    Ok(NewShare { token, id })
}

/// Resolves a presented token, checking everything except the passcode.
///
/// The passcode is separate because the two answers are different: a live link with a wrong passcode should
/// prompt again, while a revoked one should not prompt at all.
pub async fn resolve(
    pool: &sqlx::PgPool,
    token: &str,
    now: DateTime<Utc>,
) -> Result<Share, ShareRefusal> {
    let share = load_by_token(pool, token)
        .await
        .map_err(|_| ShareRefusal::NotFound)?
        .ok_or(ShareRefusal::NotFound)?;

    // Revocation first: it is the most absolute reason and the one a recipient most needs stated plainly.
    if share.revoked_at.is_some() {
        return Err(ShareRefusal::Revoked);
    }
    if share.expires_at.is_some_and(|at| now >= at) {
        return Err(ShareRefusal::Expired);
    }
    if share
        .max_downloads
        .is_some_and(|max| share.download_count >= max)
    {
        return Err(ShareRefusal::Exhausted);
    }
    Ok(share)
}

/// Checks a passcode against a live share.
///
/// Verified in constant time by argon2's own comparison. A wrong passcode and a missing hash are different
/// refusals: "a passcode is required" tells a recipient to look for one in the email, and "that passcode is
/// not correct" tells them to re-read it.
pub async fn check_passcode(
    pool: &sqlx::PgPool,
    share_id: Uuid,
    presented: Option<&str>,
) -> Result<(), ShareRefusal> {
    let stored: Option<Option<String>> =
        sqlx::query_scalar("SELECT passcode_hash FROM share_links WHERE id = $1")
            .bind(share_id)
            .fetch_optional(pool)
            .await
            .map_err(|_| ShareRefusal::NotFound)?;
    let stored = stored.ok_or(ShareRefusal::NotFound)?;

    match (stored, presented) {
        (None, _) => Ok(()),
        (Some(_), None) => Err(ShareRefusal::PasscodeRequired),
        (Some(hash), Some(presented)) => {
            let parsed = PasswordHash::new(&hash).map_err(|_| ShareRefusal::PasscodeWrong)?;
            Argon2::default()
                .verify_password(presented.as_bytes(), &parsed)
                .map_err(|_| ShareRefusal::PasscodeWrong)
        }
    }
}

/// Consumes one download against the limit, atomically.
///
/// The check and the increment are **one statement**. Reading the count, comparing it, then incrementing
/// lets two concurrent downloads both pass at the last slot — the classic race, and on a share link with
/// `max_downloads = 1` it means the asset goes out twice.
///
/// Returns the new count. `Exhausted` when there was no slot left, which is the same answer [`resolve`]
/// gives — so a caller that forgets this still cannot exceed the limit by more than the requests already in
/// flight when it checked.
pub async fn consume_download(
    pool: &sqlx::PgPool,
    share_id: Uuid,
    now: DateTime<Utc>,
) -> Result<i32, ShareRefusal> {
    let updated: Option<i32> = sqlx::query_scalar(
        "UPDATE share_links SET download_count = download_count + 1 \
         WHERE id = $1 AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > $2) \
           AND (max_downloads IS NULL OR download_count < max_downloads) \
         RETURNING download_count",
    )
    .bind(share_id)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(|_| ShareRefusal::NotFound)?;

    updated.ok_or(ShareRefusal::Exhausted)
}

/// Revokes a share link. Idempotent.
///
/// Returns whether this call was the one that revoked it, so an audit entry is written once rather than on
/// every retry.
pub async fn revoke(
    pool: &sqlx::PgPool,
    share_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let mut conn = pool.acquire().await?;
    revoke_on(&mut conn, share_id, now).await
}

/// The same revocation, on a scoped connection.
pub async fn revoke_on(
    conn: &mut sqlx::PgConnection,
    share_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let updated =
        sqlx::query("UPDATE share_links SET revoked_at = $2 WHERE id = $1 AND revoked_at IS NULL")
            .bind(share_id)
            .bind(now)
            .execute(conn)
            .await?
            .rows_affected();
    Ok(updated > 0)
}

/// One share in a management listing, with what the list draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub share: Share,
    /// The target asset's filename, so a list row says *what* is shared without a join per row in the UI.
    pub filename: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The tenant's share links, newest first.
///
/// Tokens are digests in the table and never come back out — a share whose link was lost is revoked and
/// re-created, exactly like an API key. The list is what makes revocation *findable*: a share you cannot see
/// is a share you cannot revoke.
pub async fn list_on(conn: &mut sqlx::PgConnection, limit: i64) -> Result<Vec<Listed>, Error> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<Uuid>,
            Option<DateTime<Utc>>,
            Option<i32>,
            i32,
            bool,
            bool,
            bool,
            Option<DateTime<Utc>>,
            Option<String>,
            DateTime<Utc>,
        ),
    >(
        "SELECT s.id, s.kind, s.target_id, s.expires_at, s.max_downloads, s.download_count, \
                s.allow_original, s.requires_eula, s.passcode_hash IS NOT NULL, s.revoked_at, \
                a.filename, s.created_at \
         FROM share_links s \
         LEFT JOIN assets a ON a.id = s.target_id \
         ORDER BY s.created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                kind,
                target_id,
                expires_at,
                max_downloads,
                download_count,
                allow_original,
                requires_eula,
                has_passcode,
                revoked_at,
                filename,
                created_at,
            )| Listed {
                share: Share {
                    id,
                    kind,
                    target_id,
                    expires_at,
                    max_downloads,
                    download_count,
                    allow_original,
                    requires_eula,
                    has_passcode,
                    revoked_at,
                },
                filename,
                created_at,
            },
        )
        .collect())
}

/// Whether a share link is still usable, by id./// Whether a share link is still usable, by id.
///
/// What the delivery path calls on every request for a share-issued URL. Cheap on purpose: one indexed
/// lookup, because it runs before every download and the alternative is revocation that takes effect
/// eventually.
pub async fn is_live(
    pool: &sqlx::PgPool,
    share_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, Error> {
    let live: Option<bool> = sqlx::query_scalar(
        "SELECT (revoked_at IS NULL \
                 AND (expires_at IS NULL OR expires_at > $2) \
                 AND (max_downloads IS NULL OR download_count < max_downloads)) \
         FROM share_links WHERE id = $1",
    )
    .bind(share_id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(live.unwrap_or(false))
}

/// Loads a share by its presented token.
async fn load_by_token(pool: &sqlx::PgPool, token: &str) -> Result<Option<Share>, Error> {
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<Uuid>,
            Option<DateTime<Utc>>,
            Option<i32>,
            i32,
            bool,
            bool,
            Option<String>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT id, kind, target_id, expires_at, max_downloads, download_count, allow_original, \
                requires_eula, passcode_hash, revoked_at \
         FROM share_links WHERE token = $1",
    )
    // The digest, never the token — so the plaintext appears in no statement, query log or error.
    .bind(token_digest(token))
    .fetch_optional(pool)
    .await?;

    Ok(row.map(
        |(
            id,
            kind,
            target_id,
            expires_at,
            max_downloads,
            download_count,
            allow_original,
            requires_eula,
            passcode_hash,
            revoked_at,
        )| Share {
            id,
            kind,
            target_id,
            expires_at,
            max_downloads,
            download_count,
            allow_original,
            requires_eula,
            has_passcode: passcode_hash.is_some(),
            revoked_at,
        },
    ))
}

/// The digest stored in `share_links.token`.
///
/// The column is named `token` and holds a **hash**. Domain-separated so a share digest can never collide
/// with an API-key digest or a content hash computed elsewhere.
pub fn token_digest(token: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"damrs-share-token-v1\0");
    hasher.update(token.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// argon2id over a human-chosen passcode.
fn hash_passcode(passcode: &str) -> Result<String, Error> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(passcode.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| Error::Unsupported(format!("hashing a passcode: {e}")))
}
