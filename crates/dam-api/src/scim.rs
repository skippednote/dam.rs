//! SCIM 2.0 over HTTP (G10·2b, RFC 7643/7644).
//!
//! `dam_db::scim` holds the reasoning about provisioning itself — why it mints no credential, why an external
//! id is scoped to its client, why a provider may only touch what it provisioned. This module is the wire
//! format, and the wire format is most of what makes a SCIM integration work or fail at setup.
//!
//! ## Its own authenticator, on its own prefix
//!
//! Every other route goes through `caller::authorize`, deliberately, because this codebase has one place where
//! access is decided. A SCIM client is the exception and the reason is structural rather than convenient: it
//! holds no ABAC predicate, has no membership, and administers identities rather than reading assets. There is
//! nothing for `authorize` to compile. Giving it a membership so it could go through that path would make the
//! provisioning system a *user of* the library it provisions.
//!
//! So `/scim/v2` authenticates against `scim_clients.token_hash` and reaches nothing else.
//!
//! ## The envelope is the integration
//!
//! A provider rejects a response it cannot parse, and the failures are silent from our side — the sync just
//! never works. The parts that are easy to get wrong and fatal when wrong:
//!
//! - **`status` in an error is a string**, not a number. `"404"`.
//! - **`Resources` is capitalised** in a `ListResponse`, and `startIndex` is 1-based.
//! - **`schemas` is required on every resource**, including the error and the list.
//! - **`meta.resourceType` and `meta.location`** are what a provider follows to re-read a user.
//! - **The response content type is `application/scim+json`.** Requests arrive with it too; axum's `Json`
//!   accepts it because it treats any `+json` suffix as JSON, which is the one part of this that is free.
//!
//! ## Entra sends `"False"`
//!
//! Microsoft Entra deprovisions by PATCHing `active`, and it sends the value as the *string* `"False"` rather
//! than the boolean `false`. A strict parse rejects it, the sync fails, and the symptom is an offboarded
//! employee who still has access — the exact failure SCIM was bought to prevent. So `active` is read from
//! either shape, and the string comparison is case-insensitive because the capitalisation has changed between
//! versions.
//!
//! Providers also disagree about `op`: the specification says lowercase, Entra capitalises. Matched
//! case-insensitively for the same reason.
//!
//! ## PATCH is not optional, and neither is DELETE
//!
//! Okta deprovisions with `DELETE`, Entra with `PATCH active: false`. Implementing one leaves the other
//! silently unable to offboard anybody, so both are here and both revoke credentials.

use crate::caller;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::scim::{self, Client, Filter, NewUser, ScimRefusal};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
const CONFIG_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
const SCIM_JSON: &str = "application/scim+json";

/// How many users a provider gets when it does not say.
const DEFAULT_COUNT: i64 = 100;

pub struct ScimState {
    pub global: PgPool,
}

impl std::fmt::Debug for ScimState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScimState").finish_non_exhaustive()
    }
}

pub fn router(state: ScimState) -> Router {
    Router::new()
        .route("/scim/v2/ServiceProviderConfig", get(config))
        .route("/scim/v2/Users", get(list).post(create))
        .route(
            "/scim/v2/Users/{id}",
            get(read).put(replace).patch(patch).delete(remove),
        )
        // Registering and revoking a client is administration, not provisioning, so it takes the ordinary
        // `Manage` gate rather than a SCIM token — a provisioning token must not be able to mint another.
        .route("/scim/clients", get(clients).post(register))
        .route("/scim/clients/{id}/revoke", post(revoke))
        .with_state(Arc::new(state))
}

// ─── the envelope ───────────────────────────────────────────────────────────

/// A SCIM error, in the shape a provider parses.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScimError {
    pub schemas: Vec<String>,
    /// A string, per RFC 7644 §3.12. A number here is rejected by strict providers, and the rejection looks
    /// like a network problem from their side.
    pub status: String,
    #[serde(rename = "scimType", skip_serializing_if = "Option::is_none")]
    pub scim_type: Option<String>,
    pub detail: String,
}

/// A refusal, rendered in SCIM's own error envelope.
///
/// Public because it appears in the handlers' return types, which are public so the router can be built from
/// outside — the same shape `assets::Failure` has.
#[derive(Debug)]
pub struct Refused(StatusCode, Option<&'static str>, String);

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let Refused(status, scim_type, detail) = self;
        let body = ScimError {
            schemas: vec![ERROR_SCHEMA.to_owned()],
            status: status.as_u16().to_string(),
            scim_type: scim_type.map(str::to_owned),
            detail,
        };
        scim_response(status, &body)
    }
}

/// Every response carries `application/scim+json`.
///
/// The specification says SHOULD, and providers vary in how much they care — but the ones that check reject
/// `application/json` outright, and a rejected response is a sync that never starts with no error on our side.
fn scim_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    let json = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [(header::CONTENT_TYPE, SCIM_JSON)],
        axum::body::Body::from(json),
    )
        .into_response()
}

impl From<ScimRefusal> for Refused {
    fn from(refusal: ScimRefusal) -> Self {
        match refusal {
            ScimRefusal::NoSuchUser => Refused(
                StatusCode::NOT_FOUND,
                None,
                "no such user in this tenant".to_owned(),
            ),
            ScimRefusal::AlreadyProvisioned => Refused(
                StatusCode::CONFLICT,
                // The specification's own type for this, which is what makes a provider treat it as "already
                // there, go and update instead" rather than as a failure to retry.
                Some("uniqueness"),
                refusal.to_string(),
            ),
            ScimRefusal::NotOurs => Refused(
                StatusCode::CONFLICT,
                Some("mutability"),
                refusal.to_string(),
            ),
            ScimRefusal::Invalid(detail) => {
                Refused(StatusCode::BAD_REQUEST, Some("invalidValue"), detail)
            }
            ScimRefusal::Member(inner) => Refused(
                StatusCode::BAD_REQUEST,
                Some("invalidValue"),
                inner.to_string(),
            ),
            ScimRefusal::Database(error) => {
                tracing::error!(%error, "a SCIM request failed against the database");
                Refused(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    "the request could not be completed".to_owned(),
                )
            }
        }
    }
}

impl From<dam_db::Error> for Refused {
    fn from(error: dam_db::Error) -> Self {
        Self::from(ScimRefusal::Database(error))
    }
}

// ─── the user resource ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Name {
    #[serde(rename = "formatted", skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Email {
    pub value: String,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Meta {
    #[serde(rename = "resourceType")]
    pub resource_type: String,
    pub created: String,
    #[serde(rename = "lastModified")]
    pub last_modified: String,
    /// Where to re-read this user. A provider follows it rather than reconstructing the path.
    pub location: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ScimUser {
    pub schemas: Vec<String>,
    /// Ours, and stable. A provider stores it and uses it for every later call.
    pub id: String,
    #[serde(rename = "externalId", skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(rename = "userName")]
    pub user_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Name>,
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub emails: Vec<Email>,
    pub active: bool,
    /// The tenant roles this person holds. Not a SCIM core attribute; carried so a provider that maps groups
    /// can see what it produced.
    pub roles: Vec<String>,
    pub meta: Meta,
}

fn view(user: &scim::User) -> ScimUser {
    ScimUser {
        schemas: vec![USER_SCHEMA.to_owned()],
        id: user.identity_id.to_string(),
        external_id: user.external_id.clone(),
        user_name: user.user_name.clone(),
        name: user.display_name.clone().map(|formatted| Name {
            formatted: Some(formatted),
        }),
        display_name: user.display_name.clone(),
        emails: vec![Email {
            value: user.user_name.clone(),
            primary: true,
        }],
        active: user.active,
        roles: user.roles.clone(),
        meta: Meta {
            resource_type: "User".to_owned(),
            created: user.created_at.to_rfc3339(),
            last_modified: user.updated_at.to_rfc3339(),
            location: format!("/scim/v2/Users/{}", user.identity_id),
        },
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ListResponse {
    pub schemas: Vec<String>,
    #[serde(rename = "totalResults")]
    pub total_results: i64,
    #[serde(rename = "startIndex")]
    pub start_index: i64,
    #[serde(rename = "itemsPerPage")]
    pub items_per_page: i64,
    /// Capitalised, per RFC 7644 §3.4.2. A lowercase `resources` is a provider that reads zero users and
    /// concludes the directory is empty — then creates everybody again.
    #[serde(rename = "Resources")]
    pub resources: Vec<ScimUser>,
}

// ─── endpoints ──────────────────────────────────────────────────────────────

/// What this implementation supports, which providers fetch before anything else.
#[utoipa::path(
    get,
    path = "/scim/v2/ServiceProviderConfig",
    responses((status = 200, description = "Capabilities")),
    tag = "scim",
)]
pub async fn config(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
) -> Result<Response, Refused> {
    // Authenticated, because the configuration names what a token can do and there is no reason for it to be
    // public. Providers send the token on this call too.
    let client = authenticate(&state, &headers).await?;
    scim::record_contact(&state.global, client.id, "config").await;
    Ok(scim_response(
        StatusCode::OK,
        &serde_json::json!({
            "schemas": [CONFIG_SCHEMA],
            "patch": { "supported": true },
            // Honestly false, all of them. A provider that believes we support bulk sends a bulk request and
            // gets a 404 it cannot explain; the specification's whole point here is to be believed.
            "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
            "filter": { "supported": true, "maxResults": 200 },
            "changePassword": { "supported": false },
            "sort": { "supported": false },
            "etag": { "supported": false },
            "authenticationSchemes": [{
                "type": "oauthbearertoken",
                "name": "OAuth Bearer Token",
                "description": "A provisioning token issued by this deployment.",
                "primary": true
            }],
            "meta": { "resourceType": "ServiceProviderConfig", "location": "/scim/v2/ServiceProviderConfig" }
        }),
    ))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListQuery {
    pub filter: Option<String>,
    #[serde(rename = "startIndex")]
    pub start_index: Option<i64>,
    pub count: Option<i64>,
}

/// Lists or searches users.
#[utoipa::path(
    get,
    path = "/scim/v2/Users",
    responses((status = 200, description = "A ListResponse")),
    tag = "scim",
)]
pub async fn list(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Response, Refused> {
    let client = authenticate(&state, &headers).await?;
    let filter = parse_filter(query.filter.as_deref())?;
    let start_index = query.start_index.unwrap_or(1);
    let count = query.count.unwrap_or(DEFAULT_COUNT);

    let mut conn = dam_db::TenantConn::begin(&state.global, &slug(&client)?).await?;
    let page = scim::page(
        conn.executor(),
        client.tenant_id,
        &filter,
        start_index,
        count,
    )
    .await?;
    conn.commit().await?;
    scim::record_contact(&state.global, client.id, "list").await;

    let resources: Vec<ScimUser> = page.users.iter().map(view).collect();
    Ok(scim_response(
        StatusCode::OK,
        &ListResponse {
            schemas: vec![LIST_SCHEMA.to_owned()],
            total_results: page.total,
            start_index: start_index.max(1),
            items_per_page: i64::try_from(resources.len()).unwrap_or(0),
            resources,
        },
    ))
}

/// Reads one user.
#[utoipa::path(
    get,
    path = "/scim/v2/Users/{id}",
    responses((status = 200, description = "The user")),
    tag = "scim",
)]
pub async fn read(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Refused> {
    let client = authenticate(&state, &headers).await?;
    let identity_id = parse_id(&id)?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &slug(&client)?).await?;
    let found = scim::by_id(conn.executor(), client.tenant_id, identity_id).await?;
    conn.commit().await?;
    scim::record_contact(&state.global, client.id, "read").await;
    match found {
        Some(user) => Ok(scim_response(StatusCode::OK, &view(&user))),
        None => Err(Refused::from(ScimRefusal::NoSuchUser)),
    }
}

/// What a provider sends to create or replace a user.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UserBody {
    #[serde(rename = "userName")]
    pub user_name: Option<String>,
    #[serde(rename = "externalId")]
    pub external_id: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    /// Absent means active: a provider creating somebody without saying is creating an enabled account.
    pub active: Option<serde_json::Value>,
    #[serde(default)]
    pub roles: Vec<String>,
}

impl UserBody {
    fn into_new(self) -> Result<NewUser, Refused> {
        let user_name = self.user_name.unwrap_or_default();
        if user_name.trim().is_empty() {
            return Err(Refused(
                StatusCode::BAD_REQUEST,
                Some("invalidValue"),
                "userName is required".to_owned(),
            ));
        }
        Ok(NewUser {
            user_name,
            external_id: self.external_id,
            display_name: self.display_name,
            active: self.active.as_ref().is_none_or(truthy),
            roles: self.roles,
        })
    }
}

/// Reads `active` from either a boolean or a string.
///
/// Entra sends the string `"False"`. A strict parse rejects it, the sync fails, and the symptom is an
/// offboarded employee who still has access — which is the failure SCIM was bought to prevent, so the lenient
/// read is the correct one. Case-insensitive because the capitalisation has changed between versions.
fn truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(flag) => *flag,
        serde_json::Value::String(text) => !text.eq_ignore_ascii_case("false"),
        // Anything else is a provider sending something the specification does not describe. Defaulting to
        // active would grant access on a malformed message, so it does not.
        _ => false,
    }
}

/// Creates a user.
#[utoipa::path(
    post,
    path = "/scim/v2/Users",
    request_body = UserBody,
    responses((status = 201, description = "Created")),
    tag = "scim",
)]
pub async fn create(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Json(body): Json<UserBody>,
) -> Result<Response, Refused> {
    let client = authenticate(&state, &headers).await?;
    require(&client, scim::USERS)?;
    let new = body.into_new()?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &slug(&client)?).await?;
    check_roles(conn.executor(), &new.roles).await?;
    let created = scim::provision(conn.executor(), &client, &new).await?;
    audit(
        conn.executor(),
        dam_db::audit::Action::IdentityProvisioned,
        created.identity_id,
        serde_json::json!({
            "user_name": created.user_name,
            "external_id": created.external_id,
            "roles": created.roles,
            "provider": client.label,
        }),
    )
    .await?;
    conn.commit().await?;
    scim::record_contact(&state.global, client.id, "create").await;
    Ok(scim_response(StatusCode::CREATED, &view(&created)))
}

/// Replaces a user, which is how Okta pushes an update.
#[utoipa::path(
    put,
    path = "/scim/v2/Users/{id}",
    request_body = UserBody,
    responses((status = 200, description = "Replaced")),
    tag = "scim",
)]
pub async fn replace(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UserBody>,
) -> Result<Response, Refused> {
    let client = authenticate(&state, &headers).await?;
    require(&client, scim::USERS)?;
    let identity_id = parse_id(&id)?;
    let new = body.into_new()?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &slug(&client)?).await?;
    check_roles(conn.executor(), &new.roles).await?;
    let before = scim::by_id(conn.executor(), client.tenant_id, identity_id).await?;
    let updated = scim::replace(conn.executor(), &client, identity_id, &new).await?;
    if before.is_some_and(|was| was.active) && !updated.active {
        audit_deactivation(conn.executor(), &client, &updated, "replace").await?;
    }
    conn.commit().await?;
    scim::record_contact(&state.global, client.id, "replace").await;
    Ok(scim_response(StatusCode::OK, &view(&updated)))
}

/// One PATCH operation.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PatchOperation {
    pub op: String,
    pub path: Option<String>,
    pub value: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PatchBody {
    #[serde(default)]
    pub schemas: Vec<String>,
    /// Capitalised, per RFC 7644 §3.5.2.
    #[serde(rename = "Operations", default)]
    pub operations: Vec<PatchOperation>,
}

/// Patches a user — the path Entra uses to deprovision.
#[utoipa::path(
    patch,
    path = "/scim/v2/Users/{id}",
    request_body = PatchBody,
    responses((status = 200, description = "Patched")),
    tag = "scim",
)]
pub async fn patch(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PatchBody>,
) -> Result<Response, Refused> {
    let client = authenticate(&state, &headers).await?;
    require(&client, scim::USERS)?;
    let identity_id = parse_id(&id)?;

    if !body.schemas.is_empty() && !body.schemas.iter().any(|s| s == PATCH_SCHEMA) {
        return Err(Refused(
            StatusCode::BAD_REQUEST,
            Some("invalidValue"),
            format!("a PatchOp must declare {PATCH_SCHEMA}"),
        ));
    }

    // Only `active` is honoured, and an operation naming anything else is refused rather than accepted and
    // dropped: a provider told its change applied will not send it again, so silently ignoring a PATCH is how
    // a directory and a library disagree permanently.
    let mut wanted: Option<bool> = None;
    for operation in &body.operations {
        // Case-insensitive: the specification says lowercase and Entra capitalises.
        if !operation.op.eq_ignore_ascii_case("replace")
            && !operation.op.eq_ignore_ascii_case("add")
        {
            return Err(Refused(
                StatusCode::BAD_REQUEST,
                Some("invalidValue"),
                format!("unsupported op `{}`", operation.op),
            ));
        }
        let path = operation.path.as_deref().unwrap_or_default();
        if path.eq_ignore_ascii_case("active") {
            wanted = Some(operation.value.as_ref().is_some_and(truthy));
        } else if path.is_empty() {
            // The pathless form: the value is an object of attributes. Entra sends both shapes.
            let Some(serde_json::Value::Object(map)) = &operation.value else {
                return Err(Refused(
                    StatusCode::BAD_REQUEST,
                    Some("invalidValue"),
                    "an operation with no path needs an object value".to_owned(),
                ));
            };
            if let Some(active) = map.get("active") {
                wanted = Some(truthy(active));
            }
        } else {
            return Err(Refused(
                StatusCode::BAD_REQUEST,
                Some("invalidValue"),
                format!("unsupported path `{path}`"),
            ));
        }
    }

    let Some(active) = wanted else {
        return Err(Refused(
            StatusCode::BAD_REQUEST,
            Some("invalidValue"),
            "no supported operation in this PatchOp".to_owned(),
        ));
    };

    let mut conn = dam_db::TenantConn::begin(&state.global, &slug(&client)?).await?;
    let updated = scim::set_active(conn.executor(), &client, identity_id, active).await?;
    if active {
        audit(
            conn.executor(),
            dam_db::audit::Action::IdentityReactivated,
            identity_id,
            serde_json::json!({ "user_name": updated.user_name, "provider": client.label }),
        )
        .await?;
    } else {
        audit_deactivation(conn.executor(), &client, &updated, "patch").await?;
    }
    conn.commit().await?;
    scim::record_contact(&state.global, client.id, "patch").await;
    Ok(scim_response(StatusCode::OK, &view(&updated)))
}

/// Removes a user, which is how Okta deprovisions.
#[utoipa::path(
    delete,
    path = "/scim/v2/Users/{id}",
    responses((status = 204, description = "Removed")),
    tag = "scim",
)]
pub async fn remove(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Refused> {
    let client = authenticate(&state, &headers).await?;
    require(&client, scim::USERS)?;
    let identity_id = parse_id(&id)?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &slug(&client)?).await?;
    let before = scim::by_id(conn.executor(), client.tenant_id, identity_id).await?;
    let removed = scim::deprovision(conn.executor(), &client, identity_id).await?;
    audit(
        conn.executor(),
        dam_db::audit::Action::IdentityDeprovisioned,
        identity_id,
        serde_json::json!({
            "user_name": before.map(|user| user.user_name),
            "keys_revoked": removed.keys_revoked,
            "identity_disabled": removed.identity_disabled,
            "roles_held": removed.roles_held,
            "provider": client.label,
            "via": "delete",
        }),
    )
    .await?;
    conn.commit().await?;
    scim::record_contact(&state.global, client.id, "delete").await;

    // 204, per RFC 7644 §3.6. A 200 with a body describing a user that no longer has access is a provider
    // parsing something it then has to reconcile.
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ─── administration of the clients themselves ───────────────────────────────

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ClientView {
    pub id: Uuid,
    pub label: String,
    pub scopes: Vec<String>,
    pub last_sync_at: Option<chrono::DateTime<chrono::Utc>>,
    /// What the provider last did, so a stalled integration is visible. `None` means it has never called.
    pub last_sync_status: Option<String>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RegisteredClient {
    pub client: ClientView,
    /// The token, in readable form, once.
    pub token: String,
    pub warning: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RegisterClientBody {
    pub label: String,
    /// `Users`, `Groups`. Defaults to `Users` alone, because that is what this implementation serves.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Lists the provisioning clients a tenant has.
#[utoipa::path(
    get,
    path = "/scim/clients",
    responses((status = 200, body = Vec<ClientView>)),
    tag = "scim",
)]
pub async fn clients(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ClientView>>, crate::assets::Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let found = scim::list(&state.global, caller.tenant_id).await?;
    Ok(Json(found.into_iter().map(client_view).collect()))
}

/// Registers a provisioning client and shows its token once.
#[utoipa::path(
    post,
    path = "/scim/clients",
    request_body = RegisterClientBody,
    responses((status = 201, body = RegisteredClient)),
    tag = "scim",
)]
pub async fn register(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Json(body): Json<RegisterClientBody>,
) -> Result<(StatusCode, Json<RegisteredClient>), crate::assets::Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    if body.label.trim().is_empty() {
        return Err(crate::assets::Failure::Unprocessable(
            "a provisioning client needs a label, so a stalled one can be identified".to_owned(),
        ));
    }
    let scopes = if body.scopes.is_empty() {
        vec![scim::USERS.to_owned()]
    } else {
        body.scopes.clone()
    };

    let (id, token) = scim::issue(&state.global, caller.tenant_id, &body.label, &scopes).await?;

    // In the tenant's own governance record, because a provisioning token can create and remove accounts.
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    dam_db::audit::record(
        conn.executor(),
        dam_db::audit::NewEntry {
            action: dam_db::audit::Action::KeyIssued,
            actor_kind: if caller.identity_id.is_some() {
                dam_db::audit::ActorKind::User
            } else {
                dam_db::audit::ActorKind::ApiKey
            },
            actor_id: caller.identity_id,
            target_kind: "scim_client".to_owned(),
            target_id: Some(id.to_string()),
            payload: serde_json::json!({ "label": body.label.trim(), "scopes": scopes }),
        },
    )
    .await?;
    conn.commit().await?;

    let listed = scim::list(&state.global, caller.tenant_id).await?;
    let client = listed
        .into_iter()
        .find(|found| found.id == id)
        .ok_or(crate::assets::Failure::Internal)?;

    Ok((
        StatusCode::CREATED,
        Json(RegisteredClient {
            client: client_view(client),
            token,
            warning:
                "This token is shown only here. It is stored as a hash and cannot be read back — a \
                      lost one is replaced, not recovered."
                    .to_owned(),
        }),
    ))
}

/// Revokes a provisioning client. Terminal.
#[utoipa::path(
    post,
    path = "/scim/clients/{id}/revoke",
    params(("id" = Uuid, Path, description = "The client")),
    responses((status = 200, description = "Revoked")),
    tag = "scim",
)]
pub async fn revoke(
    State(state): State<Arc<ScimState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, crate::assets::Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    // Scoped to the caller's tenant before anything else, or one tenant could revoke another's provisioning.
    let theirs = scim::list(&state.global, caller.tenant_id).await?;
    if !theirs.iter().any(|client| client.id == id) {
        return Err(crate::assets::Failure::NotFound);
    }
    if !scim::revoke(&state.global, id).await? {
        // Already revoked. Terminal, so there is nothing an operator can do about either answer.
        return Err(crate::assets::Failure::NotFound);
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    dam_db::audit::record(
        conn.executor(),
        dam_db::audit::NewEntry {
            action: dam_db::audit::Action::KeyRevoked,
            actor_kind: if caller.identity_id.is_some() {
                dam_db::audit::ActorKind::User
            } else {
                dam_db::audit::ActorKind::ApiKey
            },
            actor_id: caller.identity_id,
            target_kind: "scim_client".to_owned(),
            target_id: Some(id.to_string()),
            payload: serde_json::json!({}),
        },
    )
    .await?;
    conn.commit().await?;
    Ok(StatusCode::OK)
}

fn client_view(client: Client) -> ClientView {
    ClientView {
        id: client.id,
        label: client.label,
        scopes: client.scopes,
        last_sync_at: client.last_sync_at,
        last_sync_status: client.last_sync_status,
        revoked_at: client.revoked_at,
    }
}

// ─── plumbing ───────────────────────────────────────────────────────────────

async fn authenticate(state: &Arc<ScimState>, headers: &HeaderMap) -> Result<Client, Refused> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();

    if presented.is_empty() {
        return Err(unauthorized());
    }
    match scim::authenticate(&state.global, presented).await {
        Ok(Some(client)) => Ok(client),
        Ok(None) => Err(unauthorized()),
        Err(error) => Err(Refused::from(error)),
    }
}

/// One answer for every reason a token does not work, as `auth::authenticate` does: telling a prober which of
/// their guesses had the right shape hands them the cheap half of the search.
fn unauthorized() -> Refused {
    Refused(
        StatusCode::UNAUTHORIZED,
        None,
        "a valid provisioning token is required".to_owned(),
    )
}

fn require(client: &Client, resource: &str) -> Result<(), Refused> {
    if client.may(resource) {
        return Ok(());
    }
    Err(Refused(
        StatusCode::FORBIDDEN,
        None,
        format!("this token does not manage {resource}"),
    ))
}

fn slug(client: &Client) -> Result<dam_core::TenantSlug, Refused> {
    dam_core::TenantSlug::new(&client.tenant_slug).map_err(|error| {
        tracing::error!(%error, "a SCIM client names a tenant slug that will not parse");
        Refused::from(ScimRefusal::Invalid("unusable tenant".to_owned()))
    })
}

/// A SCIM `id` is ours, so a value that is not one of our ids is a 404 rather than a 400.
///
/// A provider sending a malformed id is a provider that stored something we never gave it, and the useful
/// answer is the same as for an id we have never issued: there is no such user.
fn parse_id(raw: &str) -> Result<Uuid, Refused> {
    Uuid::parse_str(raw).map_err(|_| Refused::from(ScimRefusal::NoSuchUser))
}

/// The two filters providers actually send, and a named refusal for anything else.
fn parse_filter(raw: Option<&str>) -> Result<Filter, Refused> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Filter::All);
    };
    let lowered = raw.to_ascii_lowercase();
    for (attribute, build) in [
        ("username eq ", Filter::UserName as fn(String) -> Filter),
        ("externalid eq ", Filter::ExternalId as fn(String) -> Filter),
    ] {
        if let Some(rest) = lowered.strip_prefix(attribute) {
            // Taken from the original rather than the lowered copy: an email is compared
            // case-insensitively by the query, but an `externalId` is an opaque provider string and
            // lowercasing it would stop matching.
            let value = raw[raw.len() - rest.len()..].trim().trim_matches('"');
            if value.is_empty() {
                break;
            }
            return Ok(build(value.to_owned()));
        }
    }
    // Refused by name rather than ignored: a filter we drop is a provider receiving the whole directory and
    // concluding every user already matches, which is how a sync becomes a no-op that looks healthy.
    Err(Refused(
        StatusCode::BAD_REQUEST,
        Some("invalidFilter"),
        format!("only `userName eq` and `externalId eq` are supported, not `{raw}`"),
    ))
}

/// Refuses role names this tenant does not define, naming them.
///
/// The same trap the human path documents: `role_names` has no foreign key and `auth` ignores a name it cannot
/// resolve, so a provider mapping a group onto `editors` when the role is `editor` provisions somebody who can
/// see nothing, with nothing anywhere saying why. A provider that receives a named 400 can fix its mapping; one
/// that receives a 201 cannot.
async fn check_roles(conn: &mut sqlx::PgConnection, wanted: &[String]) -> Result<(), Refused> {
    if wanted.is_empty() {
        return Ok(());
    }
    let known = dam_db::members::known_roles(conn)
        .await
        .map_err(Refused::from)?;
    let missing = dam_db::members::unknown_roles(wanted, &known);
    if missing.is_empty() {
        return Ok(());
    }
    Err(Refused(
        StatusCode::BAD_REQUEST,
        Some("invalidValue"),
        format!(
            "this tenant has no role called {}. It defines: {}",
            missing.join(", "),
            if known.is_empty() {
                "none yet".to_owned()
            } else {
                known.join(", ")
            }
        ),
    ))
}

async fn audit(
    conn: &mut sqlx::PgConnection,
    action: dam_db::audit::Action,
    identity_id: Uuid,
    payload: serde_json::Value,
) -> Result<(), Refused> {
    dam_db::audit::record(
        conn,
        dam_db::audit::NewEntry {
            action,
            // Not `User`: nobody pressed anything. The provider did it, and an audit row naming a person who
            // was asleep is worse than one naming a machine.
            actor_kind: dam_db::audit::ActorKind::System,
            actor_id: None,
            target_kind: "identity".to_owned(),
            target_id: Some(identity_id.to_string()),
            payload,
        },
    )
    .await
    .map_err(Refused::from)?;
    Ok(())
}

async fn audit_deactivation(
    conn: &mut sqlx::PgConnection,
    client: &Client,
    user: &scim::User,
    via: &str,
) -> Result<(), Refused> {
    audit(
        conn,
        dam_db::audit::Action::IdentityDeprovisioned,
        user.identity_id,
        serde_json::json!({
            "user_name": user.user_name,
            "provider": client.label,
            // The membership survives an `active: false`, which is the difference from a DELETE and the thing
            // somebody reading this later needs to know.
            "membership_kept": true,
            "via": via,
        }),
    )
    .await
}
