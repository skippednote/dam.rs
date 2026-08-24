//! Adding and removing the people who can use a tenant, over HTTP (G10·2a).
//!
//! The db suite proves the transitions. What only this level can show:
//!
//! - **Administration takes `Manage`.** Seeing the library is not administering who else can.
//! - **Every change lands in the governance record, in the same transaction.** `tenant_members` is in
//!   `dam_global` and `audit_log` is in the tenant schema; they are two schemas in one database, so the
//!   membership and the entry describing it commit together or not at all.
//! - **A grant and a revocation are separate entries.** A single "roles changed" action would make "show me
//!   everything that was granted" unanswerable, and the administrator flag has to count in both directions or
//!   a promotion with no role change goes unrecorded.
//! - **The response never says whether the person already existed.** `identities` is global and unique on the
//!   address, so a 409 there would answer "does this company use damrs" about an address somebody typed.
//! - **The issued key works, and stops working.** The point of the whole surface.
//! - **An unknown role is named.** `role_names` has no foreign key and `auth` ignores what it cannot resolve,
//!   so the alternative is a member who sees nothing with nothing saying why.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::members::{MemberState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    app: axum::Router,
    /// The governance router, so the record can be read back over HTTP rather than out of the table.
    governance: axum::Router,
    /// Tenant administrator, so `Manage` is held.
    key: String,
    /// A reader, holding `asset:read` and nothing else.
    reader_key: String,
    ada: Uuid,
    tenant_id: Uuid,
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

    let ada = identity(&global, "ada@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(ada)
    .execute(&global)
    .await
    .expect("membership");

    let bob = identity(&global, "bob@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{reader}', false)",
    )
    .bind(tenant_id)
    .bind(bob)
    .execute(&global)
    .await
    .expect("membership");

    for key in ["reader", "editor", "curator"] {
        sqlx::query(
            "INSERT INTO roles (id, key, label, permissions, all_asset_groups) \
             VALUES (gen_random_uuid(), $1, $1, '{asset:read}', true)",
        )
        .bind(key)
        .execute(&acme)
        .await
        .expect("role");
    }

    Fixture {
        _pg: pg,
        app: router(MemberState {
            global: global.clone(),
        }),
        governance: dam_api::governance::router(dam_api::governance::GovernanceState {
            global: global.clone(),
        }),
        key: issue(&global, tenant_id, ada).await,
        reader_key: issue(&global, tenant_id, bob).await,
        global,
        ada,
        tenant_id,
    }
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

async fn issue(global: &PgPool, tenant: Uuid, who: Uuid) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant)
    .bind(who)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn call(
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

#[tokio::test]
async fn the_member_surface() {
    let f = fixture().await;

    administration_needs_manage(&f).await;
    the_tenants_roles_are_offered(&f).await;
    adding_somebody_mints_a_working_key_and_records_it(&f).await;
    an_unknown_role_is_named(&f).await;
    a_bad_address_is_refused(&f).await;
    adding_the_same_person_twice_conflicts_without_saying_why_it_exists(&f).await;
    a_grant_and_a_revocation_are_separate_entries(&f).await;
    promoting_with_no_role_change_is_still_recorded(&f).await;
    the_only_administrator_cannot_step_down(&f).await;
    removing_somebody_revokes_their_key_and_records_the_count(&f).await;
    a_stranger_is_a_404(&f).await;
}

async fn administration_needs_manage(f: &Fixture) {
    for (method, path, body) in [
        ("GET", "/members", None),
        ("GET", "/roles", None),
        (
            "POST",
            "/members",
            Some(json!({ "email": "new@example.com" })),
        ),
    ] {
        let (status, _) = call(&f.app, method, path, Some(&f.reader_key), body.clone()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} must need manage"
        );
        let (status, _) = call(&f.app, method, path, None, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
}

async fn the_tenants_roles_are_offered(f: &Fixture) {
    // So a form can offer the keys rather than ask somebody to type one, which is how `editors` happens.
    let (status, roles) = call(&f.app, "GET", "/roles", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{roles}");
    assert_eq!(roles, json!(["curator", "editor", "reader"]));
}

async fn adding_somebody_mints_a_working_key_and_records_it(f: &Fixture) {
    let (status, added) = call(
        &f.app,
        "POST",
        "/members",
        Some(&f.key),
        Some(json!({
            "email": "  Grace@Example.com  ",
            "display_name": "Grace",
            "role_names": ["editor"],
            "is_tenant_admin": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    let key = added["api_key"].as_str().expect("a key").to_owned();
    assert!(
        added["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("shown only here")
    );
    // Nothing in the response says whether the identity pre-existed.
    assert_eq!(added.get("identity_existed"), None);

    // The property: the key works, against the tenant it was issued for.
    let authenticated = auth::authenticate(&f.global, &key)
        .await
        .expect("query")
        .expect("the issued key authenticates");
    assert_eq!(authenticated.tenant_id, f.tenant_id);

    let (_, listed) = call(&f.app, "GET", "/members", Some(&f.key), None).await;
    let grace = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|m| m["email"] == "Grace@Example.com")
        .expect("grace is listed");
    assert_eq!(grace["role_names"], json!(["editor"]));
    assert_eq!(grace["live_keys"], json!(1));
    assert_eq!(grace["status"], "active");
    assert_eq!(grace["scim_managed"], json!(false));

    let (_, log) = call(
        &f.governance,
        "GET",
        "/audit?action=identity.provisioned",
        Some(&f.key),
        None,
    )
    .await;
    let entry = &log["entries"][0];
    assert_eq!(entry["actor_id"], json!(f.ada));
    assert_eq!(entry["target_kind"], "identity");
    assert_eq!(entry["payload"]["email"], "Grace@Example.com");
    assert_eq!(entry["payload"]["role_names"], json!(["editor"]));
    // The fact the response withholds is in the record, which only this tenant's administrators read.
    assert_eq!(entry["payload"]["identity_existed"], json!(false));
}

async fn an_unknown_role_is_named(f: &Fixture) {
    let (status, body) = call(
        &f.app,
        "POST",
        "/members",
        Some(&f.key),
        Some(json!({ "email": "typo@example.com", "role_names": ["editors", "curator"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let detail = serde_json::to_string(&body).unwrap_or_default();
    assert!(detail.contains("editors"), "{detail}");
    assert!(
        !detail.contains("curator"),
        "only the unknown one, so the message points at the fix: {detail}"
    );
}

async fn a_bad_address_is_refused(f: &Fixture) {
    let (status, _) = call(
        &f.app,
        "POST",
        "/members",
        Some(&f.key),
        Some(json!({ "email": "not-an-address" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

async fn adding_the_same_person_twice_conflicts_without_saying_why_it_exists(f: &Fixture) {
    let (status, body) = call(
        &f.app,
        "POST",
        "/members",
        Some(&f.key),
        Some(json!({ "email": "grace@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let detail = serde_json::to_string(&body).unwrap_or_default();
    assert!(
        detail.contains("already a member"),
        "the answer is about this tenant, not about the identity: {detail}"
    );
}

async fn a_grant_and_a_revocation_are_separate_entries(f: &Fixture) {
    let grace = member_id(f, "Grace@Example.com").await;
    let (status, updated) = call(
        &f.app,
        "PATCH",
        &format!("/members/{grace}"),
        Some(&f.key),
        Some(json!({ "role_names": ["curator"], "is_tenant_admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["role_names"], json!(["curator"]));

    let (_, granted) = call(
        &f.governance,
        "GET",
        "/audit?action=role.granted",
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(
        granted["entries"][0]["payload"]["roles"],
        json!(["curator"])
    );
    let (_, revoked) = call(
        &f.governance,
        "GET",
        "/audit?action=role.revoked",
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(revoked["entries"][0]["payload"]["roles"], json!(["editor"]));
}

async fn promoting_with_no_role_change_is_still_recorded(f: &Fixture) {
    // Otherwise the one change that matters most — somebody becoming an administrator — is the one the record
    // misses, because no role name moved.
    let grace = member_id(f, "Grace@Example.com").await;
    let (_, before) = call(
        &f.governance,
        "GET",
        "/audit?action=role.granted",
        Some(&f.key),
        None,
    )
    .await;
    let previous_seq = before["entries"][0]["seq"].as_i64();

    let (status, _) = call(
        &f.app,
        "PATCH",
        &format!("/members/{grace}"),
        Some(&f.key),
        Some(json!({ "role_names": ["curator"], "is_tenant_admin": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, after) = call(
        &f.governance,
        "GET",
        "/audit?action=role.granted",
        Some(&f.key),
        None,
    )
    .await;
    assert_ne!(after["entries"][0]["seq"].as_i64(), previous_seq);
    assert_eq!(after["entries"][0]["payload"]["tenant_admin"], json!(true));
    assert_eq!(
        after["entries"][0]["payload"]["roles"],
        json!([]),
        "no role moved, and the entry says so rather than inventing one"
    );
}

async fn the_only_administrator_cannot_step_down(f: &Fixture) {
    // Grace is an administrator too by now, so demote her first to get back to one.
    let grace = member_id(f, "Grace@Example.com").await;
    let (status, _) = call(
        &f.app,
        "PATCH",
        &format!("/members/{grace}"),
        Some(&f.key),
        Some(json!({ "role_names": ["curator"], "is_tenant_admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &f.app,
        "PATCH",
        &format!("/members/{}", f.ada),
        Some(&f.key),
        Some(json!({ "role_names": [], "is_tenant_admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = call(
        &f.app,
        "DELETE",
        &format!("/members/{}", f.ada),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "and removal reaches the same state, so it is refused the same way"
    );
}

async fn removing_somebody_revokes_their_key_and_records_the_count(f: &Fixture) {
    let (_, added) = call(
        &f.app,
        "POST",
        "/members",
        Some(&f.key),
        Some(json!({ "email": "leaver@example.com", "role_names": ["reader"] })),
    )
    .await;
    let key = added["api_key"].as_str().expect("key").to_owned();
    let identity_id = added["identity_id"].as_str().expect("id").to_owned();
    assert!(
        auth::authenticate(&f.global, &key)
            .await
            .expect("query")
            .is_some(),
        "the premise"
    );

    let (status, removed) = call(
        &f.app,
        "DELETE",
        &format!("/members/{identity_id}"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["keys_revoked"], json!(1));
    assert_eq!(removed["identity_disabled"], json!(true));

    assert!(
        auth::authenticate(&f.global, &key)
            .await
            .expect("query")
            .is_none(),
        "removal has to reach the credential, or it is a flag"
    );

    let (_, log) = call(
        &f.governance,
        "GET",
        "/audit?action=identity.deprovisioned",
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(log["entries"][0]["payload"]["keys_revoked"], json!(1));
    assert_eq!(
        log["entries"][0]["payload"]["roles_held"],
        json!(["reader"])
    );
    assert_eq!(log["entries"][0]["payload"]["removed_self"], json!(false));

    // And the chain is still intact after all of that.
    let (_, report) = call(&f.governance, "GET", "/audit/verify", Some(&f.key), None).await;
    assert_eq!(report["intact"], json!(true), "{report}");
}

async fn a_stranger_is_a_404(f: &Fixture) {
    let stranger = Uuid::now_v7();
    let (status, _) = call(
        &f.app,
        "PATCH",
        &format!("/members/{stranger}"),
        Some(&f.key),
        Some(json!({ "role_names": [], "is_tenant_admin": false })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &f.app,
        "DELETE",
        &format!("/members/{stranger}"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn member_id(f: &Fixture, email: &str) -> String {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM dam_global.identities WHERE email = $1")
        .bind(email)
        .fetch_one(&f.global)
        .await
        .expect("identity")
        .to_string()
}
