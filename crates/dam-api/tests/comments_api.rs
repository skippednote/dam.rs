//! The comment endpoints (Q.6b).
//!
//! `dam_db`'s suite proves the two gates and the threading. What only exists here is the HTTP contract, and four
//! things that are decisions about the *interface*:
//!
//! - **Names are resolved server-side.** A thread showing uuids is unreadable, and one lookup per distinct person
//!   beats one request per comment.
//! - **`PATCH` does the words or the status, never both.** They carry different rights, so a request naming both
//!   would half-apply for a caller who holds one and not the other.
//! - **"Not yours" is 403 and only ever reachable for a comment the caller can already read.** Anything else
//!   would make the pair an existence oracle.
//! - **`/people` is scoped by the credential, not by a parameter.** There is nothing to point at another tenant
//!   with.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::comments::{CommentState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    acme: PgPool,
    /// The control-plane pool, so a case can retire an identity out from under a comment.
    global: PgPool,
    /// A tenant admin, with a person behind it.
    key: String,
    /// A second person, so private visibility is observable over HTTP too.
    other_key: String,
    /// A third, addressed to nothing.
    stranger_key: String,
    /// The people behind those keys, for asserting who a comment names.
    ada: Uuid,
    grace: Uuid,
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
    let stranger_key = person_key(&global, "acme", "mallory@example.com", &[], true).await;
    let machine_key = machine_key(&global, "acme").await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'visible', 'Visible') \
         RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
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

    let ada = identity_of(&global, "ada@example.com").await;
    let grace = identity_of(&global, "grace@example.com").await;

    let app = router(CommentState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        app,
        acme,
        global: global.clone(),
        key,
        other_key,
        stranger_key,
        machine_key,
        scoped_key,
        group,
        ada,
        grace,
    }
}

async fn identity_of(global: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM dam_global.identities WHERE email = $1")
        .bind(email)
        .fetch_one(global)
        .await
        .expect("identity")
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
async fn the_comment_http_contract_holds() {
    let f = fixture().await;
    let visible = asset(&f, "visible", true).await;
    let hidden = asset(&f, "hidden", false).await;

    a_thread_comes_back_with_names_rather_than_uuids(&f, visible).await;
    a_private_comment_names_its_recipients_and_reaches_nobody_else(&f, visible).await;
    a_patch_does_the_words_or_the_status_but_not_both(&f, visible).await;
    rewriting_somebody_elses_words_is_403_and_only_when_readable(&f, visible).await;
    the_people_list_is_scoped_by_the_credential(&f).await;
    a_key_with_no_person_cannot_comment(&f, visible).await;
    a_hidden_asset_is_404_for_comments_too(&f, hidden).await;
    the_refusals_say_what_is_wrong(&f, visible).await;
    deleting_is_204_and_takes_the_replies(&f, visible).await;
    a_comment_outlives_its_author_and_says_so(&f, visible).await;
}

async fn a_comment_outlives_its_author_and_says_so(f: &Fixture, visible: Uuid) {
    // Offboarding is the ordinary case, not an edge one. `asset_comments.author_id` has no foreign key into the
    // global schema — see the note in migration 0001 — so the comment survives the identity, which is right: the
    // words were said and deleting the person does not unsay them.
    let leaver = person_key(&f.global, "acme", "leaver@example.com", &[], true).await;
    let (status, posted) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &leaver,
        Some(json!({ "body": "Shipping this on Friday" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{posted}");
    let id = posted["id"].as_str().expect("id").to_owned();

    sqlx::query("DELETE FROM dam_global.identities WHERE email = 'leaver@example.com'")
        .execute(&f.global)
        .await
        .expect("delete identity");

    let (status, listed) = call(
        f,
        "GET",
        &format!("/assets/{visible}/comments"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let found = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|c| c["id"] == json!(id))
        .unwrap_or_else(|| panic!("the comment vanished with its author: {listed}"))
        .clone();
    assert_eq!(found["body"], json!("Shipping this on Friday"), "{found}");
    // Named as absent rather than left blank: an empty author reads as a rendering fault, and a reader needs to
    // know the difference between "nobody wrote this" and "whoever wrote it has gone".
    assert_eq!(
        found["author"]["name"],
        json!("Someone no longer here"),
        "{found}"
    );
}

async fn a_thread_comes_back_with_names_rather_than_uuids(f: &Fixture, visible: Uuid) {
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({ "body": "The crop is tight" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // Resolved server-side: a thread rendering `author_id` as a uuid is unreadable, and making the client look
    // each one up would be a request per distinct person on the page.
    assert_eq!(body["author"]["name"], json!("ada@example.com"), "{body}");
    assert_eq!(body["author"]["id"], json!(f.ada.to_string()), "{body}");
    assert_eq!(body["visibility"], json!("public"), "the default: {body}");
    assert_eq!(body["status"], json!("open"), "{body}");
    assert_eq!(
        body["status_by"],
        Value::Null,
        "nobody has moved it: {body}"
    );
    assert_eq!(body["edited_at"], Value::Null, "{body}");

    // A reply from somebody else, and the thread reads in order with both names.
    let parent = body["id"].as_str().expect("id").to_owned();
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.other_key,
        Some(json!({ "body": "Agreed, re-crop it", "parent_id": parent })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["parent_id"], json!(parent), "{body}");

    let (status, body) = call(
        f,
        "GET",
        &format!("/assets/{visible}/comments"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body
        .as_array()
        .expect("array")
        .iter()
        .map(|c| c["author"]["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        vec!["ada@example.com", "grace@example.com"],
        "{body}"
    );
}

async fn a_private_comment_names_its_recipients_and_reaches_nobody_else(
    f: &Fixture,
    visible: Uuid,
) {
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({
            "body": "Legal has not cleared this",
            "visibility": "private",
            "recipients": [f.grace.to_string()],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["visibility"], json!("private"), "{body}");
    // Recipients come back named, because a private note whose audience is a list of uuids cannot be checked by
    // the person writing it — and checking it is exactly what matters before hitting send.
    assert_eq!(
        body["recipients"][0]["name"],
        json!("grace@example.com"),
        "{body}"
    );
    let private_id = body["id"].as_str().expect("id").to_owned();

    // The recipient sees it.
    let (_, body) = call(
        f,
        "GET",
        &format!("/assets/{visible}/comments"),
        &f.other_key,
        None,
    )
    .await;
    assert!(
        body.as_array()
            .expect("array")
            .iter()
            .any(|c| c["id"] == json!(private_id)),
        "the recipient cannot read it: {body}"
    );

    // A third person with full access to the asset does not — which is the whole meaning of "private", and is
    // deliberately not qualified by any administrator override. See NEEDS-REVIEW.md.
    let (_, body) = call(
        f,
        "GET",
        &format!("/assets/{visible}/comments"),
        &f.stranger_key,
        None,
    )
    .await;
    assert!(
        !body
            .as_array()
            .expect("array")
            .iter()
            .any(|c| c["id"] == json!(private_id)),
        "a private comment reached somebody it was not addressed to: {body}"
    );
}

async fn a_patch_does_the_words_or_the_status_but_not_both(f: &Fixture, visible: Uuid) {
    let (_, posted) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({ "body": "Needs a wider crop" })),
    )
    .await;
    let id = posted["id"].as_str().expect("id").to_owned();

    // Both at once is refused rather than partially honoured: the words are the author's and the status is any
    // reader's, so a caller holding one right and not the other would get half of what they asked for.
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/comments/{id}"),
        &f.key,
        Some(json!({ "body": "changed", "status": "resolved" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("not both")),
        "{body}"
    );

    // Neither is refused too: an empty patch is a request that cannot be honoured or refused meaningfully.
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/comments/{id}"),
        &f.key,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // The words, by their author.
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/comments/{id}"),
        &f.key,
        Some(json!({ "body": "Needs a slightly wider crop" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["body"], json!("Needs a slightly wider crop"), "{body}");
    assert_ne!(body["edited_at"], Value::Null, "marked as edited: {body}");

    // The status, by somebody else — `approved` is another reader's verdict, so a status only its author could
    // move could never mean approval. And it records who.
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/comments/{id}"),
        &f.other_key,
        Some(json!({ "status": "approved" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("approved"), "{body}");
    assert_eq!(
        body["status_by"]["name"],
        json!("grace@example.com"),
        "an approval nobody owns is worth nothing: {body}"
    );
}

async fn rewriting_somebody_elses_words_is_403_and_only_when_readable(f: &Fixture, visible: Uuid) {
    let (_, posted) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({ "body": "Ada's words" })),
    )
    .await;
    let id = posted["id"].as_str().expect("id").to_owned();

    // Readable, so the refusal is about ownership: 403.
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/comments/{id}"),
        &f.other_key,
        Some(json!({ "body": "Grace's words" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Unreadable, so the refusal is about existence: 404, never 403. The other way round would confirm the id
    // exists and that somebody else owns it.
    let (_, private) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({
            "body": "Not for Mallory",
            "visibility": "private",
            "recipients": [f.grace.to_string()],
        })),
    )
    .await;
    let hidden_id = private["id"].as_str().expect("id").to_owned();
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/comments/{hidden_id}"),
        &f.stranger_key,
        Some(json!({ "body": "sneaky" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // Including for a status move, which any *reader* may do — and they are not a reader.
    let (status, _) = call(
        f,
        "PATCH",
        &format!("/comments/{hidden_id}"),
        &f.stranger_key,
        Some(json!({ "status": "resolved" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn the_people_list_is_scoped_by_the_credential(f: &Fixture) {
    let (status, body) = call(f, "GET", "/people", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let emails: Vec<&str> = body
        .as_array()
        .expect("array")
        .iter()
        .map(|p| p["email"].as_str().expect("email"))
        .collect();
    assert!(emails.contains(&"grace@example.com"), "{body}");
    // The email is present on purpose: two colleagues can share a display name, and a picker that cannot tell
    // them apart misroutes a private comment.
    assert!(
        body[0]["name"].as_str().is_some_and(|n| !n.is_empty()),
        "{body}"
    );

    // There is no parameter to point at another tenant with — the tenant comes from the credential. Asserted by
    // trying: a query string that looked like one must not change the answer.
    let (_, spoofed) = call(f, "GET", "/people?tenant=globex", &f.key, None).await;
    assert_eq!(spoofed, body, "the tenant is not a request parameter");
}

async fn a_key_with_no_person_cannot_comment(f: &Fixture, visible: Uuid) {
    // A comment is somebody's words. One posted by a service credential could never be edited, deleted or
    // attributed by anyone.
    for (method, path, payload) in [
        (
            "POST",
            format!("/assets/{visible}/comments"),
            Some(json!({ "body": "from a robot" })),
        ),
        ("GET", format!("/assets/{visible}/comments"), None),
    ] {
        let (status, body) = call(f, method, &path, &f.machine_key, payload).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }

    let orphans: i64 =
        sqlx::query_scalar("SELECT count(*) FROM asset_comments WHERE body = 'from a robot'")
            .fetch_one(&f.acme)
            .await
            .expect("count");
    assert_eq!(orphans, 0);
}

async fn a_hidden_asset_is_404_for_comments_too(f: &Fixture, hidden: Uuid) {
    let absent = Uuid::new_v4();
    for (label, id) in [("hidden", hidden), ("absent", absent)] {
        let (status, body) = call(
            f,
            "GET",
            &format!("/assets/{id}/comments"),
            &f.scoped_key,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{label}: {body}");

        let (status, _) = call(
            f,
            "POST",
            &format!("/assets/{id}/comments"),
            &f.scoped_key,
            Some(json!({ "body": "should not land" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{label}");
    }
}

async fn the_refusals_say_what_is_wrong(f: &Fixture, visible: Uuid) {
    let path = format!("/assets/{visible}/comments");

    // A private comment addressed to nobody.
    let (status, body) = call(
        f,
        "POST",
        &path,
        &f.key,
        Some(json!({ "body": "for nobody", "visibility": "private" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("recipient")),
        "{body}"
    );

    // An empty body.
    let (status, body) = call(f, "POST", &path, &f.key, Some(json!({ "body": "" }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A visibility nobody defined. Named rather than defaulted to public: silently widening a comment somebody
    // meant to keep private is the worst available outcome.
    let (status, body) = call(
        f,
        "POST",
        &path,
        &f.key,
        Some(json!({ "body": "x", "visibility": "secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .is_some_and(|r| r.contains("secret")),
        "{body}"
    );

    // A status nobody defined.
    let (_, posted) = call(f, "POST", &path, &f.key, Some(json!({ "body": "real" }))).await;
    let id = posted["id"].as_str().expect("id").to_owned();
    let (status, body) = call(
        f,
        "PATCH",
        &format!("/comments/{id}"),
        &f.key,
        Some(json!({ "status": "vibes" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["reason"].as_str().is_some_and(|r| r.contains("vibes")),
        "{body}"
    );
}

async fn deleting_is_204_and_takes_the_replies(f: &Fixture, visible: Uuid) {
    let (_, parent) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.key,
        Some(json!({ "body": "Which version shipped?" })),
    )
    .await;
    let parent_id = parent["id"].as_str().expect("id").to_owned();
    let (_, reply) = call(
        f,
        "POST",
        &format!("/assets/{visible}/comments"),
        &f.other_key,
        Some(json!({ "body": "The second", "parent_id": parent_id })),
    )
    .await;
    let reply_id = reply["id"].as_str().expect("id").to_owned();

    // Not the author.
    let (status, _) = call(
        f,
        "DELETE",
        &format!("/comments/{parent_id}"),
        &f.other_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call(f, "DELETE", &format!("/comments/{parent_id}"), &f.key, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The reply went with it: a reply to a question that no longer exists reads as corruption.
    let (_, listed) = call(
        f,
        "GET",
        &format!("/assets/{visible}/comments"),
        &f.key,
        None,
    )
    .await;
    let ids: Vec<&str> = listed
        .as_array()
        .expect("array")
        .iter()
        .map(|c| c["id"].as_str().expect("id"))
        .collect();
    assert!(!ids.contains(&parent_id.as_str()), "{listed}");
    assert!(!ids.contains(&reply_id.as_str()), "{listed}");
}
