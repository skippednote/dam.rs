//! Connected sites, and the secret each one signs with (M3d, §11).
//!
//! ## Reference, not copy — and this table is what makes it enforceable
//!
//! A connected CMS stores an asset id and renders signed transform URLs; it never downloads the bytes. That is
//! not a storage optimisation, it is what makes rights authoritative downstream: when a licence expires in the
//! DAM the image stops rendering on the site. `0004_connectors.sql` says the same thing about itself, and every
//! column below exists to bound what a remote may render.
//!
//! ## The signing secret is sealed, and this module never sees plaintext
//!
//! The remote signs render URLs *itself*, so a page render never blocks on a damrs API call (§11.3) — which
//! means the secret is a forgery capability for anything the connector is allowed to render. Sealed with the
//! deployment's keyring exactly as `crate::ai_credentials` does, for the same reason: plaintext exists in the
//! handler that minted it and nowhere else, and the sealed form carries its own key id so no column was added.
//!
//! ## Rotation has a grace window, and revocation does not
//!
//! `previous_signing_secret` keeps verifying for [`SECRET_GRACE`] after a rotation, because the DAM-side
//! rotation and the site-side config change are two separate deploys — a rotation with no window is a site
//! outage. But that is exactly wrong when the reason for rotating is that the secret leaked, so [`rotate`]
//! takes the choice as an argument rather than assuming one. A leak wants the old secret dead now.
//!
//! The window is enforced at *verification* time from `secret_rotated_at`, never by a job that clears the
//! column. A cleanup job that fails leaves a superseded secret valid forever, and nothing would say so.
//!
//! ## A connector is scoped by the ordinary machinery, not by a second one
//!
//! `asset_group_ids` here is configuration *and documentation*; the enforcement is the connector's API key,
//! whose identity holds a role carrying those groups. So a connector's reads go through `access::push_asset_filter`
//! like everybody else's. A parallel authorisation path for connectors would be a second place where access is
//! decided, which is the thing this codebase keeps refusing to have.

use crate::Error;
use chrono::{DateTime, Duration, Utc};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// How long a superseded signing secret keeps verifying.
///
/// A week. The DAM-side rotation and the site-side configuration change are separate deploys — often separate
/// teams — so the window has to cover a normal release cycle rather than a hot reload. Shorter is safer only if
/// somebody is standing by to deploy, and a rotation that takes a site down is a rotation nobody performs.
///
/// Not the answer when the secret has leaked: see [`rotate`].
pub const SECRET_GRACE: Duration = Duration::days(7);

/// What kind of system is connected.
///
/// The set the schema's CHECK allows. Kept as an enum so a caller cannot invent one and get a constraint
/// violation from a layer that cannot explain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Drupal,
    WordPress,
    AdobeCc,
    Figma,
    HubSpot,
    Salesforce,
    Generic,
}

impl Kind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drupal => "drupal",
            Self::WordPress => "wordpress",
            Self::AdobeCc => "adobe_cc",
            Self::Figma => "figma",
            Self::HubSpot => "hubspot",
            Self::Salesforce => "salesforce",
            Self::Generic => "generic",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "drupal" => Self::Drupal,
            "wordpress" => Self::WordPress,
            "adobe_cc" => Self::AdobeCc,
            "figma" => Self::Figma,
            "hubspot" => Self::HubSpot,
            "salesforce" => Self::Salesforce,
            "generic" => Self::Generic,
            _ => return None,
        })
    }
}

/// Whether a connector may be used at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    /// Temporarily stopped. Its key still exists and its references are kept.
    Paused,
    /// The remote is failing. Set by the dispatcher, cleared by an operator.
    Error,
    /// Finished with. Terminal — a revoked connector is never reactivated, because its secret is out there
    /// and reactivating would make every URL the remote ever signed live again.
    Revoked,
}

impl Status {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Error => "error",
            Self::Revoked => "revoked",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "error" => Self::Error,
            "revoked" => Self::Revoked,
            _ => return None,
        })
    }

    /// Whether a URL signed by this connector may still be honoured.
    #[must_use]
    pub const fn may_render(self) -> bool {
        matches!(self, Self::Active | Self::Error)
    }
}

/// Why a connector could not be written.
#[derive(Debug, thiserror::Error)]
pub enum ConnectorRefusal {
    #[error("no connector {0}")]
    Unknown(Uuid),

    /// A field the database refuses. Carries the constraint's name, because the CHECKs are the specification
    /// and restating them as Rust branches would be a second copy to drift from the first.
    #[error("{0}")]
    Invalid(String),

    /// Another connector of this kind already claims this site URL.
    #[error("{kind} is already connected to {site_url}")]
    AlreadyConnected { kind: String, site_url: String },

    /// A revoked connector is terminal.
    #[error("connector {0} is revoked; register a new one")]
    Revoked(Uuid),

    #[error(transparent)]
    Database(#[from] Error),
}

/// The associated data a connector's signing secret is sealed under.
///
/// `{tenant}:connector:{id}`. Every part is something a row cannot change without the secret refusing to open:
/// moved to another tenant, or copied to a new row, each fails closed. `dam_core::sealed` explains why
/// associated data is what makes that true; `crate::ai_credentials` uses the same shape for the same reason.
#[must_use]
pub fn associated_data(tenant: &str, id: Uuid) -> String {
    format!("{tenant}:connector:{id}")
}

/// A site to connect.
#[derive(Debug, Clone)]
pub struct NewConnector<'a> {
    /// Generated by the caller, because the id is part of the associated data and so has to be known before
    /// the secret is sealed. A database-generated id would force a seal-then-update, and a failure between the
    /// two would leave a row whose ciphertext is bound to an id it does not have.
    pub id: Uuid,
    pub kind: Kind,
    pub label: &'a str,
    pub site_url: &'a str,
    pub remote_version: Option<&'a str>,
    /// The key the remote authenticates with. `dam_global.api_keys.id`, no FK — tenant schemas carry no
    /// cross-schema foreign keys (0002).
    pub api_key_id: Option<Uuid>,
    /// Already sealed. This module never sees a plaintext secret.
    pub sealed_secret: &'a str,
    pub asset_group_ids: &'a [Uuid],
    pub allow_all_groups: bool,
    /// May it serve masters? Off by default: a CMS wants renditions, and a site that can fetch originals is a
    /// site that can leak the deliverable a customer paid for.
    pub allow_original: bool,
    /// May a render trigger a restore? Off by default, and §11.1 is emphatic about why — a page render must
    /// never wake Glacier. A cold original resolves to the master proxy, which is what an `<img>` wanted.
    pub allow_restore: bool,
    pub config: serde_json::Value,
}

/// A connected site.
#[derive(Debug, Clone)]
pub struct Connector {
    pub id: Uuid,
    pub kind: Kind,
    pub label: String,
    pub site_url: String,
    pub remote_version: Option<String>,
    pub api_key_id: Option<Uuid>,
    /// Ciphertext. Not a `Secret<String>` — it is not a secret, and typing it as one would make every honest
    /// caller `.expose()` something safe, which is how `.expose()` stops meaning anything.
    pub sealed_secret: String,
    /// The superseded ciphertext, while it is still inside the grace window.
    pub previous_sealed_secret: Option<String>,
    pub secret_rotated_at: Option<DateTime<Utc>>,
    pub asset_group_ids: Vec<Uuid>,
    pub allow_all_groups: bool,
    pub allow_original: bool,
    pub allow_restore: bool,
    pub config: serde_json::Value,
    pub status: Status,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Connector {
    /// Whether the superseded secret should still verify at `now`.
    ///
    /// Read from `secret_rotated_at` every time rather than cleared by a job. A cleanup job that fails leaves
    /// a superseded secret valid forever and nothing says so; a comparison cannot fail that way.
    #[must_use]
    pub fn previous_is_live(&self, now: DateTime<Utc>) -> bool {
        match (&self.previous_sealed_secret, self.secret_rotated_at) {
            (Some(_), Some(rotated)) => now < rotated + SECRET_GRACE,
            _ => false,
        }
    }

    /// The superseded ciphertext, only while it is still inside the window.
    #[must_use]
    pub fn live_previous(&self, now: DateTime<Utc>) -> Option<&str> {
        self.previous_is_live(now)
            .then_some(self.previous_sealed_secret.as_deref())
            .flatten()
    }

    /// The associated data this row's secret was sealed under.
    #[must_use]
    pub fn associated_data(&self, tenant: &str) -> String {
        associated_data(tenant, self.id)
    }
}

/// Registers a site.
pub async fn register(
    conn: &mut sqlx::PgConnection,
    new: &NewConnector<'_>,
) -> Result<Uuid, ConnectorRefusal> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "INSERT INTO connectors \
         (id, kind, label, site_url, remote_version, api_key_id, signing_secret, \
          asset_group_ids, allow_all_groups, allow_original, allow_restore, config) VALUES (",
    );
    let mut values = builder.separated(", ");
    values.push_bind(new.id);
    values.push_bind(new.kind.as_str());
    values.push_bind(new.label.trim());
    values.push_bind(new.site_url.trim_end_matches('/'));
    values.push_bind(new.remote_version);
    values.push_bind(new.api_key_id);
    values.push_bind(new.sealed_secret);
    values.push_bind(new.asset_group_ids.to_vec());
    values.push_bind(new.allow_all_groups);
    values.push_bind(new.allow_original);
    values.push_bind(new.allow_restore);
    values.push_bind(&new.config);
    builder.push(") RETURNING id");

    match builder.build_query_scalar().fetch_one(&mut *conn).await {
        Ok(id) => Ok(id),
        Err(error) => Err(classify(
            error,
            new.kind.as_str(),
            new.site_url.trim_end_matches('/'),
        )),
    }
}

/// Every connector, newest first.
pub async fn all(conn: &mut sqlx::PgConnection) -> Result<Vec<Connector>, Error> {
    let rows = sqlx::query_as::<_, Row>(SELECT_ALL)
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter().map(hydrate).collect()
}

/// One connector.
pub async fn by_id(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Option<Connector>, Error> {
    let row = sqlx::query_as::<_, Row>(SELECT_ONE)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    row.map(hydrate).transpose()
}

/// Replaces the signing secret.
///
/// `keep_previous` decides whether the superseded secret keeps verifying for [`SECRET_GRACE`]. Two different
/// situations, and assuming either would be wrong in the other:
///
/// - **A scheduled rotation** keeps it, because the site's configuration change is a separate deploy and a
///   rotation with no window is an outage.
/// - **A leak** does not. The whole point of rotating then is that the old secret must stop working, and a
///   week of grace is a week of forgery.
pub async fn rotate(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    sealed_secret: &str,
    keep_previous: bool,
    now: DateTime<Utc>,
) -> Result<(), ConnectorRefusal> {
    // Refused rather than silently rotated: a revoked connector's secret is already out there, and handing it
    // a working one would bring every URL the remote ever signed back to life.
    let current = by_id(&mut *conn, id)
        .await?
        .ok_or(ConnectorRefusal::Unknown(id))?;
    if current.status == Status::Revoked {
        return Err(ConnectorRefusal::Revoked(id));
    }

    sqlx::query(
        "UPDATE connectors SET \
            previous_signing_secret = CASE WHEN $3 THEN signing_secret ELSE NULL END, \
            signing_secret = $2, \
            secret_rotated_at = $4, \
            updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(sealed_secret)
    .bind(keep_previous)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(())
}

/// Pauses, resumes, or revokes.
///
/// Revoking clears both secrets in the same statement. Leaving them would mean a row that could be edited back
/// to `active` and immediately honour every URL the remote had ever signed; there is nothing to keep them for,
/// because a revoked connector is never reactivated.
pub async fn set_status(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    status: Status,
) -> Result<bool, ConnectorRefusal> {
    let affected = sqlx::query(
        "UPDATE connectors SET \
            status = $2, \
            signing_secret = CASE WHEN $2 = 'revoked' THEN '' ELSE signing_secret END, \
            previous_signing_secret = CASE WHEN $2 = 'revoked' THEN NULL \
                                           ELSE previous_signing_secret END, \
            updated_at = now() \
         WHERE id = $1 AND status <> 'revoked'",
    )
    .bind(id)
    .bind(status.as_str())
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?
    .rows_affected();
    Ok(affected > 0)
}

/// Records that the remote called, and what version it is running.
///
/// The version matters more than the timestamp: "drupal 11.1 / damrs_dam 1.2.0" is what an operator needs when
/// a site starts failing, and it is the one fact damrs cannot infer.
pub async fn seen(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    remote_version: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE connectors SET last_seen_at = $2, \
            remote_version = coalesce($3, remote_version), \
            last_error = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(now)
    .bind(remote_version)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Records a failure against a connector, without changing its status.
///
/// Separate from [`set_status`] because the two are different decisions: something went wrong, and whether
/// that should stop the connector working. Collapsing them would mean one bad response from a site takes it
/// offline.
pub async fn record_error(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    error: &str,
) -> Result<(), Error> {
    sqlx::query("UPDATE connectors SET last_error = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The column list, spelled once.
///
/// Two whole queries rather than a prefix plus a suffix built with `format!`: sqlx takes a `&'static str`, and
/// composing SQL out of runtime strings is the door this codebase does not want open — even when both halves
/// are its own literals.
const SELECT_ALL: &str = "SELECT id, kind, label, site_url, remote_version, api_key_id, \
                                 signing_secret, previous_signing_secret, secret_rotated_at, \
                                 asset_group_ids, allow_all_groups, allow_original, allow_restore, \
                                 config, status, last_seen_at, last_error, created_at \
                          FROM connectors ORDER BY created_at DESC";

const SELECT_ONE: &str = "SELECT id, kind, label, site_url, remote_version, api_key_id, \
                                 signing_secret, previous_signing_secret, secret_rotated_at, \
                                 asset_group_ids, allow_all_groups, allow_original, allow_restore, \
                                 config, status, last_seen_at, last_error, created_at \
                          FROM connectors WHERE id = $1";

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    kind: String,
    label: String,
    site_url: String,
    remote_version: Option<String>,
    api_key_id: Option<Uuid>,
    signing_secret: String,
    previous_signing_secret: Option<String>,
    secret_rotated_at: Option<DateTime<Utc>>,
    asset_group_ids: Vec<Uuid>,
    allow_all_groups: bool,
    allow_original: bool,
    allow_restore: bool,
    config: serde_json::Value,
    status: String,
    last_seen_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    created_at: DateTime<Utc>,
}

fn hydrate(row: Row) -> Result<Connector, Error> {
    Ok(Connector {
        // An unreadable `kind` or `status` is a row this build cannot reason about, and guessing either would
        // mean rendering for a connector whose rules we do not know. `Error::Inconsistent` says which column.
        kind: Kind::parse(&row.kind)
            .ok_or_else(|| Error::Inconsistent(format!("connectors.kind holds {:?}", row.kind)))?,
        status: Status::parse(&row.status).ok_or_else(|| {
            Error::Inconsistent(format!("connectors.status holds {:?}", row.status))
        })?,
        id: row.id,
        label: row.label,
        site_url: row.site_url,
        remote_version: row.remote_version,
        api_key_id: row.api_key_id,
        sealed_secret: row.signing_secret,
        previous_sealed_secret: row.previous_signing_secret,
        secret_rotated_at: row.secret_rotated_at,
        asset_group_ids: row.asset_group_ids,
        allow_all_groups: row.allow_all_groups,
        allow_original: row.allow_original,
        allow_restore: row.allow_restore,
        config: row.config,
        last_seen_at: row.last_seen_at,
        last_error: row.last_error,
        created_at: row.created_at,
    })
}

fn classify(error: sqlx::Error, kind: &str, site_url: &str) -> ConnectorRefusal {
    let Some(db) = error.as_database_error() else {
        return ConnectorRefusal::Database(Error::from(error));
    };
    match db.constraint() {
        Some("connectors_site_idx") => ConnectorRefusal::AlreadyConnected {
            kind: kind.to_owned(),
            site_url: site_url.to_owned(),
        },
        Some(name) => ConnectorRefusal::Invalid(name.to_owned()),
        None => ConnectorRefusal::Database(Error::from(error)),
    }
}
