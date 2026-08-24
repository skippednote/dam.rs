//! Adding and removing the people who can use a tenant (G10·2a).
//!
//! ## `/members`, not `/people`
//!
//! `/people` already exists, in `comments`, and answers a different question: who can I mention. It is gated
//! on `Read`, because anybody writing a comment needs it. This answers who has access and with what role,
//! which is administration and takes `Manage` — and one path cannot carry two gates. `tenant_members` is the
//! schema's own word for the thing being administered.
//!
//! ## Every change is one transaction with its own audit entry
//!
//! `tenant_members` lives in `dam_global` and `audit_log` in the tenant schema, and they are two schemas in
//! one database — so the transaction `TenantConn` opens covers both. That removes a choice with no good
//! answer: audit first and a failed write leaves a permanent record, in an append-only log, of a grant that
//! never happened; effect first and a failed write leaves a grant with nothing saying who made it. See
//! `dam_db::members`, which is where the connector path's claim that the two "cannot be in the tenant
//! transaction" turns out to be about databases rather than schemas.
//!
//! ## Roles are checked against the tenant's own, and the unknown ones are named
//!
//! `role_names` has no foreign key and `auth` ignores a name it cannot resolve — correct there, a trap here.
//! Granting `editors` for a role called `editor` produces somebody who can see nothing, with nothing saying
//! why. A 422 listing the names is the only version of this that a person can act on.
//!
//! ## The response never says whether the person already existed
//!
//! `identities` is global and unique on the address, so adding somebody has to find-or-create. A 409 for
//! "already exists" — or a field saying so — would answer "does this company use damrs" about an address
//! somebody merely typed. The outcome reads the same either way.
//!
//! ## The credential is the invitation
//!
//! There is no login flow in this system: the web application authenticates with an API key. So adding
//! somebody mints one and shows it once, exactly as registering a connector does. Setting
//! `identities.status = 'invited'` instead would be marking a state nothing can leave.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::audit::{self, NewEntry};
use dam_db::members::{self, MemberRefusal, NewMember};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

const SHOWN_ONCE: &str = "This key is shown only here. It is stored as a hash and cannot be read back — a \
                          lost one is replaced, not recovered.";

pub struct MemberState {
    pub global: PgPool,
}

impl std::fmt::Debug for MemberState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberState").finish_non_exhaustive()
    }
}

pub fn router(state: MemberState) -> Router {
    Router::new()
        .route("/members", get(list).post(add))
        .route("/members/{identity_id}", patch(update).delete(remove))
        .route("/roles", get(roles))
        .with_state(Arc::new(state))
}

/// One person's access, as an administrator sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MemberView {
    pub identity_id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub role_names: Vec<String>,
    pub is_tenant_admin: bool,
    /// `active`, `disabled` or `invited`. Since authentication allowlists `active`, anything else here means
    /// this person's credentials do not work — which is a different fact from having no roles.
    pub status: String,
    /// Managed by an identity provider, so roles must be changed there rather than here.
    pub scim_managed: bool,
    pub last_login_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Credentials for this tenant that still work.
    ///
    /// Shown because it is the difference between an account that has been removed and one that has been
    /// marked removed: somebody with live keys and no roles still authenticates.
    pub live_keys: i64,
    pub joined_at: chrono::DateTime<chrono::Utc>,
}

fn view(member: members::Member) -> MemberView {
    MemberView {
        identity_id: member.identity_id,
        email: member.email,
        display_name: member.display_name,
        role_names: member.role_names,
        is_tenant_admin: member.is_tenant_admin,
        status: member.status,
        scim_managed: member.scim_managed,
        last_login_at: member.last_login_at,
        live_keys: member.live_keys,
        joined_at: member.joined_at,
    }
}

/// Everyone with access to this tenant.
#[utoipa::path(
    get,
    path = "/members",
    responses(
        (status = 200, body = Vec<MemberView>),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "members",
)]
pub async fn list(
    State(state): State<Arc<MemberState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<MemberView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = members::list(conn.executor(), caller.tenant_id).await?;
    conn.commit().await?;
    Ok(Json(found.into_iter().map(view).collect()))
}

/// The role keys this tenant defines, so a form can offer them rather than ask for a string.
#[utoipa::path(
    get,
    path = "/roles",
    responses(
        (status = 200, body = Vec<String>),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
    ),
    tag = "members",
)]
pub async fn roles(
    State(state): State<Arc<MemberState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let known = members::known_roles(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(known))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MemberAddBody {
    pub email: String,
    pub display_name: Option<String>,
    /// Role keys from `GET /roles`. An unknown one is a 422 naming it.
    #[serde(default)]
    pub role_names: Vec<String>,
    /// Whether they may administer the tenant — which includes adding and removing everybody else.
    #[serde(default)]
    pub is_tenant_admin: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MemberInvitedView {
    pub identity_id: Uuid,
    /// Their credential, in readable form, once.
    pub api_key: String,
    pub warning: String,
}

/// Gives somebody access to this tenant.
#[utoipa::path(
    post,
    path = "/members",
    request_body = MemberAddBody,
    responses(
        (status = 201, body = MemberInvitedView),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 409, description = "Already a member of this tenant"),
        (status = 422, description = "Not an address, or a role this tenant does not define"),
    ),
    tag = "members",
)]
pub async fn add(
    State(state): State<Arc<MemberState>>,
    headers: HeaderMap,
    Json(body): Json<MemberAddBody>,
) -> Result<(StatusCode, Json<MemberInvitedView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    check_roles(conn.executor(), &body.role_names).await?;

    let new = NewMember {
        email: body.email.clone(),
        display_name: body.display_name.clone(),
        role_names: body.role_names.clone(),
        is_tenant_admin: body.is_tenant_admin,
    };
    let added = members::add(conn.executor(), caller.tenant_id, &new)
        .await
        .map_err(Refused)?;

    record(
        conn.executor(),
        &caller,
        audit::Action::IdentityProvisioned,
        added.identity_id,
        serde_json::json!({
            "email": body.email.trim(),
            "role_names": body.role_names,
            "is_tenant_admin": body.is_tenant_admin,
            // Whether the identity pre-existed belongs in the record, which only this tenant's
            // administrators read — not in the response, which whoever typed the address gets back.
            "identity_existed": added.identity_existed,
        }),
    )
    .await?;

    conn.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(MemberInvitedView {
            identity_id: added.identity_id,
            api_key: added.api_key,
            warning: SHOWN_ONCE.to_owned(),
        }),
    ))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MemberUpdateBody {
    /// The complete set, not a patch: it is what the screen shows, and a partial update of an array is two
    /// round trips racing each other.
    pub role_names: Vec<String>,
    pub is_tenant_admin: bool,
}

/// Replaces somebody's roles.
#[utoipa::path(
    patch,
    path = "/members/{identity_id}",
    params(("identity_id" = Uuid, Path, description = "The member")),
    request_body = MemberUpdateBody,
    responses(
        (status = 200, body = MemberView),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "Not a member of this tenant"),
        (status = 409, description = "The tenant's only administrator, or an account the IdP owns"),
        (status = 422, description = "A role this tenant does not define"),
    ),
    tag = "members",
)]
pub async fn update(
    State(state): State<Arc<MemberState>>,
    headers: HeaderMap,
    Path(identity_id): Path<Uuid>,
    Json(body): Json<MemberUpdateBody>,
) -> Result<Json<MemberView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    check_roles(conn.executor(), &body.role_names).await?;

    let previous = members::set_roles(
        conn.executor(),
        caller.tenant_id,
        identity_id,
        &body.role_names,
        body.is_tenant_admin,
    )
    .await
    .map_err(Refused)?;

    // Two entries rather than one "changed", because a grant and a revocation are different questions and a
    // combined action would make "show me everything that was granted" unanswerable. Administrator counts as
    // a grant in one direction and a revocation in the other, so a change to it alone is still recorded.
    let gained: Vec<&String> = body
        .role_names
        .iter()
        .filter(|name| !previous.role_names.contains(name))
        .collect();
    let lost: Vec<&String> = previous
        .role_names
        .iter()
        .filter(|name| !body.role_names.contains(name))
        .collect();
    let was_admin = previous.is_tenant_admin;

    if !gained.is_empty() || (body.is_tenant_admin && !was_admin) {
        record(
            conn.executor(),
            &caller,
            audit::Action::RoleGranted,
            identity_id,
            serde_json::json!({
                "roles": gained,
                "tenant_admin": body.is_tenant_admin && !was_admin,
                "now_holds": body.role_names,
            }),
        )
        .await?;
    }
    if !lost.is_empty() || (was_admin && !body.is_tenant_admin) {
        record(
            conn.executor(),
            &caller,
            audit::Action::RoleRevoked,
            identity_id,
            serde_json::json!({
                "roles": lost,
                "tenant_admin": was_admin && !body.is_tenant_admin,
                "now_holds": body.role_names,
            }),
        )
        .await?;
    }

    let found = members::list(conn.executor(), caller.tenant_id).await?;
    conn.commit().await?;
    found
        .into_iter()
        .find(|member| member.identity_id == identity_id)
        .map(|member| Json(view(member)))
        .ok_or(Failure::NotFound)
}

/// Takes away somebody's access, and their credentials with it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MemberRemovedView {
    /// Credentials revoked. The number that makes "removed" mean something.
    pub keys_revoked: u64,
    /// Whether the account itself was disabled, which happens only when this was their last tenant.
    pub identity_disabled: bool,
}

#[utoipa::path(
    delete,
    path = "/members/{identity_id}",
    params(("identity_id" = Uuid, Path, description = "The member")),
    responses(
        (status = 200, body = MemberRemovedView),
        (status = 401, description = "No usable credential"),
        (status = 403, description = "Authenticated, and holds no manage scope"),
        (status = 404, description = "Not a member of this tenant"),
        (status = 409, description = "The tenant's only administrator"),
    ),
    tag = "members",
)]
pub async fn remove(
    State(state): State<Arc<MemberState>>,
    headers: HeaderMap,
    Path(identity_id): Path<Uuid>,
) -> Result<Json<MemberRemovedView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    let removed = members::remove(conn.executor(), caller.tenant_id, identity_id)
        .await
        .map_err(Refused)?;

    record(
        conn.executor(),
        &caller,
        audit::Action::IdentityDeprovisioned,
        identity_id,
        serde_json::json!({
            "keys_revoked": removed.keys_revoked,
            "identity_disabled": removed.identity_disabled,
            "roles_held": removed.roles_held,
            "was_tenant_admin": removed.was_tenant_admin,
            // Recorded because it is the caller's own account when it is, and an administrator removing
            // themselves is a thing somebody will later ask about.
            "removed_self": caller.identity_id == Some(identity_id),
        }),
    )
    .await?;

    conn.commit().await?;
    Ok(Json(MemberRemovedView {
        keys_revoked: removed.keys_revoked,
        identity_disabled: removed.identity_disabled,
    }))
}

/// Refuses role names this tenant does not define, naming them.
async fn check_roles(conn: &mut sqlx::PgConnection, wanted: &[String]) -> Result<(), Failure> {
    if wanted.is_empty() {
        return Ok(());
    }
    let known = members::known_roles(conn).await?;
    let missing = members::unknown_roles(wanted, &known);
    if missing.is_empty() {
        return Ok(());
    }
    Err(Failure::Unprocessable(format!(
        "this tenant has no role called {}",
        missing.join(", ")
    )))
}

struct Refused(MemberRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            MemberRefusal::NotAMember => Self::NotFound,
            MemberRefusal::AlreadyAMember
            | MemberRefusal::LastAdmin
            | MemberRefusal::ScimManaged => Self::Conflict(refusal.to_string()),
            MemberRefusal::EmailInvalid(_) | MemberRefusal::UnknownRoles(_) => {
                Self::Unprocessable(refusal.to_string())
            }
            MemberRefusal::Database(error) => Self::from(error),
        }
    }
}

/// Append a governance entry for a membership change.
async fn record(
    conn: &mut sqlx::PgConnection,
    caller: &caller::Caller,
    action: audit::Action,
    identity_id: Uuid,
    payload: serde_json::Value,
) -> Result<(), Failure> {
    audit::record(
        conn,
        NewEntry {
            action,
            actor_kind: if caller.identity_id.is_some() {
                audit::ActorKind::User
            } else {
                audit::ActorKind::ApiKey
            },
            actor_id: caller.identity_id,
            target_kind: "identity".to_owned(),
            target_id: Some(identity_id.to_string()),
            payload,
        },
    )
    .await?;
    Ok(())
}
