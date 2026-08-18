//! The asset endpoints (`GET /assets`, `GET /assets/{id}`, `PATCH /assets/{id}/metadata`).
//!
//! `dam_db::assets` is tested against the predicate directly. What this suite is for is the part only an
//! HTTP layer can get wrong:
//!
//! - an unauthenticated request gets nowhere, and a *read* key cannot write;
//! - **another tenant's asset id is 404, not 403** — a 403 confirms the id exists, which is the disclosure
//!   §7 forbids;
//! - the tenant scope comes from the key rather than from anything the caller sends, so there is no
//!   parameter to tamper with;
//! - a metadata PATCH is a merge that validates as a patch, so editing one caption does not demand every
//!   required field.
//!
//! One container per group of cases, as in the TUS suite next door, and for the same measured reason.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::assets::{AssetState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    global: PgPool,
    acme: PgPool,
    globex: PgPool,
    /// An administrator on `acme`.
    key: String,
    /// An administrator on `globex`, so cross-tenant probing can be tested at all.
    other_key: String,
    /// Same tenant, same identity, restricted to reading.
    read_only_key: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    migrate::tenant(&url, "t_globex").await.expect("globex");
    let global = pg.pool().clone();

    let key = provision(&global, "acme", "a@example.com").await;
    let other_key = provision(&global, "globex", "b@example.com").await;
    let read_only_key = scoped_key(&global, "acme", &["asset:read"]).await;

    let app = router(AssetState {
        global: global.clone(),
        // No delivery state: this suite is about the endpoints' contract, not about minting tokens, and
        // `thumbnail_url` being absent is exactly what the case below asserts.
        delivery: None,
    });
    let acme = pg_schema(&pg, "t_acme").await;
    let globex = pg_schema(&pg, "t_globex").await;

    Fixture {
        _pg: pg,
        app,
        acme,
        globex,
        global,
        key,
        other_key,
        read_only_key,
    }
}

async fn pg_schema(pg: &PostgresHarness, schema: &str) -> PgPool {
    pg.pool_for_schema(schema).await.expect("tenant pool")
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

    issue_key(global, tenant_id, identity, &[]).await
}

async fn scoped_key(global: &PgPool, slug: &str, scopes: &[&str]) -> String {
    let (tenant_id, identity_id): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.id, m.identity_id FROM dam_global.tenants t \
         JOIN dam_global.tenant_members m ON m.tenant_id = t.id WHERE t.slug = $1",
    )
    .bind(slug)
    .fetch_one(global)
    .await
    .expect("tenant and member");
    issue_key(global, tenant_id, identity_id, scopes).await
}

async fn issue_key(global: &PgPool, tenant: Uuid, identity: Uuid, scopes: &[&str]) -> String {
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

async fn asset(pool: &PgPool, filename: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, width, height, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, 800, 600, $1)",
    )
    .bind(id)
    .bind(blake3::hash(filename.as_bytes()).to_hex().to_string())
    .bind(filename)
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn field(pool: &PgPool, key: &str, kind: &str, required: bool) {
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, required, display_order) \
         VALUES (gen_random_uuid(), $1, $1, $2, $3, 1) \
         ON CONFLICT (key) DO UPDATE SET required = excluded.required",
    )
    .bind(key)
    .bind(kind)
    .bind(required)
    .execute(pool)
    .await
    .expect("field def");
}

async fn send(app: &axum::Router, request: Request<Body>) -> axum::http::Response<Body> {
    app.clone().oneshot(request).await.expect("router")
}

fn get(uri: &str, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(key) = key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    builder.body(Body::empty()).expect("request")
}

fn patch_metadata(asset_id: Uuid, key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/assets/{asset_id}/metadata"))
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

async fn json(response: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

// ─── authentication ─────────────────────────────────────────────────────────

async fn no_credential_gets_nowhere(f: &Fixture) {
    for uri in ["/assets", "/assets/00000000-0000-4000-8000-000000000000"] {
        assert_eq!(
            send(&f.app, get(uri, None)).await.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} without a credential"
        );
    }
}

async fn every_bad_credential_looks_the_same(f: &Fixture) {
    // Unknown, revoked and expired keys are deliberately indistinguishable: telling a prober which of their
    // guesses had the right *shape* hands them the cheap half of the search.
    let unknown = "dam_sk_totally_made_up_key_value_here";
    assert_eq!(
        send(&f.app, get("/assets", Some(unknown))).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let revoked = scoped_key(&f.global, "acme", &[]).await;
    sqlx::query("UPDATE dam_global.api_keys SET revoked_at = now() WHERE key_hash = $1")
        .bind(auth::ApiKey::hash_of(&revoked))
        .execute(&f.global)
        .await
        .expect("revoke");
    assert_eq!(
        send(&f.app, get("/assets", Some(&revoked))).await.status(),
        StatusCode::UNAUTHORIZED,
        "a revoked key must be as anonymous as a made-up one"
    );

    let expired = scoped_key(&f.global, "acme", &[]).await;
    sqlx::query(
        "UPDATE dam_global.api_keys SET expires_at = now() - interval '1 hour' WHERE key_hash = $1",
    )
    .bind(auth::ApiKey::hash_of(&expired))
    .execute(&f.global)
    .await
    .expect("expire");
    assert_eq!(
        send(&f.app, get("/assets", Some(&expired))).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

async fn the_bearer_scheme_is_case_insensitive(f: &Fixture) {
    // RFC 9110 says the scheme is case-insensitive, and a client sending `bearer` is not wrong. Refusing it
    // produces a 401 that no amount of checking the key explains.
    let request = Request::builder()
        .method("GET")
        .uri("/assets")
        .header(header::AUTHORIZATION, format!("bearer {}", f.key))
        .body(Body::empty())
        .expect("request");
    assert_eq!(send(&f.app, request).await.status(), StatusCode::OK);
}

// ─── tenant isolation ───────────────────────────────────────────────────────

async fn the_tenant_comes_from_the_key_and_nothing_else(f: &Fixture) {
    let mine = asset(&f.acme, "acme-only.jpg").await;
    let theirs = asset(&f.globex, "globex-only.jpg").await;

    let page = json(send(&f.app, get("/assets", Some(&f.key))).await).await;
    let ids: Vec<Uuid> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["id"].as_str().expect("id").parse().expect("uuid"))
        .collect();
    assert_eq!(ids, vec![mine], "only this tenant's assets");
    assert_eq!(page["total"], 1, "and only this tenant's total");

    let other = json(send(&f.app, get("/assets", Some(&f.other_key))).await).await;
    assert_eq!(
        other["items"][0]["id"].as_str().expect("id"),
        theirs.to_string()
    );
}

async fn another_tenants_asset_is_404_rather_than_403(f: &Fixture) {
    // A 403 would confirm the id exists. The whole point of returning `None` from the read layer for both
    // "gone" and "not yours" is that the handler cannot tell them apart either.
    let theirs = asset(&f.globex, "probe-me.jpg").await;
    let response = send(&f.app, get(&format!("/assets/{theirs}"), Some(&f.key))).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let never_existed = Uuid::new_v4();
    assert_eq!(
        send(
            &f.app,
            get(&format!("/assets/{never_existed}"), Some(&f.key))
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "and an id that never existed answers identically"
    );
}

async fn a_malformed_asset_id_is_a_bad_request_rather_than_a_500(f: &Fixture) {
    let response = send(&f.app, get("/assets/not-a-uuid", Some(&f.key))).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ─── the page ───────────────────────────────────────────────────────────────

async fn the_page_carries_what_the_grid_draws(f: &Fixture) {
    let id = asset(&f.acme, "drawn.jpg").await;
    let page = json(send(&f.app, get("/assets?limit=1", Some(&f.key))).await).await;
    let item = &page["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|i| i["id"].as_str() == Some(&id.to_string()))
        .cloned()
        .unwrap_or_else(|| page["items"][0].clone());

    assert_eq!(item["mime"], "image/jpeg");
    assert_eq!(item["bytes"], 4096);
    assert_eq!(item["width"], 800);
    assert_eq!(item["height"], 600);
    assert_eq!(
        item["tier"], "hot",
        "an asset with no placement yet is not archived"
    );
    assert_eq!(
        item["rights_state"], "unknown",
        "unevaluated rights are `unknown`, and the UI must not style that like `allowed`"
    );
    assert_eq!(item["provenance_state"], "none");
    assert!(
        item["thumbnail_url"].is_null(),
        "absent until the internal-preview rights question in NEEDS-REVIEW.md is answered"
    );
    assert!(page["offset"].is_number());
}

async fn paging_parameters_are_honoured_and_clamped(f: &Fixture) {
    for n in 0..5 {
        asset(&f.acme, &format!("paged-{n}.jpg")).await;
    }
    let first = json(send(&f.app, get("/assets?offset=0&limit=2", Some(&f.key))).await).await;
    assert_eq!(first["items"].as_array().expect("items").len(), 2);
    assert_eq!(first["offset"], 0);

    let second = json(send(&f.app, get("/assets?offset=2&limit=2", Some(&f.key))).await).await;
    assert_eq!(second["offset"], 2);
    assert_ne!(
        first["items"][0]["id"], second["items"][0]["id"],
        "a second page must not repeat the first"
    );

    // From a query string, so neither can be trusted and neither should be a 500.
    let absurd = send(&f.app, get("/assets?limit=999999999", Some(&f.key))).await;
    assert_eq!(absurd.status(), StatusCode::OK);
    let negative = send(&f.app, get("/assets?offset=-10", Some(&f.key))).await;
    assert_eq!(negative.status(), StatusCode::OK);
}

async fn an_unknown_order_is_refused_rather_than_silently_defaulted(f: &Fixture) {
    // A closed set on the wire. Defaulting an unrecognised order to `newest` means a client whose sort
    // silently stopped working looks at the server for an explanation and finds none.
    let response = send(&f.app, get("/assets?order=by_vibes", Some(&f.key))).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    for order in [
        "newest",
        "oldest",
        "filename_asc",
        "filename_desc",
        "largest_first",
    ] {
        assert_eq!(
            send(&f.app, get(&format!("/assets?order={order}"), Some(&f.key)))
                .await
                .status(),
            StatusCode::OK,
            "{order} must be accepted"
        );
    }
}

// ─── metadata ───────────────────────────────────────────────────────────────

async fn a_read_only_key_cannot_edit_metadata(f: &Fixture) {
    // The scope is checked against `Manage`, not `Read`: a caller who can see an asset is not thereby
    // allowed to relabel it.
    let id = asset(&f.acme, "read-only.jpg").await;
    field(&f.acme, "caption", "text", false).await;

    assert_eq!(
        send(
            &f.app,
            get(&format!("/assets/{id}"), Some(&f.read_only_key))
        )
        .await
        .status(),
        StatusCode::OK,
        "reading is exactly what this key is for"
    );
    assert_eq!(
        send(
            &f.app,
            patch_metadata(
                id,
                &f.read_only_key,
                serde_json::json!({"values": {"caption": "x"}})
            ),
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}

async fn an_edit_merges_rather_than_replacing(f: &Fixture) {
    let id = asset(&f.acme, "merged.jpg").await;
    field(&f.acme, "caption", "text", false).await;
    field(&f.acme, "photographer", "text", false).await;

    let first = json(
        send(
            &f.app,
            patch_metadata(
                id,
                &f.key,
                serde_json::json!({"values": {"caption": "a harbour", "photographer": "Ada"}}),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(first["values"]["caption"], "a harbour");

    // One field, and the other must survive. Two clients editing different fields of one asset must not
    // overwrite each other, which a PUT of the whole document guarantees they do.
    let second = json(
        send(
            &f.app,
            patch_metadata(
                id,
                &f.key,
                serde_json::json!({"values": {"caption": "a quay"}}),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(second["values"]["caption"], "a quay");
    assert_eq!(
        second["values"]["photographer"], "Ada",
        "an absent key is left alone, not cleared"
    );

    // And the read agrees, so the panel does not have to trust its own optimistic update.
    let detail = json(send(&f.app, get(&format!("/assets/{id}"), Some(&f.key))).await).await;
    assert_eq!(detail["values"]["caption"], "a quay");
    assert_eq!(detail["values"]["photographer"], "Ada");
}

async fn a_null_clears_one_field_and_leaves_the_rest(f: &Fixture) {
    let id = asset(&f.acme, "cleared.jpg").await;
    field(&f.acme, "caption", "text", false).await;
    field(&f.acme, "photographer", "text", false).await;

    send(
        &f.app,
        patch_metadata(
            id,
            &f.key,
            serde_json::json!({"values": {"caption": "keep", "photographer": "drop"}}),
        ),
    )
    .await;

    let cleared = json(
        send(
            &f.app,
            patch_metadata(
                id,
                &f.key,
                serde_json::json!({"values": {"photographer": null}}),
            ),
        )
        .await,
    )
    .await;
    assert!(
        cleared["values"].get("photographer").is_none(),
        "an explicit null is an instruction to clear: {cleared}"
    );
    assert_eq!(cleared["values"]["caption"], "keep");
}

async fn editing_one_field_does_not_demand_every_required_one(f: &Fixture) {
    // `Mode::Patch`: an absent key is left alone, so `required` does not apply to it. Validating the merged
    // document instead would refuse an edit of one caption on any asset missing a required field — which is
    // every asset, the moment an administrator adds one.
    let id = asset(&f.acme, "partial.jpg").await;
    field(&f.acme, "caption", "text", false).await;
    field(&f.acme, "campaign", "text", true).await;

    let response = send(
        &f.app,
        patch_metadata(
            id,
            &f.key,
            serde_json::json!({"values": {"caption": "just this"}}),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "editing one optional field must not require the campaign to be filled in"
    );
}

async fn clearing_a_required_field_is_refused_with_a_code(f: &Fixture) {
    let id = asset(&f.acme, "required.jpg").await;
    field(&f.acme, "campaign", "text", true).await;

    send(
        &f.app,
        patch_metadata(
            id,
            &f.key,
            serde_json::json!({"values": {"campaign": "spring"}}),
        ),
    )
    .await;

    let response = send(
        &f.app,
        patch_metadata(
            id,
            &f.key,
            serde_json::json!({"values": {"campaign": null}}),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "a required field may not be cleared"
    );
    let problems = json(response).await;
    let first = &problems.as_array().expect("a list of problems")[0];
    assert_eq!(first["key"], "campaign");
    assert!(
        first["code"].as_str().is_some_and(|c| !c.is_empty()),
        "a stable machine-readable code, because a UI maps it to a message in the user's language: {first}"
    );
}

async fn an_undefined_field_is_refused_rather_than_stored(f: &Fixture) {
    // Otherwise the metadata document accumulates keys no schema describes, and they are invisible to
    // search, to facets and to the connector — present in the database and absent everywhere a user looks.
    let id = asset(&f.acme, "undefined.jpg").await;
    let response = send(
        &f.app,
        patch_metadata(
            id,
            &f.key,
            serde_json::json!({"values": {"not_a_field": "x"}}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

async fn another_tenants_asset_cannot_be_edited_and_answers_404(f: &Fixture) {
    let theirs = asset(&f.globex, "not-yours.jpg").await;
    assert_eq!(
        send(
            &f.app,
            patch_metadata(theirs, &f.key, serde_json::json!({"values": {}})),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "a write must be refused by the same predicate the read is, and say no more than the read does"
    );
}

// ─── drivers ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn authentication_and_isolation_hold() {
    let f = fixture().await;
    no_credential_gets_nowhere(&f).await;
    every_bad_credential_looks_the_same(&f).await;
    the_bearer_scheme_is_case_insensitive(&f).await;
    the_tenant_comes_from_the_key_and_nothing_else(&f).await;
    another_tenants_asset_is_404_rather_than_403(&f).await;
    a_malformed_asset_id_is_a_bad_request_rather_than_a_500(&f).await;
}

#[tokio::test]
async fn the_page_contract_holds() {
    let f = fixture().await;
    the_page_carries_what_the_grid_draws(&f).await;
    paging_parameters_are_honoured_and_clamped(&f).await;
    an_unknown_order_is_refused_rather_than_silently_defaulted(&f).await;
}

#[tokio::test]
async fn the_metadata_contract_holds() {
    let f = fixture().await;
    a_read_only_key_cannot_edit_metadata(&f).await;
    an_edit_merges_rather_than_replacing(&f).await;
    a_null_clears_one_field_and_leaves_the_rest(&f).await;
    editing_one_field_does_not_demand_every_required_one(&f).await;
    clearing_a_required_field_is_refused_with_a_code(&f).await;
    an_undefined_field_is_refused_rather_than_stored(&f).await;
    another_tenants_asset_cannot_be_edited_and_answers_404(&f).await;
}
