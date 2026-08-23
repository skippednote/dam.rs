//! The usage index over HTTP (M3d·4, §11.4).
//!
//! `dam_db::connector_refs` proves the pin lapse, the sweep and the two kinds of staleness. What lives only
//! here is who may write, and it is the interesting part:
//!
//! - **Only a site's own credential may report its usage.** Not `Manage`. An administrator does not know which
//!   pages render which media, and the write feeds a signal that keeps objects out of cold storage — so a
//!   caller who can forge it can hold a library in Standard. Narrowing to the one credential with first-hand
//!   knowledge is both honest and tighter.
//! - **A full sync is one request**, because report-then-sweep leaves a window where a crash orphans everything
//!   the site had just re-reported.
//! - **An asset the connector cannot see is dropped, not a refusal.** A site reporting one had its scope
//!   narrowed after caching the id — an ordinary state, and failing the whole sync over it would lose the other
//!   nine hundred references.
//! - **The impact report counts what is live and lists everything.** The counts must not include a page that is
//!   gone; the list must include the dead rows, or "one site stopped reporting three weeks ago" is invisible.
//! - **A paused or revoked site cannot report at all.**

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::references::{ReferenceState, router};
use dam_db::connectors::{self, Kind, NewConnector};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    pool: PgPool,
    app: axum::Router,
    tenant_id: Uuid,
    /// A tenant admin. Holds `Manage` and still may not report.
    admin_key: String,
    /// The connector's own credential — the only one that may.
    site_key: String,
    site: Uuid,
    /// A second connector, so one site cannot report as another.
    other: Uuid,
    other_key: String,
    /// In the connector's group.
    visible: Uuid,
    /// Outside it.
    hidden: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("tenant");

    let ada = identity(&pool, "ada@example.com").await;
    member(&pool, tenant_id, ada, "{}", true).await;
    let admin_key = issue(&pool, tenant_id, ada).await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'public', 'Public') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("group");
    let visible = asset(&pool, "on-the-site").await;
    let hidden = asset(&pool, "internal").await;
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(visible)
        .execute(&pool)
        .await
        .expect("member");

    let (site, site_key) =
        connector(&pool, tenant_id, "Marketing site", "https://a.test", group).await;
    let (other, other_key) = connector(
        &pool,
        tenant_id,
        "Campaign microsite",
        "https://b.test",
        group,
    )
    .await;

    Fixture {
        _pg: pg,
        app: router(ReferenceState {
            global: pool.clone(),
        }),
        pool,
        tenant_id,
        admin_key,
        site_key,
        site,
        other,
        other_key,
        visible,
        hidden,
    }
}

async fn identity(pool: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("identity")
}

async fn member(pool: &PgPool, tenant: Uuid, who: Uuid, roles: &str, admin: bool) {
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3::text[], $4)",
    )
    .bind(tenant)
    .bind(who)
    .bind(roles)
    .bind(admin)
    .execute(pool)
    .await
    .expect("membership");
}

async fn issue(pool: &PgPool, tenant: Uuid, who: Uuid) -> String {
    let key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant)
    .bind(who)
    .bind(key.prefix())
    .bind(key.hash())
    .execute(pool)
    .await
    .expect("key");
    key.into_plaintext()
}

/// A connector with its service account, role, membership and key — as registration builds them.
async fn connector(
    pool: &PgPool,
    tenant_id: Uuid,
    label: &str,
    url: &str,
    group: Uuid,
) -> (Uuid, String) {
    let id = Uuid::now_v7();
    let identity_id = identity(pool, &format!("connector+{id}@connectors.invalid")).await;
    let role_key = format!("connector:{id}");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), $1, $1, '{asset:read}', ARRAY[$2], false)",
    )
    .bind(&role_key)
    .bind(group)
    .execute(pool)
    .await
    .expect("role");
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, ARRAY[$3], false)",
    )
    .bind(tenant_id)
    .bind(identity_id)
    .bind(&role_key)
    .execute(pool)
    .await
    .expect("membership");
    let key = auth::ApiKey::generate();
    let api_key_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES ($1, $2, $3, $4, $5, $6, '{}')",
    )
    .bind(api_key_id)
    .bind(tenant_id)
    .bind(identity_id)
    .bind(label)
    .bind(key.prefix())
    .bind(key.hash())
    .execute(pool)
    .await
    .expect("api key");

    let mut conn = pool.acquire().await.expect("conn");
    connectors::register(
        &mut conn,
        &NewConnector {
            id,
            kind: Kind::Drupal,
            label,
            site_url: url,
            remote_version: None,
            api_key_id: Some(api_key_id),
            sealed_secret: "v1.k1.n.c",
            asset_group_ids: &[group],
            allow_all_groups: false,
            allow_original: false,
            allow_restore: false,
            config: json!({}),
        },
    )
    .await
    .expect("register");
    (id, key.into_plaintext())
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

fn sync(refs: Value, full: bool) -> Value {
    json!({ "remote_entity_type": "media", "references": refs, "full_sync": full })
}

fn one(asset_id: Uuid, entity: &str, pages: i32) -> Value {
    json!({
        "asset_id": asset_id,
        "remote_entity_id": entity,
        "remote_url": format!("https://a.test/media/{entity}"),
        "usage_count": pages,
        "usage_sample": [{ "url": "/about", "title": "About us" }],
        "synced_version_no": 1,
    })
}

async fn only_the_sites_own_credential_may_report(f: &Fixture) {
    let path = format!("/connectors/{}/refs", f.site);
    let body = sync(json!([one(f.visible, "42", 3)]), false);

    // A tenant administrator holds Manage and still may not: they do not know which pages render which media,
    // and this write keeps objects out of cold storage.
    let (status, refused) = call(f, "PUT", &path, Some(&f.admin_key), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("own credential"),
        "{refused}"
    );

    // Nor may another connector, even one scoped to the same assets.
    let (status, _) = call(f, "PUT", &path, Some(&f.other_key), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The site itself may.
    let (status, done) = call(f, "PUT", &path, Some(&f.site_key), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["written"], 1);
    assert_eq!(done["orphaned"], 0, "an incremental report orphans nothing");
    let _ = f.other;
}

async fn a_full_sync_orphans_what_is_absent_in_the_same_request(f: &Fixture) {
    let path = format!("/connectors/{}/refs", f.site);
    // Two entities.
    call(
        f,
        "PUT",
        &path,
        Some(&f.site_key),
        Some(sync(
            json!([one(f.visible, "42", 3), one(f.visible, "43", 7)]),
            false,
        )),
    )
    .await;

    // The site's next full sync mentions only one of them. One request, so a crash cannot leave a window in
    // which the re-reported rows look abandoned.
    let (status, done) = call(
        f,
        "PUT",
        &path,
        Some(&f.site_key),
        Some(sync(json!([one(f.visible, "42", 3)]), true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["written"], 1);
    assert_eq!(done["orphaned"], 1);

    let (_, listed) = call(f, "GET", &path, Some(&f.admin_key), None).await;
    let rows = listed.as_array().expect("array");
    let states: Vec<&str> = rows
        .iter()
        .map(|row| row["state"].as_str().unwrap_or_default())
        .collect();
    assert!(states.contains(&"linked"), "{listed}");
    // Kept rather than deleted: an operator asking why something stopped being pinned needs to see it was
    // once used.
    assert!(states.contains(&"orphaned"), "{listed}");
}

async fn an_asset_the_site_cannot_see_is_dropped_not_a_refusal(f: &Fixture) {
    // A site reporting one had its scope narrowed after caching the id. Failing the whole sync over it would
    // lose every other reference in the request.
    let path = format!("/connectors/{}/refs", f.site);
    let (status, done) = call(
        f,
        "PUT",
        &path,
        Some(&f.site_key),
        Some(sync(
            json!([one(f.visible, "50", 2), one(f.hidden, "51", 9)]),
            false,
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["written"], 1, "one of the two: {done}");

    let (_, listed) = call(f, "GET", &path, Some(&f.admin_key), None).await;
    let entities: Vec<&str> = listed
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["remote_entity_id"].as_str().unwrap_or_default())
        .collect();
    assert!(entities.contains(&"50"), "{listed}");
    assert!(!entities.contains(&"51"), "{listed}");
}

async fn the_impact_report_counts_the_live_and_lists_the_dead(f: &Fixture) {
    // Reset to a known set: two live entities on this site, one on the other.
    call(
        f,
        "PUT",
        &format!("/connectors/{}/refs", f.site),
        Some(&f.site_key),
        Some(sync(
            json!([one(f.visible, "42", 12), one(f.visible, "43", 3)]),
            true,
        )),
    )
    .await;
    call(
        f,
        "PUT",
        &format!("/connectors/{}/refs", f.other),
        Some(&f.other_key),
        Some(sync(json!([one(f.visible, "9", 1)]), true)),
    )
    .await;

    let (status, impact) = call(
        f,
        "GET",
        &format!("/assets/{}/references", f.visible),
        Some(&f.admin_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{impact}");
    assert_eq!(impact["sites"], 2);
    assert_eq!(impact["entities"], 3);
    assert_eq!(impact["pages"], 16, "summed from what each site said");
    let refs = impact["references"].as_array().expect("references");
    // The *counts* are exact and the *list* is not: it carries every reference ever recorded, including rows
    // orphaned by an earlier case in this suite. That asymmetry is the design — the counts are what is live,
    // the list is what explains why — so the assertions are shaped to match rather than to a length.
    assert!(refs.len() >= 3, "{impact}");
    let live: Vec<&Value> = refs.iter().filter(|row| row["state"] == "linked").collect();
    assert_eq!(live.len(), 3, "three live references: {impact}");
    // Most-used first, so a takedown report leads with what matters.
    assert_eq!(refs[0]["usage_count"], 12);
    assert_eq!(refs[0]["connector_label"], "Marketing site");
    // The URL an operator goes and looks at.
    assert!(
        refs[0]["remote_url"]
            .as_str()
            .unwrap_or_default()
            .starts_with("https://a.test/media/"),
        "{impact}"
    );

    // Orphan one and the *counts* move while the *list* keeps it — the counts are what is live, and the list
    // is what explains why.
    call(
        f,
        "PUT",
        &format!("/connectors/{}/refs", f.site),
        Some(&f.site_key),
        Some(sync(json!([one(f.visible, "42", 12)]), true)),
    )
    .await;
    let (_, after) = call(
        f,
        "GET",
        &format!("/assets/{}/references", f.visible),
        Some(&f.admin_key),
        None,
    )
    .await;
    assert_eq!(after["entities"], 2);
    assert_eq!(after["pages"], 13);
    let rows = after["references"].as_array().expect("references");
    assert_eq!(
        rows.iter().filter(|row| row["state"] == "linked").count(),
        2,
        "two live: {after}"
    );
    assert!(
        rows.iter()
            .any(|row| row["remote_entity_id"] == "43" && row["state"] == "orphaned"),
        "the orphaned row is still listed, or 'a site stopped reporting' is invisible: {after}"
    );
}

async fn an_asset_with_no_references_reports_zeroes_rather_than_nothing(f: &Fixture) {
    let (status, impact) = call(
        f,
        "GET",
        &format!("/assets/{}/references", f.hidden),
        Some(&f.admin_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{impact}");
    assert_eq!(impact["sites"], 0);
    assert_eq!(impact["pages"], 0);
    assert!(impact["references"].as_array().expect("array").is_empty());
}

async fn an_asset_the_caller_cannot_see_has_no_report(f: &Fixture) {
    // Otherwise this endpoint would tell a scoped curator which pages use assets they were never shown.
    let scoped = identity(&f.pool, "cara@example.com").await;
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'nothing', 'Nothing', '{asset:read}', ARRAY[$1], false)",
    )
    .bind(
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO asset_groups (id, key, label) \
             VALUES (gen_random_uuid(), 'empty', 'Empty') RETURNING id",
        )
        .fetch_one(&f.pool)
        .await
        .expect("group"),
    )
    .execute(&f.pool)
    .await
    .expect("role");
    member(&f.pool, f.tenant_id, scoped, "{nothing}", false).await;
    let scoped_key = issue(&f.pool, f.tenant_id, scoped).await;

    let (status, _) = call(
        f,
        "GET",
        &format!("/assets/{}/references", f.visible),
        Some(&scoped_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn a_paused_or_revoked_site_cannot_report(f: &Fixture) {
    let path = format!("/connectors/{}/refs", f.site);
    let body = sync(json!([one(f.visible, "42", 3)]), false);
    let mut conn = f.pool.acquire().await.expect("conn");

    connectors::set_status(&mut conn, f.site, connectors::Status::Paused)
        .await
        .expect("pause");
    let (status, _) = call(f, "PUT", &path, Some(&f.site_key), Some(body.clone())).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a paused site should not be trusted about what it renders",
    );

    connectors::set_status(&mut conn, f.site, connectors::Status::Active)
        .await
        .expect("resume");
    let (status, _) = call(f, "PUT", &path, Some(&f.site_key), Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK);

    connectors::set_status(&mut conn, f.site, connectors::Status::Revoked)
        .await
        .expect("revoke");
    let (status, _) = call(f, "PUT", &path, Some(&f.site_key), Some(body)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn a_sync_is_refused_before_it_can_be_useless(f: &Fixture) {
    let path = format!("/connectors/{}/refs", f.other);
    for (body, expected) in [
        (
            json!({ "remote_entity_type": "  ", "references": [] }),
            "needs an entity type",
        ),
        (
            json!({
                "remote_entity_type": "media",
                "references": (0..1_001).map(|n| one(f.visible, &n.to_string(), 1)).collect::<Vec<_>>(),
            }),
            "sync in pages",
        ),
    ] {
        let (status, refused) = call(f, "PUT", &path, Some(&f.other_key), Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert!(
            refused["reason"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "expected {expected:?}, got {refused}"
        );
    }

    // A connector that does not exist is a 404 rather than a permission answer: to whoever holds the
    // credential, there is nothing there.
    let (status, _) = call(
        f,
        "PUT",
        &format!("/connectors/{}/refs", Uuid::now_v7()),
        Some(&f.other_key),
        Some(sync(json!([]), false)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn reading_a_sites_references_is_administration(f: &Fixture) {
    let path = format!("/connectors/{}/refs", f.other);
    // The site's own key may write and not read the list: reading what a site renders is an operator's
    // question, and a site already knows its own answer.
    let (status, _) = call(f, "GET", &path, Some(&f.other_key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(f, "GET", &path, Some(&f.admin_key), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_reference_contract_holds() {
    let f = fixture().await;

    only_the_sites_own_credential_may_report(&f).await;
    a_full_sync_orphans_what_is_absent_in_the_same_request(&f).await;
    an_asset_the_site_cannot_see_is_dropped_not_a_refusal(&f).await;
    the_impact_report_counts_the_live_and_lists_the_dead(&f).await;
    an_asset_with_no_references_reports_zeroes_rather_than_nothing(&f).await;
    an_asset_the_caller_cannot_see_has_no_report(&f).await;
    a_sync_is_refused_before_it_can_be_useless(&f).await;
    reading_a_sites_references_is_administration(&f).await;
    // Last, because it revokes the connector.
    a_paused_or_revoked_site_cannot_report(&f).await;
}
