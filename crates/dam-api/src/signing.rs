//! A tenant's own C2PA signing identity — installed and stood down (the G10·3b management surface).
//!
//! The repository (`dam_db::signing_identities`) and the worker were already wired for per-tenant signing
//! identities; what was missing was the surface that installs one. This mirrors `POST /ai/credentials`: the
//! private key is sealed with the deployment's keyring before anything is stored, the certificate — which is
//! public, every verifier already holds it — is kept in the clear, and no response ever carries the key back.
//!
//! The certificate and key are validated **together** before storage: a chain that does not match its key, or
//! an algorithm outside the C2PA profile, is refused here rather than producing derivatives that verify
//! nowhere. That check builds a real signer from the inputs and throws it away — the cheapest proof they are
//! usable — reusing the same `dam_media::provenance` path the deployment identity is validated by at startup.

use crate::assets::Failure;
use crate::caller;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get};
use axum::{Json, Router};
use dam_core::Secret;
use dam_core::policy::Action;
use dam_core::sealed::SealingKeyring;
use dam_db::signing_identities::{self, NewIdentity};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the signing-identity endpoints need: the tenant database and the deployment keyring that seals the
/// private key. The same keyring `dam_ai` credentials use — a signing key and a provider key are sealed and
/// read back the same way.
pub struct SigningState {
    pub global: PgPool,
    pub keyring: SealingKeyring,
}

impl std::fmt::Debug for SigningState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningState").finish_non_exhaustive()
    }
}

pub fn router(state: SigningState) -> Router {
    Router::new()
        .route("/signing-identities", get(list).post(install))
        .route("/signing-identities/active", delete(stand_down))
        .with_state(Arc::new(state))
}

/// One identity as an administrator sees it. The certificate is public and returned; the key never is.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IdentityView {
    pub id: Uuid,
    pub label: String,
    /// The C2PA signing algorithm, e.g. `es256`.
    pub algorithm: String,
    /// The RFC 3161 timestamp authority URL, if one was configured.
    pub timestamp_authority: Option<String>,
    /// Whether this is the identity the tenant currently signs with.
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// The certificate chain, PEM. Public — every verifier of an asset signed with it already holds it.
    pub cert_pem: String,
}

impl From<signing_identities::SigningIdentity> for IdentityView {
    fn from(row: signing_identities::SigningIdentity) -> Self {
        Self {
            id: row.id,
            label: row.label,
            algorithm: row.algorithm,
            timestamp_authority: row.timestamp_authority,
            is_active: row.is_active,
            created_at: row.created_at,
            cert_pem: row.cert_pem,
        }
    }
}

/// An identity to install. The key is plaintext on the way in and sealed before anything is stored.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct NewIdentityRequest {
    /// A person's name for the identity, for the list.
    pub label: String,
    /// The certificate chain, PEM. Public.
    pub cert_pem: String,
    /// The private key, PEM. Sealed before storage and never returned.
    pub private_key_pem: String,
    /// A C2PA signing algorithm: es256, es384, es512, ps256, ps384, ps512, ed25519.
    pub algorithm: String,
    /// Optional RFC 3161 timestamp authority URL, so signatures outlive the certificate.
    #[serde(default)]
    pub timestamp_authority: Option<String>,
}

/// Every identity this tenant has had, newest first.
#[utoipa::path(
    get, path = "/signing-identities", tag = "signing",
    responses((status = 200, body = [IdentityView]), (status = 403, description = "Needs manage access")),
)]
pub async fn list(
    State(state): State<Arc<SigningState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<IdentityView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let rows = signing_identities::list(conn.executor()).await?;
    conn.commit().await?;
    Ok(Json(rows.into_iter().map(IdentityView::from).collect()))
}

/// Installs an identity and makes it the one that signs, standing the previous one down.
#[utoipa::path(
    post, path = "/signing-identities", tag = "signing",
    request_body = NewIdentityRequest,
    responses(
        (status = 201, body = IdentityView),
        (status = 422, description = "The certificate and key cannot sign, or a field is empty"),
        (status = 403, description = "Needs manage access"),
    ),
)]
pub async fn install(
    State(state): State<Arc<SigningState>>,
    headers: HeaderMap,
    Json(request): Json<NewIdentityRequest>,
) -> Result<(StatusCode, Json<IdentityView>), Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;

    if request.private_key_pem.trim().is_empty() || request.cert_pem.trim().is_empty() {
        return Err(Failure::Unprocessable(
            "both a certificate and a private key are required; nothing was stored".to_owned(),
        ));
    }

    // Validate the pair together before storing, so a mismatched key or an algorithm outside the C2PA profile
    // is refused here rather than at the thousandth derivative. Builds a real signer and drops it.
    dam_media::provenance::SigningIdentity::from_pem(
        request.cert_pem.as_bytes(),
        request.private_key_pem.as_bytes(),
        request.algorithm.trim(),
        request.timestamp_authority.clone(),
    )
    .map_err(|e| Failure::Unprocessable(format!("this certificate and key cannot sign: {e}")))?;

    // The id first: it is part of the associated data the key is sealed under, so it must exist before the
    // seal — the same reason `ai_credentials` mints it here.
    let id = Uuid::now_v7();
    let plaintext = Secret::new(request.private_key_pem.trim().to_owned());
    let aad = signing_identities::associated_data(caller.tenant_slug.as_str(), id);
    let sealed = state
        .keyring
        .seal(&plaintext, &aad)
        .map_err(|_| Failure::Internal)?;

    let new = NewIdentity {
        id,
        label: request.label.trim(),
        cert_pem: request.cert_pem.trim(),
        sealed_key: &sealed,
        sealing_key_id: state.keyring.current_key_id(),
        algorithm: request.algorithm.trim(),
        timestamp_authority: request.timestamp_authority.as_deref(),
    };
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let stored = signing_identities::activate(conn.executor(), &new).await?;
    conn.commit().await?;
    Ok((StatusCode::CREATED, Json(IdentityView::from(stored))))
}

/// Stands the active identity down; the tenant falls back to the deployment's identity rather than stopping
/// signing. 204 if there was one, 404 if there was not.
#[utoipa::path(
    delete, path = "/signing-identities/active", tag = "signing",
    responses(
        (status = 204, description = "Stood down; the deployment identity now applies"),
        (status = 404, description = "The tenant had no identity of its own"),
        (status = 403, description = "Needs manage access"),
    ),
)]
pub async fn stand_down(
    State(state): State<Arc<SigningState>>,
    headers: HeaderMap,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let stood_down = signing_identities::deactivate(conn.executor()).await?;
    conn.commit().await?;
    if stood_down {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Failure::NotFound)
    }
}
