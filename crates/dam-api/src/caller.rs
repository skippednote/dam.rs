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
}

/// Why a request was refused before it reached a handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No credential, or one that does not work. Every reason collapses here — unknown, revoked, expired,
    /// or belonging to a deleted tenant — because telling a prober which of their guesses had the right
    /// *shape* hands them the cheap half of the search.
    Unauthorized,
    /// Authenticated, and holds nothing for this action.
    Forbidden,
    Internal,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // No body. An error body here could only ever describe the tenant's state to a caller who has
        // already been refused.
        status.into_response()
    }
}

impl From<dam_db::Error> for Refusal {
    fn from(error: dam_db::Error) -> Self {
        tracing::error!(%error, "authenticating a request");
        Self::Internal
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

    let scopes: Vec<&str> = authenticated.scopes.iter().map(String::as_str).collect();
    // The role definitions live in the tenant schema and the membership in the global one, which is the D2
    // boundary rather than an accident — and the tenant side has to be a `TenantConn`, because an
    // unqualified `FROM roles` resolves through that transaction's `search_path`.
    let mut conn = dam_db::TenantConn::begin(global, &authenticated.tenant_slug).await?;
    let grants = auth::grants_for(
        global,
        conn.executor(),
        authenticated.tenant_id,
        identity,
        &scopes,
    )
    .await?;
    conn.commit().await?;

    let predicate = policy::compile(&grants, action, chrono::Utc::now());
    if predicate.matches_nothing() {
        return Err(Refusal::Forbidden);
    }

    // Refused rather than rendered: decision 4 says a rule-based group is evaluated live, and the language
    // its predicate is written in has no renderer yet. Ignoring the rule would grant *less* than the
    // administrator configured — fail-closed, but silently, so the first anyone would know is an asset that
    // should have been visible and was not.
    dam_db::access::check_groups_are_renderable(global, &predicate).await?;

    Ok(Caller {
        tenant_id: authenticated.tenant_id,
        tenant_slug: authenticated.tenant_slug,
        identity_id: authenticated.identity_id,
        api_key_id: authenticated.api_key_id,
        predicate,
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
