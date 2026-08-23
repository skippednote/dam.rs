//! Collections over HTTP (Q.14b).
//!
//! `dam_db` proves ordering, the pin union and the portal guard against the storage directly. What lives only
//! here is the HTTP contract, and four decisions about it:
//!
//! - **Manage throughout, including to read.** Membership is a curatorial statement and the list of
//!   collections is a map of work in progress. A key holding only `asset:read` gets nothing.
//! - **The predicate applies in both directions.** Adding filters through the caller's own scope, so a
//!   collection cannot become a way to put an unseeable asset onto a public page; *listing* filters the same
//!   way, so it cannot become a way to learn that such an asset exists.
//! - **The key is immutable over HTTP.** There is no field for it on the amend body. A portal references a
//!   collection by key, so a rename that moved it would break or silently repoint every portal built on it.
//! - **A refusal says what to do.** A duplicate key and a published collection both come back as a 409 with a
//!   sentence, because both are things the person at the keyboard can fix.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::collections::{CollectionState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    app: axum::Router,
    tenant_id: Uuid,
    key: String,
    read_only_key: String,
    /// Holds `asset:manage` over one group only. Sees `harbour` and nothing else.
    scoped_key: String,
    harbour: Uuid,
    boardroom: Uuid,
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
    let admin = identity(&global, "ada@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(admin)
    .execute(&global)
    .await
    .expect("membership");
    let key = issue(&global, tenant_id, Some(admin), &[]).await;
    let read_only_key = issue(&global, tenant_id, Some(admin), &["asset:read"]).await;

    let harbour = asset(&acme, "harbour").await;
    let boardroom = asset(&acme, "boardroom").await;

    // A curator scoped to one group. Not a contrivance: this is the ordinary shape of an agency user who
    // curates their own client's work inside a shared library.
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) \
         VALUES (gen_random_uuid(), 'harbourside', 'Harbourside') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(harbour)
        .execute(&acme)
        .await
        .expect("group member");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped_curator', 'Scoped curator', \
                 '{asset:read,asset:manage}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let curator = identity(&global, "curator@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{scoped_curator}', false)",
    )
    .bind(tenant_id)
    .bind(curator)
    .execute(&global)
    .await
    .expect("membership");
    let scoped_key = issue(&global, tenant_id, Some(curator), &[]).await;

    let app = router(CollectionState {
        global: global.clone(),
        // No delivery: this suite is about the contract, and a thumbnail link needs a rendered derivative
        // this fixture's assets do not have. `None` proves the honest shape — a member with no thumbnail
        // comes back with `thumbnail_url: null` rather than a link that 404s.
        delivery: None,
    });

    Fixture {
        _pg: pg,
        global,
        acme,
        app,
        tenant_id,
        key,
        read_only_key,
        scoped_key,
        harbour,
        boardroom,
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

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn call(
    f: &Fixture,
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
    let request = request
        .body(match &body {
            Some(value) => Body::from(value.to_string()),
            None => Body::empty(),
        })
        .expect("request");
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

use tower::ServiceExt;

/// Creates a collection and returns its id.
async fn create(f: &Fixture, key: &str, label: &str, body: Value) -> String {
    let mut payload = json!({ "key": key, "label": label });
    if let (Some(base), Some(extra)) = (payload.as_object_mut(), body.as_object()) {
        for (name, value) in extra {
            base.insert(name.clone(), value.clone());
        }
    }
    let (status, made) = call(f, "POST", "/collections", Some(&f.key), Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    made["id"].as_str().expect("id").to_owned()
}

// ─── who may do this at all ─────────────────────────────────────────────────

async fn reading_a_collection_needs_manage_not_read(f: &Fixture) {
    for (method, path) in [("GET", "/collections"), ("POST", "/collections")] {
        let (status, _) = call(
            f,
            method,
            path,
            Some(&f.read_only_key),
            (method == "POST").then(|| json!({ "key": "nope", "label": "Nope" })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path} is administration, and a read-only key holds none of it"
        );
    }
    let (status, _) = call(f, "GET", "/collections", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no credential at all");
}

// ─── the collection ─────────────────────────────────────────────────────────

async fn a_collection_is_created_private_and_unpinned_by_default(f: &Fixture) {
    let id = create(f, "spring", "Spring", json!({})).await;

    let (status, listed) = call(f, "GET", "/collections", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK);
    let made = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|one| one["id"] == id)
        .expect("the new collection is listed");
    // The safe end of both axes: a collection is somebody's working set until they say otherwise, and
    // pinning costs money — it holds originals in the hottest class indefinitely.
    assert_eq!(made["visibility"], "private");
    assert_eq!(made["pin_hot"], false);
    assert_eq!(made["item_count"], 0);
}

async fn a_duplicate_key_is_a_conflict_that_names_it(f: &Fixture) {
    create(f, "twice", "Twice", json!({})).await;
    let (status, refused) = call(
        f,
        "POST",
        "/collections",
        Some(&f.key),
        Some(json!({ "key": "twice", "label": "Again" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .expect("reason")
            .contains("twice"),
        "the refusal names the key so the person typing can change it: {refused}"
    );
}

async fn amending_moves_everything_except_the_key(f: &Fixture) {
    let id = create(f, "stable", "Before", json!({})).await;
    let (status, amended) = call(
        f,
        "PATCH",
        &format!("/collections/{id}"),
        Some(&f.key),
        Some(json!({
            "label": "After",
            "description": "now described",
            "visibility": "public",
            "pin_hot": true,
            // Sent deliberately, and ignored: `AmendBody` has no such field, so this is what a client
            // hoping to rename the key actually gets.
            "key": "moved",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{amended}");
    assert_eq!(
        amended["key"], "stable",
        "the key a portal references does not move"
    );
    assert_eq!(amended["label"], "After");
    assert_eq!(amended["visibility"], "public");
    assert_eq!(amended["pin_hot"], true);

    let (status, _) = call(
        f,
        "PATCH",
        &format!("/collections/{}", Uuid::new_v4()),
        Some(&f.key),
        Some(json!({ "label": "Nobody", "visibility": "private", "pin_hot": false })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn an_invented_visibility_is_refused_with_the_three_that_work(f: &Fixture) {
    let (status, refused) = call(
        f,
        "POST",
        "/collections",
        Some(&f.key),
        Some(json!({ "key": "invented", "label": "Invented", "visibility": "world-readable" })),
    )
    .await;
    // 422 rather than the 409 a taken key gets, and the distinction is the useful part: a bad visibility is
    // a field to correct, a taken key is something already in the way. A client can retry the first with a
    // different value in the same field and cannot retry the second at all.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let reason = refused["reason"].as_str().expect("reason");
    assert!(
        reason.contains("private") && reason.contains("shared") && reason.contains("public"),
        "a refusal that lists the valid values is one the caller can act on: {reason}"
    );
}

// ─── membership ─────────────────────────────────────────────────────────────

async fn adding_reports_what_arrived_and_what_was_out_of_scope(f: &Fixture) {
    let id = create(f, "mixed", "Mixed", json!({})).await;
    let (status, added) = call(
        f,
        "POST",
        &format!("/collections/{id}/items"),
        // The *scoped* curator: `boardroom` is outside their group and a fabricated id is outside
        // everybody's, and the two are indistinguishable on purpose.
        Some(&f.scoped_key),
        Some(json!({ "asset_ids": [f.harbour, f.boardroom, Uuid::new_v4()] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(added["added"], 1, "only the one they can see went in");
    assert_eq!(added["out_of_scope"], 2);
    assert!(
        added.get("out_of_scope_ids").is_none(),
        "counted, never named: naming them would confirm assets exist that this caller cannot read"
    );

    // And the admin sees exactly the one member, so the refusal was a refusal and not a silent success.
    let (_, items) = call(
        f,
        "GET",
        &format!("/collections/{id}/items"),
        Some(&f.key),
        None,
    )
    .await;
    let rows = items.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["asset_id"], f.harbour.to_string());
    assert_eq!(
        rows[0]["filename"], "harbour.jpg",
        "a screen can draw this without a second call"
    );
    assert_eq!(rows[0]["mime"], "image/jpeg");
}

async fn listing_membership_shows_only_what_the_caller_can_see(f: &Fixture) {
    // The mirror of the add rule, and the half that is easy to forget: an admin curates a collection holding
    // both assets, then a scoped curator lists it. If they saw both ids they would have learned that an
    // asset exists outside their scope — the same oracle, reached from the other side.
    let id = create(f, "both", "Both", json!({})).await;
    let (status, added) = call(
        f,
        "POST",
        &format!("/collections/{id}/items"),
        Some(&f.key),
        Some(json!({ "asset_ids": [f.boardroom, f.harbour] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(added["added"], 2);

    let (_, wide) = call(
        f,
        "GET",
        &format!("/collections/{id}/items"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(wide.as_array().expect("array").len(), 2);

    let (status, narrow) = call(
        f,
        "GET",
        &format!("/collections/{id}/items"),
        Some(&f.scoped_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{narrow}");
    let rows = narrow.as_array().expect("array");
    assert_eq!(rows.len(), 1, "one of the two, and not a hint of the other");
    assert_eq!(rows[0]["asset_id"], f.harbour.to_string());
    // Position 1, not 0: the real position is kept, so the gap is the honest signal that the collection holds
    // something this caller is not being shown. Renumbering would have been a quieter lie.
    assert_eq!(rows[0]["position"], 1);
}

async fn removing_and_reordering_a_non_member_is_not_found(f: &Fixture) {
    let id = create(f, "ordered", "Ordered", json!({})).await;
    call(
        f,
        "POST",
        &format!("/collections/{id}/items"),
        Some(&f.key),
        Some(json!({ "asset_ids": [f.harbour] })),
    )
    .await;

    // A no-op reorder would be worse than a refusal: `move_item` does nothing for a non-member, so without
    // the membership check the caller would get a 200 and a list that ignored them.
    let (status, _) = call(
        f,
        "POST",
        &format!("/collections/{id}/items/{}/position", f.boardroom),
        Some(&f.key),
        Some(json!({ "position": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        f,
        "DELETE",
        &format!("/collections/{id}/items/{}", f.boardroom),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, order) = call(
        f,
        "POST",
        &format!("/collections/{id}/items/{}/position", f.harbour),
        Some(&f.key),
        Some(json!({ "position": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{order}");
    assert_eq!(order.as_array().expect("array").len(), 1);
}

async fn a_published_collection_cannot_be_deleted(f: &Fixture) {
    let id = create(f, "published", "Published", json!({})).await;
    sqlx::query(
        "INSERT INTO portals (id, key, title, kind, collection_id) \
         VALUES (gen_random_uuid(), 'a-portal', 'A portal', 'standard', $1)",
    )
    .bind(Uuid::parse_str(&id).expect("uuid"))
    .execute(&f.acme)
    .await
    .expect("portal");

    let (status, refused) = call(
        f,
        "DELETE",
        &format!("/collections/{id}"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let reason = refused["reason"].as_str().expect("reason");
    assert!(
        reason.contains("portal"),
        "the refusal says what is publishing it and what to do: {reason}"
    );

    sqlx::query("UPDATE portals SET retired_at = now() WHERE key = 'a-portal'")
        .execute(&f.acme)
        .await
        .expect("retire");
    let (status, _) = call(
        f,
        "DELETE",
        &format!("/collections/{id}"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        f,
        "DELETE",
        &format!("/collections/{id}"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "deleting it twice is a 404, not a second 204"
    );
}

#[tokio::test]
async fn the_collections_contract_holds() {
    let f = fixture().await;

    reading_a_collection_needs_manage_not_read(&f).await;
    a_collection_is_created_private_and_unpinned_by_default(&f).await;
    a_duplicate_key_is_a_conflict_that_names_it(&f).await;
    amending_moves_everything_except_the_key(&f).await;
    an_invented_visibility_is_refused_with_the_three_that_work(&f).await;

    adding_reports_what_arrived_and_what_was_out_of_scope(&f).await;
    listing_membership_shows_only_what_the_caller_can_see(&f).await;
    removing_and_reordering_a_non_member_is_not_found(&f).await;
    a_published_collection_cannot_be_deleted(&f).await;

    // Belongs to the fixture rather than a case: proves the scoped key is genuinely narrower than the admin
    // one, so the two assertions above are about the predicate and not about an empty library.
    let (_, all) = call(&f, "GET", "/collections", Some(&f.scoped_key), None).await;
    assert!(
        !all.as_array().expect("array").is_empty(),
        "the scoped curator can still see the collections themselves — it is asset scope that narrows, \
         not the administration surface"
    );
    assert!(f.tenant_id != Uuid::nil() && !f.global.is_closed());
}
