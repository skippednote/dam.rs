//! Vocabulary administration over HTTP (Q.20b).
//!
//! `dam_db` proves the gate, the paths and the lifecycle. What lives only here are five decisions about the
//! interface:
//!
//! - **Manage throughout.** Unlike categories, where reading the tree is `Read` because nobody can navigate a
//!   library without it. What this surface exposes is the machinery — thresholds, precision, retirement, and
//!   which vocabularies a model may draw on.
//! - **Opening a vocabulary to a model is its own endpoint.** Not a field on an update body: it decides what
//!   an LLM is told about a customer's library, and it must not be changeable while editing a label.
//! - **There is no delete.** `asset_tags` cascades, so deleting a term untags every asset that carried it.
//!   Retire, or merge into the term that took over the meaning.
//! - **The term id in the URL is checked against the vocabulary in the URL.** Without it the path segment
//!   would be decoration, and a caller who guessed a term id would learn it exists.
//! - **The threshold comes back as stored.** It is clamped, so echoing what was sent would hide that 1.5
//!   became 1.0.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::vocabularies::{VocabularyState, router};
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
    key: String,
    read_only_key: String,
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
    let admin: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), 'ada@example.com', 'Ada') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("identity");
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
    let app = router(VocabularyState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        global,
        acme,
        app,
        key,
        read_only_key,
    }
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

/// Creates a vocabulary and returns its id.
async fn vocabulary(f: &Fixture, key: &str) -> String {
    let (status, made) = call(
        f,
        "POST",
        "/vocabularies",
        Some(&f.key),
        Some(json!({ "key": key, "label": key })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    made["id"].as_str().expect("id").to_owned()
}

/// Adds a term and returns its id.
async fn term(f: &Fixture, vocab: &str, slug: &str, synonyms: Value) -> String {
    let (status, made) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms"),
        Some(&f.key),
        Some(json!({ "slug": slug, "label": slug, "synonyms": synonyms })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    made["id"].as_str().expect("id").to_owned()
}

async fn administration_needs_manage(f: &Fixture) {
    let (status, _) = call(f, "GET", "/vocabularies", Some(&f.read_only_key), None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "thresholds and machine-tagging are administration, and a read-only key holds none of it"
    );
    let (status, _) = call(f, "GET", "/vocabularies", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn a_new_vocabulary_is_closed_to_machine_tagging(f: &Fixture) {
    let id = vocabulary(f, "moods").await;
    let (_, listed) = call(f, "GET", "/vocabularies", Some(&f.key), None).await;
    let made = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|one| one["id"] == id)
        .expect("listed");
    // The governed default: a vocabulary created five seconds ago has not been reviewed for machine use.
    assert_eq!(made["ai_taggable"], false);
    assert_eq!(made["term_count"], 0);

    let (status, opened) = call(
        f,
        "POST",
        &format!("/vocabularies/{id}/ai"),
        Some(&f.key),
        Some(json!({ "ai_taggable": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    assert_eq!(opened["ai_taggable"], true);

    // A category tree is not a vocabulary, and asking by id gets a 404 rather than becoming one. Reached
    // through this endpoint because it is the one that would matter: opening a browse hierarchy to an LLM.
    let tree: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'subject', 'Subject', 'category') RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("tree");
    let (status, _) = call(
        f,
        "POST",
        &format!("/vocabularies/{tree}/ai"),
        Some(&f.key),
        Some(json!({ "ai_taggable": true })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // And it is not in the list either, so nobody can be invited to try.
    let (_, listed) = call(f, "GET", "/vocabularies", Some(&f.key), None).await;
    assert!(
        !listed
            .as_array()
            .expect("array")
            .iter()
            .any(|one| one["key"] == "subject")
    );
}

async fn a_taken_key_is_a_conflict_that_does_not_say_by_what(f: &Fixture) {
    vocabulary(f, "twice").await;
    let (status, refused) = call(
        f,
        "POST",
        "/vocabularies",
        Some(&f.key),
        Some(json!({ "key": "twice", "label": "Again" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    let reason = refused["reason"].as_str().expect("reason");
    assert!(reason.contains("twice"), "{reason}");
    // `taxonomies.key` is shared with the category trees, so the refusal says the key is taken without saying
    // what took it: "there is already a category tree called twice" is a small existence oracle over a
    // surface this caller may not administer.
    assert!(
        !reason.contains("category") && !reason.contains("tree"),
        "the refusal must not disclose what kind of taxonomy took the key: {reason}"
    );
}

async fn synonyms_are_tidied_before_they_cost_prompt_bytes(f: &Fixture) {
    let vocab = vocabulary(f, "weather").await;
    let (status, made) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms"),
        Some(&f.key),
        // What a hand-typed list looks like: blanks, padding, and a repeat in different case.
        Some(json!({
            "slug": "overcast",
            "label": "Overcast",
            "synonyms": ["cloudy", "  grey  ", "", "  ", "Cloudy"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    // Trimmed, emptied and de-duplicated case-insensitively: they cost bytes on every enrichment call, and
    // `dam_ai::enrich` already matches without regard to case, so "Cloudy" beside "cloudy" widens nothing.
    assert_eq!(made["synonyms"], json!(["cloudy", "grey"]));
    assert_eq!(made["slug"], "overcast");
    assert!(
        made["ai_precision"].is_null(),
        "nobody has reviewed one yet"
    );
    assert_eq!(made["asset_count"], 0);
}

async fn the_threshold_comes_back_as_stored(f: &Fixture) {
    let vocab = vocabulary(f, "clamped").await;
    let id = term(f, &vocab, "impossible", json!([])).await;

    let (status, amended) = call(
        f,
        "PATCH",
        &format!("/vocabularies/{vocab}/terms/{id}"),
        Some(&f.key),
        // A typo that expresses a real intention: "never auto-apply".
        Some(json!({ "label": "Impossible", "ai_threshold": 1.5 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{amended}");
    // Read back, not echoed. A screen showing 1.5 would not tell the operator what actually governs.
    assert_eq!(amended["ai_threshold"], 1.0);
    // And the slug is not a parameter at all, so it cannot be moved by an amend.
    assert_eq!(amended["slug"], "impossible");
}

async fn a_term_belongs_to_the_vocabulary_in_its_url(f: &Fixture) {
    let left = vocabulary(f, "left").await;
    let right = vocabulary(f, "right").await;
    let stranger = term(f, &right, "stranger", json!([])).await;

    // Without the ownership check the path segment would be decoration: the amend would land on right's term
    // and answer 200, and a caller who guessed an id would learn it exists.
    let (status, _) = call(
        f,
        "PATCH",
        &format!("/vocabularies/{left}/terms/{stranger}"),
        Some(&f.key),
        Some(json!({ "label": "Hijacked", "ai_threshold": 0.5 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And it really was not changed.
    let (_, listed) = call(
        f,
        "GET",
        &format!("/vocabularies/{right}/terms"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(listed[0]["label"], "stranger");
}

async fn retiring_keeps_the_assets_and_the_id(f: &Fixture) {
    let vocab = vocabulary(f, "colours").await;
    let parent = term(f, &vocab, "warm", json!([])).await;
    let (status, child) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms"),
        Some(&f.key),
        Some(json!({ "slug": "amber", "label": "Amber", "parent_id": parent })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{child}");
    assert_eq!(
        child["path"], "warm.amber",
        "the path comes from the parent"
    );

    // A live child blocks retirement, because retiring the parent would leave an assignable term under a
    // branch no picker offers — and the refusal says how many rather than cascading silently.
    let (status, refused) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms/{parent}/retire"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .expect("reason")
            .contains("live child"),
        "{refused}"
    );

    let child_id = child["id"].as_str().expect("id").to_owned();
    let (status, retired) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms/{child_id}/retire"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retired}");
    assert!(retired["deprecated_at"].is_string());
    // Retiring a retired term is what a retried request looks like, so it succeeds rather than 409ing.
    let (status, _) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms/{child_id}/retire"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Still listed, because an administrator has to be able to see what was retired and where it went.
    let (_, listed) = call(
        f,
        "GET",
        &format!("/vocabularies/{vocab}/terms"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(listed.as_array().expect("array").len(), 2);
}

async fn merging_moves_the_meaning_and_says_where(f: &Fixture) {
    let vocab = vocabulary(f, "merging").await;
    let from = term(f, &vocab, "lorry", json!([])).await;
    let into = term(f, &vocab, "truck", json!([])).await;

    let (status, merged) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms/{from}/merge"),
        Some(&f.key),
        Some(json!({ "into": into })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{merged}");
    // Retired *and* pointing at the survivor. The pointer is the difference between a retired term and a lost
    // one: every id stored outside this database — a saved search, a Drupal field — still resolves.
    assert!(merged["deprecated_at"].is_string());
    assert_eq!(merged["superseded_by"], into);

    // Merging into the retired term now would close a loop, and the refusal says so specifically rather than
    // reporting the generic "the target is deprecated".
    let (status, refused) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms/{into}/merge"),
        Some(&f.key),
        Some(json!({ "into": from })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"].as_str().expect("reason").contains("loop"),
        "{refused}"
    );

    // Across vocabularies is refused too: it would change what an asset means, not which term carries it.
    let elsewhere = vocabulary(f, "elsewhere").await;
    let other = term(f, &elsewhere, "van", json!([])).await;
    let (status, refused) = call(
        f,
        "POST",
        &format!("/vocabularies/{vocab}/terms/{into}/merge"),
        Some(&f.key),
        Some(json!({ "into": other })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert!(
        refused["reason"]
            .as_str()
            .expect("reason")
            .contains("different taxonomies"),
        "{refused}"
    );
}

async fn there_is_no_way_to_delete_a_term(f: &Fixture) {
    let vocab = vocabulary(f, "permanent").await;
    let id = term(f, &vocab, "kept", json!([])).await;
    // Not an omission. `asset_tags` cascades, so a delete untags every asset that carried the term — years of
    // somebody's work, gone quietly, discovered when a search comes back empty. Retire or merge instead.
    let (status, _) = call(
        f,
        "DELETE",
        &format!("/vocabularies/{vocab}/terms/{id}"),
        Some(&f.key),
        None,
    )
    .await;
    // 405, because the path exists and PATCH is what it accepts.
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // 404 rather than 405 for the vocabulary itself, and the difference is real: there is no
    // `/vocabularies/{id}` route at all. Nothing needs one — a vocabulary is described by its list entry and
    // changed through `/ai`, and the thing somebody would reach for a route like this to do is delete it.
    let (status, _) = call(
        f,
        "DELETE",
        &format!("/vocabularies/{vocab}"),
        Some(&f.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_vocabulary_contract_holds() {
    let f = fixture().await;

    administration_needs_manage(&f).await;
    a_new_vocabulary_is_closed_to_machine_tagging(&f).await;
    a_taken_key_is_a_conflict_that_does_not_say_by_what(&f).await;
    synonyms_are_tidied_before_they_cost_prompt_bytes(&f).await;
    the_threshold_comes_back_as_stored(&f).await;
    a_term_belongs_to_the_vocabulary_in_its_url(&f).await;
    retiring_keeps_the_assets_and_the_id(&f).await;
    merging_moves_the_meaning_and_says_where(&f).await;
    there_is_no_way_to_delete_a_term(&f).await;

    assert!(!f.global.is_closed());
}
