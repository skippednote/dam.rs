//! The dashboard endpoint (Q.7).
//!
//! `dam_db`'s suite proves the feed's gates and the counts. What only exists here is the HTTP contract, and three
//! things that are decisions about the *interface*:
//!
//! - **Every number is the caller's.** §7: a count is a disclosure, so a scoped reader must not be shown the
//!   library's totals with their own results beneath them.
//! - **A feed line must not carry a secret.** A share event records that a share was made and never its token; a
//!   comment event records that somebody commented and never the words.
//! - **A spotlight's count is named as cached**, because it is computed for nobody in particular and presenting it
//!   as the viewer's would leak how many assets exist beyond their scope.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::dashboard::{DashboardState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    acme: PgPool,
    /// The control-plane pool. Unused by this suite's cases, and kept because `person_key` needs one — see the
    /// note there.
    #[allow(dead_code)]
    global: PgPool,
    /// A tenant admin, with a person behind it.
    key: String,
    /// A third person, to read a feed without being a comment's audience.
    stranger_key: String,
    /// Grace's identity, for addressing a private comment at somebody.
    grace: Uuid,
    /// A key with no identity: a service credential.
    machine_key: String,
    /// A person who may see only `group`.
    scoped_key: String,
    group: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let key = provision(&global, "acme", "ada@example.com").await;
    // Grace exists so a private comment has somebody to be addressed to; this suite never calls as her.
    person_key(&global, "acme", "grace@example.com", &[], true).await;
    let stranger_key = person_key(&global, "acme", "mallory@example.com", &[], true).await;
    let machine_key = machine_key(&global, "acme").await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'visible', 'Visible') \
         RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'visible_only', 'Visible only', '{asset:read}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let scoped_key = person_key(
        &global,
        "acme",
        "scoped@example.com",
        &["visible_only"],
        false,
    )
    .await;

    let grace = identity_of(&global, "grace@example.com").await;

    // The comment and share routers alongside, because the feed is only interesting once something has written
    // to it — and what writes to it is those paths.
    let app = router(DashboardState {
        global: global.clone(),
    })
    .merge(dam_api::comments::router(dam_api::comments::CommentState {
        global: global.clone(),
    }));

    Fixture {
        _pg: pg,
        app,
        acme,
        global: global.clone(),
        key,
        stranger_key,
        machine_key,
        scoped_key,
        group,
        grace,
    }
}

async fn identity_of(global: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM dam_global.identities WHERE email = $1")
        .bind(email)
        .fetch_one(global)
        .await
        .expect("identity")
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
    issue(global, tenant_id, Some(identity), &[]).await
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

async fn person_key(
    global: &PgPool,
    slug: &str,
    email: &str,
    roles: &[&str],
    admin: bool,
) -> String {
    let tenant_id: Uuid = sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
        .bind(slug)
        .fetch_one(global)
        .await
        .expect("tenant");
    let identity = identity(global, email).await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(roles.iter().map(|r| (*r).to_owned()).collect::<Vec<String>>())
    .bind(admin)
    .execute(global)
    .await
    .expect("membership");
    issue(global, tenant_id, Some(identity), &[]).await
}

/// A key belonging to the tenant but to nobody in it.
///
/// This is the shape a service credential has, and building one is the only way to test the identity rule — no
/// other suite had needed a key with a null `identity_id`.
async fn machine_key(global: &PgPool, slug: &str) -> String {
    let tenant_id: Uuid = sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
        .bind(slug)
        .fetch_one(global)
        .await
        .expect("tenant");
    issue(global, tenant_id, None, &["asset:read"]).await
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
            .map(|s| (*s).to_owned())
            .collect::<Vec<String>>(),
    )
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

#[tokio::test]
async fn the_dashboard_http_contract_holds() {
    let f = fixture().await;
    let visible = asset(&f, "visible", true).await;
    let hidden = asset(&f, "hidden", false).await;

    an_empty_library_has_zeroes_rather_than_nothing(&f).await;
    activity_appears_with_the_actor_named(&f, visible).await;
    a_scoped_caller_sees_their_own_counts_and_feed(&f, visible, hidden).await;
    a_comment_event_carries_no_words(&f, visible).await;
    the_work_queue_counts_assets_with_nothing_written(&f).await;
    a_machine_credential_has_no_dashboard(&f).await;
}

async fn an_empty_library_has_zeroes_rather_than_nothing(f: &Fixture) {
    let (status, body) = call(f, "GET", "/dashboard", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Zeroes, not absent keys: a page that has to distinguish "none" from "not told" cannot render a number.
    assert_eq!(body["counts"]["uploads_this_week"], json!(0), "{body}");
    assert_eq!(body["counts"]["comments_this_week"], json!(0), "{body}");
    assert_eq!(body["activity"], json!([]), "{body}");
    assert_eq!(body["spotlights"], json!([]), "{body}");
    assert_eq!(body["counts"]["assets"], json!(2), "both fixtures: {body}");
}

async fn activity_appears_with_the_actor_named(f: &Fixture, visible: Uuid) {
    let (status, posted) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({ "body": "The crop is tight" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{posted}");

    let (_, body) = call(f, "GET", "/dashboard", &f.key, None).await;
    let line = body["activity"]
        .as_array()
        .expect("activity")
        .first()
        .unwrap_or_else(|| panic!("nothing in the feed: {body}"))
        .clone();
    assert_eq!(line["kind"], json!("comment"), "{line}");
    // The filename and the actor's name, both resolved server-side: "Ada commented on harbour.jpg" is the whole
    // value of a feed, and a line of uuids has none of it.
    assert_eq!(line["filename"], json!("visible.jpg"), "{line}");
    assert_eq!(line["actor"]["email"], json!("ada@example.com"), "{line}");
    assert_eq!(body["counts"]["comments_this_week"], json!(1), "{body}");
}

async fn a_scoped_caller_sees_their_own_counts_and_feed(f: &Fixture, visible: Uuid, hidden: Uuid) {
    // Something happens to the asset the scoped caller cannot see.
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{hidden}/comments"),
        &f.key,
        Some(json!({ "body": "About the embargoed one" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, wide) = call(f, "GET", "/dashboard", &f.key, None).await;
    let (_, narrow) = call(f, "GET", "/dashboard", &f.scoped_key, None).await;

    // Every number is the caller's. A dashboard showing the library total would tell a scoped reader exactly how
    // much of it they cannot reach.
    assert_eq!(wide["counts"]["assets"], json!(2), "{wide}");
    assert_eq!(narrow["counts"]["assets"], json!(1), "{narrow}");
    assert!(
        narrow["counts"]["comments_this_week"].as_i64()
            < wide["counts"]["comments_this_week"].as_i64(),
        "the comment count ignored the predicate: {narrow} vs {wide}"
    );

    // And the feed does not name the asset — or its filename, which is the part that leaks what it is.
    let lines = narrow["activity"].as_array().expect("activity");
    assert!(
        lines
            .iter()
            .all(|line| line["asset_id"] != json!(hidden.to_string())),
        "{narrow}"
    );
    assert!(
        lines
            .iter()
            .all(|line| line["filename"] != json!("hidden.jpg")),
        "the feed leaked a filename: {narrow}"
    );
    // Not simply empty, or the two assertions above would hold for the wrong reason.
    assert!(
        lines
            .iter()
            .any(|line| line["asset_id"] == json!(visible.to_string())),
        "{narrow}"
    );
}

async fn a_comment_event_carries_no_words(f: &Fixture, visible: Uuid) {
    let secret = "Legal has not cleared this yet";
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({
            "body": secret,
            "visibility": "private",
            "recipients": [f.grace.to_string()],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The third person can read the feed and must not find the words there. A private comment restricted to two
    // people, with an excerpt on a dashboard, would be a leak wearing the shape of a convenience.
    let (_, body) = call(f, "GET", "/dashboard", &f.stranger_key, None).await;
    let text = body.to_string();
    assert!(
        !text.contains(secret),
        "the feed carried a private comment's words: {text}"
    );
    // The event is there, though — that somebody commented is not the same as what they said.
    assert!(
        body["activity"]
            .as_array()
            .expect("activity")
            .iter()
            .any(|line| line["kind"] == json!("comment")),
        "{body}"
    );
    // And it says which kind of comment it was, which is useful and discloses nothing.
    assert!(text.contains("private"), "{text}");
}

async fn the_work_queue_counts_assets_with_nothing_written(f: &Fixture) {
    let (_, before) = call(f, "GET", "/dashboard", &f.key, None).await;
    assert_eq!(
        before["counts"]["without_metadata"],
        json!(2),
        "neither fixture has metadata: {before}"
    );

    // An empty document still counts: ingest gives every asset a row, so a queue that only looked for a missing
    // one would report no work over a library nobody has described.
    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values) SELECT id, '{}'::jsonb FROM assets LIMIT 1",
    )
    .execute(&f.acme)
    .await
    .expect("empty metadata");
    let (_, still) = call(f, "GET", "/dashboard", &f.key, None).await;
    assert_eq!(still["counts"]["without_metadata"], json!(2), "{still}");

    sqlx::query("UPDATE asset_metadata SET values = '{\"caption\": \"described\"}'::jsonb")
        .execute(&f.acme)
        .await
        .expect("described");
    let (_, after) = call(f, "GET", "/dashboard", &f.key, None).await;
    assert_eq!(after["counts"]["without_metadata"], json!(1), "{after}");
}

async fn a_machine_credential_has_no_dashboard(f: &Fixture) {
    // A dashboard is somebody's: the spotlights are their saved searches, and there is no identity to compare a
    // role share against. `authorize` refuses a key with no identity for every endpoint anyway — no identity means
    // no membership, so no grants — so this asserts the contract rather than a branch in the handler.
    let (status, body) = call(f, "GET", "/dashboard", &f.machine_key, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}
