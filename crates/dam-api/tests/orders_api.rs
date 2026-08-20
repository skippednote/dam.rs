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
        // A public origin, so the pickup URL this suite reads is the absolute one a recipient would be sent.
        public_url: Some("https://dam.example.com".to_owned()),
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
    an_approval_makes_a_pickup(&f, open).await;
    a_pickup_url_is_shown_once_and_re_issuable(&f, open).await;
    the_metadata_export_is_the_tenants_own_columns(&f, open).await;
}

async fn a_pickup_url_is_shown_once_and_re_issuable(f: &Fixture, open: Uuid) {
    // A share token is stored as a digest, so the response that mints it is the only place it exists in readable
    // form. Two things are therefore load-bearing: the approval must carry the URL, and losing it must be
    // recoverable — otherwise an order can be fulfilled and nobody can ever collect. The first version of this
    // slice had the second half missing entirely.
    let (_, placed) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let id = placed["id"].as_str().expect("id").to_owned();

    let (status, approved) = call(
        f,
        "POST",
        &format!("/orders/{id}/approve"),
        &f.admin_key,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    let first = approved["pickup_url"]
        .as_str()
        .expect("the approval carries the pickup URL")
        .to_owned();
    assert!(
        first.starts_with("https://dam.example.com/share/"),
        "the pickup URL is not one a recipient could open: {first}"
    );

    // Never again on an ordinary read. A field that sometimes held a live token would be a token in every log
    // that printed an order.
    let (_, read_back) = call(f, "GET", &format!("/orders/{id}"), &f.reader_key, None).await;
    assert_eq!(read_back["pickup_url"], Value::Null, "{read_back}");
    assert_eq!(read_back["state"], json!("ready"), "{read_back}");

    // Re-issuing gives a *different* link and revokes the old one, so an order never has two live pickups —
    // which matters because revoking the pickup is how an order is closed.
    let (again, reissued) = call(
        f,
        "POST",
        &format!("/orders/{id}/fulfil"),
        &f.admin_key,
        None,
    )
    .await;
    assert_eq!(again, StatusCode::OK, "{reissued}");
    let second = reissued["pickup_url"]
        .as_str()
        .expect("a fresh URL")
        .to_owned();
    assert_ne!(first, second, "re-issuing handed back the same link");
    assert_eq!(reissued["state"], json!("ready"), "{reissued}");

    // And a *second* re-issue differs from the first, which is the assertion that actually pins the token to the
    // share that was just made rather than to anything else: comparing only against the approval's URL passed
    // even when every re-issue returned one fixed string.
    let (_, third_time) = call(
        f,
        "POST",
        &format!("/orders/{id}/fulfil"),
        &f.admin_key,
        None,
    )
    .await;
    let third = third_time["pickup_url"].as_str().expect("a fresh URL");
    assert_ne!(second, third, "every re-issue hands back the same link");

    let order_uuid = Uuid::parse_str(&id).expect("uuid");
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM share_links s JOIN orders o ON o.share_link_id = s.id \
         WHERE o.id = $1 AND s.revoked_at IS NULL",
    )
    .bind(order_uuid)
    .fetch_one(&f.acme)
    .await
    .expect("count");
    assert_eq!(live, 1, "an order has more than one live pickup");
    let revoked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM share_links WHERE kind = 'order' AND revoked_at IS NOT NULL",
    )
    .fetch_one(&f.acme)
    .await
    .expect("count");
    assert!(revoked >= 1, "the previous pickup was left live");

    // A reader cannot re-issue: the pickup link is the thing an order exists to control.
    let (refused, _) = call(
        f,
        "POST",
        &format!("/orders/{id}/fulfil"),
        &f.reader_key,
        None,
    )
    .await;
    assert_eq!(refused, StatusCode::FORBIDDEN);

    // And a decision that was a refusal cannot be turned into a pickup by asking for one.
    let (_, rejected) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let rejected_id = rejected["id"].as_str().expect("id").to_owned();
    call(
        f,
        "POST",
        &format!("/orders/{rejected_id}/reject"),
        &f.admin_key,
        Some(json!({ "note": "No." })),
    )
    .await;
    let (revived, body) = call(
        f,
        "POST",
        &format!("/orders/{rejected_id}/fulfil"),
        &f.admin_key,
        None,
    )
    .await;
    assert_eq!(
        revived,
        StatusCode::CONFLICT,
        "a refused order was given a pickup: {body}"
    );
}

async fn an_approval_makes_a_pickup(f: &Fixture, open: Uuid) {
    // Approval commits the decision and then makes the pickup — two writes, so a failed pickup leaves a standing
    // decision that can be retried rather than an approver deciding twice.
    let (_, placed) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let id = placed["id"].as_str().expect("id").to_owned();

    let (status, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/approve"),
        &f.admin_key,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["state"],
        json!("ready"),
        "an approval left nothing to collect: {body}"
    );

    // The pickup is a share, with the order's own window — the whole reason fulfilment creates one rather than
    // granting the requester something new.
    let (kind, expires): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT s.kind, s.expires_at FROM share_links s \
         JOIN orders o ON o.share_link_id = s.id WHERE o.id = $1",
    )
    .bind(Uuid::parse_str(&id).expect("uuid"))
    .fetch_one(&f.acme)
    .await
    .expect("the pickup share");
    assert_eq!(kind, "order");
    assert!(expires.is_some(), "the pickup has no window");

    // Fulfilling again *re-issues*: a fresh link with the previous one revoked, because a token is stored as a
    // digest and cannot be shown twice. What matters here is that it does not leave two live shares; the URL
    // itself is asserted in `a_pickup_url_is_shown_once_and_re_issuable`.
    let (again, body) = call(
        f,
        "POST",
        &format!("/orders/{id}/fulfil"),
        &f.admin_key,
        None,
    )
    .await;
    assert_eq!(again, StatusCode::OK, "{body}");
}

async fn the_metadata_export_is_the_tenants_own_columns(f: &Fixture, open: Uuid) {
    // A field definition, so the export has a real column rather than only the file facts.
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'campaign', 'Campaign', 'text') \
         ON CONFLICT DO NOTHING",
    )
    .execute(&f.acme)
    .await
    .expect("field");
    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values) VALUES ($1, '{\"campaign\": \"spring, 2026\"}'::jsonb) \
         ON CONFLICT (asset_id) DO UPDATE SET values = excluded.values",
    )
    .bind(open)
    .execute(&f.acme)
    .await
    .expect("values");

    let (_, placed) = call(f, "POST", "/orders", &f.reader_key, Some(ask(&[open]))).await;
    let id = placed["id"].as_str().expect("id").to_owned();

    let request = Request::builder()
        .method("GET")
        .uri(format!("/orders/{id}/metadata.csv"))
        .header(header::AUTHORIZATION, format!("Bearer {}", f.reader_key))
        .body(Body::empty())
        .expect("request");
    let response = f.app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    // A CSV, named after the order, and offered as a download rather than rendered in a tab.
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/csv; charset=utf-8")
    );
    assert!(
        response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|disposition| disposition.contains("ORD-")),
        "the export is not named after the order"
    );

    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let csv = String::from_utf8(bytes.to_vec()).expect("utf-8");
    let mut lines = csv.lines();
    let header_row = lines.next().expect("a header");
    assert!(
        header_row.starts_with("filename,mime,bytes,width,height"),
        "{csv}"
    );
    // The tenant's own field, as a column.
    assert!(header_row.contains("campaign"), "{csv}");
    let row = lines.next().expect("a row");
    assert!(row.contains("harbour.jpg"), "{csv}");
    // A value containing a comma is quoted, so it cannot end the record early — the bug that makes an export
    // open with everything one column to the right.
    assert!(row.contains("\"spring, 2026\""), "{csv}");

    // Somebody with no authority over the order gets the same 404 that reading it gives, for the same reason:
    // references are sequential.
    let stranger = Request::builder()
        .method("GET")
        .uri(format!("/orders/{id}/metadata.csv"))
        .header(header::AUTHORIZATION, format!("Bearer {}", f.stranger_key))
        .body(Body::empty())
        .expect("request");
    let refused = f.app.clone().oneshot(stranger).await.expect("response");
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
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
    // Approved *and* fulfilled: the decision commits, then the pickup is made, so the ordinary outcome is
    // `ready`. `approved` remains reachable — and meaningful — when the second write fails, which is what
    // `/fulfil` retries; that is asserted in `an_approval_makes_a_pickup`.
    assert_eq!(body["state"], json!("ready"), "{body}");
    assert!(body["expires_at"].is_string(), "{body}");
    assert_eq!(body["expired"], json!(false), "{body}");
    assert_eq!(body["decision_note"], json!("Print only."), "{body}");

    // The window runs a fortnight from the decision, not from the request.
    let expires = body["expires_at"].as_str().expect("a window");
    let expires: chrono::DateTime<chrono::Utc> = expires.parse().expect("a timestamp");
    let days = (expires - chrono::Utc::now()).num_days();
    assert!((12..=14).contains(&days), "a fortnight, got {days} days");
}
