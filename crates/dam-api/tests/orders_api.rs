//! The order endpoints (Q.13b).
//!
//! `dam_db`'s suite proves the state machine. What only exists here is the HTTP contract, and four decisions about
//! the *interface*:
//!
//! - **Asking is Read; deciding is Manage.** The point of an order is that somebody who may see assets but not
//!   take them can ask — requiring Download to place one would restrict the feature to the people who do not need
//!   it.
//! - **An order is readable by its requester and by whoever may decide**, and by nobody else. References are
//!   sequential, so "not yours" would confirm one exists.
//! - **A second decision is 409, naming what the order is**, because two approvers opening the same queue is the
//!   commonest way here and "cannot approve" leaves the second refreshing a screen.
//! - **An approver who cannot see the assets gets a 403 with a count**, not a 404: the problem is their scope,
//!   not the order's existence.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::orders::{OrderState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    acme: PgPool,
    /// A tenant admin: Read and Manage.
    admin_key: String,
    /// Read only — the person an order exists for.
    reader_key: String,
    /// Another reader, to prove an order is not everybody's business.
    stranger_key: String,
    /// Manage, but scoped to `group`, so some assets are outside their reach.
    scoped_admin_key: String,
    group: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let admin_key = provision(&global, "acme", "ada@example.com").await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'narrow', 'Narrow') \
         RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");

    for (role, permissions, groups) in [
        ("reader", vec!["asset:read"], None),
        ("stranger", vec!["asset:read"], None),
        (
            "narrow_admin",
            vec!["asset:read", "asset:manage"],
            Some(group),
        ),
    ] {
        let (ids, all) = match groups {
            Some(id) => (vec![id], false),
            None => (vec![], true),
        };
        sqlx::query(
            "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
             VALUES (gen_random_uuid(), $1, $1, $2, $3, $4)",
        )
        .bind(role)
        .bind(
            permissions
                .iter()
                .map(|p| (*p).to_owned())
                .collect::<Vec<String>>(),
        )
        .bind(&ids)
        .bind(all)
        .execute(&acme)
        .await
        .expect("role");
    }

    let reader_key = person_key(&global, "acme", "rita@example.com", &["reader"]).await;
    let stranger_key = person_key(&global, "acme", "sam@example.com", &["stranger"]).await;
    let scoped_admin_key = person_key(&global, "acme", "nina@example.com", &["narrow_admin"]).await;

    let app = router(OrderState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        app,
        acme,
        admin_key,
        reader_key,
        stranger_key,
        scoped_admin_key,
        group,
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
    let identity = identity(global, email).await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(identity)
    .execute(global)
    .await
    .expect("membership");
    issue(global, tenant_id, Some(identity)).await
}

async fn identity(global: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(global)
    .await
    .expect("identity")
}

async fn person_key(global: &PgPool, slug: &str, email: &str, roles: &[&str]) -> String {
    let tenant_id: Uuid = sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
        .bind(slug)
        .fetch_one(global)
        .await
        .expect("tenant");
    let identity = identity(global, email).await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3, false)",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(roles.iter().map(|r| (*r).to_owned()).collect::<Vec<String>>())
    .execute(global)
    .await
    .expect("membership");
    issue(global, tenant_id, Some(identity)).await
}

async fn issue(global: &PgPool, tenant: Uuid, identity: Option<Uuid>) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant)
    .bind(identity)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn asset(f: &Fixture, label: &str, in_group: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(&f.acme)
    .await
    .expect("asset");
    if in_group {
        sqlx::query("INSERT INTO asset_group_members (asset_id, group_id) VALUES ($1, $2)")
            .bind(id)
            .bind(f.group)
            .execute(&f.acme)
            .await
            .expect("membership");
    }
    id
}

async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json");
    let request = match &body {
        Some(json) => request.body(Body::from(json.to_string())).expect("request"),
        None => request.body(Body::empty()).expect("request"),
    };
    let response = f.app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ask(ids: &[Uuid]) -> Value {
    json!({
        "asset_ids": ids.iter().map(Uuid::to_string).collect::<Vec<String>>(),
        "purpose": "The spring brochure, print run of 20,000.",
        "channel": "print",
        "territory": "GB",
        "include_metadata": true,
        "recipients": ["agency@example.com"]
    })
}

#[tokio::test]
async fn the_order_http_contract_holds() {
    let f = fixture().await;
    let open = asset(&f, "harbour", false).await;
    let narrow = asset(&f, "quay", true).await;

    a_reader_may_ask(&f, open).await;
    an_order_needs_a_reason(&f, open).await;
    only_a_manager_sees_the_queue(&f).await;
    an_order_is_not_everybodys_business(&f, open).await;
    an_approver_outside_the_scope_is_told_why(&f, open, narrow).await;
    a_second_decision_is_a_conflict(&f, open).await;
    a_requester_withdraws_their_own_and_only_before_a_decision(&f, open).await;
    an_approval_opens_a_window_without_handing_anything_over(&f, open).await;
}

async fn a_reader_may_ask(f: &Fixture, open: Uuid) {
    // The whole point: somebody who may see assets and not take them can ask. Read is enough.
    let (status, body) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        body["reference"]
            .as_str()
            .is_some_and(|reference| reference.starts_with("ORD-")),
        "{body}"
    );
    assert_eq!(body["state"], json!("submitted"), "{body}");
    assert_eq!(body["expired"], json!(false), "{body}");
    assert_eq!(body["self_approved"], json!(false), "{body}");
    // The person is named, the reason travels, and so do the two answers the rest of the system wants.
    assert_eq!(
        body["requested_by"]["email"],
        json!("rita@example.com"),
        "{body}"
    );
    assert!(
        body["purpose"]
            .as_str()
            .is_some_and(|purpose| purpose.contains("brochure")),
        "{body}"
    );
    assert_eq!(body["channel"], json!("print"), "{body}");
    assert_eq!(body["items"][0]["filename"], json!("harbour.jpg"), "{body}");

    // And it is in their own list.
    let (_, mine) = call(f, "GET", "/orders", &f.reader_key, None).await;
    assert_eq!(mine.as_array().expect("array").len(), 1, "{mine}");
    // Not in somebody else's.
    let (_, theirs) = call(f, "GET", "/orders", &f.stranger_key, None).await;
    assert_eq!(theirs, json!([]), "{theirs}");
}

async fn an_order_needs_a_reason(f: &Fixture, open: Uuid) {
    // The reason is the entire question an approver answers. An order without one forces them to guess or to go
    // and ask, which is the email this feature replaces.
    let mut anonymous = ask(&[open]);
    anonymous["purpose"] = json!("   ");
    let (status, body) = call(f, "POST", "/orders", &f.reader_key, Some(anonymous)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // And something to order.
    let mut nothing = ask(&[]);
    nothing["asset_ids"] = json!([]);
    let (empty, body) = call(f, "POST", "/orders", &f.reader_key, Some(nothing)).await;
    assert_eq!(empty, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

async fn only_a_manager_sees_the_queue(f: &Fixture) {
    let (refused, _) = call(f, "GET", "/orders/queue", &f.reader_key, None).await;
    assert_eq!(
        refused,
        StatusCode::FORBIDDEN,
        "a reader can see what everybody has asked for"
    );

    let (status, body) = call(f, "GET", "/orders/queue", &f.admin_key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.as_array().expect("array").is_empty(), "{body}");
    // Only what needs deciding, or the queue never empties.
    assert!(
        body.as_array()
            .expect("array")
            .iter()
            .all(|order| order["state"] == json!("submitted")),
        "{body}"
    );
}

async fn an_order_is_not_everybodys_business(f: &Fixture, open: Uuid) {
    let (_, placed) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let id = placed["id"].as_str().expect("id").to_owned();

    // Theirs: readable.
    let (mine, _) = call(f, "GET", &format!("/orders/{id}"), &f.reader_key, None).await;
    assert_eq!(mine, StatusCode::OK);
    // An approver's: readable, because deciding requires reading.
    let (approver, _) = call(f, "GET", &format!("/orders/{id}"), &f.admin_key, None).await;
    assert_eq!(approver, StatusCode::OK);

    // A colleague with no authority over it: 404, not 403. References are sequential, so "not yours" would
    // confirm that one exists.
    let (stranger, _) = call(f, "GET", &format!("/orders/{id}"), &f.stranger_key, None).await;
    assert_eq!(stranger, StatusCode::NOT_FOUND);
    let (nowhere, _) = call(
        f,
        "GET",
        &format!("/orders/{}", Uuid::new_v4()),
        &f.stranger_key,
        None,
    )
    .await;
    assert_eq!(nowhere, StatusCode::NOT_FOUND, "the two answers differ");
}

async fn an_approver_outside_the_scope_is_told_why(f: &Fixture, open: Uuid, narrow: Uuid) {
    // An order containing an asset the approver cannot see. Agreeing to hand over something you cannot inspect
    // is a signature on a blank page, so it is refused — with a count, because the problem is their scope rather
    // than the order being gone.
    let (_, placed) = call(
        f,
        "POST",
        "/orders",
        &f.reader_key,
        Some(ask(&[open, narrow])),
    )
    .await;
    let id = placed["id"].as_str().expect("id").to_owned();

    let (status, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/approve"),
        &f.scoped_admin_key,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("outside your scope")),
        "{body}"
    );

    // Rejection needs no such visibility: saying no to something you cannot see is defensible, and requiring it
    // would leave orders nobody can close.
    let (rejected, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/reject"),
        &f.scoped_admin_key,
        Some(json!({ "note": "Not for external use." })),
    )
    .await;
    assert_eq!(rejected, StatusCode::OK, "{body}");
    assert_eq!(body["state"], json!("rejected"), "{body}");
    assert_eq!(
        body["decided_by"]["email"],
        json!("nina@example.com"),
        "{body}"
    );
    assert_eq!(
        body["decision_note"],
        json!("Not for external use."),
        "{body}"
    );
}

async fn a_second_decision_is_a_conflict(f: &Fixture, open: Uuid) {
    let (_, placed) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let id = placed["id"].as_str().expect("id").to_owned();

    let (first, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/approve"),
        &f.admin_key,
        Some(json!({})),
    )
    .await;
    assert_eq!(first, StatusCode::OK, "{body}");

    let (second, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/approve"),
        &f.admin_key,
        Some(json!({})),
    )
    .await;
    assert_eq!(second, StatusCode::CONFLICT, "{body}");
    // Says what it *is*, so the second approver is not left refreshing a screen.
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("approved")),
        "{body}"
    );
}

async fn a_requester_withdraws_their_own_and_only_before_a_decision(f: &Fixture, open: Uuid) {
    let (_, placed) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let id = placed["id"].as_str().expect("id").to_owned();

    let (someone_else, _) = call(
        f,
        "POST",
        &format!("/orders/{id}/cancel"),
        &f.stranger_key,
        None,
    )
    .await;
    assert_eq!(someone_else, StatusCode::FORBIDDEN);

    let (status, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/cancel"),
        &f.reader_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], json!("cancelled"), "{body}");

    // After a decision there is nothing to cancel: an approval is somebody else's recorded act.
    let (_, decided) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let decided_id = decided["id"].as_str().expect("id").to_owned();
    call(
        f,
        "POST",
        &format!("/orders/{decided_id}/approve"),
        &f.admin_key,
        Some(json!({})),
    )
    .await;
    let (too_late, body) = call(
        f,
        "POST",
        &format!("/orders/{decided_id}/cancel"),
        &f.reader_key,
        None,
    )
    .await;
    assert_eq!(too_late, StatusCode::CONFLICT, "{body}");
}

async fn an_approval_opens_a_window_without_handing_anything_over(f: &Fixture, open: Uuid) {
    let (_, placed) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let id = placed["id"].as_str().expect("id").to_owned();

    let (status, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/approve"),
        &f.admin_key,
        Some(json!({ "note": "Print only." })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Approved, with a window — and *nothing to collect yet*. The gap between `approved` and `ready` is the
    // difference between a decision having been made and the bytes being reachable, which is what stops an
    // approval from being a grant.
    assert_eq!(body["state"], json!("approved"), "{body}");
    assert!(body["expires_at"].is_string(), "{body}");
    assert_eq!(body["expired"], json!(false), "{body}");
    assert_eq!(body["decision_note"], json!("Print only."), "{body}");

    // The window runs a fortnight from the decision, not from the request.
    let expires = body["expires_at"].as_str().expect("a window");
    let expires: chrono::DateTime<chrono::Utc> = expires.parse().expect("a timestamp");
    let days = (expires - chrono::Utc::now()).num_days();
    assert!((12..=14).contains(&days), "a fortnight, got {days} days");
}
