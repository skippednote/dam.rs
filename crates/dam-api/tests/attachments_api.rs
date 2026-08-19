//! The attachment endpoints (Q.9).
//!
//! `dam_db`'s suite proves the model and the library-exclusion rule. What only exists here is the HTTP contract:
//!
//! - **Manage to attach, Read to see.** Attaching paperwork asserts something about an asset's rights.
//! - **The state-of-the-world refusals are 409**, each naming what is in the way — already attached, a version,
//!   paperwork about paperwork. An unknown *kind* is 422, because that is the request being wrong.
//! - **`has_attachment` reaches the grid**, scoped, so paperwork the caller cannot see does not set the flag.
//! - **Detaching is not deleting.** 204, and the document is an ordinary asset again.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::attachments::{AttachmentState, router};
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
    let app = router(AttachmentState {
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
async fn the_attachment_http_contract_holds() {
    let f = fixture().await;
    let photo = asset(&f, "portrait", true).await;
    let release = asset(&f, "release-form", true).await;

    an_asset_with_no_paperwork_has_an_empty_list(&f, photo).await;
    attaching_returns_the_list_and_hides_the_document(&f, photo, release).await;
    the_grid_says_which_assets_have_paperwork(&f, photo).await;
    an_unknown_kind_is_422_and_names_the_choices(&f, photo).await;
    the_state_of_the_world_refusals_are_409(&f, photo, release).await;
    attaching_needs_manage_and_reading_needs_read(&f).await;
    detaching_is_not_deleting(&f, photo, release).await;
    the_grid_flag_does_not_disclose_paperwork_out_of_scope(&f).await;
}

async fn the_grid_flag_does_not_disclose_paperwork_out_of_scope(f: &Fixture) {
    // A parent the scoped caller can see, with paperwork they cannot. Setting the flag would tell them a document
    // exists — which is the same disclosure as listing it, arriving through a boolean instead of a row.
    let parent = asset(f, "scoped-parent", true).await;
    let secret = asset(f, "secret-licence", false).await;
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{parent}/attachments"),
        &f.key,
        Some(json!({ "document_id": secret.to_string(), "kind": "licence" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The administrator sees the flag.
    let (_, wide) = call(f, "GET", "/assets?limit=50", &f.key, None).await;
    let row = wide["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"] == json!(parent.to_string()))
        .expect("the parent")
        .clone();
    assert_eq!(row["has_attachment"], json!(true), "{row}");

    // The scoped caller does not.
    let (_, narrow) = call(f, "GET", "/assets?limit=50", &f.scoped_key, None).await;
    let scoped_row = narrow["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"] == json!(parent.to_string()))
        .expect("the parent is in their scope")
        .clone();
    assert_eq!(
        scoped_row["has_attachment"],
        json!(false),
        "the flag disclosed a document out of scope: {scoped_row}"
    );

    // And the list is empty for them too, so the two agree.
    let (_, listed) = call(
        f,
        "GET",
        &format!("/assets/{parent}/attachments"),
        &f.scoped_key,
        None,
    )
    .await;
    assert_eq!(listed, json!([]), "{listed}");
}

async fn an_asset_with_no_paperwork_has_an_empty_list(f: &Fixture, photo: Uuid) {
    let (status, body) = call(
        f,
        "GET",
        &format!("/assets/{photo}/attachments"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
}

async fn attaching_returns_the_list_and_hides_the_document(
    f: &Fixture,
    photo: Uuid,
    release: Uuid,
) {
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{photo}/attachments"),
        &f.key,
        Some(json!({ "document_id": release.to_string(), "kind": "release" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body[0]["kind"], json!("release"), "{body}");
    assert_eq!(body[0]["attached_to"], json!(photo.to_string()), "{body}");
    // The uploader is named where there is one, so a panel reads as a sentence.
    assert!(
        body[0]["filename"]
            .as_str()
            .is_some_and(|n| n.contains("release-form")),
        "{body}"
    );

    // Out of the library: nobody browses to a release form.
    let (_, page) = call(f, "GET", "/assets?limit=50", &f.key, None).await;
    let names: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["filename"].as_str().expect("filename"))
        .collect();
    assert!(
        !names.contains(&"release-form.jpg"),
        "paperwork is in the library: {names:?}"
    );
    assert!(names.contains(&"portrait.jpg"), "{names:?}");

    // And still readable by id, which is the entire reason for attaching it.
    let (status, _) = call(f, "GET", &format!("/assets/{release}"), &f.key, None).await;
    assert_eq!(status, StatusCode::OK);
}

async fn the_grid_says_which_assets_have_paperwork(f: &Fixture, photo: Uuid) {
    let (_, page) = call(f, "GET", "/assets?limit=50", &f.key, None).await;
    let row = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"] == json!(photo.to_string()))
        .expect("the photo")
        .clone();
    // A boolean, not a count: the question a cell answers is whether the rights picture is documented.
    assert_eq!(row["has_attachment"], json!(true), "{row}");

    let bare = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"] != json!(photo.to_string()));
    if let Some(other) = bare {
        assert_eq!(other["has_attachment"], json!(false), "{other}");
    }
}

async fn an_unknown_kind_is_422_and_names_the_choices(f: &Fixture, photo: Uuid) {
    let doc = asset(f, "mystery-doc", true).await;
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{photo}/attachments"),
        &f.key,
        Some(json!({ "document_id": doc.to_string(), "kind": "vibes" })),
    )
    .await;
    // 422: the *request* is wrong, unlike the 409s below where the request is fine and the world refuses it.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("release")),
        "the refusal does not name the choices: {body}"
    );
}

async fn the_state_of_the_world_refusals_are_409(f: &Fixture, photo: Uuid, release: Uuid) {
    // Already attached elsewhere.
    let other = asset(f, "second-portrait", true).await;
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{other}/attachments"),
        &f.key,
        Some(json!({ "document_id": release.to_string(), "kind": "release" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("already")),
        "{body}"
    );

    // Paperwork about paperwork.
    let appendix = asset(f, "appendix", true).await;
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{release}/attachments"),
        &f.key,
        Some(json!({ "document_id": appendix.to_string(), "kind": "other" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("paperwork")),
        "{body}"
    );
    let _ = photo;
}

async fn attaching_needs_manage_and_reading_needs_read(f: &Fixture) {
    let parent = asset(f, "perm-parent", true).await;
    let doc = asset(f, "perm-doc", true).await;

    // A read-scoped caller can look.
    let (status, _) = call(
        f,
        "GET",
        &format!("/assets/{parent}/attachments"),
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // And cannot assert something about the asset's rights.
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{parent}/attachments"),
        &f.read_only_key,
        Some(json!({ "document_id": doc.to_string(), "kind": "licence" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

async fn detaching_is_not_deleting(f: &Fixture, photo: Uuid, release: Uuid) {
    let (status, _) = call(
        f,
        "DELETE",
        &format!("/assets/{photo}/attachments/{release}"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Back in the library, bytes and row intact. Somebody correcting a mis-attachment does not want a destructive
    // verb.
    let (_, page) = call(f, "GET", "/assets?limit=50", &f.key, None).await;
    let names: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["filename"].as_str().expect("filename"))
        .collect();
    assert!(names.contains(&"release-form.jpg"), "{names:?}");

    let (_, list) = call(
        f,
        "GET",
        &format!("/assets/{photo}/attachments"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(list, json!([]), "{list}");
}
