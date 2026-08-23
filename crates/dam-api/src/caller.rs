//! Who is asking, and what they may see.
//!
//! One place, because the alternative is every handler re-deriving a predicate. §12's argument is that the
//! access rules are compiled once and reused, *because divergence is a data leak* — and a handler that
//! compiled its own would be a fourth consumer nobody counted.
//!
//! ## The predicate travels with the caller
//!
//! [`Caller`] carries the compiled [`AccessPredicate`], not the raw grants. A handler that received grants
//! would have to compile them, which is the same as deciding access — and it would be doing it for one
//! action, in one endpoint, where nobody would look for it later.
//!
//! ## A machine key grants nothing
//!
//! A key with no identity behind it has no membership and therefore no roles. That is fail-closed and
//! intended: issuing such a key today grants nothing, which is the safe direction for a shape the role model
//! does not yet describe.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use dam_core::policy::{self, AccessPredicate, Action};
use dam_db::auth;
use sqlx::PgPool;
use uuid::Uuid;

/// An authenticated caller and the scope they hold for one action.
#[derive(Debug, Clone)]
pub struct Caller {
    pub tenant_id: Uuid,
    pub tenant_slug: dam_core::TenantSlug,
    /// `Some` for a key issued to a person. A handler that writes an audit row needs this and must not
    /// invent one.
    pub identity_id: Option<Uuid>,
    pub api_key_id: Uuid,
    /// The compiled visibility scope. Every query this caller drives renders *this* value.
    pub predicate: AccessPredicate,
    /// The tenant roles this caller holds.
    ///
    /// Carried because sharing is expressed in terms of roles as well as identities: a saved search shared with
    /// `editors` is visible to whoever holds that role, and the dashboard cannot answer that from the predicate —
    /// which has already been compiled down to asset groups and knows nothing about role *names*.
    pub role_names: Vec<String>,
    /// The fine-grained permission strings this caller's active roles carry.
    ///
    /// Carried for the same reason as `role_names`: the compiled predicate has already been reduced to asset
    /// groups and knows nothing about the strings. Q.11's download formats are the first feature to need them —
    /// a "Print TIFF" may require `conversion:print`.
    ///
    /// **Narrowing only.** Nothing is granted by what is in here: a handler reaches these after `authorize` has
    /// allowed the action, and a permission can only remove a choice from what the predicate already permitted.
    pub permissions: Vec<String>,
}

/// Why a request was refused before it reached a handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No credential, or one that does not work. Every reason collapses here — unknown, revoked, expired,
    /// or belonging to a deleted tenant — because telling a prober which of their guesses had the right
    /// *shape* hands them the cheap half of the search.
    Unauthorized,
    /// Authenticated, and holds nothing for this action.
    Forbidden,
    /// The caller's *configuration* names something this build cannot honour — today, an asset group whose
    /// membership is a rule the query IR cannot yet render (task 2.4).
    ///
    /// Its own variant because the alternative collapsed it into `Internal`, and that defeated the point of
    /// refusing: the refusal exists so a half-supported configuration is *loud*, and a 500 with no body is
    /// indistinguishable from a crash. An administrator reading logs saw an incident where the honest answer
    /// was "this group needs a feature that does not exist yet".
    Unsupported(String),
    Internal,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        // 501 carries a body, and it is the one refusal here that should: it describes the *deployment's*
        // limitation rather than the tenant's data, and the person who can fix it is the one reading it.
        if let Self::Unsupported(reason) = self {
            return (
                StatusCode::NOT_IMPLEMENTED,
                axum::Json(serde_json::json!({ "reason": reason })),
            )
                .into_response();
        }
        let status = match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // No body for the rest. An error body there could only ever describe the tenant's state to a caller
        // who has already been refused.
        status.into_response()
    }
}

impl From<dam_db::Error> for Refusal {
    fn from(error: dam_db::Error) -> Self {
        match error {
            // Not an incident: a configuration this build cannot honour, deliberately refused. Logged at
            // `warn` because somebody should fix it, not paged because nothing is broken.
            dam_db::Error::Unsupported(reason) => {
                tracing::warn!(%reason, "refusing a caller whose configuration this build cannot render");
                Self::Unsupported(reason)
            }
            other => {
                tracing::error!(error = %other, "authenticating a request");
                Self::Internal
            }
        }
    }
}

/// Authenticates the bearer token and compiles the caller's scope for `action`.
///
/// Refuses a caller whose predicate matches nothing rather than handing back a scope that renders as
/// `false`. Both are safe; a 403 is the useful one, because an empty library and a refusal look identical
/// to a user and only one of them is actionable.
pub async fn authorize(
    global: &PgPool,
    headers: &HeaderMap,
    action: Action,
) -> Result<Caller, Refusal> {
    let presented = bearer(headers).ok_or(Refusal::Unauthorized)?;

    let authenticated = auth::authenticate(global, presented)
        .await?
        .ok_or(Refusal::Unauthorized)?;

    // See the module docs: no identity, no membership, no grants.
    let identity = authenticated.identity_id.ok_or(Refusal::Forbidden)?;

    // Read here as well as inside `grants_for`, because the two want different things from one row: that function
    // compiles roles into a predicate, and a caller needs the names themselves for anything shared *with a role*.
    // One extra read on a table already being consulted, rather than threading a second return value out of the
    // grant loader and changing every one of its callers.
    let role_names: Vec<String> = sqlx::query_scalar(
        "SELECT role_names FROM dam_global.tenant_members WHERE tenant_id = $1 AND identity_id = $2",
    )
    .bind(authenticated.tenant_id)
    .bind(identity)
    .fetch_optional(global)
    .await
    .map_err(dam_db::Error::from)?
    .unwrap_or_default();

    authorize_as(
        global,
        &Authorized {
            tenant_id: authenticated.tenant_id,
            tenant_slug: authenticated.tenant_slug,
            identity_id: identity,
            api_key_id: authenticated.api_key_id,
            scopes: authenticated.scopes,
            role_names,
        },
        action,
    )
    .await
}

/// An identity established by *something* — a bearer key, or a credential a connected site signed.
///
/// The split exists because §11.1's browse endpoint authenticates with a token a site minted itself, and the
/// grant loading, predicate compilation and both guards below must be the same code for both. A second path
/// that resolved a scope for a connector would be a second place access is decided, and the two would drift in
/// exactly the way this codebase keeps refusing to allow.
#[derive(Debug, Clone)]
pub struct Authorized {
    pub tenant_id: Uuid,
    pub tenant_slug: dam_core::TenantSlug,
    /// Required, not optional. A caller with no identity has no membership and therefore no grants (see the
    /// module docs), so there is nothing for this function to compile — `authorize` refuses that case before
    /// getting here.
    pub identity_id: Uuid,
    pub api_key_id: Uuid,
    /// Scopes narrowing the key, if any. A signed browse token carries none: it says which connector is
    /// calling and nothing about what that connector may see.
    pub scopes: Vec<String>,
    pub role_names: Vec<String>,
}

/// Compiles the scope an established identity holds for `action`.
pub async fn authorize_as(
    global: &PgPool,
    who: &Authorized,
    action: Action,
) -> Result<Caller, Refusal> {
    let scopes: Vec<&str> = who.scopes.iter().map(String::as_str).collect();
    // The role definitions live in the tenant schema and the membership in the global one, which is the D2
    // boundary rather than an accident — and the tenant side has to be a `TenantConn`, because an
    // unqualified `FROM roles` resolves through that transaction's `search_path`.
    let mut conn = dam_db::TenantConn::begin(global, &who.tenant_slug).await?;
    let grants = auth::grants_for(
        global,
        conn.executor(),
        who.tenant_id,
        who.identity_id,
        &scopes,
    )
    .await?;

    let now = chrono::Utc::now();
    // One instant for both, so a role expiring between the two cannot leave a caller holding a permission the
    // predicate no longer reflects.
    let permissions = grants.permissions_at(now);
    let predicate = policy::compile(&grants, action, now);
    if predicate.matches_nothing() {
        conn.commit().await?;
        return Err(Refusal::Forbidden);
    }

    // Refused rather than rendered: decision 4 says a rule-based group is evaluated live, and the language
    // its predicate is written in has no renderer yet. Ignoring the rule would grant *less* than the
    // administrator configured — fail-closed, but silently, so the first anyone would know is an asset that
    // should have been visible and was not.
    //
    // Inside the tenant transaction, because `asset_groups` is a tenant table. It used to run on the global
    // pool after this commit, where the unqualified name resolved against `dam_global` — so every
    // group-scoped caller got a 500 from the check meant to protect them.
    dam_db::access::check_groups_are_renderable(conn.executor(), &predicate).await?;
    conn.commit().await?;

    Ok(Caller {
        tenant_id: who.tenant_id,
        tenant_slug: who.tenant_slug.clone(),
        identity_id: Some(who.identity_id),
        api_key_id: who.api_key_id,
        predicate,
        role_names: who.role_names.clone(),
        permissions,
    })
}

/// The bearer token, if the header is well formed.
///
/// Case-insensitive on the scheme, because RFC 9110 says the scheme is case-insensitive and a client
/// sending `bearer` is not wrong — refusing it produces a 401 that no amount of checking the key explains.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            value.parse().expect("a header value"),
        );
        headers
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively() {
        assert_eq!(bearer(&headers("Bearer abc")), Some("abc"));
        assert_eq!(bearer(&headers("bearer abc")), Some("abc"));
        assert_eq!(bearer(&headers("BEARER abc")), Some("abc"));
    }

    #[test]
    fn anything_that_is_not_a_bearer_token_is_absent_rather_than_guessed_at() {
        assert_eq!(bearer(&headers("Basic abc")), None);
        assert_eq!(bearer(&headers("abc")), None, "no scheme at all");
        assert_eq!(
            bearer(&headers("Bearer ")),
            None,
            "an empty token is not a token"
        );
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_rather_than_presented_to_the_hash() {
        // A key pasted from a terminal picks up a trailing newline, and the hash of "k\n" is not the hash of
        // "k". The failure is a 401 that survives every attempt to check the key.
        assert_eq!(bearer(&headers("Bearer  abc ")), Some("abc"));
    }
}
