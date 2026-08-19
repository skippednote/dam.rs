//! The engagement endpoints (Q.5b).
//!
//! `dam_db`'s suite proves the access rules and the arithmetic. What only exists here is the HTTP contract, and
//! three things that are decisions about the *interface*:
//!
//! - **A key with no person behind it is refused.** A rating is somebody's opinion; a machine key has none, and
//!   recording one against it would write a row nobody can ever own or clear. 403, because the caller is the
//!   problem rather than the request. Note that `caller::authorize` is what enforces this, for every endpoint —
//!   the check inside the engagement handlers is a fail-closed unwrap behind it, so mutating that unwrap does not
//!   change this suite's outcome. Asserted here anyway, because it is the contract this endpoint promises.
//! - **Read is enough.** Whoever may look at an asset may have an opinion about it. Requiring Manage would mean
//!   only administrators could favourite anything.
//! - **A hidden asset is 404, exactly like an absent one.** Two statuses would rebuild the existence oracle the
//!   db layer collapses.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::engagement::{EngagementState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    acme: PgPool,
    /// A tenant admin, with a person behind it.
    key: String,
    /// A second person, so "my rating" can be shown to be per-caller over HTTP too.
    other_key: String,
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
    let other_key = person_key(&global, "acme", "grace@example.com", &[], true).await;
    let machine_key = machine_key(&global, "acme").await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'visible', 'Visible') \
         RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    // Group scoping lives on a role, and a tenant admin bypasses it — so the scoped caller needs a non-admin
    // membership naming a role bound to that one group.
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

    let app = router(EngagementState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        app,
        acme,
        key,
        other_key,
        machine_key,
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
async fn the_engagement_http_contract_holds() {
    let f = fixture().await;
    let visible = asset(&f, "visible", true).await;
    let hidden = asset(&f, "hidden", false).await;

    a_rating_comes_back_with_the_average_it_produced(&f, visible).await;
    my_stars_is_per_caller_over_http_too(&f, visible).await;
    stars_outside_one_to_five_are_422(&f, visible).await;
    clearing_is_delete_and_is_idempotent(&f, visible).await;
    favouriting_and_watching_are_idempotent_toggles(&f, visible).await;
    a_key_with_no_person_behind_it_is_forbidden(&f, visible).await;
    a_hidden_asset_is_404_exactly_like_an_absent_one(&f, hidden).await;
    the_private_lists_are_the_callers_own(&f, visible).await;
    a_scoped_caller_sees_only_what_they_may(&f, visible, hidden).await;
}

async fn a_rating_comes_back_with_the_average_it_produced(f: &Fixture, visible: Uuid) {
    let (status, body) = call(
        f,
        "PUT",
        &format!("/assets/{visible}/rating"),
        &f.key,
        Some(json!({ "stars": 4 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Not 204: the widget has to redraw the average, and it moved because of this request — so the number on
    // screen comes from the write rather than from a read that raced it.
    assert_eq!(body["my_stars"], json!(4), "{body}");
    assert_eq!(body["average_stars"], json!(4.0), "{body}");
    assert_eq!(body["rating_count"], json!(1), "{body}");
    assert_eq!(body["asset_id"], json!(visible.to_string()), "{body}");
}

async fn my_stars_is_per_caller_over_http_too(f: &Fixture, visible: Uuid) {
    let (status, body) = call(
        f,
        "PUT",
        &format!("/assets/{visible}/rating"),
        &f.other_key,
        Some(json!({ "stars": 2 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["my_stars"], json!(2), "grace's own: {body}");
    assert_eq!(body["average_stars"], json!(3.0), "{body}");

    // Ada's four is untouched, and she sees the moved average. A single global "stars" field would have made
    // one of these two wrong.
    let (_, body) = call(
        f,
        "PUT",
        &format!("/assets/{visible}/rating"),
        &f.key,
        Some(json!({ "stars": 4 })),
    )
    .await;
    assert_eq!(body["my_stars"], json!(4), "{body}");
    assert_eq!(body["rating_count"], json!(2), "still two people: {body}");
}

async fn stars_outside_one_to_five_are_422(f: &Fixture, visible: Uuid) {
    for bad in [0, 6, -1] {
        let (status, body) = call(
            f,
            "PUT",
            &format!("/assets/{visible}/rating"),
            &f.key,
            Some(json!({ "stars": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
        // The reason says the range, because the person who clicked cannot see the enum.
        assert!(
            body["reason"]
                .as_str()
                .is_some_and(|r| r.contains("1 to 5")),
            "{body}"
        );
    }
    // Zero especially: it is the value a naive client sends to mean "clear", and accepting it would put a
    // zero-star opinion into every average.
    let (_, body) = call(
        f,
        "PUT",
        &format!("/assets/{visible}/rating"),
        &f.key,
        Some(json!({ "stars": 4 })),
    )
    .await;
    assert_eq!(
        body["my_stars"],
        json!(4),
        "unchanged by the refusals: {body}"
    );
}

async fn clearing_is_delete_and_is_idempotent(f: &Fixture, visible: Uuid) {
    let (status, body) = call(
        f,
        "DELETE",
        &format!("/assets/{visible}/rating"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["my_stars"], Value::Null, "{body}");
    // Grace's two is the only rating left, so the average is 2 — not 1, which is what treating a cleared rating
    // as a zero would give.
    assert_eq!(body["average_stars"], json!(2.0), "{body}");
    assert_eq!(body["rating_count"], json!(1), "{body}");

    // Twice is fine: the star widget is a toggle and a double click is not a fault.
    let (status, body) = call(
        f,
        "DELETE",
        &format!("/assets/{visible}/rating"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rating_count"], json!(1), "{body}");
}

async fn favouriting_and_watching_are_idempotent_toggles(f: &Fixture, visible: Uuid) {
    for (what, flag) in [("favourite", "is_favourite"), ("watch", "is_watched")] {
        let path = format!("/assets/{visible}/{what}");
        let (status, body) = call(f, "PUT", &path, &f.key, None).await;
        assert_eq!(status, StatusCode::OK, "{what}: {body}");
        assert_eq!(body[flag], json!(true), "{what}: {body}");

        let (status, body) = call(f, "PUT", &path, &f.key, None).await;
        assert_eq!(status, StatusCode::OK, "{what} twice: {body}");
        assert_eq!(body[flag], json!(true), "{what}: {body}");

        let (status, body) = call(f, "DELETE", &path, &f.key, None).await;
        assert_eq!(status, StatusCode::OK, "{what}: {body}");
        assert_eq!(body[flag], json!(false), "{what}: {body}");
    }

    // A favourite has a public count and a watch does not — the two are not the same feature with a different
    // label. See DECISIONS.md on why nobody is told how many colleagues are watching a file.
    call(
        f,
        "PUT",
        &format!("/assets/{visible}/favourite"),
        &f.key,
        None,
    )
    .await;
    let (_, body) = call(f, "PUT", &format!("/assets/{visible}/watch"), &f.key, None).await;
    assert_eq!(body["favourite_count"], json!(1), "{body}");
    assert!(
        body.get("watch_count").is_none(),
        "there is no watch count: {body}"
    );
}

async fn a_key_with_no_person_behind_it_is_forbidden(f: &Fixture, visible: Uuid) {
    // Every route, not just the writes: a service credential has no favourites list to read either, and
    // returning an empty one would imply it could have a non-empty one.
    let attempts = [
        (
            "PUT",
            format!("/assets/{visible}/rating"),
            Some(json!({ "stars": 3 })),
        ),
        ("DELETE", format!("/assets/{visible}/rating"), None),
        ("PUT", format!("/assets/{visible}/favourite"), None),
        ("DELETE", format!("/assets/{visible}/favourite"), None),
        ("PUT", format!("/assets/{visible}/watch"), None),
        ("DELETE", format!("/assets/{visible}/watch"), None),
        ("GET", "/favourites".to_owned(), None),
        ("GET", "/watches".to_owned(), None),
    ];
    for (method, path, body) in attempts {
        let (status, response) = call(f, method, &path, &f.machine_key, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {response}");
    }

    // And nothing was recorded against a null identity, which is the row that would have been unownable.
    let orphans: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM asset_ratings WHERE identity_id IS NULL) \
              + (SELECT count(*) FROM asset_favourites WHERE identity_id IS NULL) \
              + (SELECT count(*) FROM asset_watches WHERE identity_id IS NULL)",
    )
    .fetch_one(&f.acme)
    .await
    .expect("count");
    assert_eq!(orphans, 0);
}

async fn a_hidden_asset_is_404_exactly_like_an_absent_one(f: &Fixture, hidden: Uuid) {
    // The admin key *can* see `hidden`, so this uses the scoped caller — for whom it is out of reach.
    let absent = Uuid::new_v4();
    for (label, id) in [("hidden", hidden), ("absent", absent)] {
        let (status, body) = call(
            f,
            "PUT",
            &format!("/assets/{id}/rating"),
            &f.scoped_key,
            Some(json!({ "stars": 5 })),
        )
        .await;
        // One status for both. Two would let a prober tell "exists but not yours" from "does not exist" by the
        // shape of the refusal, which is the whole disclosure the db layer collapses.
        assert_eq!(status, StatusCode::NOT_FOUND, "{label}: {body}");
    }
}

async fn the_private_lists_are_the_callers_own(f: &Fixture, visible: Uuid) {
    let (status, body) = call(f, "GET", "/favourites", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1), "{body}");
    assert_eq!(
        body["asset_ids"],
        json!([visible.to_string()]),
        "ids, not whole assets — the grid already knows how to render those: {body}"
    );

    // Grace favourited nothing, so her list is empty even though Ada's is not.
    let (status, body) = call(f, "GET", "/favourites", &f.other_key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0), "{body}");
    assert_eq!(body["asset_ids"], json!([]), "{body}");

    // Watches are a different list, and made *observably* different: a second asset is watched but not
    // favourited, so the two lists cannot be confused for one another. With both holding the same single id, the
    // watch route could have been wired to the favourites table and this passed — mutation testing said so.
    let watched_only = asset(f, "watched-only", true).await;
    let (status, body) = call(
        f,
        "PUT",
        &format!("/assets/{watched_only}/watch"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, body) = call(f, "GET", "/watches", &f.key, None).await;
    let watches = body["asset_ids"].as_array().expect("array").clone();
    assert!(
        watches.contains(&json!(watched_only.to_string())),
        "the watch list is missing the watched asset: {body}"
    );
    assert_eq!(body["total"], json!(2), "{body}");

    let (_, body) = call(f, "GET", "/favourites", &f.key, None).await;
    let favourites = body["asset_ids"].as_array().expect("array").clone();
    assert!(
        !favourites.contains(&json!(watched_only.to_string())),
        "watching something added it to the favourites list: {body}"
    );
    assert_eq!(body["total"], json!(1), "{body}");
}

async fn a_scoped_caller_sees_only_what_they_may(f: &Fixture, visible: Uuid, hidden: Uuid) {
    // `visible` is in the group the scoped role names, so rating it is allowed — Read is enough, and requiring
    // Manage would mean only administrators could ever favourite anything.
    let (status, body) = call(
        f,
        "PUT",
        &format!("/assets/{visible}/favourite"),
        &f.scoped_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_favourite"], json!(true), "{body}");
    // The count is the asset's and includes Ada's, because it is a fact about an asset this caller can see.
    assert_eq!(body["favourite_count"], json!(2), "{body}");
    // But no rating of their own, and no hint of who else favourited it.
    assert_eq!(body["my_stars"], Value::Null, "{body}");
    assert!(
        body.get("favourited_by").is_none() && body.get("rated_by").is_none(),
        "identities are never returned: {body}"
    );

    let (_, body) = call(f, "GET", "/favourites", &f.scoped_key, None).await;
    assert_eq!(
        body["asset_ids"],
        json!([visible.to_string()]),
        "only the reachable one: {body}"
    );
    assert!(
        !body["asset_ids"]
            .as_array()
            .expect("array")
            .contains(&json!(hidden.to_string())),
        "{body}"
    );
}
