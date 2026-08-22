//! Attached documents over HTTP (Q.9).
//!
//! ## Attaching is joining, as adding a version is
//!
//! `POST /assets/{id}/attachments` names an asset already uploaded through the ordinary route. Same reasoning as
//! versions: a second upload path is a second place for sniffing, probing and placement to diverge.
//!
//! ## Manage to attach, Read to see
//!
//! Attaching paperwork asserts something about an asset's rights, which is a content decision. Reading it is Read,
//! and deliberately no narrower than the asset — the paperwork exists to answer "may we use this", and a rights
//! question somebody cannot check is one they will answer by guessing.

use crate::assets::Failure;
use crate::caller;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use dam_core::policy::Action;
use dam_db::attachments::{self, Attachment, AttachmentRefusal, Kind};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

/// What the attachment endpoints need.
pub struct AttachmentState {
    pub global: PgPool,
}

impl std::fmt::Debug for AttachmentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentState").finish_non_exhaustive()
    }
}

/// The attachment routes.
pub fn router(state: AttachmentState) -> Router {
    Router::new()
        .route("/assets/{asset_id}/attachments", get(list).post(attach))
        .route(
            "/assets/{asset_id}/attachments/{document_id}",
            axum::routing::delete(detach),
        )
        .with_state(Arc::new(state))
}

/// One attached document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AttachmentView {
    pub asset_id: Uuid,
    pub attached_to: Uuid,
    /// `release`, `licence`, `contract`, `permit` or `other`.
    pub kind: String,
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
    pub uploaded_by: Option<crate::comments::PersonView>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Which already-uploaded asset to attach, and as what.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AttachRequest {
    pub document_id: Uuid,
    pub kind: String,
}

/// Everything attached to an asset.
#[utoipa::path(
    get,
    path = "/assets/{asset_id}/attachments",
    responses(
        (status = 200, body = Vec<AttachmentView>),
        (status = 404, description = "No such asset, or not one this caller may see"),
    ),
    tag = "attachments",
)]
pub async fn list(
    State(state): State<Arc<AttachmentState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Vec<AttachmentView>>, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Read).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = attachments::on_asset(conn.executor(), asset_id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    present(&state, found).await
}

/// Attaches an already-uploaded asset as paperwork.
#[utoipa::path(
    post,
    path = "/assets/{asset_id}/attachments",
    request_body = AttachRequest,
    responses(
        (status = 200, body = Vec<AttachmentView>),
        (status = 404, description = "Either asset is unknown, or not one this caller may manage"),
        (status = 409, description = "Already attached elsewhere, a version, or paperwork about paperwork"),
        (status = 422, description = "An unknown kind"),
    ),
    tag = "attachments",
)]
pub async fn attach(
    State(state): State<Arc<AttachmentState>>,
    headers: HeaderMap,
    Path(asset_id): Path<Uuid>,
    Json(request): Json<AttachRequest>,
) -> Result<Json<Vec<AttachmentView>>, Failure> {
    // Manage: attaching paperwork asserts something about an asset's rights.
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let kind = Kind::parse(&request.kind).ok_or_else(|| {
        Failure::Unprocessable(format!(
            "a document is a release, licence, contract, permit or other, not {:?}",
            request.kind
        ))
    })?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = attachments::attach(
        conn.executor(),
        asset_id,
        request.document_id,
        kind,
        &caller.predicate,
    )
    .await
    .map_err(Refused)?;

    // Recorded against the *parent*, because that is the asset whose rights picture changed. The document's own
    // filename is in the context so a feed line can name it without a second read.
    if let Some(actor) = caller.identity_id {
        dam_db::events::record(
            conn.executor(),
            dam_db::events::NewEvent::by(dam_db::events::Kind::Edited, asset_id, actor).with(
                serde_json::json!({ "attached": kind.as_str(), "document": request.document_id }),
            ),
        )
        .await?;
    }
    conn.commit().await?;
    present(&state, found).await
}

/// Detaches a document, returning it to the library.
#[utoipa::path(
    delete,
    path = "/assets/{asset_id}/attachments/{document_id}",
    responses(
        (status = 204, description = "Detached. The document is an ordinary asset again — nothing was deleted"),
        (status = 404, description = "No such document, or not one this caller may manage"),
    ),
    tag = "attachments",
)]
pub async fn detach(
    State(state): State<Arc<AttachmentState>>,
    headers: HeaderMap,
    Path((asset_id, document_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, Failure> {
    let caller = caller::authorize(&state.global, &headers, Action::Manage).await?;
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    attachments::detach(conn.executor(), document_id, &caller.predicate)
        .await
        .map_err(Refused)?;
    conn.commit().await?;
    // The parent id is in the path for the sake of a client's own routing and is not used to find the document: a
    // document knows what it is attached to, and taking the parent as authority would let a caller detach a
    // document by naming the wrong one.
    let _ = asset_id;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolves uploader names in one lookup and renders.
async fn present(
    state: &Arc<AttachmentState>,
    found: Vec<Attachment>,
) -> Result<Json<Vec<AttachmentView>>, Failure> {
    let ids: Vec<Uuid> = {
        let mut ids: Vec<Uuid> = found.iter().filter_map(|a| a.uploaded_by).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let people = dam_db::comments::people_by_id(&state.global, &ids).await?;

    Ok(Json(
        found
            .into_iter()
            .map(|attachment| AttachmentView {
                asset_id: attachment.asset_id,
                attached_to: attachment.attached_to,
                kind: attachment.kind.as_str().to_owned(),
                filename: attachment.filename,
                mime: attachment.mime,
                bytes: attachment.bytes,
                uploaded_by: attachment.uploaded_by.and_then(|id| {
                    people.iter().find(|person| person.id == id).map(|person| {
                        crate::comments::PersonView {
                            id: person.id,
                            name: person.display_name.clone(),
                            email: person.email.clone(),
                        }
                    })
                }),
                created_at: attachment.created_at,
            })
            .collect(),
    ))
}

/// Maps an [`AttachmentRefusal`] onto a status.
struct Refused(AttachmentRefusal);

impl From<Refused> for Failure {
    fn from(Refused(refusal): Refused) -> Self {
        match refusal {
            AttachmentRefusal::UnknownAsset(_) => Self::NotFound,
            // 409 for all three: the request is well formed and the *state of the world* refuses it. Each reason
            // names what is in the way, which is what a conflict is for.
            AttachmentRefusal::AlreadyAttached(_)
            | AttachmentRefusal::ParentIsAttachment
            | AttachmentRefusal::IsAVersion(_) => Self::Conflict(refusal.to_string()),
            AttachmentRefusal::Database(error) => error.into(),
        }
    }
}
