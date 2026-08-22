//! The version endpoints (Q.8).
//!
//! `dam_db`'s suite proves the model and the listing filter. What only exists here is the HTTP contract, and three
//! things that are decisions about the *interface*:
//!
//! - **Reading a history is Read; superseding is Manage.** Which bytes everybody gets from now on is a content
//!   decision, not a view preference.
//! - **A stale supersede is 409, not 422.** The request is well formed and the world moved on, so the client's
//!   correct response is to reload and retry — which is what a conflict says and a 422 does not.
//! - **Adding a version joins an already-uploaded asset.** A multipart endpoint here would be a second ingest path,
//!   and two ingest paths diverge.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::versions::{VersionState, router};
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
    /// A key with `asset:read` and nothing else, to separate Read from Manage.
    read_only_key: String,

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
    let read_only_key = plain_key(&global, "acme", &["asset:read"]).await;

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

    // The comment and share routers alongside, because the feed is only interesting once something has written
    // to it — and what writes to it is those paths.
    // The asset routes alongside, because "the library shows one row per group" is only observable through them.
    let app = router(VersionState {
        global: global.clone(),
    })
    .merge(dam_api::assets::router(dam_api::assets::AssetState {
        global: global.clone(),
        delivery: None,
    }));

    Fixture {
        _pg: pg,
        app,
        acme,
        global: global.clone(),
        key,
        read_only_key,
        scoped_key,
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

/// A key for the tenant's first member with an explicit scope list.
///
/// Separate from `person_key` because this one narrows what the *credential* may do rather than what the person is:
/// the difference between Read and Manage in this suite is the key's scopes, not the membership.
async fn plain_key(global: &PgPool, slug: &str, scopes: &[&str]) -> String {
    let (tenant_id, identity_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, m.identity_id FROM dam_global.tenants t \
         JOIN dam_global.tenant_members m ON m.tenant_id = t.id WHERE t.slug = $1 \
         ORDER BY m.identity_id LIMIT 1",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant and member");
    issue(global, tenant_id, Some(identity_id), scopes).await
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
async fn the_version_http_contract_holds() {
    let f = fixture().await;

    a_history_of_one_is_still_a_history(&f).await;
    superseding_returns_the_whole_history(&f).await;
    the_library_shows_one_row_per_group(&f).await;
    an_old_version_is_still_readable_by_id(&f).await;
    a_stale_supersede_is_a_conflict(&f).await;
    reading_needs_read_and_superseding_needs_manage(&f).await;
    an_asset_outside_the_caller_scope_is_404(&f).await;
    making_an_earlier_version_current_again(&f).await;
}

async fn a_history_of_one_is_still_a_history(f: &Fixture) {
    let only = asset(f, "single", true).await;
    let (status, body) = call(f, "GET", &format!("/assets/{only}/versions"), &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // One entry, not an empty list: every asset is version 1 of itself, and a screen that showed nothing here
    // would suggest versioning was unavailable rather than unused.
    assert_eq!(body.as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body[0]["version_no"], json!(1), "{body}");
    assert_eq!(body[0]["is_current"], json!(true), "{body}");
    assert_eq!(body[0]["replaces_id"], Value::Null, "{body}");
}

async fn superseding_returns_the_whole_history(f: &Fixture) {
    let first = asset(f, "brochure-a", true).await;
    let second = asset(f, "brochure-b", true).await;

    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{first}/versions"),
        &f.key,
        Some(json!({ "new_asset_id": second.to_string() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The whole history, so a panel redraws from the response rather than making a second request that could
    // disagree with it.
    assert_eq!(body.as_array().map(Vec::len), Some(2), "{body}");
    assert_eq!(body[0]["version_no"], json!(2), "newest first: {body}");
    assert_eq!(body[0]["asset_id"], json!(second.to_string()), "{body}");
    assert_eq!(body[0]["replaces_id"], json!(first.to_string()), "{body}");
    assert_eq!(body[1]["is_current"], json!(false), "demoted: {body}");
}

async fn the_library_shows_one_row_per_group(f: &Fixture) {
    let (status, body) = call(f, "GET", "/assets?limit=50", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["filename"].as_str().expect("filename"))
        .collect();
    // The version that superseded, and not the one it superseded. Before `is_current` was filtered anywhere, both
    // appeared — and nothing showed it, because no version had ever existed.
    assert!(names.contains(&"brochure-b.jpg"), "{names:?}");
    assert!(
        !names.contains(&"brochure-a.jpg"),
        "a superseded version is in the library: {names:?}"
    );
    // And the total agrees with the rows, or pagination contradicts itself.
    assert_eq!(body["total"].as_u64(), Some(names.len() as u64), "{body}");
}

async fn an_old_version_is_still_readable_by_id(f: &Fixture) {
    let old: Uuid = sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-a.jpg'")
        .fetch_one(&f.acme)
        .await
        .expect("old");

    // Keeping versions is pointless if the old one cannot be fetched. Listings hide it; naming it works.
    let (status, body) = call(f, "GET", &format!("/assets/{old}"), &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["filename"], json!("brochure-a.jpg"), "{body}");

    // Its history is reachable from it, too — somebody looking at the old cut needs to see what replaced it.
    let (status, history) = call(f, "GET", &format!("/assets/{old}/versions"), &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history.as_array().map(Vec::len), Some(2), "{history}");
}

async fn a_stale_supersede_is_a_conflict(f: &Fixture) {
    let old: Uuid = sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-a.jpg'")
        .fetch_one(&f.acme)
        .await
        .expect("old");
    let third = asset(f, "brochure-c", true).await;

    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{old}/versions"),
        &f.key,
        Some(json!({ "new_asset_id": third.to_string() })),
    )
    .await;
    // 409: the request is fine and the world moved on. The reason says what to do about it.
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("reload")),
        "{body}"
    );
}

async fn reading_needs_read_and_superseding_needs_manage(f: &Fixture) {
    let target = asset(f, "perm-a", true).await;
    let replacement = asset(f, "perm-b", true).await;

    // A read-scoped caller can see the history.
    let (status, _) = call(
        f,
        "GET",
        &format!("/assets/{target}/versions"),
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // And cannot supersede: that decides which bytes everybody gets from now on.
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{target}/versions"),
        &f.read_only_key,
        Some(json!({ "new_asset_id": replacement.to_string() })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Nor make an earlier version current.
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{target}/versions/current"),
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn an_asset_outside_the_caller_scope_is_404(f: &Fixture) {
    let hidden = asset(f, "out-of-scope", false).await;
    let absent = Uuid::new_v4();

    for (label, id) in [("hidden", hidden), ("absent", absent)] {
        let (status, body) = call(
            f,
            "GET",
            &format!("/assets/{id}/versions"),
            &f.scoped_key,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{label}: {body}");
    }
}

async fn making_an_earlier_version_current_again(f: &Fixture) {
    let old: Uuid = sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'brochure-a.jpg'")
        .fetch_one(&f.acme)
        .await
        .expect("old");

    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{old}/versions/current"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restored = body
        .as_array()
        .expect("history")
        .iter()
        .find(|version| version["asset_id"] == json!(old.to_string()))
        .expect("the restored version")
        .clone();
    assert_eq!(restored["is_current"], json!(true), "{restored}");
    // Its number is unchanged: a promotion, not a copy. Renumbering it would claim somebody uploaded it again.
    assert_eq!(restored["version_no"], json!(1), "{restored}");

    // The library follows.
    let (_, page) = call(f, "GET", "/assets?limit=50", &f.key, None).await;
    let names: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["filename"].as_str().expect("filename"))
        .collect();
    assert!(names.contains(&"brochure-a.jpg"), "{names:?}");
    assert!(!names.contains(&"brochure-b.jpg"), "{names:?}");
}
