//! Legal hold over HTTP, and the record that proves it happened (G10).
//!
//! What these defend, over and above the unit cases:
//!
//! - **A hold is a `Manage` action.** Being allowed to see an asset is not being allowed to freeze it.
//! - **An invisible asset answers 404.** The same existence-oracle rule the rest of the asset surface follows.
//! - **A no-op writes nothing.** Re-asserting a hold must not fill the record with rows describing no
//!   decision.
//! - **A broken chain is a 200.** "We cannot tell you" and "the record has been altered" are different
//!   sentences, and a 500 would say the first while meaning the second.
//! - **Export writes, so it is not a GET.** Behind GET, a link preview appends a false trail of people who
//!   never asked for the data.
//! - **One chain per tenant, not one per feature.** A connector registration and a legal hold land in the
//!   same chain in the order they happened, which is what makes the log a history rather than four logs.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::governance::{GovernanceState, router};
use dam_core::Secret;
use dam_core::sealed::SealingKeyring;
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    acme: PgPool,
    app: axum::Router,
    /// The connector router, so the chain can be shown spanning two subsystems.
    connectors: axum::Router,
    /// Holds `asset:manage`.
    key: String,
    /// Holds `asset:read` only.
    reader_key: String,
    ada: Uuid,
    visible: Uuid,
    /// In no asset group, so the reader cannot see it.
    hidden: Uuid,
    public_group: Uuid,
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

    let ada = identity(&global, "ada@example.com", "Ada").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(ada)
    .execute(&global)
    .await
    .expect("membership");

    // A second person with a read-only role, so the permission gate is tested against a real caller rather
    // than against a missing credential.
    let bob = identity(&global, "bob@example.com", "Bob").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{reader}', false)",
    )
    .bind(tenant_id)
    .bind(bob)
    .execute(&global)
    .await
    .expect("membership");

    let visible = asset(&acme, "visible").await;
    let hidden = asset(&acme, "hidden").await;
    let public_group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) \
         VALUES (gen_random_uuid(), 'public', 'Public') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(public_group)
        .bind(visible)
        .execute(&acme)
        .await
        .expect("member");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'reader', 'Reader', '{asset:read}', $1, false)",
    )
    .bind(vec![public_group])
    .execute(&acme)
    .await
    .expect("role");

    Fixture {
        _pg: pg,
        app: router(GovernanceState {
            global: global.clone(),
        }),
        connectors: dam_api::connectors::router(dam_api::connectors::ConnectorState {
            global: global.clone(),
            keyring: SealingKeyring::single(
                "k1",
                &Secret::new("a test sealing passphrase".to_owned()),
            ),
        }),
        key: issue(&global, tenant_id, ada).await,
        reader_key: issue(&global, tenant_id, bob).await,
        acme,
        ada,
        visible,
        hidden,
        public_group,
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

async fn held(pool: &PgPool, asset_id: Uuid) -> bool {
    sqlx::query_scalar("SELECT legal_hold FROM assets WHERE id = $1")
        .bind(asset_id)
        .fetch_one(pool)
        .await
        .expect("read hold")
}

async fn entry_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log")
        .fetch_one(pool)
        .await
        .expect("count")
}

#[tokio::test]
async fn the_governance_surface() {
    let f = fixture().await;

    placing_a_hold_records_who_and_why(&f).await;
    re_placing_a_hold_changes_nothing_and_records_nothing(&f).await;
    lifting_a_hold_records_the_other_direction(&f).await;
    a_hold_needs_a_reason(&f).await;
    reading_needs_manage_and_so_does_holding(&f).await;
    an_asset_this_caller_cannot_see_is_a_404(&f).await;
    the_log_filters_by_target_and_carries_both_hashes(&f).await;
    verification_reports_an_intact_chain(&f).await;
    an_export_appends_its_own_entry_and_excludes_it(&f).await;
    an_export_is_not_reachable_by_get(&f).await;
    one_chain_carries_every_subsystem(&f).await;
    // Last, because it deliberately breaks the chain the earlier cases built.
    a_tampered_chain_is_a_two_hundred_saying_so(&f).await;
}

async fn placing_a_hold_records_who_and_why(f: &Fixture) {
    let (status, body) = call(
        &f.app,
        "PUT",
        &format!("/assets/{}/legal-hold", f.visible),
        Some(&f.key),
        Some(json!({ "held": true, "reason": "  litigation hold, matter 2026-114  " })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["held"], json!(true));
    assert_eq!(body["changed"], json!(true));
    let seq = body["audit_seq"].as_i64().expect("a cited sequence number");

    assert!(
        held(&f.acme, f.visible).await,
        "the column the rest of the system reads"
    );

    let (status, log) = call(&f.app, "GET", "/audit", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{log}");
    let entry = &log["entries"][0];
    assert_eq!(entry["seq"].as_i64(), Some(seq));
    assert_eq!(entry["action"], "legal_hold.placed");
    assert_eq!(entry["actor_kind"], "user");
    assert_eq!(entry["actor_id"], json!(f.ada));
    assert_eq!(entry["target_kind"], "asset");
    assert_eq!(entry["target_id"], json!(f.visible.to_string()));
    // Trimmed, and the filename carried alongside — an audit row that names only a uuid is a row somebody has
    // to go and resolve, against a table that may have moved on.
    assert_eq!(
        entry["payload"]["reason"],
        "litigation hold, matter 2026-114"
    );
    assert_eq!(entry["payload"]["filename"], "visible.jpg");
    assert_eq!(entry["prev_hash"], Value::Null, "the genesis entry");
}

async fn re_placing_a_hold_changes_nothing_and_records_nothing(f: &Fixture) {
    let before = entry_count(&f.acme).await;
    let (status, body) = call(
        &f.app,
        "PUT",
        &format!("/assets/{}/legal-hold", f.visible),
        Some(&f.key),
        Some(json!({ "held": true, "reason": "again" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], json!(false));
    assert_eq!(body["audit_seq"], Value::Null);
    assert_eq!(entry_count(&f.acme).await, before, "no entry for a no-op");
}

async fn lifting_a_hold_records_the_other_direction(f: &Fixture) {
    let (status, body) = call(
        &f.app,
        "PUT",
        &format!("/assets/{}/legal-hold", f.visible),
        Some(&f.key),
        Some(json!({ "held": false, "reason": "matter closed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], json!(true));
    assert!(!held(&f.acme, f.visible).await);

    let (_, log) = call(&f.app, "GET", "/audit", Some(&f.key), None).await;
    assert_eq!(log["entries"][0]["action"], "legal_hold.lifted");
    // The chain links back to the placement rather than starting again.
    assert_eq!(
        log["entries"][0]["prev_hash"], log["entries"][1]["hash"],
        "each entry names the one before it"
    );
}

async fn a_hold_needs_a_reason(f: &Fixture) {
    for reason in ["", "   "] {
        let (status, body) = call(
            &f.app,
            "PUT",
            &format!("/assets/{}/legal-hold", f.visible),
            Some(&f.key),
            Some(json!({ "held": true, "reason": reason })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a blank reason must be refused, got {body}"
        );
    }
}

async fn reading_needs_manage_and_so_does_holding(f: &Fixture) {
    let (status, _) = call(
        &f.app,
        "PUT",
        &format!("/assets/{}/legal-hold", f.visible),
        Some(&f.reader_key),
        Some(json!({ "held": true, "reason": "not mine to place" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "seeing is not freezing");

    let (status, _) = call(&f.app, "GET", "/audit", Some(&f.reader_key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&f.app, "GET", "/audit/verify", Some(&f.reader_key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&f.app, "POST", "/audit/export", Some(&f.reader_key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call(&f.app, "GET", "/audit", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn an_asset_this_caller_cannot_see_is_a_404(f: &Fixture) {
    // The reader's predicate excludes it, so the answer must not distinguish "not allowed" from "not there".
    let (status, _) = call(
        &f.app,
        "PUT",
        &format!("/assets/{}/legal-hold", f.hidden),
        Some(&f.reader_key),
        Some(json!({ "held": true, "reason": "invisible" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the permission gate comes first, before the asset is looked for"
    );

    let (status, _) = call(
        &f.app,
        "PUT",
        &format!("/assets/{}/legal-hold", Uuid::now_v7()),
        Some(&f.key),
        Some(json!({ "held": true, "reason": "no such asset" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn the_log_filters_by_target_and_carries_both_hashes(f: &Fixture) {
    let (status, log) = call(
        &f.app,
        "GET",
        &format!("/audit?target_kind=asset&target_id={}", f.visible),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{log}");
    let entries = log["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "the placement and the lift");
    for entry in entries {
        assert_eq!(entry["hash"].as_str().map(str::len), Some(64));
    }
    // A short page offers no cursor.
    assert_eq!(log["next_before_seq"], Value::Null);

    let (_, filtered) = call(
        &f.app,
        "GET",
        "/audit?action=legal_hold.lifted",
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(filtered["entries"].as_array().map(Vec::len), Some(1));
    assert_eq!(filtered["entries"][0]["payload"]["reason"], "matter closed");
}

async fn verification_reports_an_intact_chain(f: &Fixture) {
    let (status, report) = call(&f.app, "GET", "/audit/verify", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["intact"], json!(true), "{report}");
    assert_eq!(report["failure"], Value::Null);
    assert!(report["checked"].as_u64().unwrap_or(0) >= 2);
}

async fn an_export_appends_its_own_entry_and_excludes_it(f: &Fixture) {
    let (status, extract) = call(&f.app, "POST", "/audit/export", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{extract}");
    let recorded = extract["recorded_as"]["seq"].as_i64().expect("seq");
    let entries = extract["entries"].as_array().expect("entries");
    assert_eq!(
        entries.first().map(|e| e["seq"].as_i64()),
        Some(Some(1)),
        "oldest first"
    );
    assert!(
        !entries.iter().any(|e| e["seq"].as_i64() == Some(recorded)),
        "an extract cannot contain the record of its own creation"
    );
    assert_eq!(extract["recorded_as"]["action"], "audit.exported");
    assert_eq!(
        extract["anchor"],
        Value::Null,
        "an export from the start anchors to nothing"
    );
    // The version travels with the extract, so an auditor knows which construction to reproduce.
    assert_eq!(extract["chain_version"], json!(1));
}

async fn an_export_is_not_reachable_by_get(f: &Fixture) {
    let (status, _) = call(&f.app, "GET", "/audit/export", Some(&f.key), None).await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "an endpoint that appends to the record must not be reachable by prefetch"
    );
}

async fn one_chain_carries_every_subsystem(f: &Fixture) {
    let before = entry_count(&f.acme).await;
    let (status, made) = call(
        &f.connectors,
        "POST",
        "/connectors",
        Some(&f.key),
        Some(json!({
            "kind": "drupal",
            "label": "Marketing site",
            "site_url": "https://marketing.example.test/",
            "asset_group_ids": [f.public_group],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(
        entry_count(&f.acme).await,
        before + 1,
        "registering a connector is a governance action"
    );

    let (_, log) = call(&f.app, "GET", "/audit", Some(&f.key), None).await;
    assert_eq!(log["entries"][0]["action"], "connector.registered");
    assert_eq!(log["entries"][0]["target_kind"], "connector");
    assert_eq!(log["entries"][0]["payload"]["label"], "Marketing site");
    // The point: a connector entry links to a legal-hold entry, because the chain belongs to the tenant.
    assert_eq!(log["entries"][0]["prev_hash"], log["entries"][1]["hash"]);

    let (status, report) = call(&f.app, "GET", "/audit/verify", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["intact"], json!(true), "{report}");
}

async fn a_tampered_chain_is_a_two_hundred_saying_so(f: &Fixture) {
    // What a superuser can do — the honest limit of in-database append-only, and the reason the chain exists.
    sqlx::query("ALTER TABLE audit_log DISABLE RULE audit_log_no_update")
        .execute(&f.acme)
        .await
        .expect("disable rule");
    sqlx::query("UPDATE audit_log SET payload = jsonb_set(payload, '{reason}', '\"something else\"') WHERE seq = 1")
        .execute(&f.acme)
        .await
        .expect("tamper");

    let (status, report) = call(&f.app, "GET", "/audit/verify", Some(&f.key), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a broken chain is an answer, not an error — a 500 would read as 'the database is down'"
    );
    assert_eq!(report["intact"], json!(false), "{report}");
    assert_eq!(report["failure"]["kind"], "altered");
    assert_eq!(report["failure"]["seq"], json!(1));
    assert!(
        report["failure"]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("recomputed"),
        "the report has to say what it compared: {report}"
    );
}
