//! Webhook subscriptions over HTTP (Q.20c, §11).
//!
//! `dam_db` proves the outbox and the ordering; `dam_connect` proves the signature over a real socket. What
//! lives only here are the decisions about the administration surface, and one of them is a security control
//! rather than a preference:
//!
//! - **A subscription URL is an SSRF vector.** It is a server-side request to an address a tenant chose, so a
//!   tenant who could register `http://169.254.169.254/` would have damrs fetch cloud instance credentials on
//!   their behalf — and read the response out of the delivery log. Loopback, private, link-local and
//!   credential-bearing URLs are refused, and `https` is required outside development.
//! - **The secret is shown exactly once.** A receiver cannot verify anything without it, so it must be
//!   returned; returning it on every read would put it in the response of an endpoint an integration polls.
//! - **The log carries no payloads.** It is the largest column on the query a screen runs most often, and
//!   returning them would make this the cheapest way to read a tenant's whole change history.
//! - **Retry takes only a dead letter**, and only one belonging to the subscription in the path. Reviving
//!   something in flight would break the per-asset ordering the outbox exists to keep.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::webhooks::{WebhookState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    /// Staging and production: https, and no loopback.
    app: axum::Router,
    /// A developer's machine: http and loopback permitted, private and link-local still refused.
    dev: axum::Router,
    key: String,
    read_only_key: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");
    let admin: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), 'ada@example.com', 'Ada') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("identity");
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(admin)
    .execute(&global)
    .await
    .expect("membership");

    Fixture {
        _pg: pg,
        app: router(WebhookState {
            global: global.clone(),
            development: false,
        }),
        dev: router(WebhookState {
            global: global.clone(),
            development: true,
        }),
        key: issue(&global, tenant_id, Some(admin), &[]).await,
        read_only_key: issue(&global, tenant_id, Some(admin), &["asset:read"]).await,
        global,
        acme,
    }
}

async fn issue(global: &PgPool, tenant: Uuid, identity: Option<Uuid>, scopes: &[&str]) -> String {
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
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<String>>(),
    )
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn call_on(
    app: &axum::Router,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(match &body {
                    Some(value) => Body::from(value.to_string()),
                    None => Body::empty(),
                })
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call_on(&f.app, method, path, key, body).await
}

/// Registers a subscription and returns `(id, secret)`.
async fn subscribe(f: &Fixture, url: &str, kinds: Value) -> (String, String) {
    let (status, made) = call(
        f,
        "POST",
        "/webhooks",
        Some(&f.key),
        Some(json!({ "url": url, "event_kinds": kinds })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    (
        made["id"].as_str().expect("id").to_owned(),
        made["secret"].as_str().expect("secret").to_owned(),
    )
}

// ─── the SSRF guard ─────────────────────────────────────────────────────────

async fn the_server_refuses_to_post_to_itself_or_its_host(f: &Fixture) {
    // The control that matters. A subscription is a server-side request to an address the tenant chose, so
    // without this a tenant registers the cloud metadata service and reads instance credentials out of the
    // delivery log.
    let refused = [
        (
            "https://169.254.169.254/latest/meta-data/",
            "link-local, and the metadata service",
        ),
        ("https://127.0.0.1/hook", "loopback"),
        ("https://localhost/hook", "loopback by name"),
        ("https://sub.localhost/hook", "and by suffix"),
        ("https://10.0.0.5/hook", "private"),
        ("https://192.168.1.1/hook", "private"),
        ("https://172.16.0.1/hook", "private"),
        ("https://[::1]/hook", "loopback, v6"),
        ("https://[fd00::1]/hook", "unique-local, v6"),
        ("https://[fe80::1]/hook", "link-local, v6"),
        ("https://0.0.0.0/hook", "unspecified"),
    ];
    for (url, why) in refused {
        let (status, body) = call(
            f,
            "POST",
            "/webhooks",
            Some(&f.key),
            Some(json!({ "url": url })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{url} is {why} and must be refused, got {body}"
        );
    }

    // And the ones that are refused for a different reason.
    for (url, expected) in [
        ("http://example.test/hook", "must be https"),
        (
            "https://user:pass@example.test/hook",
            "must not carry credentials",
        ),
        ("ftp://example.test/hook", "not a scheme"),
        ("not-a-url", "is not a URL"),
    ] {
        let (status, body) = call(
            f,
            "POST",
            "/webhooks",
            Some(&f.key),
            Some(json!({ "url": url })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {body}");
        let reason = body["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(expected),
            "{url} should say {expected:?}, said {reason:?}"
        );
    }

    // Development permits exactly two of those: http, and a loopback receiver. The second one is a defect
    // fix rather than a convenience — the first version of this refused `http://127.0.0.1:9099/hook`, which
    // is the shape of every receiver a developer writes, so building a webhook integration locally was
    // impossible without hand-written SQL. There is no privilege boundary to cross on a developer's own
    // machine: the tenant, the server and the receiver are all theirs.
    for url in [
        "http://example.test/hook",
        "http://127.0.0.1:9099/hook",
        "http://localhost:9099/hook",
        "http://[::1]:9099/hook",
    ] {
        let (status, body) = call_on(
            &f.dev,
            "POST",
            "/webhooks",
            Some(&f.key),
            Some(json!({ "url": url })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{url} should register in development: {body}"
        );
    }

    // And the one that stays refused everywhere, including development: a development box is often a cloud
    // VM, and 169.254.169.254 is the metadata service. Nobody has a reason to point a webhook at it, so the
    // one address where a mistake is unrecoverable is never allowed.
    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5/hook",
        "http://192.168.1.1/hook",
        "http://[fd00::1]/hook",
    ] {
        let (status, body) = call_on(
            &f.dev,
            "POST",
            "/webhooks",
            Some(&f.key),
            Some(json!({ "url": url })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{url} must be refused even in development: {body}"
        );
        let reason = body["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("even in development"),
            "the refusal should say the rule is not relaxed here: {reason}"
        );
    }
}

// ─── the secret ─────────────────────────────────────────────────────────────

async fn the_secret_is_returned_once_and_never_again(f: &Fixture) {
    let (id, secret) = subscribe(f, "https://example.test/once", json!([])).await;
    assert!(secret.len() > 20, "a generated secret, not a placeholder");

    let (_, listed) = call(f, "GET", "/webhooks", Some(&f.key), None).await;
    let rendered = listed.to_string();
    assert!(
        !rendered.contains(&secret),
        "the list must not carry the signing key: {rendered}"
    );
    let row = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|one| one["id"] == id)
        .expect("listed");
    assert!(row.get("secret").is_none(), "not even as a field");
    assert_eq!(row["active"], true);
    assert_eq!(row["consecutive_failures"], 0);
    assert!(row["disabled_reason"].is_null());

    // And the response that did carry it explains how to use it, so a customer does not have to go looking.
    let (_, made) = call(
        f,
        "POST",
        "/webhooks",
        Some(&f.key),
        Some(json!({ "url": "https://example.test/explained" })),
    )
    .await;
    let note = made["signature_note"].as_str().expect("a note");
    assert!(note.contains("X-Damrs-Signature"), "{note}");
    assert!(note.contains("HMAC-SHA256"), "{note}");
    assert!(
        note.contains("replayed"),
        "and why the timestamp matters: {note}"
    );
}

async fn registering_a_webhook_needs_manage(f: &Fixture) {
    for (method, path) in [("GET", "/webhooks"), ("POST", "/webhooks")] {
        let (status, _) = call(
            f,
            method,
            path,
            Some(&f.read_only_key),
            (method == "POST").then(|| json!({ "url": "https://example.test/x" })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    let (status, _) = call(f, "GET", "/webhooks", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ─── the log and the retry ──────────────────────────────────────────────────

async fn the_log_shows_state_without_payloads(f: &Fixture) {
    let (id, _) = subscribe(f, "https://example.test/logged", json!([])).await;
    let uuid = Uuid::parse_str(&id).expect("uuid");
    let queued = dam_db::webhooks::enqueue(
        &mut f.acme.acquire().await.expect("conn"),
        "asset.published",
        None,
        &json!({"confidential": "must not appear in a log"}),
    )
    .await
    .expect("enqueue");
    assert!(queued >= 1);

    let (status, log) = call(
        f,
        "GET",
        &format!("/webhooks/{id}/deliveries"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{log}");
    let rendered = log.to_string();
    assert!(
        !rendered.contains("must not appear"),
        "the log carries no payloads: {rendered}"
    );
    let rows = log.as_array().expect("array");
    let row = rows
        .iter()
        .find(|one| one["event_kind"] == "asset.published")
        .expect("the row");
    assert_eq!(row["state"], "pending");
    assert_eq!(row["attempts"], 0);
    // Absent rather than zero: a timeout and a 500 are different diagnoses.
    assert!(row["response_status"].is_null());
    let _ = uuid;
}

async fn only_a_dead_delivery_can_be_retried(f: &Fixture) {
    let (id, _) = subscribe(f, "https://example.test/retryable", json!([])).await;
    let uuid = Uuid::parse_str(&id).expect("uuid");
    dam_db::webhooks::enqueue(
        &mut f.acme.acquire().await.expect("conn"),
        "asset.expired",
        None,
        &json!({}),
    )
    .await
    .expect("enqueue");
    let delivery: Uuid = sqlx::query_scalar(
        "SELECT id FROM webhook_deliveries WHERE subscription_id = $1 AND event_kind = 'asset.expired'",
    )
    .bind(uuid)
    .fetch_one(&f.acme)
    .await
    .expect("delivery");

    // Pending: reviving it would jump the queue for something already in line, which breaks the per-asset
    // ordering the whole outbox exists to keep.
    let (status, _) = call(
        f,
        "POST",
        &format!("/webhooks/{id}/deliveries/{delivery}/retry"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a pending delivery is not retryable"
    );

    sqlx::query("UPDATE webhook_deliveries SET state = 'dead' WHERE id = $1")
        .bind(delivery)
        .execute(&f.acme)
        .await
        .expect("dead");
    let (status, _) = call(
        f,
        "POST",
        &format!("/webhooks/{id}/deliveries/{delivery}/retry"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // And a delivery belonging to another subscription is not reachable through this one's path, or the id in
    // the URL would be decoration and a guessed id would confirm the delivery exists.
    let (other, _) = subscribe(f, "https://example.test/elsewhere", json!([])).await;
    sqlx::query("UPDATE webhook_deliveries SET state = 'dead' WHERE id = $1")
        .bind(delivery)
        .execute(&f.acme)
        .await
        .expect("dead again");
    let (status, _) = call(
        f,
        "POST",
        &format!("/webhooks/{other}/deliveries/{delivery}/retry"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn a_disabled_subscription_can_be_enabled_again(f: &Fixture) {
    let (id, _) = subscribe(f, "https://example.test/revivable", json!([])).await;
    let uuid = Uuid::parse_str(&id).expect("uuid");
    sqlx::query(
        "UPDATE webhook_subscriptions \
         SET active = false, disabled_reason = 'disabled automatically', consecutive_failures = 5 \
         WHERE id = $1",
    )
    .bind(uuid)
    .execute(&f.acme)
    .await
    .expect("disable");

    let (status, enabled) = call(
        f,
        "POST",
        &format!("/webhooks/{id}/enable"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    assert_eq!(enabled["active"], true);
    assert!(enabled["disabled_reason"].is_null());
    // Forgiven, or one more failure would disable it again and "enable" would look broken.
    assert_eq!(enabled["consecutive_failures"], 0);

    let (status, _) = call(
        f,
        "POST",
        &format!("/webhooks/{}/enable", Uuid::new_v4()),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn removing_a_subscription_takes_its_queue(f: &Fixture) {
    let (id, _) = subscribe(f, "https://example.test/leaving", json!([])).await;
    let uuid = Uuid::parse_str(&id).expect("uuid");
    dam_db::webhooks::enqueue(
        &mut f.acme.acquire().await.expect("conn"),
        "asset.published",
        None,
        &json!({}),
    )
    .await
    .expect("enqueue");

    let (status, _) = call(f, "DELETE", &format!("/webhooks/{id}"), Some(&f.key), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE subscription_id = $1")
            .bind(uuid)
            .fetch_one(&f.acme)
            .await
            .expect("count");
    assert_eq!(left, 0, "the queue goes with it, by cascade");

    let (status, _) = call(f, "DELETE", &format!("/webhooks/{id}"), Some(&f.key), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "twice is a 404, not a second 204"
    );
}

async fn an_event_filter_is_stored_as_given(f: &Fixture) {
    let (id, _) = subscribe(
        f,
        "https://example.test/filtered",
        json!(["asset.published", "asset.expired"]),
    )
    .await;
    let (_, listed) = call(f, "GET", "/webhooks", Some(&f.key), None).await;
    let row = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|one| one["id"] == id)
        .expect("listed");
    assert_eq!(
        row["event_kinds"],
        json!(["asset.published", "asset.expired"])
    );

    // And the empty filter means everything, which is the schema's default and the useful reading — a client
    // that has not thought about filtering wants events rather than silence.
    let (other, _) = subscribe(f, "https://example.test/unfiltered", json!([])).await;
    let (_, listed) = call(f, "GET", "/webhooks", Some(&f.key), None).await;
    let row = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|one| one["id"] == other)
        .expect("listed");
    assert_eq!(row["event_kinds"], json!([]));
}

#[tokio::test]
async fn the_webhook_contract_holds() {
    let f = fixture().await;

    registering_a_webhook_needs_manage(&f).await;
    the_server_refuses_to_post_to_itself_or_its_host(&f).await;
    the_secret_is_returned_once_and_never_again(&f).await;
    an_event_filter_is_stored_as_given(&f).await;
    the_log_shows_state_without_payloads(&f).await;
    only_a_dead_delivery_can_be_retried(&f).await;
    a_disabled_subscription_can_be_enabled_again(&f).await;
    removing_a_subscription_takes_its_queue(&f).await;

    assert!(!f.global.is_closed());
}
