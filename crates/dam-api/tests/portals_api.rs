//! Portals: the endpoints, and what a visitor with no account can reach (Q.14).
//!
//! `dam_db` proves the storage and the one-source rule. What exists only here is the HTTP contract, and five
//! decisions about it:
//!
//! - **A portal adds no access mechanism.** Every visit resolves the portal's share link, so a revoked, expired
//!   or exhausted portal refuses in the share machinery's own words — the same words an asset link uses.
//! - **The slug works only when the portal is public.** A slug is guessable and a token is not; that column is
//!   the whole difference between a press kit and an unreleased campaign.
//! - **Retiring revokes.** A URL that was handed out stops working, and both halves stop together.
//! - **Presentation can change; the set cannot.** A portal that swapped its collection would show a different
//!   library to everyone holding the old link.
//! - **A live-query source shows only published assets**, so the query narrows a set a person admitted rather
//!   than defining one. See `DECISIONS.md` on why publication is a per-asset act.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::portals::{PortalState, router};
use dam_core::Secret;
use dam_core::signed_url::Keyring;
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    // Held so the state's own pool is not the only handle on the global schema.
    _global: PgPool,
    acme: PgPool,
    app: axum::Router,
    key: String,
    read_only_key: String,
    collection: Uuid,
    photo: Uuid,
    clip: Uuid,
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
    let identity: Uuid = sqlx::query_scalar(
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
    .bind(identity)
    .execute(&global)
    .await
    .expect("membership");
    let key = issue(&global, tenant_id, Some(identity), &[]).await;
    let read_only_key = issue(&global, tenant_id, Some(identity), &["asset:read"]).await;

    let collection: Uuid = sqlx::query_scalar(
        "INSERT INTO collections (id, key, label) \
         VALUES (gen_random_uuid(), 'press', 'Press kit') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("collection");
    let photo = asset(&acme, "harbour", "image/jpeg").await;
    let clip = asset(&acme, "advert", "video/mp4").await;
    for (position, asset_id) in [photo, clip].into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO collection_items (collection_id, asset_id, position) VALUES ($1, $2, $3)",
        )
        .bind(collection)
        .bind(asset_id)
        .bind(i32::try_from(position).expect("small"))
        .execute(&acme)
        .await
        .expect("member");
    }
    // An asset outside the collection: a portal must not show it, whatever else is true.
    asset(&acme, "boardroom", "image/jpeg").await;

    let delivery = Arc::new(dam_api::delivery::DeliveryState::new(
        acme.clone(),
        acme.clone(),
        Arc::new(dam_store::FakeS3Store::with_test_clock().0),
        Keyring::single("k1", Secret::new("a-signing-key".to_owned())),
        tenant_id,
        dam_core::TenantSlug::new("acme").expect("a slug"),
    ));
    let app = router(PortalState {
        global: global.clone(),
        delivery,
    });

    Fixture {
        _pg: pg,
        _global: global,
        acme,
        app,
        key,
        read_only_key,
        collection,
        photo,
        clip,
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

async fn asset(pool: &PgPool, name: &str, mime: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, $4, 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.file"))
    .bind(mime)
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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn new_portal(f: &Fixture, overrides: Value) -> Value {
    let mut body = json!({
        "key": "press-kit",
        "title": "Acme press kit",
        "intro": "Everything a journalist needs.",
        "kind": "standard",
        "collection_id": f.collection.to_string(),
        "is_public": true,
        "allow_search": true,
    });
    if let (Some(base), Some(extra)) = (body.as_object_mut(), overrides.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    body
}

/// Creates a portal and returns `(id, token)`.
async fn create(f: &Fixture, overrides: Value) -> (String, String) {
    let (status, created) = call(
        f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(f, overrides)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["portal"]["id"].as_str().expect("id").to_owned();
    let url = created["url"].as_str().expect("url").to_owned();
    let token = url.rsplit('/').next().expect("token").to_owned();
    (id, token)
}

#[tokio::test]
async fn a_portal_is_created_with_one_readable_token_and_a_public_address() {
    let f = fixture().await;
    let (status, created) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(&f, json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["portal"]["key"], "press-kit");
    assert_eq!(created["portal"]["reachable"], true);
    // Both addresses, because a public portal has two and a private one has one.
    assert!(created["url"].as_str().expect("url").contains("/share/"));
    assert!(
        created["public_url"]
            .as_str()
            .expect("public")
            .ends_with("/portal/press-kit")
    );

    // Stored as a digest, so the response is the only copy of the token.
    let digests: Vec<String> =
        sqlx::query_scalar("SELECT token FROM share_links WHERE kind = 'portal'")
            .fetch_all(&f.acme)
            .await
            .expect("shares");
    assert_eq!(digests.len(), 1);
    let token = created["url"]
        .as_str()
        .expect("url")
        .rsplit('/')
        .next()
        .expect("token");
    assert!(
        !digests[0].contains(token),
        "the token was stored in the clear"
    );
}

#[tokio::test]
async fn a_private_portal_has_no_public_address() {
    let f = fixture().await;
    let (_, created) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(&f, json!({"is_public": false}))),
    )
    .await;
    // `null` rather than a URL that would 404: a link that does not work is worse than no link.
    assert!(created["public_url"].is_null(), "{created}");

    // And the slug does not resolve — not a 403, which would confirm the name.
    let (status, body) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["reason"]
            .as_str()
            .expect("reason")
            .contains("no portal at that address"),
        "{body}"
    );

    // The token still works, because that is the point of a private portal.
    let token = created["url"]
        .as_str()
        .expect("url")
        .rsplit('/')
        .next()
        .expect("token");
    let (status, page) = call(
        &f,
        "POST",
        &format!("/share/{token}/portal"),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["title"], "Acme press kit");
}

#[tokio::test]
async fn a_portal_shows_its_collection_and_nothing_else() {
    let f = fixture().await;
    create(&f, json!({})).await;
    let (status, page) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");

    assert_eq!(
        page["total"], 2,
        "the collection holds two; the library holds three"
    );
    let ids: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["asset_id"].as_str())
        .collect();
    assert!(ids.contains(&f.photo.to_string().as_str()));
    assert!(ids.contains(&f.clip.to_string().as_str()));
    assert_eq!(ids.len(), 2);

    // No item has a preview, and each says why per item rather than being omitted — a portal that hid its
    // undeliverable assets would look like a smaller collection than the one somebody published.
    //
    // The reason is the *licence*, not the missing rendition, and the order is deliberate: rights are evaluated
    // before deliverability, so a portal never tells a visitor "no preview yet" about an asset it would refuse
    // even once one existed. That is the more honest of the two sentences.
    let first = &page["items"][0];
    assert!(first["preview_url"].is_null(), "{first}");
    assert!(
        first["preview_unavailable"]
            .as_str()
            .expect("reason")
            .contains("not licensed"),
        "{first}"
    );
}

#[tokio::test]
async fn a_portal_shows_neither_old_versions_nor_paperwork() {
    // `LIBRARY_ROWS`, the same rule the grid and search follow. A portal listing three versions of one
    // photograph, or a model release beside the photograph it belongs to, is a portal nobody would send to a
    // client — and both rows are in `collection_items` legitimately, because a version inherits its group's
    // membership and an attachment is added alongside its parent.
    let f = fixture().await;

    let superseded = asset(&f.acme, "older", "image/jpeg").await;
    // `version_no` too: the group's uniqueness index covers `(version_group_id, version_no)`, so joining a row
    // to a group without renumbering it collides with the current version rather than sitting behind it.
    sqlx::query(
        "UPDATE assets SET is_current = false, version_group_id = $2, version_no = 2 WHERE id = $1",
    )
    .bind(superseded)
    .bind(f.photo)
    .execute(&f.acme)
    .await
    .expect("supersede");
    let release = asset(&f.acme, "model-release", "application/pdf").await;
    // Both columns or neither, per `assets_attachment_complete`: an attachment with no kind is a row nobody can
    // render a label for.
    sqlx::query("UPDATE assets SET attached_to = $2, attachment_kind = 'release' WHERE id = $1")
        .bind(release)
        .bind(f.photo)
        .execute(&f.acme)
        .await
        .expect("attach");
    for (position, extra) in [superseded, release].into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO collection_items (collection_id, asset_id, position) VALUES ($1, $2, $3)",
        )
        .bind(f.collection)
        .bind(extra)
        .bind(i32::try_from(position).expect("small") + 2)
        .execute(&f.acme)
        .await
        .expect("member");
    }

    create(&f, json!({})).await;
    let (status, page) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    // Four members, two rows: the count and the list agree, which is the other half of this — a total of four
    // over a list of two is how a portal tells a visitor something is missing.
    assert_eq!(page["total"], 2, "{page}");
    let ids: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["asset_id"].as_str())
        .collect();
    assert!(!ids.contains(&superseded.to_string().as_str()), "{page}");
    assert!(!ids.contains(&release.to_string().as_str()), "{page}");
}

#[tokio::test]
async fn a_video_portal_shows_only_video() {
    // The kind is presentation, and for `video` that presentation includes not showing stills: a video portal
    // full of photographs is a video portal in name only.
    let f = fixture().await;
    create(&f, json!({"kind": "video"})).await;
    let (status, page) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["kind"], "video");
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["asset_id"], f.clip.to_string());
}

#[tokio::test]
async fn searching_inside_a_portal_narrows_and_cannot_reach_outside_it() {
    let f = fixture().await;
    create(&f, json!({})).await;

    let (_, page) = call(&f, "GET", "/portal/press-kit?q=harbour", None, None).await;
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["asset_id"], f.photo.to_string());
    assert_eq!(page["query"], "harbour");

    // The asset outside the collection is not reachable by naming it: the set is the outer bound.
    let (_, page) = call(&f, "GET", "/portal/press-kit?q=boardroom", None, None).await;
    assert_eq!(page["total"], 0, "{page}");
    assert_eq!(page["items"].as_array().expect("items").len(), 0);

    // And a portal with searching off ignores the term rather than half-applying it.
    create(&f, json!({"key": "closed", "allow_search": false})).await;
    let (_, page) = call(&f, "GET", "/portal/closed?q=harbour", None, None).await;
    assert_eq!(page["total"], 2);
    assert!(page["query"].is_null(), "{page}");
}

#[tokio::test]
async fn a_passcode_is_required_before_anything_is_listed() {
    let f = fixture().await;
    create(&f, json!({"passcode": "open sesame"})).await;

    let (status, body) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // Nothing about the set leaked into the refusal.
    assert!(!body.to_string().contains("harbour"), "{body}");

    let (status, body) = call(&f, "GET", "/portal/press-kit?passcode=wrong", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let (status, page) = call(
        &f,
        "GET",
        "/portal/press-kit?passcode=open%20sesame",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 2);
}

#[tokio::test]
async fn retiring_a_portal_stops_both_addresses() {
    let f = fixture().await;
    let (id, token) = create(&f, json!({})).await;

    let (status, retired) = call(&f, "DELETE", &format!("/portals/{id}"), Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{retired}");
    assert!(retired["retired_at"].is_string());
    assert_eq!(retired["reachable"], false, "the link went with it");

    // The slug, and the token: both dead, because retiring one half and leaving the other live is the failure
    // this pairing exists to prevent.
    let (status, _) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = call(
        &f,
        "POST",
        &format!("/share/{token}/portal"),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["reason"].as_str().expect("reason").contains("revoked"),
        "the share machinery's own words: {body}"
    );

    // And it is still listed, because somebody has to be able to see what they retired.
    let (_, listed) = call(&f, "GET", "/portals", Some(&f.key), None).await;
    let rows = listed.as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["retired_at"].is_string());

    // Editing it is refused too, and the same way an unknown one is: a retired portal is not a thing to edit,
    // and saying which of the two it was would invite un-retiring by editing.
    let (status, body) = call(
        &f,
        "PATCH",
        &format!("/portals/{id}"),
        Some(&f.key),
        Some(json!({
            "title": "Back from the dead",
            "kind": "standard",
            "accent": "#2563eb",
            "is_public": true,
            "allow_search": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // The second gate, reachable only with a hand-edited database: un-revoke the link and the token still
    // refuses, because the portal itself is checked and not only the share. Retiring revokes both halves
    // together, so this is defence in depth — and defence in depth nobody tests is decoration.
    sqlx::query("UPDATE share_links SET revoked_at = NULL WHERE kind = 'portal'")
        .execute(&f.acme)
        .await
        .expect("un-revoke");
    let (status, body) = call(
        &f,
        "POST",
        &format!("/share/{token}/portal"),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .expect("reason")
            .contains("no longer available"),
        "{body}"
    );
}

#[tokio::test]
async fn presentation_can_change_and_the_set_cannot() {
    let f = fixture().await;
    let (id, _) = create(&f, json!({})).await;

    let (status, updated) = call(
        &f,
        "PATCH",
        &format!("/portals/{id}"),
        Some(&f.key),
        Some(json!({
            "title": "Acme newsroom",
            "intro": "Updated.",
            "kind": "brand",
            "logo_asset_id": f.photo.to_string(),
            "accent": "#ff6600",
            "is_public": false,
            "allow_search": false,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["title"], "Acme newsroom");
    assert_eq!(updated["kind"], "brand");
    assert_eq!(updated["accent"], "#ff6600");
    assert_eq!(updated["is_public"], false);
    // The collection is untouched, and there is no field in the request that could have changed it: a portal
    // that swapped its set would show a different library to everyone holding the old URL.
    assert_eq!(updated["collection_id"], f.collection.to_string());

    // Made private, so the slug stops resolving — the same column, taking effect immediately.
    let (status, _) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_nonsense_portal_is_refused_with_the_rule_it_broke() {
    let f = fixture().await;

    // A slug the URL shape refuses.
    let (status, body) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(&f, json!({"key": "Not A Slug"}))),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A kind this build does not have — named, so somebody can see which four exist.
    let (status, body) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(&f, json!({"kind": "microsite"}))),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.to_string()
            .contains("standard, brand, video or channel"),
        "{body}"
    );

    // A collection that does not exist: 404, so a guessed id does not confirm anything.
    let (status, _) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(
            &f,
            json!({"collection_id": Uuid::now_v7().to_string()}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A name already taken: 409, because the fix is a different name rather than a different value.
    create(&f, json!({})).await;
    let (status, body) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(&f, json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_live_query_portal_publishes_only_what_somebody_published() {
    // The decision behind `assets.published_at`. A portal backed by a live query would otherwise publish every
    // future asset that happens to match it — nobody decides, a rule does. So the query narrows an explicitly
    // published set rather than defining one, and an asset nobody published is not on the page whatever it
    // matches.
    let f = fixture().await;

    // A saved search over the whole library, and one of the three assets published.
    let search: Uuid = sqlx::query_scalar(
        "INSERT INTO saved_searches (id, name, query, owner_id) \
         VALUES (gen_random_uuid(), 'everything', '{\"kind\":\"all\"}'::jsonb, NULL) RETURNING id",
    )
    .fetch_one(&f.acme)
    .await
    .expect("saved search");
    sqlx::query("UPDATE assets SET published_at = now() WHERE id = $1")
        .bind(f.photo)
        .execute(&f.acme)
        .await
        .expect("publish");

    let (status, created) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(
            &f,
            json!({"collection_id": Value::Null, "saved_search_id": search.to_string()}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, page) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["total"], 1,
        "the search matches the whole library and one asset is published: {page}"
    );
    assert_eq!(page["items"][0]["asset_id"], f.photo.to_string(), "{page}");

    // Publishing a second asset changes the page with no edit to the portal — which is the point of a live
    // source, and is safe precisely because publication is the act that admits it.
    sqlx::query("UPDATE assets SET published_at = now() WHERE id = $1")
        .bind(f.clip)
        .execute(&f.acme)
        .await
        .expect("publish");
    let (_, page) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(page["total"], 2, "{page}");

    // And unpublishing removes it again.
    sqlx::query("UPDATE assets SET published_at = NULL WHERE id = $1")
        .bind(f.clip)
        .execute(&f.acme)
        .await
        .expect("unpublish");
    let (_, page) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(page["total"], 1, "{page}");
}

#[tokio::test]
async fn a_media_class_portal_is_every_published_asset_of_that_class() {
    let f = fixture().await;
    // The video and the photo are published; the third asset is not. Publishing everything would make this
    // case pass whether or not the publication gate applied at all.
    sqlx::query("UPDATE assets SET published_at = now() WHERE id = ANY($1)")
        .bind(vec![f.clip, f.photo])
        .execute(&f.acme)
        .await
        .expect("publish");
    let unpublished_clip = asset(&f.acme, "unpublished-reel", "video/mp4").await;

    let (status, created) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(
            &f,
            json!({"collection_id": Value::Null, "media_class": "video", "kind": "channel"}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    let (status, page) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["total"], 1,
        "one published video, and a second video nobody published: {page}"
    );
    assert_eq!(page["items"][0]["asset_id"], f.clip.to_string(), "{page}");
    let listed: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["asset_id"].as_str())
        .collect();
    assert!(
        !listed.contains(&unpublished_clip.to_string().as_str()),
        "an unpublished video reached a public page: {page}"
    );

    // A class the schema does not recognise is refused rather than rendered as an empty page nobody can
    // explain.
    let (status, body) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(
            &f,
            json!({"key": "typo", "collection_id": Value::Null, "media_class": "vidoe"}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("media class"), "{body}");
}

#[tokio::test]
async fn a_portal_needs_exactly_one_source() {
    let f = fixture().await;
    // Two sources at once, and none at all: both are the same mistake, and neither picks one silently.
    for source in [
        json!({"media_class": "video"}),
        json!({"collection_id": Value::Null}),
    ] {
        let (status, body) = call(
            &f,
            "POST",
            "/portals",
            Some(&f.key),
            Some(new_portal(&f, source)),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.to_string().contains("exactly one"), "{body}");
    }

    // A source pointing at nothing is a 404 rather than a portal over an empty set: a portal created over a
    // guessed id is the one mistake in this feature that publishes the wrong assets.
    let (status, body) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(
            &f,
            json!({"collection_id": Value::Null, "saved_search_id": Uuid::now_v7().to_string()}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn publishing_part_of_the_library_needs_manage() {
    let f = fixture().await;
    // The widest editorial act in the system: a portal is visible to people with no account at all.
    let (status, _) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.read_only_key),
        Some(new_portal(&f, json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&f, "GET", "/portals", Some(&f.read_only_key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_expired_portal_says_so_in_the_share_machinery_s_words() {
    let f = fixture().await;
    let (_, token) = create(&f, json!({"expires_in_days": 1})).await;
    sqlx::query(
        "UPDATE share_links SET expires_at = now() - interval '1 hour' WHERE kind = 'portal'",
    )
    .execute(&f.acme)
    .await
    .expect("expire");

    // Both addresses, one vocabulary: a portal is a share, and a visitor learns "expired" rather than
    // "not found" because a token is 256 random bits and the holder is the person it was sent to.
    let (status, body) = call(&f, "GET", "/portal/press-kit", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["reason"].as_str().expect("reason").contains("expired"),
        "{body}"
    );
    let (status, body) = call(
        &f,
        "POST",
        &format!("/share/{token}/portal"),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["reason"].as_str().expect("reason").contains("expired"),
        "{body}"
    );
}

#[tokio::test]
async fn an_asset_link_is_not_a_portal() {
    // The flat answer, so the holder of one kind of link learns nothing about what other kinds exist.
    let f = fixture().await;
    let mut conn = f.acme.acquire().await.expect("connection");
    let share = dam_db::shares::create_on(
        &mut conn,
        &dam_db::shares::ShareSpec {
            kind: "asset",
            target_id: Some(f.photo),
            search_query: None,
            passcode: None,
            expires_at: None,
            max_downloads: None,
            allow_original: false,
            requires_eula: false,
            created_by: None,
        },
    )
    .await
    .expect("share");

    let (status, body) = call(
        &f,
        "POST",
        &format!("/share/{}/portal", share.token()),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["reason"]
            .as_str()
            .expect("reason")
            .contains("not a portal"),
        "{body}"
    );
}

#[tokio::test]
async fn a_portal_without_an_accent_inherits_the_tenants_own() {
    // The defect this fixes: `accent` defaulted to our own `#2563eb` literal, so a tenant who had set their
    // colour still got ours on every portal they made without naming it — and nothing on screen said why. Six
    // press kits meant setting one colour six times, and the seventh silently reverted.
    let f = fixture().await;
    sqlx::query("UPDATE site_branding SET accent = '#ff6600' WHERE id")
        .execute(&f.acme)
        .await
        .expect("branding");

    // No accent in the body at all — the field is `Option` now, not a defaulting function.
    let mut body = new_portal(&f, json!({ "key": "inherits" }));
    body.as_object_mut().expect("object").remove("accent");
    let (status, created) = call(&f, "POST", "/portals", Some(&f.key), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["portal"]["accent"], "#ff6600");

    // An explicit accent still wins: a portal for one campaign may legitimately differ from the house colour.
    let (status, chosen) = call(
        &f,
        "POST",
        "/portals",
        Some(&f.key),
        Some(new_portal(
            &f,
            json!({ "key": "chooses", "accent": "#123456" }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{chosen}");
    assert_eq!(chosen["portal"]["accent"], "#123456");
}
