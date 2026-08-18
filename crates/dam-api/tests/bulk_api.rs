//! The bulk-operation endpoints.
//!
//! `dam_pipeline`'s suite proves execution and `dam_db`'s proves the bookkeeping; this proves the HTTP
//! contract — above all that **the target list is the caller's to manage**, because a bulk request is a list
//! of client-assembled ids and a caller scoped to one corner of the library must not be able to bulk-delete
//! the rest by guessing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::bulk::{BulkState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    global: PgPool,
    acme: PgPool,
    globex: PgPool,
    key: String,
    read_only_key: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    migrate::tenant(&url, "t_globex").await.expect("globex");
    let global = pg.pool().clone();

    let key = provision(&global, "acme", "a@example.com").await;
    provision(&global, "globex", "b@example.com").await;
    let read_only_key = scoped_key(&global, "acme", &["asset:read"]).await;

    let app = router(BulkState {
        global: global.clone(),
    });
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");
    let globex = pg.pool_for_schema("t_globex").await.expect("globex pool");

    Fixture {
        _pg: pg,
        app,
        global,
        acme,
        globex,
        key,
        read_only_key,
    }
}

async fn provision(global: &PgPool, slug: &str, email: &str) -> String {
    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), $1, 't_' || $1, $1, $1 || '/', 'active') RETURNING id",
    )
    .bind(slug)
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
    issue(global, tenant_id, identity, &[]).await
}

async fn scoped_key(global: &PgPool, slug: &str, scopes: &[&str]) -> String {
    let (tenant_id, identity_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, m.identity_id FROM dam_global.tenants t \
         JOIN dam_global.tenant_members m ON m.tenant_id = t.id WHERE t.slug = $1",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant and member");
    issue(global, tenant_id, identity_id, scopes).await
}

async fn issue(global: &PgPool, tenant: Uuid, identity: Uuid, scopes: &[&str]) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, $5)",
    )
    .bind(tenant)
    .bind(identity)
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
    .expect("key");
    api_key.into_plaintext()
}

async fn asset(pool: &PgPool, filename: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(filename.as_bytes()).to_hex().to_string())
    .bind(filename)
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn send(app: &axum::Router, request: Request<Body>) -> axum::http::Response<Body> {
    app.clone().oneshot(request).await.expect("router")
}

fn post(uri: &str, key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn get(uri: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .body(Body::empty())
        .expect("request")
}

async fn json(response: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// ─── authorization ──────────────────────────────────────────────────────────

async fn a_read_only_key_cannot_touch_bulk_at_all(f: &Fixture) {
    // Manage, not Read, on every route — including the status read, because an operation's failure report
    // names assets by id and the ids in it are management detail.
    let id = asset(&f.acme, "ro-target.jpg").await;
    let body = json!({"kind": "delete", "asset_ids": [id]});

    for (label, request) in [
        (
            "preview",
            post("/bulk/preview", &f.read_only_key, body.clone()),
        ),
        ("create", post("/bulk", &f.read_only_key, body.clone())),
        (
            "status",
            get(&format!("/bulk/{}", Uuid::new_v4()), &f.read_only_key),
        ),
    ] {
        assert_eq!(
            send(&f.app, request).await.status(),
            StatusCode::FORBIDDEN,
            "{label} must require the manage scope"
        );
    }
}

// ─── scope filtering ────────────────────────────────────────────────────────

async fn ids_outside_the_callers_scope_fall_out_silently(f: &Fixture) {
    // The property this API exists to hold: the target list is client-assembled, and a caller must not be
    // able to act on assets they cannot manage by naming them. The foreign ids fall out; the caller learns a
    // *count* and nothing else — which of them were real assets is exactly what §7 forbids disclosing.
    let mine = asset(&f.acme, "scope-mine.jpg").await;
    let theirs = asset(&f.globex, "scope-theirs.jpg").await;
    let phantom = Uuid::new_v4();

    let preview = json(
        send(
            &f.app,
            post(
                "/bulk/preview",
                &f.key,
                json!({"kind": "delete", "asset_ids": [mine, theirs, phantom]}),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(
        preview["target_count"], 1,
        "only the manageable asset counts"
    );
    assert_eq!(
        preview["out_of_scope"], 2,
        "the rest are a count, not a list: {preview}"
    );
    assert_eq!(preview["sample"].as_array().expect("sample").len(), 1);

    // And creation applies the same filter, so the dialog's number is the operation's number.
    let accepted = json(
        send(
            &f.app,
            post(
                "/bulk",
                &f.key,
                json!({"kind": "delete", "asset_ids": [mine, theirs, phantom]}),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(accepted["target_count"], 1);

    // The other tenant's asset is untouched, whatever the executor later does with this operation.
    let their_row: bool = sqlx::query_scalar("SELECT deleted_at IS NULL FROM assets WHERE id = $1")
        .bind(theirs)
        .fetch_one(&f.globex)
        .await
        .expect("their asset");
    assert!(
        their_row,
        "another tenant's asset must never enter the operation"
    );
}

async fn a_selection_with_nothing_manageable_is_refused(f: &Fixture) {
    // Not an instantly-completed no-op: the history is where somebody looks to find what they ran, and a
    // "completed" operation over nothing hides the mistake.
    let theirs = asset(&f.globex, "nothing-mine.jpg").await;
    let response = send(
        &f.app,
        post(
            "/bulk",
            &f.key,
            json!({"kind": "delete", "asset_ids": [theirs]}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ─── the contract ───────────────────────────────────────────────────────────

async fn an_unexecutable_kind_is_a_422_not_a_dead_job(f: &Fixture) {
    let id = asset(&f.acme, "zip-me.jpg").await;
    let response = send(
        &f.app,
        post(
            "/bulk",
            &f.key,
            json!({"kind": "download_zip", "asset_ids": [id]}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json(response).await;
    assert!(
        body["message"]
            .as_str()
            .expect("a message")
            .contains("download_zip"),
        "the refusal names the kind: {body}"
    );
}

async fn creation_queues_the_job_and_status_reports_progress(f: &Fixture) {
    let one = asset(&f.acme, "queue-1.jpg").await;
    let two = asset(&f.acme, "queue-2.jpg").await;

    let response = send(
        &f.app,
        post(
            "/bulk",
            &f.key,
            json!({"kind": "delete", "asset_ids": [one, two]}),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "202: accepted, not done"
    );
    let accepted = json(response).await;
    let operation_id = accepted["id"].as_str().expect("id").to_owned();
    assert_eq!(accepted["state"], "queued");
    assert_eq!(accepted["target_count"], 2);
    assert_eq!(accepted["terminal"], false);

    // A worker job exists for it — the row without the job is a progress bar that never moves.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs WHERE kind = 'bulk' AND state = 'queued' \
         AND payload->>'operation_id' = $1",
    )
    .bind(&operation_id)
    .fetch_one(&f.global)
    .await
    .expect("job");
    assert_eq!(queued, 1);

    let status = json(send(&f.app, get(&format!("/bulk/{operation_id}"), &f.key)).await).await;
    assert_eq!(status["id"].as_str().expect("id"), operation_id);
    assert_eq!(status["terminal"], false);
}

async fn an_unknown_operation_is_404(f: &Fixture) {
    assert_eq!(
        send(&f.app, get(&format!("/bulk/{}", Uuid::new_v4()), &f.key))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_bulk_contract_holds() {
    let f = fixture().await;
    a_read_only_key_cannot_touch_bulk_at_all(&f).await;
    ids_outside_the_callers_scope_fall_out_silently(&f).await;
    a_selection_with_nothing_manageable_is_refused(&f).await;
    an_unexecutable_kind_is_a_422_not_a_dead_job(&f).await;
    creation_queues_the_job_and_status_reports_progress(&f).await;
    an_unknown_operation_is_404(&f).await;
}
