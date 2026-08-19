//! Comments over HTTP (Q.6b).
//!
//! ## Read, not Manage — and a person, not a key
//!
//! Commenting on something you can see is not administration, so `Read` is the bar; the dam-db layer already
//! refuses an asset the caller cannot see. But a comment is somebody's words, so a key with no identity behind it
//! is refused: an anonymous comment could never be edited, deleted or attributed by anyone.
//!
//! ## Names are resolved server-side
//!
//! A thread showing `author_id` as a uuid is unreadable, and making the client resolve them would be one request
//! per distinct person on the page. So each comment carries its author and recipients as `{id, name}`, from one
//! lookup per request against the global schema.
//!
//! ## `PATCH` does one thing or the other, not both
//!
//! Rewriting the words is the author's alone; moving the status is any reader's. Accepting both in one request
//! would mean a body that half-applies when the caller owns one right and not the other, so a request naming both
//! is refused outright rather than partially honoured.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_core::query::{Planned, Query as AssetQuery};
use dam_db::comments::{self, Comment, CommentRefusal, NewComment, Person, Status, Visibility};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the comment endpoints need.
pub struct CommentState {
    pub global: PgPool,
}

impl std::fmt::Debug for CommentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommentState").finish_non_exhaustive()
    }
}

/// The comment routes.
pub fn router(state: CommentState) -> Router {
    Router::new()
        .route("/assets/{asset_id}/comments", get(list).post(post_comment))
        .route(
            "/comments/{comment_id}",
            axum::routing::patch(amend).delete(remove),
        )
        .route("/people", get(people))
        .route("/me", get(me))
        .with_state(Arc::new(state))
}

/// A person, as a thread names them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PersonView {
    pub id: Uuid,
    /// The name to show, falling back to the email when nobody set one.
    pub name: String,
    /// Included because two colleagues can share a display name, and a picker that cannot tell them apart
    /// misroutes a private comment — the one failure this list exists to prevent.
    pub email: String,
}

/// One comment, as a thread draws it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CommentView {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub author: PersonView,
    pub body: String,
    /// `public` or `private`. A private comment reaches its author and its recipients, and nobody else.
    pub visibility: String,
    /// `open`, `resolved`, `approved` or `changes_requested`.
    pub status: String,
    /// Who last moved the status. Absent while it is still as posted.
    pub status_by: Option<PersonView>,
    /// The comment this replies to, if any. Threads are one level deep.
    pub parent_id: Option<Uuid>,
    /// Who this was addressed to. Routing — and, on a private comment, also who may read it.
    pub recipients: Vec<PersonView>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Set when the words changed after posting, so a thread can say "edited" rather than silently showing
    /// different text than whoever replied to it read.
    pub edited_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A comment to post.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PostCommentRequest {
    pub body: String,
    /// Defaults to public. Private is the deliberate choice, so it is the one you have to name.
    #[serde(default = "public")]
    pub visibility: String,
    /// Who to route it to. Required when `visibility` is `private`.
    #[serde(default)]
    pub recipients: Vec<Uuid>,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

fn public() -> String {
    "public".to_owned()
}

/// What to change about a comment. One of the two, never both — see the module docs.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AmendCommentRequest {
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// Every comment on an asset this caller may read.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/comments",
    responses(
        (status = 200, body = Vec<CommentView>),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "comments",
)]
pub async fn list(
    State(state): State<Arc<CommentState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Vec<CommentView>>, Failure> {
    let (caller, planned, reader) = context(&state, &headers).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = comments::on_asset(conn.executor(), asset_id, reader, &planned)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    present_all(&state, found).await
}

/// Posts a comment.
#[utoipa::path(
    post,
    path = "/assets/{asset_id}/comments",
    request_body = PostCommentRequest,
    responses(
        (status = 201, body = CommentView),
        (status = 404, description = "No such asset or parent comment, or not one this caller may see"),
        (status = 422, description = "Empty or over-long body, an unknown visibility, a private comment with no recipients, or a reply to a reply"),
    ),
    tag = "comments",
)]
pub async fn post_comment(
    State(state): State<Arc<CommentState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(request): Json<PostCommentRequest>,
) -> Result<(StatusCode, Json<CommentView>), Failure> {
    let (caller, planned, author) = context(&state, &headers).await?;
    let visibility = parse_visibility(&request.visibility)?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let posted = comments::post(
        conn.executor(),
        NewComment {
            asset_id,
            author_id: author,
            body: request.body,
            visibility,
            recipients: request.recipients,
            parent_id: request.parent_id,
        },
        &planned,
    )
    .await
    .map_err(Refused)?;
    conn.commit().await?;

    let Json(mut views) = present_all(&state, vec![posted]).await?;
    let view = views.pop().ok_or(Failure::Internal)?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Rewrites a comment's words, or moves its status.
#[utoipa::path(
    patch,
    path = "/comments/{comment_id}",
    request_body = AmendCommentRequest,
    responses(
        (status = 200, body = CommentView),
        (status = 403, description = "Rewriting somebody else's words"),
        (status = 404, description = "No such comment, or not one this caller may read"),
        (status = 422, description = "Both a body and a status, neither, an unknown status, or a bad length"),
    ),
    tag = "comments",
)]
pub async fn amend(
    State(state): State<Arc<CommentState>>,
    headers: HeaderMap,
    Path(comment_id): Path<Uuid>,
    Json(request): Json<AmendCommentRequest>,
) -> Result<Json<CommentView>, Failure> {
    let (caller, planned, actor) = context(&state, &headers).await?;

    // One or the other. The two carry different rights — the words are the author's, the status is any reader's —
    // so a request naming both would half-apply for a caller who holds one and not the other.
    let changed = match (&request.body, &request.status) {
        (Some(_), Some(_)) => {
            return Err(Failure::Unprocessable(
                "change the words or the status, not both in one request: they are different rights"
                    .to_owned(),
            ));
        }
        (None, None) => {
            return Err(Failure::Unprocessable(
                "nothing to change: name a body or a status".to_owned(),
            ));
        }
        pair => pair,
    };

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let after = match changed {
        (Some(body), _) => comments::amend(conn.executor(), comment_id, actor, body, &planned)
            .await
            .map_err(Refused)?,
        (_, Some(status)) => {
            let status = parse_status(status)?;
            comments::set_status(conn.executor(), comment_id, actor, status, &planned)
                .await
                .map_err(Refused)?
        }
        // Unreachable: the match above returned for the both-none case.
        (None, None) => return Err(Failure::Internal),
    };
    conn.commit().await?;

    let Json(mut views) = present_all(&state, vec![after]).await?;
    views.pop().map(Json).ok_or(Failure::Internal)
}

/// Deletes a comment and its replies. The author only.
#[utoipa::path(
    delete,
    path = "/comments/{comment_id}",
    responses(
        (status = 204, description = "Deleted, along with any replies to it"),
        (status = 403, description = "Deleting somebody else's comment"),
        (status = 404, description = "No such comment, or not one this caller may read"),
    ),
    tag = "comments",
)]
pub async fn remove(
    State(state): State<Arc<CommentState>>,
    headers: HeaderMap,
    Path(comment_id): Path<Uuid>,
) -> Result<StatusCode, Failure> {
    let (caller, planned, actor) = context(&state, &headers).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    comments::remove(conn.executor(), comment_id, actor, &planned)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Everyone in this tenant, for a recipient picker.
#[utoipa::path(
    get,
    path = "/people",
    responses((status = 200, body = Vec<PersonView>)),
    tag = "comments",
)]
pub async fn people(
    State(state): State<Arc<CommentState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<PersonView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    // This tenant's members only, and the tenant comes from the *credential* rather than from the request — there
    // is no parameter to point at another tenant with.
    let found = comments::people(&state.global, caller.tenant_id).await?;
    Ok(Json(found.into_iter().map(person).collect()))
}

/// Who the caller is.
///
/// Needed because a UI has to know which comments it may offer to edit. The alternative — offering Edit on
/// everything and letting the server refuse — puts a control on screen that exists only to fail, which teaches
/// people to distrust every other control beside it.
///
/// Answers about the caller and nobody else, so there is no id to pass and nothing to point at somebody with.
#[utoipa::path(
    get,
    path = "/me",
    responses(
        (status = 200, body = PersonView),
        (status = 403, description = "The key has no person behind it"),
    ),
    tag = "comments",
)]
pub async fn me(
    State(state): State<Arc<CommentState>>,
    headers: HeaderMap,
) -> Result<Json<PersonView>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let identity = caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))?;
    let found = comments::people_by_id(&state.global, &[identity]).await?;
    found
        .into_iter()
        .next()
        .map(|person| Json(self::person(person)))
        // Authenticated against an identity row that has since gone. A 403 rather than a 500: the credential is
        // no longer usable, which is a fact about the caller rather than a fault in the server.
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))
}

/// Authorises, resolves the person, and builds the plan every comment call needs.
async fn context(
    state: &Arc<CommentState>,
    headers: &HeaderMap,
) -> Result<(caller::Caller, Planned, Uuid), Failure> {
    let caller = caller::authorize(&state.global, headers, Action::Read).await?;
    // A comment is somebody's words. An anonymous one could never be edited, deleted or attributed by anyone, so
    // a key with no identity is refused rather than given a placeholder author.
    //
    // Enforced upstream in `caller::authorize`, which refuses such a key for every endpoint in the system — so
    // this is a fail-closed unwrap behind an existing guarantee, and mutating it changes no test outcome. Same
    // shape as the engagement handlers; the `Option` that makes both necessary is the TASKS.md cleanup item.
    let identity = caller
        .identity_id
        .ok_or(Failure::Refused(caller::Refusal::Forbidden))?;
    // `Query::All` plus the caller's predicate: there is no user query here, only the access filter — which is
    // what decides whether the asset under discussion exists as far as this caller is concerned.
    let planned = Planned::new(AssetQuery::All, caller.predicate.clone(), &[])
        .map_err(|_| Failure::Internal)?;
    Ok((caller, planned, identity))
}

/// Resolves every id a set of comments mentions, in one query, and renders them.
///
/// One lookup for the whole page rather than one per comment: a thread of twenty comments between four people
/// mentions four names, and asking twenty times would be nineteen wasted round trips.
async fn present_all(
    state: &Arc<CommentState>,
    found: Vec<Comment>,
) -> Result<Json<Vec<CommentView>>, Failure> {
    let mut ids: Vec<Uuid> = Vec::new();
    for comment in &found {
        ids.push(comment.author_id);
        ids.extend(comment.recipients.iter().copied());
        ids.extend(comment.status_by);
    }
    ids.sort_unstable();
    ids.dedup();

    let people = comments::people_by_id(&state.global, &ids).await?;
    let name = |id: Uuid| -> PersonView {
        people
            .iter()
            .find(|person| person.id == id)
            .cloned()
            .map_or_else(
                || PersonView {
                    id,
                    // An identity that no longer exists. Named as such rather than left blank: a comment whose
                    // author has been deleted is still a comment, and an empty name reads as a rendering fault.
                    name: "Someone no longer here".to_owned(),
                    email: String::new(),
                },
                person,
            )
    };

    Ok(Json(
        found
            .into_iter()
            .map(|comment| CommentView {
                id: comment.id,
                asset_id: comment.asset_id,
                author: name(comment.author_id),
                body: comment.body,
                visibility: comment.visibility.as_str().to_owned(),
                status: comment.status.as_str().to_owned(),
                status_by: comment.status_by.map(name),
                parent_id: comment.parent_id,
                recipients: comment.recipients.into_iter().map(name).collect(),
                created_at: comment.created_at,
                edited_at: comment.edited_at,
            })
            .collect(),
    ))
}

fn person(from: Person) -> PersonView {
    PersonView {
        id: from.id,
        name: from.display_name,
        email: from.email,
    }
}

fn parse_visibility(text: &str) -> Result<Visibility, Failure> {
    match text {
        "public" => Ok(Visibility::Public),
        "private" => Ok(Visibility::Private),
        other => Err(Failure::Unprocessable(format!(
            "visibility is `public` or `private`, not {other:?}"
        ))),
    }
}

fn parse_status(text: &str) -> Result<Status, Failure> {
    match text {
        "open" => Ok(Status::Open),
        "resolved" => Ok(Status::Resolved),
        "approved" => Ok(Status::Approved),
        "changes_requested" => Ok(Status::ChangesRequested),
        other => Err(Failure::Unprocessable(format!(
            "a status is open, resolved, approved or changes_requested, not {other:?}"
        ))),
    }
}

/// Maps a [`CommentRefusal`] onto a status.
struct Refused(CommentRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            // 404 for both, as the db layer already collapses them: splitting them here would rebuild the
            // existence oracle it exists to prevent.
            CommentRefusal::UnknownAsset(_) | CommentRefusal::UnknownComment(_) => Self::NotFound,
            // 403: the request is fine and the caller is the problem. Reachable only for a comment they can
            // already read, so it discloses nothing they did not know.
            CommentRefusal::NotYours => Self::Refused(caller::Refusal::Forbidden),
            CommentRefusal::BadLength(_)
            | CommentRefusal::TooDeep
            | CommentRefusal::PrivateWithNoRecipients => Self::Unprocessable(refusal.to_string()),
            CommentRefusal::Database(error) => error.into(),
        }
    }
}
