//! The TUS upload surface (1.6).
//!
//! The protocol half of resumable upload. The engine underneath is already tested against a real S3
//! server; what is under test here is the contract a `tus-js-client` in a browser depends on, plus the
//! two properties that are about security rather than protocol:
//!
//! - an unauthenticated request gets nowhere;
//! - **another tenant's upload id returns 404, not 403** — a 403 would confirm the id exists, which is
//!   exactly the disclosure §7 forbids for assets and which applies just as much to an upload.
//!
//! The store is `FakeS3Store`. That is deliberate rather than lazy: the byte-level behaviour is covered
//! against SeaweedFS in `dam-store`, and repeating it here would test the same thing twice while making
//! the protocol suite slow enough that people stop running it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::tus;
use dam_db::{auth, migrate, testing::PostgresHarness};
use dam_store::FakeS3Store;
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const TUS_VERSION: &str = "1.0.0";

/// One container per *group* of cases, not per case.
///
/// Nineteen containers for one suite exceeded what the Docker host takes: the run went from 12 s to
/// 231 s and then failed on connection timeouts. Two tidier-looking fixes do not work here. A single
/// shared `PgPool` hangs, because each `#[tokio::test]` builds its own runtime and a pool is bound to
/// the one that created it. Parking the harness in a `static` avoids that and abandons the container
/// instead of stopping it on drop, which its own contract rules out.
///
/// So the cases below are plain `async fn`s over a borrowed fixture, and three `#[tokio::test]`
/// drivers each build one. Every assertion carries its own message, so a failure still names the
/// property that broke; the cost is that a panic skips the later cases in its group.
struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    global: PgPool,
    key: String,
    other_key: String,
    /// A key on the *same* tenant and identity as `key`, scoped to reading only.
    read_only_key: String,
}

/// Two tenants, each with a key — the second exists so cross-tenant probing can be tested at all.
async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    migrate::tenant(&url, "t_globex").await.expect("globex");
    let global = pg.pool().clone();

    let key = provision(&global, "acme", "Acme", "a@example.com").await;
    let other_key = provision(&global, "globex", "Globex", "b@example.com").await;
    let read_only_key = scoped_key(&global, "acme", "scoped", &["asset:read"]).await;

    let store: Arc<dyn dam_store::ResumableStore> = Arc::new(FakeS3Store::with_test_clock().0);
    let app = tus::router(tus::AppState::new(global.clone(), store));

    Fixture {
        _pg: pg,
        app,
        global,
        key,
        other_key,
        read_only_key,
    }
}

async fn provision(global: &PgPool, slug: &str, name: &str, email: &str) -> String {
    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), $1, 't_' || $1, $2, $1 || '/', 'active') RETURNING id",
    )
    .bind(slug)
    .bind(name)
    .fetch_one(global)
    .await
    .expect("tenant");

    let identity: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(global)
    .await
    .expect("identity");

    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(identity)
    .execute(global)
    .await
    .expect("membership");

    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys (id, tenant_id, identity_id, name, key_prefix, key_hash) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4)",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

/// Issues an extra key on an existing tenant, restricted to `scopes`.
///
/// Deliberately on the same identity as the unrestricted key: that is what makes the scope test about
/// the *scope* rather than about the identity's roles. An empty `scopes` means unscoped.
async fn scoped_key(global: &PgPool, slug: &str, name: &str, scopes: &[&str]) -> String {
    let (tenant_id, identity_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, m.identity_id FROM dam_global.tenants t \
         JOIN dam_global.tenant_members m ON m.tenant_id = t.id WHERE t.slug = $1",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant and member");

    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6)",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .bind(name)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .bind(
        scopes
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<String>>(),
    )
    .execute(global)
    .await
    .expect("scoped key");
    api_key.into_plaintext()
}

/// Sends a request, cloning the router so each call is independent.
async fn send(app: &axum::Router, request: Request<Body>) -> axum::http::Response<Body> {
    app.clone().oneshot(request).await.expect("router")
}

fn header_of(response: &axum::http::Response<Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Creates an upload, returning its id.
async fn create(app: &axum::Router, key: &str, length: u64) -> String {
    let response = send(
        app,
        Request::post("/uploads")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Length", length.to_string())
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "create must succeed"
    );
    let location = header_of(&response, "location").expect("a Location header");
    location
        .rsplit('/')
        .next()
        .expect("an id in the Location")
        .to_owned()
}

// ─── protocol basics ────────────────────────────────────────────────────────

async fn options_advertises_the_version_and_extensions(f: &Fixture) {
    // How tus-js-client decides what it may do. An absent Tus-Extension means the client falls back to a
    // non-resumable upload, silently losing the whole point at G21 file sizes.
    let response = send(
        &f.app,
        Request::builder()
            .method("OPTIONS")
            .uri("/uploads")
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        header_of(&response, "tus-version").as_deref(),
        Some(TUS_VERSION)
    );
    let extensions = header_of(&response, "tus-extension").expect("Tus-Extension");
    for expected in ["creation", "termination"] {
        assert!(
            extensions.contains(expected),
            "missing {expected} in {extensions}"
        );
    }
    assert!(
        header_of(&response, "tus-max-size").is_some(),
        "a client needs the cap before it starts a 200 GB upload, not after"
    );
}

async fn every_response_carries_the_protocol_version(f: &Fixture) {
    let id = create(&f.app, &f.key, 128).await;
    let response = send(
        &f.app,
        Request::head(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(
        header_of(&response, "tus-resumable").as_deref(),
        Some(TUS_VERSION)
    );
}

async fn a_request_without_the_version_header_is_refused_with_412(f: &Fixture) {
    // The protocol requires it, and refusing is what stops a client written against a future version
    // from silently getting 1.0.0 semantics.
    let response = send(
        &f.app,
        Request::post("/uploads")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Upload-Length", "10")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
}

async fn a_head_reports_the_offset_and_forbids_caching(f: &Fixture) {
    // A cached HEAD is a corrupt upload: the client reads a stale offset and resumes from the wrong
    // place, so the bytes interleave. `Cache-Control: no-store` is part of the protocol for this reason.
    let id = create(&f.app, &f.key, 64).await;
    let response = send(
        &f.app,
        Request::head(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, "upload-offset").as_deref(), Some("0"));
    assert_eq!(header_of(&response, "upload-length").as_deref(), Some("64"));
    let cache = header_of(&response, "cache-control").expect("Cache-Control");
    assert!(cache.contains("no-store"), "got {cache}");
}

async fn a_patch_advances_the_offset_and_a_second_one_resumes_from_it(f: &Fixture) {
    let id = create(&f.app, &f.key, 10).await;

    let first = send(
        &f.app,
        Request::patch(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header(header::CONTENT_TYPE, "application/offset+octet-stream")
            .header("Upload-Offset", "0")
            .body(Body::from("hello"))
            .expect("request"),
    )
    .await;
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    assert_eq!(header_of(&first, "upload-offset").as_deref(), Some("5"));

    let second = send(
        &f.app,
        Request::patch(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header(header::CONTENT_TYPE, "application/offset+octet-stream")
            .header("Upload-Offset", "5")
            .body(Body::from("world"))
            .expect("request"),
    )
    .await;
    assert_eq!(second.status(), StatusCode::NO_CONTENT);
    assert_eq!(header_of(&second, "upload-offset").as_deref(), Some("10"));
}

async fn a_patch_at_the_wrong_offset_is_409_and_reports_the_real_one(f: &Fixture) {
    // How a client that lost its connection mid-chunk recovers. Without the authoritative offset in the
    // response it has to HEAD again, and a client that guesses instead duplicates bytes.
    let id = create(&f.app, &f.key, 10).await;
    let response = send(
        &f.app,
        Request::patch(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header(header::CONTENT_TYPE, "application/offset+octet-stream")
            .header("Upload-Offset", "7")
            .body(Body::from("nope"))
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(header_of(&response, "upload-offset").as_deref(), Some("0"));
}

async fn a_patch_with_the_wrong_content_type_is_415(f: &Fixture) {
    // The protocol mandates application/offset+octet-stream. Accepting anything else means a proxy that
    // rewrites the type can corrupt an upload without either end noticing.
    let id = create(&f.app, &f.key, 10).await;
    let response = send(
        &f.app,
        Request::patch(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header("Upload-Offset", "0")
            .body(Body::from("hello"))
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

async fn an_upload_larger_than_the_cap_is_refused_before_any_bytes_arrive(f: &Fixture) {
    let response = send(
        &f.app,
        Request::post("/uploads")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Length", u64::MAX.to_string())
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

async fn terminating_an_upload_removes_it(f: &Fixture) {
    let id = create(&f.app, &f.key, 10).await;

    let deleted = send(
        &f.app,
        Request::delete(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let after = send(
        &f.app,
        Request::head(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(
        after.status(),
        StatusCode::NOT_FOUND,
        "a terminated upload must not be resumable"
    );
}

// ─── authentication and tenancy ─────────────────────────────────────────────

async fn an_unauthenticated_request_is_refused(f: &Fixture) {
    for request in [
        Request::post("/uploads")
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Length", "10")
            .body(Body::empty())
            .expect("request"),
        Request::head("/uploads/anything")
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    ] {
        let response = send(&f.app, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

async fn a_revoked_key_stops_working_immediately(f: &Fixture) {
    let id = create(&f.app, &f.key, 10).await;

    // Its own key, revoked by name. Revocation goes through the same query authentication uses, so this
    // also proves the middleware is not caching the credential for the life of the process.
    let doomed = scoped_key(&f.global, "acme", "to-be-revoked", &[]).await;
    let before = send(
        &f.app,
        Request::head(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {doomed}"))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(
        before.status(),
        StatusCode::OK,
        "the key must work before it is revoked, or the test proves nothing"
    );

    sqlx::query("UPDATE dam_global.api_keys SET revoked_at = now() WHERE name = 'to-be-revoked'")
        .execute(&f.global)
        .await
        .expect("revoke");

    let response = send(
        &f.app,
        Request::head(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {doomed}"))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

async fn another_tenants_upload_is_404_and_not_403(f: &Fixture) {
    // The disclosure that matters. A 403 confirms the id exists, which lets one customer enumerate
    // another's uploads by trying ids — and an upload id is guessable if anyone ever makes it sequential.
    // §7 forbids exactly this for assets, and an upload deserves the same answer.
    let id = create(&f.app, &f.key, 10).await;

    for method in ["HEAD", "DELETE"] {
        let response = send(
            &f.app,
            Request::builder()
                .method(method)
                .uri(format!("/uploads/{id}"))
                .header(header::AUTHORIZATION, format!("Bearer {}", f.other_key))
                .header("Tus-Resumable", TUS_VERSION)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} on another tenant's upload must be indistinguishable from a missing one"
        );
    }
}

async fn an_unknown_upload_id_is_also_404_so_the_two_cases_match(f: &Fixture) {
    // The other half of the same property: if a missing upload answered differently from a forbidden
    // one, the pair of responses would still disclose which ids exist.
    let response = send(
        &f.app,
        Request::head("/uploads/definitely-not-a-real-id")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

async fn an_upload_id_that_could_escape_its_key_prefix_is_refused(f: &Fixture) {
    // The id reaches `Key::staging`, so a traversal attempt must not even reach the database.
    for hostile in ["..%2Fetc%2Fpasswd", "a/b", "has%20space", "%00"] {
        let response = send(
            &f.app,
            Request::head(format!("/uploads/{hostile}"))
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("Tus-Resumable", TUS_VERSION)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert!(
            response.status() == StatusCode::NOT_FOUND
                || response.status() == StatusCode::BAD_REQUEST,
            "{hostile:?} returned {}",
            response.status()
        );
    }
}

async fn a_read_only_key_cannot_start_an_upload(f: &Fixture) {
    // The reason a key has scopes at all: it is safe to paste into a read-only integration. Its owner
    // here is a tenant administrator, so the refusal comes from the scope narrowing the grant and from
    // nothing else — a union instead of an intersection would let anyone widen their own key.
    let response = send(
        &f.app,
        Request::post("/uploads")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.read_only_key))
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Length", "10")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "asset:read must not carry ingest"
    );

    // And the unrestricted key on the same identity still works, which is what proves the refusal was
    // the scope rather than a broken fixture.
    let _ = create(&f.app, &f.key, 10).await;
}

async fn a_replayed_chunk_is_refused_rather_than_appended_twice(f: &Fixture) {
    // The failure this prevents is silent. A client whose 204 was lost retries the *same* chunk at the
    // *previous* offset; appending it would duplicate the bytes and produce an object whose digest
    // matches nothing the client can compute — discovered, if ever, long after the upload "succeeded".
    let id = create(&f.app, &f.key, 10).await;

    let mut statuses = Vec::new();
    for _ in 0..2 {
        let response = send(
            &f.app,
            Request::patch(format!("/uploads/{id}"))
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("Tus-Resumable", TUS_VERSION)
                .header(header::CONTENT_TYPE, "application/offset+octet-stream")
                .header("Upload-Offset", "0")
                .body(Body::from("hello"))
                .expect("request"),
        )
        .await;
        statuses.push((response.status(), header_of(&response, "upload-offset")));
    }

    assert_eq!(statuses[0].0, StatusCode::NO_CONTENT);
    assert_eq!(statuses[0].1.as_deref(), Some("5"));
    assert_eq!(statuses[1].0, StatusCode::CONFLICT);
    assert_eq!(
        statuses[1].1.as_deref(),
        Some("5"),
        "the 409 must carry the real offset so the retry does not need a HEAD"
    );

    // And the offset really is 5, not 10 — the second chunk was not applied anywhere.
    let head = send(
        &f.app,
        Request::head(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(header_of(&head, "upload-offset").as_deref(), Some("5"));
}

async fn a_chunk_over_the_memory_bound_is_refused(f: &Fixture) {
    // The engine buffers a sub-part tail in memory, so an unbounded PATCH body is a memory-exhaustion
    // primitive: one request per connection, each holding its whole chunk. Refused on Content-Length,
    // so nothing is buffered to find out.
    let id = create(&f.app, &f.key, u64::from(u32::MAX)).await;
    let oversized = vec![0u8; dam_api::tus::MAX_CHUNK_BYTES + 1];

    let response = send(
        &f.app,
        Request::patch(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header(header::CONTENT_TYPE, "application/offset+octet-stream")
            .header("Upload-Offset", "0")
            .body(Body::from(oversized))
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

async fn an_upload_of_unknown_length_is_accepted_and_says_so(f: &Fixture) {
    // A browser streaming from a MediaRecorder does not know the size in advance. The protocol allows
    // deferring it, and HEAD has to answer "still deferred" distinguishably from "I lost your length".
    let created = send(
        &f.app,
        Request::post("/uploads")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Defer-Length", "1")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = header_of(&created, "location")
        .expect("Location")
        .rsplit('/')
        .next()
        .expect("id")
        .to_owned();

    let head = send(
        &f.app,
        Request::head(format!("/uploads/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(
        header_of(&head, "upload-defer-length").as_deref(),
        Some("1")
    );
    assert!(
        header_of(&head, "upload-length").is_none(),
        "a deferred length must be absent rather than reported as 0, which a client would read as \
         an empty file"
    );
}

async fn a_post_that_declares_nothing_at_all_is_refused(f: &Fixture) {
    // Neither Upload-Length nor Upload-Defer-Length. Accepting it would create a session whose size is
    // unknown *and* unknowable, which the reaper would then have to guess about.
    let response = send(
        &f.app,
        Request::post("/uploads")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ─── the presigned direct-to-S3 path ───────────────────────────────────────

async fn a_presigned_put_is_issued_with_a_session_behind_it(f: &Fixture) {
    // The point of this path: the bytes never traverse the API server. What makes it safe is the
    // session recorded alongside the URL — finalisation compares the stored object against the declared
    // length, and an object with no session behind it is one nothing will ever adopt.
    let response = send(
        &f.app,
        Request::post("/uploads/presign")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Length", "2048")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body"),
    )
    .expect("json");

    let upload_id = body["upload_id"].as_str().expect("an upload id");
    assert!(
        body["url"].as_str().is_some_and(|u| u.starts_with("http")),
        "expected a URL, got {:?}",
        body["url"]
    );
    assert_eq!(body["expires_in_seconds"].as_u64(), Some(900));

    // The session exists and is resumable through the ordinary TUS surface, which is what "a session
    // behind it" has to mean in practice.
    let head = send(
        &f.app,
        Request::head(format!("/uploads/{upload_id}"))
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(header_of(&head, "upload-length").as_deref(), Some("2048"));
}

async fn a_presigned_key_is_always_inside_the_callers_own_prefix(f: &Fixture) {
    // The property that stops this endpoint being a signing oracle. The key comes from the caller's
    // tenant id and a server-generated upload id; if a client could influence it, one customer could
    // obtain a signed write into another's prefix — or anywhere in the bucket.
    let mut keys = Vec::new();
    for key_header in [&f.key, &f.other_key] {
        let response = send(
            &f.app,
            Request::post("/uploads/presign")
                .header(header::AUTHORIZATION, format!("Bearer {key_header}"))
                .header("Tus-Resumable", TUS_VERSION)
                .header("Upload-Length", "10")
                // A hostile attempt to steer the key. Every one of these is ignored, not sanitised —
                // there is no parameter for it to land in.
                .header(
                    "Upload-Metadata",
                    "key ../../../etc/passwd,filename ZXZpbC5wbmc=",
                )
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("body"),
        )
        .expect("json");
        let staging = body["staging_key"]
            .as_str()
            .expect("a staging key")
            .to_owned();
        assert!(
            staging.contains("/staging/") && !staging.contains(".."),
            "got {staging}"
        );
        keys.push(staging);
    }

    // Two tenants, two prefixes. Equal prefixes would mean the tenant id is not in the key at all.
    let prefix_of = |k: &String| k.split('/').next().expect("a tenant prefix").to_owned();
    assert_ne!(
        prefix_of(&keys[0]),
        prefix_of(&keys[1]),
        "each tenant's staging keys must live under its own prefix"
    );
}

async fn a_presign_without_a_declared_length_is_refused(f: &Fixture) {
    // Unlike the TUS path, this one cannot defer. The server never sees the bytes, so the declared
    // length is the *only* cross-check finalisation has — without it an object of any size is
    // indistinguishable from the expected one.
    let response = send(
        &f.app,
        Request::post("/uploads/presign")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Defer-Length", "1")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

async fn a_read_only_key_cannot_obtain_a_presigned_url(f: &Fixture) {
    // The gate that matters most on this endpoint: a presigned PUT is a write credential that outlives
    // the request and travels outside this server's control, so handing one to a read-only key would be
    // worse than letting it upload through the proxied path.
    let response = send(
        &f.app,
        Request::post("/uploads/presign")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.read_only_key))
            .header("Tus-Resumable", TUS_VERSION)
            .header("Upload-Length", "10")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ─── drivers ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_protocol_surface_holds() {
    let f = fixture().await;
    options_advertises_the_version_and_extensions(&f).await;
    every_response_carries_the_protocol_version(&f).await;
    a_request_without_the_version_header_is_refused_with_412(&f).await;
    a_head_reports_the_offset_and_forbids_caching(&f).await;
    an_upload_of_unknown_length_is_accepted_and_says_so(&f).await;
    a_post_that_declares_nothing_at_all_is_refused(&f).await;
    an_upload_larger_than_the_cap_is_refused_before_any_bytes_arrive(&f).await;
}

#[tokio::test]
async fn the_offset_arithmetic_holds() {
    let f = fixture().await;
    a_patch_advances_the_offset_and_a_second_one_resumes_from_it(&f).await;
    a_patch_at_the_wrong_offset_is_409_and_reports_the_real_one(&f).await;
    a_patch_with_the_wrong_content_type_is_415(&f).await;
    a_replayed_chunk_is_refused_rather_than_appended_twice(&f).await;
    a_chunk_over_the_memory_bound_is_refused(&f).await;
    terminating_an_upload_removes_it(&f).await;
}

#[tokio::test]
async fn the_presigned_path_holds() {
    let f = fixture().await;
    a_presigned_put_is_issued_with_a_session_behind_it(&f).await;
    a_presigned_key_is_always_inside_the_callers_own_prefix(&f).await;
    a_presign_without_a_declared_length_is_refused(&f).await;
    a_read_only_key_cannot_obtain_a_presigned_url(&f).await;
}

#[tokio::test]
async fn authentication_and_tenancy_hold() {
    let f = fixture().await;
    an_unauthenticated_request_is_refused(&f).await;
    a_read_only_key_cannot_start_an_upload(&f).await;
    another_tenants_upload_is_404_and_not_403(&f).await;
    an_unknown_upload_id_is_also_404_so_the_two_cases_match(&f).await;
    an_upload_id_that_could_escape_its_key_prefix_is_refused(&f).await;
    // Last in the group: it revokes a key, and although it revokes only its own, ordering it last
    // means a future edit that widens that UPDATE cannot silently break the cases above.
    a_revoked_key_stops_working_immediately(&f).await;
}
