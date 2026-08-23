//! Connected sites over HTTP (M3d·1, §11).
//!
//! ## Registration composes the ordinary machinery rather than adding a second one
//!
//! A connector needs to authenticate and to be scoped to asset groups. Both of those already exist: an
//! identity, a membership, a role carrying the groups, and an API key. So registering a site creates exactly
//! those four things and nothing new — which means a connector's reads go through `access::push_asset_filter`
//! like everybody else's.
//!
//! The alternative would be a connector-shaped authorisation path, and this codebase has spent a lot of effort
//! not having two places where access is decided. §11.1 is explicit about the property it buys: "a
//! misconfigured Drupal view cannot surface an unapproved asset, because the ABAC predicate already excluded
//! it." That is only true if the connector goes through the predicate.
//!
//! ## The service account is deliberately not a person
//!
//! It needs an email because `identities.email` is unique and not null, so it gets one at `.invalid` — the
//! reserved TLD (RFC 2606) that can never resolve and can never receive mail. A synthetic address in a real
//! domain would eventually be somebody's, and an address that could receive a password reset for a service
//! account is a way in.
//!
//! ## Two secrets, two lifetimes, both shown once
//!
//! The **API key** is how the remote calls damrs. The **signing secret** is how the remote signs render URLs
//! itself, so a page render never blocks on an API call (§11.3) — and it is therefore a forgery capability for
//! whatever the connector is allowed to render. Neither is ever readable again: the key is stored as a hash and
//! the secret is sealed, and a response that could return either would make a database read enough to
//! impersonate a site.
//!
//! ## Rotation asks which situation it is
//!
//! A scheduled rotation keeps the old secret verifying for a week, because the site's configuration change is
//! a separate deploy. A leak does not, because that week would be a week of forgery. The endpoint takes the
//! answer rather than picking one — see `dam_db::connectors::rotate`.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::Secret;
use dam_core::policy::Action;
use dam_core::sealed::SealingKeyring;
use dam_db::connectors::{self, ConnectorRefusal, Kind, NewConnector, Status};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// The reserved TLD a service account's address lives in.
///
/// RFC 2606 guarantees `.invalid` never resolves, so this address can never be delivered to and can never
/// receive a password reset. A synthetic address in a real domain eventually belongs to somebody.
const SERVICE_ACCOUNT_DOMAIN: &str = "connectors.invalid";

pub struct ConnectorState {
    pub global: PgPool,
    /// Seals the signing secret. The deployment's keyring, built once at startup.
    pub keyring: SealingKeyring,
}

impl std::fmt::Debug for ConnectorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectorState").finish_non_exhaustive()
    }
}

pub fn router(state: ConnectorState) -> Router {
    Router::new()
        .route("/connectors", get(list).post(register))
        .route("/connectors/{id}", get(read))
        .route("/connectors/{id}/rotate", post(rotate))
        .route("/connectors/{id}/status", post(set_status))
        .with_state(Arc::new(state))
}

/// A connected site, as an operator sees it.
///
/// Carries no secret and no ciphertext. The sealed form is not itself dangerous, but putting it in the response
/// of an endpoint an administration screen polls is how it ends up in a log, a browser cache and a bug report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConnectorView {
    pub id: Uuid,
    /// `drupal`, `wordpress`, `adobe_cc`, `figma`, `hubspot`, `salesforce` or `generic`.
    pub kind: String,
    pub label: String,
    pub site_url: String,
    /// What the remote says it is running, e.g. `drupal 11.1 / damrs_dam 1.2.0`. The one fact damrs cannot
    /// infer, and the first thing worth knowing when a site starts failing.
    pub remote_version: Option<String>,
    /// `active`, `paused`, `error` or `revoked`.
    pub status: String,
    /// Whether URLs this connector signed are still honoured. `error` still renders — whatever went wrong is
    /// not a reason to blank the images on somebody's home page.
    pub may_render: bool,
    pub allow_original: bool,
    pub allow_restore: bool,
    pub all_asset_groups: bool,
    pub asset_group_ids: Vec<Uuid>,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    /// When the signing secret was last replaced.
    pub secret_rotated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether the superseded secret is still inside its grace window *right now*. Computed rather than
    /// stored, so a screen can say "the old secret stops working on Friday" instead of "a secret was rotated".
    pub previous_secret_live: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A site to connect.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
pub struct RegisterBody {
    /// `drupal`, `wordpress`, `adobe_cc`, `figma`, `hubspot`, `salesforce` or `generic`.
    pub kind: String,
    pub label: String,
    /// The remote's canonical origin. Also the CORS allowlist entry for the asset picker and the audience of
    /// issued tokens, so it has to be the origin the browser will actually send.
    pub site_url: String,
    /// Which asset groups the site may see. Empty with `all_asset_groups` false means it sees nothing, which is
    /// a legitimate way to register a connector before deciding what it may have.
    #[serde(default)]
    pub asset_group_ids: Vec<Uuid>,
    #[serde(default)]
    pub all_asset_groups: bool,
    /// May it fetch masters? Off unless asked for: a CMS wants renditions, and a site that can fetch originals
    /// is a site that can leak the deliverable a customer paid for.
    #[serde(default)]
    pub allow_original: bool,
    /// May a render trigger a restore? Off unless asked for, and §11.1 is emphatic: a page render must never
    /// wake Glacier.
    #[serde(default)]
    pub allow_restore: bool,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

/// What a registration returns, once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RegisteredView {
    pub connector: ConnectorView,
    /// The API key the remote authenticates with. Stored as a hash, so this is the only time it exists.
    pub api_key: String,
    /// The secret the remote signs render URLs with. Stored sealed, so this is the only time it is readable.
    pub signing_secret: String,
    /// Said in the response body, because a UI that forgets to say it produces a support ticket a week later.
    pub warning: String,
}

const SHOWN_ONCE: &str = "The API key and signing secret are shown only here. The key is stored as a hash \
                          and the secret is encrypted at rest, so neither can be read back — a lost one is \
                          replaced, not recovered.";

#[utoipa::path(
    get,
    path = "/connectors",
    responses((status = 200, body = [ConnectorView])),
    tag = "connectors",
)]
pub async fn list(
    State(state): State<Arc<ConnectorState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ConnectorView>>, Failure> {
    // Manage: a connected site is a standing grant of read access to part of the library, and seeing which
    // sites hold one is administration.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = connectors::all(conn.executor()).await?;
    conn.commit().await?;
    let now = chrono::Utc::now();
    Ok(Json(rows.into_iter().map(|row| view(&row, now)).collect()))
}

#[utoipa::path(
    get,
    path = "/connectors/{id}",
    responses(
        (status = 200, body = ConnectorView),
        (status = 404, description = "No such connector"),
    ),
    tag = "connectors",
)]
pub async fn read(
    State(state): State<Arc<ConnectorState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ConnectorView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = connectors::by_id(conn.executor(), id).await?;
    conn.commit().await?;
    found
        .map(|row| Json(view(&row, chrono::Utc::now())))
        .ok_or(Failure::NotFound)
}

#[utoipa::path(
    post,
    path = "/connectors",
    request_body = RegisterBody,
    responses(
        (status = 201, body = RegisteredView),
        (status = 409, description = "That kind is already connected to that site"),
        (status = 422, description = "Not a connector kind, or a label or site URL that will not do"),
    ),
    tag = "connectors",
)]
pub async fn register(
    State(state): State<Arc<ConnectorState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterBody>,
) -> Result<(StatusCode, Json<RegisteredView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;

    let kind = Kind::parse(body.kind.trim()).ok_or_else(|| {
        Failure::Unprocessable(format!(
            "{:?} is not a connector kind; use drupal, wordpress, adobe_cc, figma, hubspot, \
             salesforce or generic",
            body.kind
        ))
    })?;
    if body.label.trim().is_empty() {
        return Err(Failure::Unprocessable(
            "a connector needs a label; it is how an operator recognises the site later".to_owned(),
        ));
    }
    let site_url = body.site_url.trim().trim_end_matches('/').to_owned();
    // Checked here rather than left to a constraint, because this string is a CORS allowlist entry and an
    // audience claim. A value that is not an absolute http(s) origin would be a silent mismatch at request
    // time rather than a refusal now.
    let origin = url::Url::parse(&site_url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .filter(|url| url.host_str().is_some())
        .ok_or_else(|| {
            Failure::Unprocessable(format!(
                "{site_url:?} is not an absolute http or https origin, and this value is used as a \
                 CORS allowlist entry"
            ))
        })?;

    // The id first: it is part of the associated data the secret is sealed under, so it has to exist before
    // the secret does. See `dam_db::connectors::NewConnector`.
    let id = Uuid::now_v7();
    let secret = dam_db::auth::ApiKey::generate().into_plaintext();
    let sealed = state
        .keyring
        .seal(
            &Secret::new(secret.clone()),
            &connectors::associated_data(caller.tenant_slug.as_str(), id),
        )
        .map_err(|error| {
            tracing::error!(%error, "sealing a connector signing secret");
            Failure::Internal
        })?;

    // The service account and its scoping, in one transaction with the connector row. A failure part-way
    // through would otherwise leave a key that authenticates as an identity with no role — which fails closed,
    // but as a permissions mystery rather than as a registration that did not happen.
    let api_key = dam_db::auth::ApiKey::generate();
    // The key's row id, generated here because the connector row references it and both writes have to agree.
    // `api_keys.id` has no FK from the tenant schema (0002), so nothing else enforces that agreement.
    let api_key_id = Uuid::now_v7();
    let email = format!("connector+{id}@{SERVICE_ACCOUNT_DOMAIN}");
    let role_key = format!("connector:{id}");

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // Permissions kept to the minimum the connector actually uses. Rendering does not go through the API at
    // all — the remote signs its own URLs — so read is enough unless it may fetch masters.
    let mut permissions = vec!["asset:read".to_owned()];
    if body.allow_original {
        permissions.push("asset:download".to_owned());
    }
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5)",
    )
    .bind(&role_key)
    .bind(format!("{} (connector)", body.label.trim()))
    .bind(&permissions)
    .bind(&body.asset_group_ids)
    .bind(body.all_asset_groups)
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;

    let registered = connectors::register(
        conn.executor(),
        &NewConnector {
            id,
            kind,
            label: &body.label,
            site_url: &site_url,
            remote_version: None,
            api_key_id: Some(api_key_id),
            sealed_secret: &sealed,
            asset_group_ids: &body.asset_group_ids,
            allow_all_groups: body.all_asset_groups,
            allow_original: body.allow_original,
            allow_restore: body.allow_restore,
            config: body.config.clone().unwrap_or_else(|| serde_json::json!({})),
        },
    )
    .await
    .map_err(Refused);
    let registered = match registered {
        Ok(id) => id,
        Err(failure) => {
            // Rolled back explicitly rather than dropped, so the role does not survive a refused registration.
            conn.rollback().await?;
            return Err(failure.into());
        }
    };
    let row = connectors::by_id(conn.executor(), registered)
        .await?
        .ok_or(Failure::Internal)?;
    conn.commit().await?;

    // The identity, membership and key live in the control plane, so they cannot be in the tenant transaction
    // above. Written after it commits: an identity with no connector is inert, whereas a connector whose key
    // does not exist is a site that cannot call.
    let identity_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $2) RETURNING id",
    )
    .bind(&email)
    .bind(format!(
        "{} ({})",
        body.label.trim(),
        origin.host_str().unwrap_or_default()
    ))
    .fetch_one(&state.global)
    .await
    .map_err(dam_db::Error::from)?;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, ARRAY[$3], false)",
    )
    .bind(caller.tenant_id)
    .bind(identity_id)
    .bind(&role_key)
    .execute(&state.global)
    .await
    .map_err(dam_db::Error::from)?;
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(api_key_id)
    .bind(caller.tenant_id)
    .bind(identity_id)
    .bind(format!("connector: {}", body.label.trim()))
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .bind(&permissions)
    .execute(&state.global)
    .await
    .map_err(dam_db::Error::from)?;

    Ok((
        StatusCode::CREATED,
        Json(RegisteredView {
            connector: view(&row, chrono::Utc::now()),
            api_key: api_key.into_plaintext(),
            signing_secret: secret,
            warning: SHOWN_ONCE.to_owned(),
        }),
    ))
}

/// Whether the superseded secret keeps working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
pub struct RotateBody {
    /// `true` for a scheduled rotation: the old secret keeps verifying for a week while the site's own
    /// configuration change is deployed. `false` when the secret has leaked — then the week would be a week of
    /// forgery.
    ///
    /// No default. The two situations want opposite answers, and picking one for a caller who did not say
    /// would be wrong half the time in the direction that matters.
    pub keep_previous: bool,
}

#[utoipa::path(
    post,
    path = "/connectors/{id}/rotate",
    request_body = RotateBody,
    responses(
        (status = 200, body = RegisteredView, description = "The new secret, shown once"),
        (status = 404, description = "No such connector"),
        (status = 409, description = "The connector is revoked"),
    ),
    tag = "connectors",
)]
pub async fn rotate(
    State(state): State<Arc<ConnectorState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<RotateBody>,
) -> Result<Json<RegisteredView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let secret = dam_db::auth::ApiKey::generate().into_plaintext();
    let sealed = state
        .keyring
        .seal(
            &Secret::new(secret.clone()),
            &connectors::associated_data(caller.tenant_slug.as_str(), id),
        )
        .map_err(|error| {
            tracing::error!(%error, "sealing a rotated connector secret");
            Failure::Internal
        })?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    connectors::rotate(
        conn.executor(),
        id,
        &sealed,
        body.keep_previous,
        chrono::Utc::now(),
    )
    .await
    .map_err(Refused)?;
    let row = connectors::by_id(conn.executor(), id)
        .await?
        .ok_or(Failure::NotFound)?;
    conn.commit().await?;

    Ok(Json(RegisteredView {
        connector: view(&row, chrono::Utc::now()),
        // The API key is untouched by a secret rotation: they are separate credentials with separate reasons
        // to be replaced, and returning a blank here rather than a new key is what says so.
        api_key: String::new(),
        signing_secret: secret,
        warning: SHOWN_ONCE.to_owned(),
    }))
}

/// What to set a connector to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
pub struct StatusBody {
    /// `active`, `paused` or `revoked`. Not `error` — that is something the dispatcher records, not a state an
    /// operator sets.
    #[serde(with = "status_serde")]
    pub status: SettableStatus,
}

/// The statuses an operator may set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SettableStatus {
    Active,
    Paused,
    /// Terminal. Both secrets are cleared, and nothing brings it back — the secret is already out there, and
    /// reactivating would make every URL the remote ever signed live again.
    Revoked,
}

mod status_serde {
    use super::SettableStatus;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SettableStatus, D::Error>
    where
        D: Deserializer<'de>,
    {
        SettableStatus::deserialize(deserializer)
    }
}

#[utoipa::path(
    post,
    path = "/connectors/{id}/status",
    request_body = StatusBody,
    responses(
        (status = 200, body = ConnectorView),
        (status = 404, description = "No such connector, or it is already revoked"),
    ),
    tag = "connectors",
)]
pub async fn set_status(
    State(state): State<Arc<ConnectorState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<StatusBody>,
) -> Result<Json<ConnectorView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let status = match body.status {
        SettableStatus::Active => Status::Active,
        SettableStatus::Paused => Status::Paused,
        SettableStatus::Revoked => Status::Revoked,
    };

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    if !connectors::set_status(conn.executor(), id, status)
        .await
        .map_err(Refused)?
    {
        conn.rollback().await?;
        // One answer for "no such connector" and "already revoked". Revocation is terminal, so there is
        // nothing an operator can do about either, and distinguishing them says only that a revoked
        // registration once existed.
        return Err(Failure::NotFound);
    }
    let row = connectors::by_id(conn.executor(), id)
        .await?
        .ok_or(Failure::Internal)?;
    conn.commit().await?;
    Ok(Json(view(&row, chrono::Utc::now())))
}

fn view(row: &connectors::Connector, now: chrono::DateTime<chrono::Utc>) -> ConnectorView {
    ConnectorView {
        id: row.id,
        kind: row.kind.as_str().to_owned(),
        label: row.label.clone(),
        site_url: row.site_url.clone(),
        remote_version: row.remote_version.clone(),
        status: row.status.as_str().to_owned(),
        may_render: row.status.may_render(),
        allow_original: row.allow_original,
        allow_restore: row.allow_restore,
        all_asset_groups: row.allow_all_groups,
        asset_group_ids: row.asset_group_ids.clone(),
        last_seen_at: row.last_seen_at,
        last_error: row.last_error.clone(),
        secret_rotated_at: row.secret_rotated_at,
        previous_secret_live: row.previous_is_live(now),
        created_at: row.created_at,
    }
}

/// Maps a refusal onto a status, keeping the database's own sentence.
struct Refused(ConnectorRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            ConnectorRefusal::Unknown(_) => Self::NotFound,
            ConnectorRefusal::Revoked(_) => Self::Conflict(refusal.to_string()),
            ConnectorRefusal::AlreadyConnected { .. } => Self::Conflict(refusal.to_string()),
            ConnectorRefusal::Invalid(_) => Self::Unprocessable(refusal.to_string()),
            ConnectorRefusal::Database(error) => Self::from(error),
        }
    }
}
