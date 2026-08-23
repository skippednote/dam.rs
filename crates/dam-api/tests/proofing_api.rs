//! Proofing rounds over HTTP (M6b).
//!
//! `dam_db` proves the derived outcome, the snapshot and the visibility rule. What lives only here are the
//! decisions about who may do what, and one of them is the interesting one:
//!
//! - **Giving a verdict needs only `Read`.** A reviewer is somebody asked to look at pictures; requiring
//!   `Manage` to answer would mean only administrators could ever be asked to review anything. The round's own
//!   reviewer list is the authorisation, and the assets still have to be visible.
//! - **Opening and cancelling need `Manage`**, because both are the requester's act: one asks named people to
//!   spend time, the other withdraws the request.
//! - **A key with no identity reaches nothing here.** Every endpoint needs to know who is calling, and such a
//!   key has no membership and therefore no grants at all (`caller.rs`) — it is refused before the reviewer
//!   list is consulted.
//! - **But "nothing is waiting on you" is an empty list, not a refusal**, for a real identity who can see a
//!   round and is simply not a reviewer on it. A 403 there would read as a problem with the endpoint.
//! - **A closed round is a 409 that says what to do instead**, not a 422: nothing is wrong with the request.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::proofing::{ProofingState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    app: axum::Router,
    /// Tenant admin: Manage over everything.
    key: String,
    /// A reviewer: `asset:read` only, and named on the rounds below.
    reviewer_key: String,
    reviewer_id: Uuid,
    /// A key with no identity behind it, and so no membership, no roles and no grants.
    machine_key: String,
    /// Scoped to one group; cannot see `hidden`.
    scoped_key: String,
    visible: Uuid,
    hidden: Uuid,
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
    let admin = identity(&global, "ada@example.com", "Ada").await;
    member(&global, tenant_id, admin, "{}", true).await;

    let visible = asset(&acme, "visible").await;
    let hidden = asset(&acme, "hidden").await;

    // A reviewer with read-only access to everything: the ordinary shape of somebody asked to look.
    let reviewer_id = identity(&global, "bob@example.com", "Bob").await;
    member(&global, tenant_id, reviewer_id, "{}", true).await;

    // And a curator scoped to one group, to prove the visibility rule from the API side.
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(visible)
        .execute(&acme)
        .await
        .expect("member");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped', 'Scoped', '{asset:read,asset:manage}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let curator = identity(&global, "cara@example.com", "Cara").await;
    member(&global, tenant_id, curator, "{scoped}", false).await;

    Fixture {
        _pg: pg,
        app: router(ProofingState {
            global: global.clone(),
            // No delivery configured, so thumbnails come back absent — which is a state the screen has to
            // handle anyway, and asserting on a signed URL would test the signer rather than this surface.
            delivery: None,
        }),
        key: issue(&global, tenant_id, Some(admin), &[]).await,
        reviewer_key: issue(&global, tenant_id, Some(reviewer_id), &["asset:read"]).await,
        machine_key: issue(&global, tenant_id, None, &[]).await,
        scoped_key: issue(&global, tenant_id, Some(curator), &[]).await,
        reviewer_id,
        global,
        acme,
        visible,
        hidden,
    }
}

async fn identity(global: &PgPool, email: &str, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $2) RETURNING id",
    )
    .bind(email)
    .bind(name)
    .fetch_one(global)
    .await
    .expect("identity")
}

async fn member(global: &PgPool, tenant: Uuid, who: Uuid, roles: &str, admin: bool) {
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3::text[], $4)",
    )
    .bind(tenant)
    .bind(who)
    .bind(roles)
    .bind(admin)
    .execute(global)
    .await
    .expect("membership");
}

async fn issue(global: &PgPool, tenant: Uuid, who: Option<Uuid>, scopes: &[&str]) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, $5)",
    )
    .bind(tenant)
    .bind(who)
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
    let response = f
        .app
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

/// Opens a round over `visible`, asking the reviewer. Returns its id.
async fn round(f: &Fixture, title: &str) -> String {
    let (status, made) = call(
        f,
        "POST",
        "/proofing",
        Some(&f.key),
        Some(json!({
            "title": title,
            "brief": "check the crops",
            "asset_ids": [f.visible],
            "reviewer_ids": [f.reviewer_id],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    made["id"].as_str().expect("id").to_owned()
}

async fn opening_needs_manage_and_names_everybody(f: &Fixture) {
    let (status, _) = call(
        f,
        "POST",
        "/proofing",
        Some(&f.reviewer_key),
        Some(json!({ "title": "Nope", "asset_ids": [f.visible], "reviewer_ids": [f.reviewer_id] })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "asking named people to spend time is administration"
    );

    let (status, made) = call(
        f,
        "POST",
        "/proofing",
        Some(&f.key),
        Some(json!({
            "title": "  Spring campaign  ",
            "brief": "check the crops",
            "asset_ids": [f.visible],
            "reviewer_ids": [f.reviewer_id],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["title"], "Spring campaign", "trimmed");
    assert_eq!(made["number"], 1);
    assert_eq!(made["outcome"], "open");
    assert_eq!(made["asset_count"], 1);
    assert!(made["closed_at"].is_null());
    // Names resolved, both ways: who asked, and who was asked.
    assert_eq!(made["requested_by"]["name"], "Ada");
    let reviewers = made["reviewers"].as_array().expect("reviewers");
    assert_eq!(reviewers.len(), 1);
    assert_eq!(reviewers[0]["person"]["name"], "Bob");
    assert_eq!(reviewers[0]["verdict"], "pending");
    assert!(reviewers[0]["decided_at"].is_null());
}

async fn a_round_needs_a_title_assets_and_reviewers(f: &Fixture) {
    for (body, expected) in [
        (
            json!({ "title": "  ", "asset_ids": [f.visible], "reviewer_ids": [f.reviewer_id] }),
            "needs a title",
        ),
        (
            json!({ "title": "No assets", "asset_ids": [], "reviewer_ids": [f.reviewer_id] }),
            "at least one asset",
        ),
        (
            json!({ "title": "No reviewers", "asset_ids": [f.visible], "reviewer_ids": [] }),
            "at least one reviewer",
        ),
    ] {
        let (status, refused) = call(f, "POST", "/proofing", Some(&f.key), Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        let reason = refused["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(expected),
            "expected {expected:?}, got {reason:?}"
        );
    }
}

async fn a_round_over_assets_you_cannot_see_is_refused(f: &Fixture) {
    let (status, refused) = call(
        f,
        "POST",
        "/proofing",
        Some(&f.scoped_key),
        Some(json!({
            "title": "Mixed",
            "asset_ids": [f.visible, f.hidden],
            "reviewer_ids": [f.reviewer_id],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let reason = refused["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("cannot be narrowed silently"),
        "the refusal says why it is not just dropping them: {reason}"
    );
}

async fn reviewing_needs_only_read(f: &Fixture) {
    // The interesting permission. Requiring Manage to answer would mean only administrators could ever be
    // asked to review anything, which is the opposite of what a review is for.
    let id = round(f, "Read is enough").await;
    let (status, decided) = call(
        f,
        "POST",
        &format!("/proofing/{id}/verdict"),
        Some(&f.reviewer_key),
        Some(json!({ "verdict": "approved", "note": "looks right" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    assert_eq!(
        decided["outcome"], "approved",
        "the last reviewer closes it"
    );
    assert!(decided["closed_at"].is_string());
    let reviewers = decided["reviewers"].as_array().expect("reviewers");
    assert_eq!(reviewers[0]["verdict"], "approved");
    assert_eq!(reviewers[0]["note"], "looks right");
    assert!(reviewers[0]["decided_at"].is_string());
}

async fn somebody_not_on_the_list_is_forbidden(f: &Fixture) {
    let id = round(f, "Guarded").await;
    // The admin can *read* the round but is not a reviewer on it, so cannot answer for one.
    let (status, refused) = call(
        f,
        "POST",
        &format!("/proofing/{id}/verdict"),
        Some(&f.key),
        Some(json!({ "verdict": "approved" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    // And the round is still open, so an outsider cannot close it by accident.
    let (_, read) = call(f, "GET", &format!("/proofing/{id}"), Some(&f.key), None).await;
    assert_eq!(read["outcome"], "open");
}

async fn a_closed_round_says_what_to_do_instead(f: &Fixture) {
    let id = round(f, "Closed").await;
    call(
        f,
        "POST",
        &format!("/proofing/{id}/verdict"),
        Some(&f.reviewer_key),
        Some(json!({ "verdict": "changes_requested", "note": "tighter crops" })),
    )
    .await;

    let (status, refused) = call(
        f,
        "POST",
        &format!("/proofing/{id}/verdict"),
        Some(&f.reviewer_key),
        Some(json!({ "verdict": "approved" })),
    )
    .await;
    // 409, not 422: nothing is wrong with the request — the round is over.
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("a new round"),
        "the refusal says what to do instead: {refused}"
    );

    // A second pass, pointing at the first.
    let (status, second) = call(
        f,
        "POST",
        "/proofing",
        Some(&f.key),
        Some(json!({
            "title": "Closed",
            "asset_ids": [f.visible],
            "reviewer_ids": [f.reviewer_id],
            "supersedes": id,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["number"], 2);
    assert_eq!(second["supersedes"], id);
    // And round one still says what happened.
    let (_, first) = call(f, "GET", &format!("/proofing/{id}"), Some(&f.key), None).await;
    assert_eq!(first["outcome"], "changes_requested");
    assert_eq!(first["reviewers"][0]["note"], "tighter crops");
}

async fn a_key_with_no_identity_reaches_nothing_here(f: &Fixture) {
    let id = round(f, "Machines").await;
    // Every endpoint on this surface needs to know who is calling: to decide, to be listed as waiting, or
    // simply to have grants at all. A key with no identity behind it has no membership and therefore no roles
    // (`caller.rs`), so it is refused before the reviewer list is even consulted. Fail-closed, and worth an
    // assertion because a reviewer list is exactly the kind of thing somebody might later be tempted to treat
    // as sufficient authorisation on its own.
    for (method, path) in [
        ("GET", "/proofing".to_owned()),
        ("GET", "/proofing/mine".to_owned()),
        ("GET", format!("/proofing/{id}")),
        ("GET", format!("/proofing/{id}/assets")),
        ("POST", format!("/proofing/{id}/verdict")),
        ("POST", format!("/proofing/{id}/cancel")),
    ] {
        let body = method.eq("POST").then(|| json!({ "verdict": "approved" }));
        let (status, _) = call(f, method, &path, Some(&f.machine_key), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

async fn an_invented_verdict_is_refused_by_name(f: &Fixture) {
    let id = round(f, "Invented").await;
    for verdict in ["pending", "maybe", ""] {
        let (status, refused) = call(
            f,
            "POST",
            &format!("/proofing/{id}/verdict"),
            Some(&f.reviewer_key),
            Some(json!({ "verdict": verdict })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{verdict:?}: {refused}"
        );
        let reason = refused["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("approved") && reason.contains("changes_requested"),
            "the refusal lists the verdicts that work: {reason}"
        );
    }
    // `pending` in particular: a starting state, not an answer. Accepting it would let a reviewer un-decide.
}

async fn a_rounds_assets_come_back_in_snapshot_order(f: &Fixture) {
    // Two assets, given in an order that is not their id order, so "snapshot order" means something.
    let (status, made) = call(
        f,
        "POST",
        "/proofing",
        Some(&f.key),
        Some(json!({
            "title": "Ordered",
            "asset_ids": [f.hidden, f.visible],
            "reviewer_ids": [f.reviewer_id],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().expect("id");

    let (status, listed) = call(
        f,
        "GET",
        &format!("/proofing/{id}/assets"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed.as_array().expect("array");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0]["asset_id"],
        f.hidden.to_string(),
        "the order asked for, not id order"
    );
    assert_eq!(rows[1]["asset_id"], f.visible.to_string());
    assert_eq!(rows[0]["position"], 0);
    assert_eq!(rows[0]["filename"], "hidden.jpg");
    // Delivery is unconfigured in this fixture, so there is nothing to sign. Absent, not an error.
    assert!(rows[0]["thumbnail_url"].is_null());

    // And the visibility rule is the round's, not the item's: the scoped curator cannot see one of these two,
    // so they get the same 404 the round itself gives rather than a partial list. A review screen showing one
    // of two pictures would be asking somebody to approve a set they never saw.
    let (status, _) = call(
        f,
        "GET",
        &format!("/proofing/{id}/assets"),
        Some(&f.scoped_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn my_list_holds_only_what_is_waiting_on_me(f: &Fixture) {
    let waiting = round(f, "Waiting on Bob").await;
    let (status, mine) = call(f, "GET", "/proofing/mine", Some(&f.reviewer_key), None).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let ids: Vec<&str> = mine
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["id"].as_str().expect("id"))
        .collect();
    assert!(ids.contains(&waiting.as_str()), "{mine}");

    // Answering removes it, even though the round may still be open for others.
    call(
        f,
        "POST",
        &format!("/proofing/{waiting}/verdict"),
        Some(&f.reviewer_key),
        Some(json!({ "verdict": "approved" })),
    )
    .await;
    let (_, after) = call(f, "GET", "/proofing/mine", Some(&f.reviewer_key), None).await;
    let ids: Vec<&str> = after
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["id"].as_str().expect("id"))
        .collect();
    assert!(!ids.contains(&waiting.as_str()));

    // And the admin, who is on no reviewer list, has nothing waiting.
    let (_, admins) = call(f, "GET", "/proofing/mine", Some(&f.key), None).await;
    assert_eq!(
        admins,
        json!([]),
        "being able to see a round is not being asked about it"
    );
}

async fn a_partly_visible_round_is_a_404(f: &Fixture) {
    // Opened by the admin over both assets; the scoped curator is a reviewer but cannot see one of them.
    let (status, made) = call(
        f,
        "POST",
        "/proofing",
        Some(&f.key),
        Some(json!({
            "title": "Both",
            "asset_ids": [f.visible, f.hidden],
            "reviewer_ids": [f.reviewer_id],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let id = made["id"].as_str().expect("id");

    let (status, _) = call(
        f,
        "GET",
        &format!("/proofing/{id}"),
        Some(&f.scoped_key),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a round whose scope you can only partly see is not visible at all"
    );
    let (_, listed) = call(f, "GET", "/proofing", Some(&f.scoped_key), None).await;
    let ids: Vec<&str> = listed
        .as_array()
        .expect("array")
        .iter()
        .map(|r| r["id"].as_str().expect("id"))
        .collect();
    assert!(!ids.contains(&id), "{listed}");
}

async fn cancelling_is_the_requesters_act_and_keeps_the_verdicts(f: &Fixture) {
    let id = round(f, "Withdrawn").await;

    // A reviewer cannot withdraw the review they were asked to do.
    let (status, _) = call(
        f,
        "POST",
        &format!("/proofing/{id}/cancel"),
        Some(&f.reviewer_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, cancelled) = call(
        f,
        "POST",
        &format!("/proofing/{id}/cancel"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["outcome"], "cancelled");
    assert!(
        cancelled["closed_at"].is_string(),
        "cancelled implies closed"
    );

    // Twice is a 404 rather than a second cancellation, so the recorded moment is the first one.
    let (status, _) = call(
        f,
        "POST",
        &format!("/proofing/{id}/cancel"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_proofing_contract_holds() {
    let f = fixture().await;

    opening_needs_manage_and_names_everybody(&f).await;
    a_round_needs_a_title_assets_and_reviewers(&f).await;
    a_round_over_assets_you_cannot_see_is_refused(&f).await;

    reviewing_needs_only_read(&f).await;
    somebody_not_on_the_list_is_forbidden(&f).await;
    a_closed_round_says_what_to_do_instead(&f).await;
    a_key_with_no_identity_reaches_nothing_here(&f).await;
    an_invented_verdict_is_refused_by_name(&f).await;

    a_rounds_assets_come_back_in_snapshot_order(&f).await;
    my_list_holds_only_what_is_waiting_on_me(&f).await;
    a_partly_visible_round_is_a_404(&f).await;
    cancelling_is_the_requesters_act_and_keeps_the_verdicts(&f).await;

    assert!(!f.global.is_closed() && !f.acme.is_closed());
}
