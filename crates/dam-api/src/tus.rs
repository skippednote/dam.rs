//! The TUS 1.0.0 resumable-upload surface (1.6).
//!
//! The HTTP layer over `dam_store::resumable`. Everything interesting about restarting an interrupted
//! 200 GB upload lives in that engine; what lives here is the protocol a `tus-js-client` in a browser
//! speaks, and the four decisions that are about this system rather than about TUS:
//!
//! ## A missing upload and another tenant's upload answer identically
//!
//! Both are `404`. A `403` on someone else's id would confirm the id exists, which turns an upload id
//! into an oracle: try ids, keep the ones that answer differently. §7 forbids exactly this shape of
//! disclosure for assets, and an upload is no less sensitive — its id is enough to resume it.
//!
//! Two independent mechanisms produce that answer, and it is worth being precise about which does what.
//! The isolation is D2's: `TenantConn` sets `search_path` to the caller's schema, so another tenant's
//! row is not in a table this query can see. The `tenant_id` predicate in `uploads::load` is the second
//! one, and it exists because the first fails *unsafely* — a row reachable in the wrong schema would
//! otherwise be loaded and then rejected, and a rejection is a distinguishable status. With the
//! predicate the lookup simply returns nothing, which is the same answer a missing id gets.
//!
//! ## Ingest requires `asset:manage`
//!
//! An upload becomes an asset, so creating one is a write. A key scoped to `asset:read` — the kind you
//! paste into a read-only integration — must not be able to start one. The permission is resolved from
//! the caller's roles through `dam_db::auth::grants_for`, which intersects the key's scopes, so a scope
//! can only narrow.
//!
//! ## `Cache-Control: no-store` on HEAD is a correctness requirement, not hygiene
//!
//! A cached HEAD hands the client a stale offset. It then resumes from the wrong place and the bytes
//! interleave, producing an object whose digest matches nothing anyone can compute — and it fails
//! *silently*, at whatever fraction of uploads a proxy happens to serve from cache.
//!
//! ## Chunks are bounded
//!
//! [`MAX_CHUNK_BYTES`] caps a single PATCH. The engine assembles the sub-part tail in memory, so an
//! unbounded body is a memory exhaustion primitive: one request per connection, each holding its whole
//! chunk. The cap is a memory bound and not a limit on upload size — a client sends a 200 GB file as
//! however many chunks it likes.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use dam_core::StorageClass;
use dam_core::policy::Action;
use dam_db::{auth, uploads};
use dam_store::{ResumableStore, resumable};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// The only protocol version this server implements.
///
/// A request that does not name it is refused rather than assumed: a client written against a future
/// version silently getting 1.0.0 semantics is how offset arithmetic goes wrong in production.
pub const TUS_VERSION: &str = "1.0.0";

/// Extensions advertised on OPTIONS.
///
/// `creation` and `termination` only. Absent ones matter as much as present ones: without
/// `checksum` a client knows not to bother sending `Upload-Checksum`, and without `concatenation`
/// it knows not to try parallel part uploads — both are things a client would otherwise attempt and
/// have silently ignored.
const TUS_EXTENSIONS: &str = "creation,termination";

/// The largest single PATCH body. See the module docs — this is a memory bound.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// The largest upload this server will accept.
///
/// Advertised on OPTIONS so a client learns the cap *before* spending an hour on an upload that will
/// be refused at the end.
pub const MAX_UPLOAD_BYTES: u64 = 5 * 1024 * 1024 * 1024 * 1024;

/// A single PATCH must be able to carry a legal S3 part, or the engine could never assemble one.
const _: () = assert!(MAX_CHUNK_BYTES >= 5 * 1024 * 1024);

/// Shared handler state.
///
/// One pool, not one per tenant: `TenantConn` supplies isolation through a per-request transaction's
/// `search_path` (§5.2), which is what makes a thousand tenants cost a thousand transactions rather
/// than a thousand connection pools.
#[derive(Clone)]
pub struct AppState {
    global: PgPool,
    store: Arc<dyn ResumableStore>,
    max_upload: u64,
    class: StorageClass,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The pool's Debug prints its connection string, which carries the password.
        f.debug_struct("AppState")
            .field("max_upload", &self.max_upload)
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub fn new(global: PgPool, store: Arc<dyn ResumableStore>) -> Self {
        Self {
            global,
            store,
            max_upload: MAX_UPLOAD_BYTES,
            // Staging objects are written once and read once, within minutes. Standard rather than a
            // cheaper tier because every colder class has a minimum storage duration that a staging
            // object deleted after ten minutes would be billed for in full.
            class: StorageClass::Standard,
        }
    }

    /// Overrides the maximum upload size, for tests and for deployments with a smaller quota.
    #[must_use]
    pub fn with_max_upload(mut self, bytes: u64) -> Self {
        self.max_upload = bytes;
        self
    }
}

/// How long a presigned PUT stays valid.
///
/// Long enough for a large single-shot upload on a poor connection, short enough that a URL captured
/// from a log or a browser history is useless by the time anyone reads it. The URL is a bearer
/// credential for one write to one key, which is the whole reason it is not measured in hours.
const PRESIGN_TTL: Duration = Duration::from_secs(15 * 60);

/// The upload routes.
pub fn router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/uploads", post(create).options(options))
        .route("/uploads/presign", post(presign))
        // `get(...)` carries the HEAD, because axum answers HEAD from a GET route by running the
        // handler and dropping the body — which would drop the offset with it. Registering HEAD
        // explicitly is the only way it reaches this handler with its headers intact.
        .route(
            "/uploads/{upload_id}",
            get(head_upload)
                .head(head_upload)
                .patch(patch_upload)
                .delete(delete_upload),
        )
        .layer(DefaultBodyLimit::max(MAX_CHUNK_BYTES))
        .layer(middleware::from_fn(protocol_version))
        .with_state(Arc::new(state))
}

/// Enforces `Tus-Resumable` on the way in and stamps it on the way out.
///
/// Outermost, so even a `404` from the router carries the version — a client that cannot tell a
/// TUS server from any other 404 has to guess whether to retry.
async fn protocol_version(request: Request<axum::body::Body>, next: Next) -> Response {
    let exempt = request.method() == Method::OPTIONS;
    let named = request
        .headers()
        .get("tus-resumable")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == TUS_VERSION);

    let mut response = if exempt || named {
        next.run(request).await
    } else {
        // 412 and not 400: the protocol names this case, and a client library keys its "server speaks
        // a different version" path off exactly this status.
        StatusCode::PRECONDITION_FAILED.into_response()
    };

    response
        .headers_mut()
        .insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    response
}

/// Capability discovery. Deliberately unauthenticated: it discloses nothing about any tenant, and a
/// client needs it before it has decided which credential to use.
async fn options(State(state): State<Arc<AppState>>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("tus-version", HeaderValue::from_static(TUS_VERSION));
    headers.insert("tus-extension", HeaderValue::from_static(TUS_EXTENSIONS));
    if let Ok(value) = HeaderValue::from_str(&state.max_upload.to_string()) {
        headers.insert("tus-max-size", value);
    }
    (StatusCode::NO_CONTENT, headers).into_response()
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Refusal> {
    let caller = authorize(&state, &headers, Action::Manage).await?;

    // `Upload-Defer-Length: 1` means "I do not know the size yet" — legal, and the engine already
    // models it as `declared_length: None`. Anything else present must parse.
    let deferred = header_str(&headers, "upload-defer-length") == Some("1");
    let declared = match header_str(&headers, "upload-length") {
        Some(raw) => Some(raw.parse::<u64>().map_err(|_| Refusal::BadRequest)?),
        None if deferred => None,
        // Neither header: the client has not said what it is sending, which the protocol requires.
        None => return Err(Refusal::BadRequest),
    };

    if declared.is_some_and(|length| length > state.max_upload) {
        // Refused before a single byte moves. The alternative is discovering it at completion, after
        // the client has spent the bandwidth.
        return Err(Refusal::TooLarge);
    }

    // The tenant's caps, checked here for exactly the reason above (G19). Refusing at finalise would be a
    // correct answer given far too late: the worker runs from a queue, so the client would have uploaded the
    // whole file and be waiting on a job that was always going to refuse it.
    //
    // Levels rather than flows, so the numbers come from the metering pass rather than from a counter this
    // request increments — see `dam_db::quotas::observe`. Which means the check is one indexed read and is
    // slightly behind: a tenant crosses a cap and keeps uploading until the next pass. That is the same
    // bounded overshoot `quotas` documents for AI spend, and the alternative — recounting the library on the
    // path of every upload — is worse by a wide margin.
    //
    // A soft cap warns and proceeds, which is the point of having two enforcement modes: a hard cap on ingest
    // loses a customer's work, so it is a choice an operator makes deliberately per tenant.
    {
        let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
        let period = dam_db::quotas::month_start(chrono::Utc::now());
        for key in [dam_db::quotas::STORAGE_BYTES, dam_db::quotas::ASSET_COUNT] {
            let verdict =
                dam_db::quotas::check(conn.executor(), caller.tenant_id, key, period).await?;
            if !verdict.allowed() {
                conn.rollback().await?;
                tracing::warn!(
                    tenant = %caller.tenant_id,
                    quota = key,
                    ?verdict,
                    "refusing an upload over a hard cap",
                );
                return Err(Refusal::OverQuota);
            }
            if let dam_db::quotas::Verdict::Warned { used, limit } = verdict {
                // Logged rather than returned: a TUS response carries no body, and a warning that stopped the
                // upload would be a hard cap by another name.
                tracing::info!(
                    tenant = %caller.tenant_id,
                    quota = key,
                    used,
                    limit,
                    "a tenant is past its warning line",
                );
            }
        }
        conn.commit().await?;
    }

    let metadata = Metadata::parse(header_str(&headers, "upload-metadata").unwrap_or_default());

    // 122 bits of randomness, hex-encoded. Unguessable on purpose: an id is a bearer token for the
    // upload it names, so a sequential id would let anyone resume — or corrupt — a neighbour's upload.
    let upload_id = uuid::Uuid::new_v4().simple().to_string();

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;

    // Resolved from the key here, so the *session* records which profile it was made under: finalise can run
    // from a queue long after this request, and the profile has to be recoverable from the row rather than from
    // whatever header is still in scope. An unknown key resolves to nothing and finalise falls back — see
    // `Metadata::profile` for why that is better than refusing.
    let profile_id = match metadata.profile.as_deref() {
        Some(key) => dam_db::upload_profiles::by_key_on(conn.executor(), key)
            .await
            .map_err(|_| Refusal::Internal)?
            .map(|profile| profile.id),
        None => None,
    };

    uploads::create(
        conn.executor(),
        caller.tenant_id,
        &upload_id,
        declared,
        metadata.filename.as_deref(),
        metadata.mime.as_deref(),
        caller.identity_id,
        profile_id,
    )
    .await?;
    conn.commit().await?;

    let mut out = HeaderMap::new();
    out.insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("/uploads/{upload_id}")).map_err(|_| Refusal::Internal)?,
    );
    Ok((StatusCode::CREATED, out).into_response())
}

/// Issues a presigned `PUT` for a direct-to-S3 upload.
///
/// The alternative to TUS, and the right one for a 3 MB photograph: the bytes go straight to the bucket
/// and never traverse this process, which is the difference between a stateless API server and one
/// sized for its customers' bandwidth.
///
/// What it costs is the validation that a proxied upload gets for free. A presigned PUT hands out a URL
/// and steps out of the way, so the server sees neither the bytes nor their length — the client can
/// upload anything, of any size, whatever it declared here. That is why `dam_media::ingest` re-sniffs
/// and re-measures the object after the fact, and why this endpoint records a session rather than
/// trusting the response: the session's declared length is the cross-check that finalisation compares
/// against, and a key with no session behind it is an object nothing will ever adopt.
async fn presign(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Refusal> {
    let caller = authorize(&state, &headers, Action::Manage).await?;

    // Required here, unlike on the TUS path: `Upload-Defer-Length` has no meaning for a single PUT, and
    // without a declared length finalisation has nothing to compare the stored object against.
    let declared: u64 = header_str(&headers, "upload-length")
        .ok_or(Refusal::BadRequest)?
        .parse()
        .map_err(|_| Refusal::BadRequest)?;
    if declared > state.max_upload {
        return Err(Refusal::TooLarge);
    }

    let metadata = Metadata::parse(header_str(&headers, "upload-metadata").unwrap_or_default());
    let upload_id = uuid::Uuid::new_v4().simple().to_string();

    // The key is derived from the tenant and an id this server generated. A client-supplied key would
    // make this endpoint a signing oracle for any path in the bucket, including another tenant's.
    let key = dam_store::Key::staging(caller.tenant_id, &upload_id)?;
    let url = state.store.presign_put(&key, PRESIGN_TTL).await?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    // The presigned path resolves the profile too. Both intakes have to, or which endpoint a client happened to
    // use would decide whether its intake's rules applied.
    let profile_id = match metadata.profile.as_deref() {
        Some(key) => dam_db::upload_profiles::by_key_on(conn.executor(), key)
            .await
            .map_err(|_| Refusal::Internal)?
            .map(|profile| profile.id),
        None => None,
    };
    uploads::create(
        conn.executor(),
        caller.tenant_id,
        &upload_id,
        Some(declared),
        metadata.filename.as_deref(),
        metadata.mime.as_deref(),
        caller.identity_id,
        profile_id,
    )
    .await?;
    conn.commit().await?;

    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/uploads/{upload_id}"))],
        axum::Json(serde_json::json!({
            "upload_id": upload_id,
            "url": url,
            "expires_in_seconds": PRESIGN_TTL.as_secs(),
            // Echoed so a client does not have to reconstruct it, and named `staging` so nobody mistakes
            // it for the asset's final content key — the promotion to that key happens server-side after
            // the digest is known.
            "staging_key": key.as_str(),
        })),
    )
        .into_response())
}

async fn head_upload(
    State(state): State<Arc<AppState>>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Refusal> {
    let caller = authorize(&state, &headers, Action::Manage).await?;
    let session = load(&state, &caller, &upload_id).await?;

    let mut out = HeaderMap::new();
    out.insert(
        "upload-offset",
        HeaderValue::from_str(&session.offset.to_string()).map_err(|_| Refusal::Internal)?,
    );
    match session.declared_length {
        Some(length) => {
            out.insert(
                "upload-length",
                HeaderValue::from_str(&length.to_string()).map_err(|_| Refusal::Internal)?,
            );
        }
        // The protocol pairs an unknown length with this header rather than omitting both, so a client
        // can distinguish "still deferred" from "the server lost my length".
        None => {
            out.insert("upload-defer-length", HeaderValue::from_static("1"));
        }
    }
    // See the module docs: a cached offset corrupts the upload.
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    Ok((StatusCode::OK, out).into_response())
}

async fn patch_upload(
    State(state): State<Arc<AppState>>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Refusal> {
    if header_str(&headers, "content-type") != Some("application/offset+octet-stream") {
        // Mandated by the protocol, and load-bearing: a proxy that rewrites the type is a proxy that
        // may have rewritten the body, and appending rewritten bytes at a byte offset is unrecoverable.
        return Err(Refusal::UnsupportedMediaType);
    }
    let at_offset: u64 = header_str(&headers, "upload-offset")
        .ok_or(Refusal::BadRequest)?
        .parse()
        .map_err(|_| Refusal::BadRequest)?;

    let caller = authorize(&state, &headers, Action::Manage).await?;
    let mut session = load(&state, &caller, &upload_id).await?;

    let outcome = resumable::patch(
        state.store.as_ref(),
        &mut session,
        at_offset,
        body,
        state.class,
    )
    .await?;

    match outcome {
        resumable::PatchOutcome::Accepted { new_offset } => {
            // Saved after the bytes land, never before: a persisted offset ahead of the stored bytes
            // makes the client skip a chunk it never actually sent.
            let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
            uploads::save(conn.executor(), &session).await?;
            conn.commit().await?;

            // The last chunk. Queued here rather than waiting for a sweeper, because this is the moment the
            // upload is complete and a user is watching for their asset to appear — and queued rather than
            // done inline, because assembling a multipart upload and hashing it is not work to do inside a
            // request the client is holding open.
            //
            // A client with `Upload-Defer-Length` never declares a size, so there is nothing to compare
            // against and nothing is queued: that client finalises through its own explicit call, which is
            // what the deferred-length extension is for.
            if session.declared_length == Some(new_offset) {
                // A failure here is logged and not returned. The bytes are safely stored and the client's
                // upload *did* succeed — answering 500 would make a well-behaved client retry a chunk that
                // already landed, and the reaper plus a re-queue can recover a missing job. Losing the job is
                // recoverable; telling the client its complete upload failed is not.
                if let Err(error) = dam_pipeline::worker::enqueue_finalise(
                    &state.global,
                    caller.tenant_id,
                    &upload_id,
                )
                .await
                {
                    tracing::error!(%error, upload_id, "could not queue finalisation for a completed upload");
                }
            }

            let mut out = HeaderMap::new();
            out.insert(
                "upload-offset",
                HeaderValue::from_str(&new_offset.to_string()).map_err(|_| Refusal::Internal)?,
            );
            Ok((StatusCode::NO_CONTENT, out).into_response())
        }
        resumable::PatchOutcome::OffsetConflict { expected, .. } => {
            // The authoritative offset travels with the 409 so the client can resume immediately.
            // Without it, recovery costs an extra HEAD — and a client that guesses instead duplicates
            // bytes.
            let mut out = HeaderMap::new();
            out.insert(
                "upload-offset",
                HeaderValue::from_str(&expected.to_string()).map_err(|_| Refusal::Internal)?,
            );
            Ok((StatusCode::CONFLICT, out).into_response())
        }
    }
}

async fn delete_upload(
    State(state): State<Arc<AppState>>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Refusal> {
    let caller = authorize(&state, &headers, Action::Manage).await?;
    let mut session = load(&state, &caller, &upload_id).await?;

    // The store first. An aborted multipart upload stops the billing meter on parts already uploaded;
    // marking the row terminated while leaving them behind bills the customer for storage nobody can
    // reach. If the store call fails the row stays Active and the reaper retries.
    resumable::terminate(state.store.as_ref(), &mut session).await?;

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    uploads::save(conn.executor(), &session).await?;
    conn.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Loads a session belonging to the caller's tenant.
///
/// The tenant scoping happens in the query. A terminated session reports `NotFound` for the same
/// reason another tenant's does: the two answers must be indistinguishable, or the pair discloses
/// which ids exist.
async fn load(
    state: &AppState,
    caller: &auth::Authenticated,
    upload_id: &str,
) -> Result<resumable::ResumableSession, Refusal> {
    // Before the database, not after. A NUL byte in an id is rejected by Postgres itself, which
    // produced a 500 where a 404 belonged — and a status that varies with the input is exactly the
    // signal a prober needs. Same rule `Key::staging` enforces, imported rather than restated.
    if dam_store::validate_upload_id(upload_id).is_err() {
        return Err(Refusal::NotFound);
    }

    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let found = uploads::load(conn.executor(), caller.tenant_id, upload_id).await?;
    conn.commit().await?;

    match found {
        Some(session) if !matches!(session.status, resumable::SessionStatus::Terminated) => {
            Ok(session)
        }
        _ => Err(Refusal::NotFound),
    }
}

/// Authenticates the bearer token and checks the caller may perform `action`.
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    action: Action,
) -> Result<auth::Authenticated, Refusal> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Refusal::Unauthorized)?;

    let caller = auth::authenticate(&state.global, presented)
        .await?
        .ok_or(Refusal::Unauthorized)?;

    // A key with no identity behind it — a machine credential — has no membership and therefore no
    // roles. That is fail-closed and intended: issuing such a key today grants nothing, which is the
    // safe direction for a shape the role model does not yet describe.
    //
    // Note this is `auth::authenticate`'s type, not `caller::Caller`: authentication happens before
    // authorisation, so an identity is genuinely optional here. `Caller` is the type that guarantees one,
    // and only because `authorize` refuses this case on the way to building it.
    let identity = caller.identity_id.ok_or(Refusal::Forbidden)?;

    let scopes: Vec<&str> = caller.scopes.iter().map(String::as_str).collect();
    let mut conn = dam_db::TenantConn::begin(&state.global, &caller.tenant_slug).await?;
    let grants = auth::grants_for(
        &state.global,
        conn.executor(),
        caller.tenant_id,
        identity,
        &scopes,
    )
    .await?;
    conn.commit().await?;

    // Compiled through the same predicate compiler every other consumer uses (§12), rather than a
    // hand-rolled permission string comparison that would drift from it.
    let predicate = dam_core::policy::compile(&grants, action, chrono::Utc::now());
    if predicate.matches_nothing() {
        return Err(Refusal::Forbidden);
    }
    Ok(caller)
}

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// What the client declared about the file, from `Upload-Metadata`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Metadata {
    filename: Option<String>,
    mime: Option<String>,
    /// The upload profile the client named, by key (Q.3).
    ///
    /// A key rather than an id, because a client that knows its intake by name should not have to look up a
    /// uuid first — and because an id in a header is one more thing to get wrong in an integration script. An
    /// unknown key resolves to the tenant's fallback rather than failing: the bytes are the point, and a
    /// mistyped profile is recoverable afterwards while a refused upload is not.
    profile: Option<String>,
}

impl Metadata {
    /// Parses `key base64value,key2 base64value2`.
    ///
    /// Every value here is attacker-controlled and is stored, so nothing is trusted: a filename is
    /// recorded as *declared* and the real type comes from sniffing the bytes (1.4). A malformed pair
    /// is skipped rather than failing the request — a client that sends an unrecognised key should
    /// still be able to upload.
    fn parse(raw: &str) -> Self {
        use base64::Engine as _;

        let mut out = Self::default();
        for pair in raw.split(',') {
            let pair = pair.trim();
            let (key, encoded) = match pair.split_once(' ') {
                Some((key, value)) => (key, value),
                // A valueless key is legal TUS and carries no information we want.
                None => continue,
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
                continue;
            };
            let Ok(value) = String::from_utf8(bytes) else {
                continue;
            };
            // Truncated at the column widths. A client can send a megabyte of "filename" otherwise,
            // and the failure would surface as a database error on an otherwise valid upload.
            let value: String = value.chars().take(512).collect();
            match key {
                "filename" | "name" => out.filename = Some(value),
                "filetype" | "type" | "mimetype" => out.mime = Some(value),
                "profile" | "uploadprofile" => out.profile = Some(value),
                _ => {}
            }
        }
        out
    }
}

/// Every way a request can be refused, and the status each maps to.
#[derive(Debug)]
enum Refusal {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    TooLarge,
    /// The tenant is over a hard storage or asset cap (G19).
    ///
    /// **507, not 413.** The distinction is one a client can act on: `TooLarge` means send a smaller file, and
    /// this means nothing they send will work until somebody raises the cap. Collapsing them would have an
    /// integration retrying with progressively smaller files forever.
    OverQuota,
    UnsupportedMediaType,
    /// The deployment is briefly out of capacity — a connection pool with nothing free.
    ///
    /// **503 with `Retry-After`, not 500**, and for a TUS client the difference decides whether a file
    /// arrives. A 500 says the request is broken and must not be repeated, so an uploader drops it; this
    /// clears itself as connections return, so the useful answer is "try again shortly".
    ///
    /// Found by uploading 2056 files across four tenants at once against a sixteen-connection pool: thirty
    /// came back 500, every one of them "pool timed out while waiting for an open connection".
    ///
    /// The classifying lives in `dam_db::Error::is_capacity` rather than here, because this is not the only
    /// surface that can be refused for it — every `From<dam_db::Error>` in this crate currently answers a
    /// saturated pool with 500, and the upload path is only the one that was measured saturating. Moving the
    /// rest over is a question of which of them a client can usefully retry, not of how to tell.
    Unavailable,
    Internal,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let status = match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::OverQuota => StatusCode::INSUFFICIENT_STORAGE,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // No body. A TUS client reads status and headers, and an error body here could only ever leak
        // detail about the tenant's state to a caller who has already been refused.
        //
        // `Retry-After` is the exception, because it is not detail — it is the instruction that makes the
        // refusal recoverable. A second, not a minute: the pool frees as in-flight requests finish, and a
        // client that waits a minute has turned a blip into a stalled upload.
        if matches!(self, Self::Unavailable) {
            return (status, [(axum::http::header::RETRY_AFTER, "1")]).into_response();
        }
        status.into_response()
    }
}

impl From<dam_db::Error> for Refusal {
    fn from(error: dam_db::Error) -> Self {
        if error.is_capacity() {
            // Warn, not error. An error-level line per refused request turns a brief saturation into pages of
            // alarm, and what an operator needs is the count rather than each instance.
            tracing::warn!(%error, "upload refused: out of capacity, retryable");
            return Self::Unavailable;
        }
        tracing::error!(%error, "upload database error");
        Self::Internal
    }
}

impl From<dam_store::Error> for Refusal {
    fn from(error: dam_store::Error) -> Self {
        // A malformed upload id reaches `Key::staging` and fails there. Reporting that as 404 rather
        // than 400 keeps a hostile id indistinguishable from a missing one — see the module docs.
        let message = error.to_string();
        if message.contains("upload id must be") {
            return Self::NotFound;
        }
        tracing::error!(%error, "upload store error");
        Self::Internal
    }
}

impl From<dam_core::Error> for Refusal {
    fn from(error: dam_core::Error) -> Self {
        tracing::error!(%error, "upload core error");
        Self::Internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn encode(value: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(value)
    }

    #[test]
    fn a_pool_timeout_is_a_retryable_503_rather_than_a_500() {
        // A TUS client reads the status to decide whether the file is still uploadable: 500 says the
        // upload is broken, so a well-behaved client stops trying and the file is lost. 503 with
        // `Retry-After` says come back. Under a load run against a saturated database an entire
        // 32-wide burst refused here, so which of the two it is decides whether those uploads were
        // delayed by a second or dropped.
        let refusal = Refusal::from(dam_db::Error::Sqlx(sqlx::Error::PoolTimedOut));
        assert!(
            matches!(refusal, Refusal::Unavailable),
            "running out of connections is capacity, not a bad request or a bug"
        );

        let response = refusal.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("1"),
            "a client that is not told when to come back comes back immediately"
        );
    }

    #[test]
    fn a_database_fault_that_is_not_capacity_is_still_a_500() {
        // The other half of the classification: only pool exhaustion is retryable. A broken query is
        // a bug, and telling the client to retry it would hide the bug behind an infinite loop.
        let refusal = Refusal::from(dam_db::Error::Sqlx(sqlx::Error::RowNotFound));
        assert!(matches!(refusal, Refusal::Internal));
        assert_eq!(
            refusal.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn metadata_decodes_a_filename_a_type_and_a_profile() {
        let raw = format!(
            "filename {},filetype {},profile {}",
            encode("holiday photo.jpg"),
            encode("image/jpeg"),
            encode("press")
        );
        assert_eq!(
            Metadata::parse(&raw),
            Metadata {
                filename: Some("holiday photo.jpg".to_owned()),
                mime: Some("image/jpeg".to_owned()),
                profile: Some("press".to_owned()),
            }
        );
    }

    #[test]
    fn an_upload_that_names_no_profile_says_so_rather_than_guessing() {
        // `None` here is what makes finalise fall back to the tenant's default profile. An empty string would
        // instead be a *named* profile that resolves to nothing, which is a different and worse answer.
        let raw = format!("filename {}", encode("a.png"));
        assert_eq!(Metadata::parse(&raw).profile, None);
    }

    #[test]
    fn an_undecodable_pair_is_skipped_rather_than_failing_the_upload() {
        // A client that sends a key we do not understand, or mangles one, should still be able to
        // upload — the metadata is advisory, and the bytes are what matter.
        let raw = format!("filename {},garbage !!!not-base64!!!", encode("a.png"));
        assert_eq!(Metadata::parse(&raw).filename.as_deref(), Some("a.png"));
    }

    #[test]
    fn a_filename_is_truncated_before_it_reaches_the_database() {
        let raw = format!("filename {}", encode(&"a".repeat(10_000)));
        assert_eq!(
            Metadata::parse(&raw).filename.map(|f| f.chars().count()),
            Some(512)
        );
    }

    #[test]
    fn non_utf8_metadata_does_not_panic() {
        // The value is base64 of arbitrary bytes, so this is reachable from any client.
        let raw = format!(
            "filename {}",
            base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe, 0xfd])
        );
        assert_eq!(Metadata::parse(&raw), Metadata::default());
    }

    #[test]
    fn empty_metadata_is_not_an_error() {
        assert_eq!(Metadata::parse(""), Metadata::default());
    }
}
