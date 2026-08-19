//! The category endpoints (Q.2b).
//!
//! `dam_db`'s suite proves the tree, the rollup and the filing rules. This proves the HTTP contract, and two
//! properties that only exist at this layer:
//!
//! - **Reading the tree needs Read; changing it needs Manage.** A browse tree is not secret — every reader
//!   needs it to navigate — but a read-only integration key must not re-file the library.
//! - **The counts and the worklist are scoped to the caller.** §7 says counts disclose, so a caller who can
//!   see nine assets must not be told the branch holds sixty.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::categories::{CategoryState, router};
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
    /// A key whose role sees everything.
    key: String,
    /// A key with `asset:read` only.
    read_only_key: String,
    /// A key scoped to one asset group, so the count-disclosure case has something to hide.
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

    let key = provision(&global, "acme", "a@example.com").await;
    let read_only_key = plain_key(&global, "acme", &["asset:read"]).await;

    // A group the scoped caller may see, and assets outside it that it may not.
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'visible', 'Visible') \
         RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");

    // Group scoping lives on a *role*, and a tenant admin bypasses it — so the scoped caller needs its own
    // identity with a non-admin membership naming a role bound to that one group. No API test had built one
    // before, which is why the count-disclosure case below is new coverage rather than a restatement.
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'visible_only', 'Visible only', '{asset:read}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let scoped = role_key(&global, "acme", "scoped@example.com", &["visible_only"]).await;

    let app = router(CategoryState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        app,
        global: global.clone(),
        acme,
        key,
        read_only_key,
        scoped_key: scoped,
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

/// A key for the tenant's existing (admin) identity, with explicit scopes.
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
    issue(global, tenant_id, identity_id, scopes).await
}

/// A key for a *new* non-admin identity whose membership names `roles`.
async fn role_key(global: &PgPool, slug: &str, email: &str, roles: &[&str]) -> String {
    let tenant_id: Uuid = sqlx::query_scalar("SELECT id FROM dam_global.tenants WHERE slug = $1")
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
         VALUES ($1, $2, $3, false)",
    )
    .bind(tenant_id)
    .bind(identity)
    .bind(roles.iter().map(|r| (*r).to_owned()).collect::<Vec<String>>())
    .execute(global)
    .await
    .expect("membership");
    // No scopes on the key: the role is what grants, and an empty scope list means "whatever the role says".
    issue(global, tenant_id, identity, &[]).await
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

/// An asset, optionally in a group the scoped key can see.
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

#[tokio::test]
async fn the_category_http_contract_holds() {
    let f = fixture().await;

    let (tree, exterior, yellow) = a_tree_is_built_over_http(&f).await;
    reading_needs_read_and_changing_needs_manage(&f, tree).await;
    an_asset_is_filed_and_unfiled(&f, yellow).await;
    the_tree_reports_counts_the_caller_can_see(&f, tree, exterior, yellow).await;
    the_uncategorised_worklist_is_scoped_too(&f, tree).await;
    a_vocabulary_is_refused_as_a_tree(&f).await;
    filing_something_that_is_not_a_category_is_refused(&f).await;
    a_caller_scoped_to_a_rule_based_group_is_refused_loudly(&f).await;
}

async fn a_tree_is_built_over_http(f: &Fixture) -> (Uuid, Uuid, Uuid) {
    let (status, tree) = call(
        f,
        "POST",
        "/categories",
        &f.key,
        Some(json!({ "key": "shades", "label": "Designs & Shades" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{tree}");
    let tree_id: Uuid = tree["id"].as_str().expect("id").parse().expect("uuid");

    let (status, exterior) = call(
        f,
        "POST",
        &format!("/categories/{tree_id}/nodes"),
        &f.key,
        Some(json!({ "slug": "exterior", "label": "Exterior" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{exterior}");
    let exterior_id: Uuid = exterior["id"].as_str().expect("id").parse().expect("uuid");

    let (status, yellow) = call(
        f,
        "POST",
        &format!("/categories/{tree_id}/nodes"),
        &f.key,
        Some(json!({ "slug": "yellow", "label": "Yellow", "parent_id": exterior_id })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{yellow}");
    let yellow_id: Uuid = yellow["id"].as_str().expect("id").parse().expect("uuid");
    assert_eq!(yellow["path"], "exterior.yellow");
    assert_eq!(yellow["depth"], 1);

    // A sibling clash is a conflict on something that exists, not a malformed request.
    let (status, body) = call(
        f,
        "POST",
        &format!("/categories/{tree_id}/nodes"),
        &f.key,
        Some(json!({ "slug": "yellow", "label": "Yellow again", "parent_id": exterior_id })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    (tree_id, exterior_id, yellow_id)
}

async fn reading_needs_read_and_changing_needs_manage(f: &Fixture, tree: Uuid) {
    // A browse tree is not secret: every reader needs it to navigate, so listing is Read.
    let (status, body) = call(f, "GET", "/categories", &f.read_only_key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(
        f,
        "GET",
        &format!("/categories/{tree}"),
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Everything that changes where things are filed is Manage.
    for (method, path, payload) in [
        (
            "POST",
            "/categories".to_owned(),
            Some(json!({ "key": "x", "label": "X" })),
        ),
        (
            "POST",
            format!("/categories/{tree}/nodes"),
            Some(json!({ "slug": "sneak", "label": "Sneak" })),
        ),
    ] {
        let (status, _) = call(f, method, &path, &f.read_only_key, payload).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

async fn an_asset_is_filed_and_unfiled(f: &Fixture, yellow: Uuid) {
    let id = asset(f, "filed-over-http", false).await;

    let (status, body) = call(
        f,
        "PUT",
        &format!("/assets/{id}/categories/{yellow}"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The response is the asset's categories, so a client redraws its chips without a second request.
    assert_eq!(body[0]["path"], "exterior.yellow");

    // Read-only cannot file.
    let (status, _) = call(
        f,
        "PUT",
        &format!("/assets/{id}/categories/{yellow}"),
        &f.read_only_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Filing twice is the same state, not an error.
    let (status, _) = call(
        f,
        "PUT",
        &format!("/assets/{id}/categories/{yellow}"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        f,
        "DELETE",
        &format!("/assets/{id}/categories/{yellow}"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.as_array().expect("array").is_empty(),
        "no categories left"
    );
}

async fn the_tree_reports_counts_the_caller_can_see(
    f: &Fixture,
    tree: Uuid,
    exterior: Uuid,
    yellow: Uuid,
) {
    // Two assets under `yellow`: one the scoped key may see, one it may not.
    let visible = asset(f, "count-visible", true).await;
    let hidden = asset(f, "count-hidden", false).await;
    for id in [visible, hidden] {
        let (status, _) = call(
            f,
            "PUT",
            &format!("/assets/{id}/categories/{yellow}"),
            &f.key,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let count_for = |body: &Value, id: Uuid| -> i64 {
        body.as_array()
            .expect("array")
            .iter()
            .find(|node| node["id"] == id.to_string())
            .and_then(|node| node["assets"].as_i64())
            .unwrap_or(-1)
    };

    let (status, all) = call(f, "GET", &format!("/categories/{tree}"), &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert!(all.is_array(), "expected a node list, got {all}");
    assert_eq!(count_for(&all, yellow), 2, "{all}");
    assert_eq!(
        count_for(&all, exterior),
        2,
        "the branch rolls its descendants up"
    );

    // The property this case exists for: the scoped key is told 1, not 2. A tree that showed the true total
    // would disclose the size of the part of the library this caller cannot reach — §7 in its quietest form.
    // Isolating which layer refuses: the tree list uses the same authorize + TenantConn path.
    let (probe_status, probe) = call(f, "GET", "/categories", &f.scoped_key, None).await;
    assert_eq!(
        probe_status,
        StatusCode::OK,
        "the scoped key must authenticate at all: {probe}"
    );

    let (status, scoped) = call(
        f,
        "GET",
        &format!("/categories/{tree}"),
        &f.scoped_key,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the scoped key must be able to read the tree: {scoped}"
    );
    assert_eq!(
        count_for(&scoped, yellow),
        1,
        "counts are the caller's own: {scoped}"
    );
    assert_eq!(count_for(&scoped, exterior), 1);
}

async fn the_uncategorised_worklist_is_scoped_too(f: &Fixture, tree: Uuid) {
    // One unfiled asset the scoped key can see, one it cannot.
    asset(f, "orphan-visible", true).await;
    asset(f, "orphan-hidden", false).await;

    let (status, all) = call(
        f,
        "GET",
        &format!("/categories/{tree}/uncategorised"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    let total = all["total"].as_i64().expect("total");
    assert!(
        total >= 2,
        "both orphans, plus whatever earlier cases left: {all}"
    );
    assert!(
        all["sample"].as_array().expect("sample").len() <= total as usize,
        "the sample is drawn from the total it reports"
    );

    let (_, scoped) = call(
        f,
        "GET",
        &format!("/categories/{tree}/uncategorised"),
        &f.scoped_key,
        None,
    )
    .await;
    assert!(
        scoped["total"].as_i64().expect("total") < total,
        "a scoped caller's worklist is their own: {scoped}"
    );
}

async fn a_vocabulary_is_refused_as_a_tree(f: &Fixture) {
    let vocabulary: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'colours', 'Colours', 'vocabulary') RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("vocabulary");

    // 404 rather than 422: the path names something that is not a category tree, and from the caller's side
    // "there is no such tree" is the honest answer — it also avoids confirming that a taxonomy by that id
    // exists for some other purpose.
    let (status, _) = call(f, "GET", &format!("/categories/{vocabulary}"), &f.key, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn filing_something_that_is_not_a_category_is_refused(f: &Fixture) {
    let id = asset(f, "cannot-file-nonsense", false).await;
    let (status, _) = call(
        f,
        "PUT",
        &format!("/assets/{id}/categories/{}", Uuid::new_v4()),
        &f.key,
        None,
    )
    .await;
    // 422: the path addresses an asset that exists, and the category segment names something unreal — the
    // request is wrong rather than the target missing.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

async fn a_caller_scoped_to_a_rule_based_group_is_refused_loudly(f: &Fixture) {
    // `authorize` refuses a caller whose groups include a rule-based one, because evaluating such a predicate
    // needs the query IR (task 2.4) and *ignoring* it would grant less access than the administrator
    // configured — fail-closed, but silently, so the first anyone would know is an asset that should have been
    // visible and was not. ARCHITECTURE decision 4 says refuse rather than approximate.
    //
    // Asserted here rather than only in `dam_db`, where the function is called directly: nothing proved that
    // `authorize` still *calls* it, so deleting the call passed the whole suite. Mutation testing said so.
    let rule_based: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label, predicate) \
         VALUES (gen_random_uuid(), 'recent', 'Recent', '{\"field\":\"created_at\"}'::jsonb) \
         RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("rule-based group");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'rule_scoped', 'Rule scoped', '{asset:read}', ARRAY[$1], false)",
    )
    .bind(rule_based)
    .execute(&f.acme)
    .await
    .expect("role");
    let key = role_key(&f.global, "acme", "rule@example.com", &["rule_scoped"]).await;

    let (status, body) = call(f, "GET", "/categories", &key, None).await;
    assert_eq!(
        status,
        StatusCode::NOT_IMPLEMENTED,
        "a rule-based group is refused as an unsupported configuration, not as a crash: {body}"
    );
    // With a body, unlike every other refusal here: this one describes the *deployment's* limitation rather
    // than the tenant's data, and the person who can fix it is the one reading it. It reached callers as a
    // bare 500 until this case existed.
    assert!(
        body["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("recent"),
        "the refusal names the group an administrator has to change: {body}"
    );
}
