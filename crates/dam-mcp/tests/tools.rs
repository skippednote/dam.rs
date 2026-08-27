//! The MCP tools, driven as an agent would drive them (M5d·2, §8.5).
//!
//! Real JSON-RPC over the real router, against a real database. What this suite exists to prove is one sentence
//! from §8.5 — "over the **same ABAC layer** as the REST API, so an external agent can never see more than the
//! acting user" — and every case below is an attempt to break it:
//!
//! - **A key with no scope for an asset gets "no such asset".** Not a refusal, not an empty field: the same
//!   collapse the REST layer makes, because the gap between the two answers is an existence oracle and an agent
//!   is exactly the caller that would map it.
//! - **Search returns only what the key may see**, and the count agrees with the rows.
//! - **A read-only key cannot mint a download**, and is told which of the two things is wrong.
//! - **Authorisation is per call.** A session initialised with a key that is then revoked stops working on the
//!   next call, not at the end of the session.
//! - **Every refusal is a tool error with a sentence**, because a protocol error tells an agent's client the
//!   server is broken, which is a different and usually false claim.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    app: axum::Router,
    /// A tenant admin: Read, Download and Manage over everything.
    key: String,
    /// `asset:read` only — no download.
    read_only_key: String,
    /// Read and download, but scoped to one asset group.
    scoped_key: String,
    /// In the scoped key's group.
    visible: Uuid,
    /// Outside it.
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

    // A group, a role scoped to it, and a key holding that role: the shape every access case here turns on.
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped', 'Scoped', '{asset:read,asset:download}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let scoped_identity = identity(&global, "sam@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{scoped}', false)",
    )
    .bind(tenant_id)
    .bind(scoped_identity)
    .execute(&global)
    .await
    .expect("membership");
    let scoped_key = issue(&global, tenant_id, Some(scoped_identity), &[]).await;

    let visible = asset(&acme, "harbour", Some(group)).await;
    let hidden = asset(&acme, "boardroom", None).await;

    // One index directory per fixture, so two tests running at once do not read each other's documents.
    let indexes = Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
        std::env::temp_dir().join(format!("damrs-mcp-{}", Uuid::now_v7())),
    )));
    // Built, because `search_assets` with no relational clause goes to the index — an empty one would make
    // every access assertion here pass for the wrong reason.
    let defs = dam_db::fields::load(&mut *acme.acquire().await.expect("conn"))
        .await
        .expect("defs");
    let schema = dam_search::IndexSchema::new(defs);
    let slug = dam_core::TenantSlug::new("acme").expect("slug");
    dam_search::reindex::tenant(&acme, &indexes, &slug, &schema, 500)
        .await
        .expect("reindex");

    let search_state = Arc::new(dam_api::search::SearchState {
        global: global.clone(),
        indexes,
        delivery: None,
    });
    // A real signer, so a download refusal is the *licence's* refusal rather than "this deployment cannot mint".
    // The distinction is the whole point of one of the cases below.
    let store: Arc<dyn dam_store::BlobStore> =
        Arc::new(dam_store::FakeS3Store::with_test_clock().0);
    let delivery = Arc::new(dam_api::delivery::DeliveryState::new(
        acme.clone(),
        acme.clone(),
        store,
        dam_core::signed_url::Keyring::single(
            "k1",
            dam_core::Secret::new("a-signing-key".to_owned()),
        ),
        tenant_id,
        dam_core::TenantSlug::new("acme").expect("a slug"),
    ));
    let download_state = Arc::new(dam_api::downloads::DownloadState {
        global: global.clone(),
        delivery: Some(delivery),
    });
    let app = dam_mcp::router(
        Arc::new(dam_mcp::McpState {
            search: search_state,
            downloads: download_state,
        }),
        None,
    );

    Fixture {
        _pg: pg,
        global,
        acme,
        app,
        key,
        read_only_key,
        scoped_key,
        visible,
        hidden,
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

async fn asset(pool: &PgPool, name: &str, group: Option<Uuid>) -> Uuid {
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
    if let Some(group) = group {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(group)
            .bind(id)
            .execute(pool)
            .await
            .expect("membership");
    }
    id
}

/// One JSON-RPC request over the MCP endpoint, as a client sends it.
async fn rpc(f: &Fixture, key: &str, id: i64, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        // Both required by the transport: it negotiates content types, and `Host` is validated against the
        // allowed list to stop DNS rebinding against a local server.
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header("mcp-protocol-version", "2025-06-18")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = f.app.clone().oneshot(request).await.expect("response");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the transport refused {method}"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // The transport may answer as SSE even with `json_response`; take the data line either way.
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix("data: ").or(Some(line)))
        .unwrap_or(&text);
    serde_json::from_str(json).unwrap_or_else(|error| panic!("not json ({error}): {text}"))
}

/// Calls one tool and returns the result object.
async fn call(f: &Fixture, key: &str, tool: &str, arguments: Value) -> Value {
    let answer = rpc(
        f,
        key,
        2,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    )
    .await;
    answer["result"].clone()
}

/// The text of a tool result, whatever shape it came in.
fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<&str>>()
                .join("")
        })
        .unwrap_or_default()
}

fn is_error(result: &Value) -> bool {
    result["isError"] == json!(true)
}

#[tokio::test]
async fn the_server_introduces_itself_with_the_two_rules_that_matter() {
    let f = fixture().await;
    let answer = rpc(
        &f,
        &f.key,
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"},
        }),
    )
    .await;

    let result = &answer["result"];
    assert_eq!(result["serverInfo"]["name"], "damrs");
    assert!(result["capabilities"]["tools"].is_object(), "{result}");
    let instructions = result["instructions"].as_str().expect("instructions");
    // The two rules an agent has to know before it asks anything: what it can see, and that seeing is not
    // permission.
    assert!(
        instructions.contains("only what the API key"),
        "{instructions}"
    );
    assert!(instructions.contains("check_rights"), "{instructions}");
}

#[tokio::test]
async fn the_tools_are_the_five_the_architecture_names() {
    let f = fixture().await;
    let answer = rpc(&f, &f.key, 1, "tools/list", json!({})).await;
    let names: Vec<&str> = answer["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "search_assets",
            "get_asset",
            "get_brand_guidelines",
            "check_rights",
            "get_download_url",
        ]
    );
}

#[tokio::test]
async fn an_agent_sees_only_what_its_key_may_see() {
    let f = fixture().await;

    // The admin sees both assets.
    let all = call(&f, &f.key, "search_assets", json!({})).await;
    assert!(!is_error(&all), "{all}");
    let structured = &all["structuredContent"];
    assert_eq!(structured["total"], 2);

    // The scoped key sees one, and the count agrees with the rows — §7's point: a count is a disclosure too.
    let mine = call(&f, &f.scoped_key, "search_assets", json!({})).await;
    assert_eq!(mine["structuredContent"]["total"], 1);
    let assets = mine["structuredContent"]["assets"]
        .as_array()
        .expect("assets");
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0]["asset_id"], f.visible.to_string());
}

#[tokio::test]
async fn an_asset_out_of_scope_does_not_exist() {
    let f = fixture().await;

    // In scope: the whole record.
    let mine = call(
        &f,
        &f.scoped_key,
        "get_asset",
        json!({"asset_id": f.visible.to_string()}),
    )
    .await;
    assert!(!is_error(&mine), "{mine}");
    assert_eq!(mine["structuredContent"]["filename"], "harbour.jpg");

    // Out of scope: not "forbidden", not an empty record — absent. The same answer a deleted asset gets, which
    // is what stops an agent using the difference to map the library.
    let theirs = call(
        &f,
        &f.scoped_key,
        "get_asset",
        json!({"asset_id": f.hidden.to_string()}),
    )
    .await;
    assert!(is_error(&theirs), "{theirs}");
    assert_eq!(
        text_of(&theirs),
        "no such asset, or not one this key may see"
    );

    // And an id that never existed gets the identical sentence.
    let nobody = call(
        &f,
        &f.scoped_key,
        "get_asset",
        json!({"asset_id": Uuid::now_v7().to_string()}),
    )
    .await;
    assert_eq!(text_of(&nobody), text_of(&theirs));
}

#[tokio::test]
async fn a_rights_question_is_also_gated_on_seeing_the_asset() {
    let f = fixture().await;
    // Otherwise "may I use asset X" becomes a way to ask "does asset X exist".
    let refused = call(
        &f,
        &f.scoped_key,
        "check_rights",
        json!({"asset_id": f.hidden.to_string()}),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    assert_eq!(
        text_of(&refused),
        "no such asset, or not one this key may see"
    );

    // For an asset it can see, the verdict comes back with its reasons. An unlicensed asset is refused, and
    // saying which clause refused it is the point of the tool.
    let verdict = call(
        &f,
        &f.scoped_key,
        "check_rights",
        json!({"asset_id": f.visible.to_string(), "channel": "web", "territory": "GB"}),
    )
    .await;
    assert!(!is_error(&verdict), "{verdict}");
    let structured = &verdict["structuredContent"];
    assert_eq!(structured["channel"], "web");
    assert_eq!(structured["territory"], "GB");
    assert_eq!(
        structured["may_distribute"], false,
        "no licence, no distribution"
    );
    assert!(
        structured["reasons"]
            .as_array()
            .is_some_and(|reasons| !reasons.is_empty()),
        "a refusal with no reason is not actionable: {structured}"
    );
}

#[tokio::test]
async fn a_download_of_an_invisible_asset_is_the_same_absence() {
    // The collapse again, on the path that reaches it through the REST handler's own `Failure` rather than
    // through this crate's inline check. Both have to say the same thing, or the difference between them is the
    // oracle: "forbidden" here and "no such asset" there tells an agent the asset exists.
    let f = fixture().await;
    let refused = call(
        &f,
        &f.scoped_key,
        "get_download_url",
        json!({"asset_id": f.hidden.to_string()}),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    assert_eq!(
        text_of(&refused),
        "no such asset, or not one this key may see"
    );
}

#[tokio::test]
async fn a_read_only_key_cannot_mint_a_download() {
    let f = fixture().await;
    let refused = call(
        &f,
        &f.read_only_key,
        "get_download_url",
        json!({"asset_id": f.visible.to_string()}),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    // Which of the two things is wrong: the key is fine and does not carry this.
    assert!(
        text_of(&refused).contains("does not carry the permission"),
        "{}",
        text_of(&refused)
    );

    // And the same key *can* search, so the refusal is about the action rather than the key being broken.
    let searched = call(&f, &f.read_only_key, "search_assets", json!({})).await;
    assert!(!is_error(&searched), "{searched}");
}

#[tokio::test]
async fn rights_refuse_a_download_that_the_permissions_would_allow() {
    let f = fixture().await;
    // The admin holds Download over everything, and the asset has no licence — so the refusal here is the
    // licence's, not the key's. Two different gates, and an agent needs to be able to tell them apart.
    let refused = call(
        &f,
        &f.key,
        "get_download_url",
        json!({"asset_id": f.visible.to_string(), "channel": "web", "territory": "GB"}),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    assert!(
        text_of(&refused).contains("rights refuse"),
        "{}",
        text_of(&refused)
    );
}

#[tokio::test]
async fn authorisation_is_per_call_rather_than_per_session() {
    let f = fixture().await;
    // A call that works.
    let before = call(&f, &f.scoped_key, "search_assets", json!({})).await;
    assert!(!is_error(&before), "{before}");

    // The key is revoked while the "session" continues.
    sqlx::query(
        "UPDATE dam_global.api_keys SET revoked_at = now() \
          WHERE identity_id = (SELECT id FROM dam_global.identities WHERE email = 'sam@example.com')",
    )
    .execute(&f.global)
    .await
    .expect("revoke");

    let after = call(&f, &f.scoped_key, "search_assets", json!({})).await;
    assert!(is_error(&after), "a revoked key kept working: {after}");
    assert!(
        text_of(&after).contains("not authenticated"),
        "{}",
        text_of(&after)
    );
}

#[tokio::test]
async fn a_call_with_no_credential_is_refused_rather_than_answered_anonymously() {
    let f = fixture().await;
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .body(Body::from(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "search_assets", "arguments": {}},
            })
            .to_string(),
        ))
        .expect("request");
    let response = f.app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(text.contains("not authenticated"), "{text}");
    // Nothing about the library leaked into the refusal.
    assert!(!text.contains("harbour.jpg"), "{text}");
}

#[tokio::test]
async fn the_guidance_and_the_vocabulary_are_what_an_agent_is_told_to_read_first() {
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("connection");
    dam_db::enrichment::save_settings(
        &mut conn,
        &dam_db::enrichment::Settings {
            guidance: "Say 'trainers', not 'sneakers'.".to_owned(),
            language: "British English".to_owned(),
            ..dam_db::enrichment::Settings::default()
        },
    )
    .await
    .expect("settings");

    // `kind` and `ai_taggable` are both stated rather than left to their defaults, because since 0034 the
    // vocabulary an agent is handed is the *governed* one: a taxonomy that is not a vocabulary, or one nobody
    // opened to machine use, is offered to nobody. That coupling is deliberate — this tool tells an agent which
    // words to use, and it must be the same closed set the zero-shot pass scores against, or the two would
    // disagree about the library's own language.
    let taxonomy: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind, ai_taggable) \
         VALUES (gen_random_uuid(), 'subject', 'Subject', 'vocabulary', true) RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("taxonomy");
    sqlx::query(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, path, slug, label, synonyms) \
         VALUES (gen_random_uuid(), $1, extensions.text2ltree('footwear'), 'footwear', 'Footwear', '{shoes}')",
    )
    .bind(taxonomy)
    .execute(&f.acme)
    .await
    .expect("term");

    let guidelines = call(&f, &f.scoped_key, "get_brand_guidelines", json!({})).await;
    assert!(!is_error(&guidelines), "{guidelines}");
    let structured = &guidelines["structuredContent"];
    assert_eq!(structured["guidance"], "Say 'trainers', not 'sneakers'.");
    assert_eq!(structured["language"], "British English");
    let vocabulary = structured["vocabulary"].as_array().expect("vocabulary");
    assert_eq!(vocabulary[0]["slug"], "footwear");
    assert_eq!(vocabulary[0]["synonyms"][0], "shoes");
    assert_eq!(structured["vocabulary_truncated"], false);
}

#[tokio::test]
async fn a_malformed_argument_is_explained_rather_than_crashed_on() {
    let f = fixture().await;
    let refused = call(&f, &f.key, "get_asset", json!({"asset_id": "not-a-uuid"})).await;
    assert!(is_error(&refused), "{refused}");
    assert!(
        text_of(&refused).contains("not an asset id"),
        "{}",
        text_of(&refused)
    );

    let missing = call(&f, &f.key, "get_asset", json!({})).await;
    assert!(is_error(&missing), "{missing}");
    assert!(
        text_of(&missing).contains("asset_id is required"),
        "{}",
        text_of(&missing)
    );

    // A tool that does not exist is a *protocol* error: the client asked for something not in `tools/list`.
    let answer = rpc(
        &f,
        &f.key,
        3,
        "tools/call",
        json!({"name": "delete_everything", "arguments": {}}),
    )
    .await;
    assert!(answer["error"].is_object(), "{answer}");
    assert_eq!(answer["error"]["code"], -32602);
}

#[tokio::test]
async fn a_query_that_does_not_parse_says_where() {
    let f = fixture().await;
    let refused = call(
        &f,
        &f.key,
        "search_assets",
        json!({"query": "campaign:spring"}),
    )
    .await;
    assert!(is_error(&refused), "{refused}");
    let text = text_of(&refused);
    assert!(text.contains("does not parse"), "{text}");
    // The column, so an agent can fix the query rather than guess at it.
    assert!(text.contains("at character"), "{text}");
}
