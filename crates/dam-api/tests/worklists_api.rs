//! The worklists over HTTP (Q.20, Q.2c·3).
//!
//! `dam_db` proves the eight conditions against the storage. What lives only here are four decisions about the
//! interface:
//!
//! - **Read, not Manage.** The person who fixes an uncategorised asset is whoever can edit it. Gating the
//!   *finding* behind a permission the *fixing* does not need is how a backlog becomes one person's job.
//! - **Every count is the caller's own**, so two readers legitimately see different numbers — and the page
//!   under a count agrees with it, which is §7's rule that a total is a disclosure.
//! - **An unknown key is a 404.** Not an empty list and not a default: a mistyped URL that quietly showed a
//!   different worklist would have somebody working through the wrong backlog.
//! - **The rows are the grid's rows**, thumbnails and all, so a worklist opens into something a person can act
//!   in rather than a table of uuids.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::worklists::{WorklistState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    acme: PgPool,
    app: axum::Router,
    key: String,
    /// Scoped to a group holding one of the three assets.
    scoped_key: String,
    filed: Uuid,
    bare: Uuid,
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

    // Three assets: one filed and licensed, one bare, one expired-and-still-active.
    let filed = asset(&acme, "filed").await;
    let bare = asset(&acme, "bare").await;
    let lapsed = asset(&acme, "lapsed").await;

    let taxonomy: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'subject', 'Subject', 'category') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("taxonomy");
    let term: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, slug, label, path) \
         VALUES (gen_random_uuid(), $1, 'harbour', 'Harbour', 'harbour'::extensions.ltree) \
         RETURNING id",
    )
    .bind(taxonomy)
    .fetch_one(&acme)
    .await
    .expect("term");
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'confirmed', 'human')",
    )
    .bind(filed)
    .bind(term)
    .execute(&acme)
    .await
    .expect("filed");
    sqlx::query("UPDATE assets SET expires_at = now() - interval '2 days' WHERE id = $1")
        .bind(lapsed)
        .execute(&acme)
        .await
        .expect("lapsed");

    // A curator who can see only the bare one.
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) \
         VALUES (gen_random_uuid(), 'theirs', 'Theirs') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(bare)
        .execute(&acme)
        .await
        .expect("member");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped_reader', 'Scoped', '{asset:read}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let curator = identity(&global, "curator@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{scoped_reader}', false)",
    )
    .bind(tenant_id)
    .bind(curator)
    .execute(&global)
    .await
    .expect("membership");
    let scoped_key = issue(&global, tenant_id, Some(curator), &[]).await;

    let app = router(WorklistState {
        global: global.clone(),
        // No delivery: these assets have no rendered derivatives, so a thumbnail link would be a URL that
        // 404s. `None` is the honest shape and what a build without delivery configured produces.
        delivery: None,
    });

    Fixture {
        _pg: pg,
        global,
        acme,
        app,
        key,
        scoped_key,
        filed,
        bare,
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

async fn call(f: &Fixture, path: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().method("GET").uri(path);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = f
        .app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
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

fn count_of(listed: &Value, key: &str) -> i64 {
    listed
        .as_array()
        .expect("array")
        .iter()
        .find(|row| row["key"] == key)
        .unwrap_or_else(|| panic!("{key} is not in the list"))["count"]
        .as_i64()
        .expect("count")
}

async fn every_worklist_is_listed_with_a_sentence(f: &Fixture) {
    let (status, listed) = call(f, "/worklists", Some(&f.key)).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let rows = listed.as_array().expect("array");
    assert_eq!(rows.len(), 10);

    // The explanation is the point of the endpoint: a row saying "3" next to "Missing required metadata" and
    // nothing else does not tell an administrator what to do or why the upload did not refuse.
    for row in rows {
        let explanation = row["explanation"].as_str().expect("explanation");
        assert!(
            explanation.len() > 40,
            "{} needs a sentence, not a label: {explanation:?}",
            row["key"]
        );
    }
    // Exposure leads, whatever the counts are.
    assert_eq!(rows[0]["key"], "expired");
    assert_eq!(rows[0]["urgent"], true);
    // Three, and `no-licence` is deliberately not among them: every asset arrives unlicensed, so a tenant on
    // its first day would see its whole library badged as an exposure — which is how a badge stops being read.
    // Urgent marks a *change*, not an absence that has always been there.
    let urgent: Vec<&str> = rows
        .iter()
        .filter(|row| row["urgent"] == true)
        .map(|row| row["key"].as_str().expect("key"))
        .collect();
    assert_eq!(urgent, vec!["expired", "rights-expiring", "rights-denied"]);

    assert_eq!(count_of(&listed, "expired"), 1);
    assert_eq!(
        count_of(&listed, "uncategorised"),
        2,
        "the filed one is filed"
    );
    assert_eq!(count_of(&listed, "no-licence"), 3);
    assert_eq!(count_of(&listed, "no-thumbnail"), 3);
    assert_eq!(count_of(&listed, "embargoed"), 0);
}

async fn the_rights_lists_agree_with_the_badge_on_the_asset(f: &Fixture) {
    // The defect that put these two lists here. `assets.rights_state` is what the grid renders as a badge, and
    // the first version of this module answered "is anything expiring?" from `assets.expires_at` instead — so
    // the grid showed three assets badged "Expiring" while the worklist called "expiring" reported zero. One
    // question, two answers, and the wrong one was the one about a contract.
    //
    // Reading the same column is what makes them unable to disagree, so that is what is asserted: set the
    // column, and the list moves.
    sqlx::query("UPDATE assets SET rights_state = 'expiring' WHERE id = $1")
        .bind(f.filed)
        .execute(&f.acme)
        .await
        .expect("expiring");
    sqlx::query("UPDATE assets SET rights_state = 'denied' WHERE id = $1")
        .bind(f.bare)
        .execute(&f.acme)
        .await
        .expect("denied");

    let listed = call(f, "/worklists", Some(&f.key)).await.1;
    assert_eq!(count_of(&listed, "rights-expiring"), 1);
    assert_eq!(count_of(&listed, "rights-denied"), 1);
    // And the scheduled-expiry lists are untouched, because they are about a different column: a licence term
    // ending is not a retention date arriving, and conflating them is how one of them becomes wrong.
    assert_eq!(count_of(&listed, "expiring-soon"), 0);

    let (status, page) = call(f, "/worklists/rights-expiring", Some(&f.key)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["id"], f.filed.to_string());
    // The row carries the same state the badge draws from, so a reader can see why it is on the list.
    assert_eq!(page["items"][0]["rights_state"], "expiring");

    sqlx::query("UPDATE assets SET rights_state = 'unknown'")
        .execute(&f.acme)
        .await
        .expect("reset");
}

async fn a_page_carries_the_grids_own_rows(f: &Fixture) {
    let (status, page) = call(f, "/worklists/uncategorised", Some(&f.key)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], 2);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    // A grid row, not a bare id: the fix is one click from the finding.
    assert!(items[0]["filename"].as_str().is_some());
    assert!(items[0]["mime"].as_str().is_some());
    assert!(
        items[0]["thumbnail_url"].is_null(),
        "no derivative exists, so no link is minted rather than one that 404s"
    );
    assert_eq!(
        page["ranked"], false,
        "a backlog is not a relevance ranking"
    );
    // Oldest first, because a worklist is a backlog and the longest-waiting asset is the one to fix.
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids[0], f.bare.to_string(), "uuid v7 orders by creation");
    assert!(!ids.contains(&f.filed.to_string().as_str()));
}

async fn a_count_and_its_page_are_the_callers_own(f: &Fixture) {
    // The §7 property, on the surface where it is easiest to get wrong: the scoped reader can see one of the
    // three assets, so their worklist is one long — not three with two of them 404ing when clicked.
    let (status, listed) = call(f, "/worklists", Some(&f.scoped_key)).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(count_of(&listed, "uncategorised"), 1);
    assert_eq!(count_of(&listed, "no-licence"), 1);
    assert_eq!(
        count_of(&listed, "expired"),
        0,
        "the expired one is not theirs to see"
    );

    let (_, page) = call(f, "/worklists/uncategorised", Some(&f.scoped_key)).await;
    assert_eq!(page["total"], 1, "the total counts what the rows are");
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], f.bare.to_string());
}

async fn an_unknown_worklist_is_not_found(f: &Fixture) {
    for key in ["uncategorized", "everything", "expired%20", ""] {
        let (status, _) = call(f, &format!("/worklists/{key}"), Some(&f.key)).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::OK && key.is_empty(),
            "{key:?} should not resolve to a worklist, got {status}"
        );
    }
    // Spelled out: the American spelling is a plausible typo and must not quietly show a different list.
    let (status, _) = call(f, "/worklists/uncategorized", Some(&f.key)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn a_worklist_needs_a_credential(f: &Fixture) {
    for path in ["/worklists", "/worklists/expired"] {
        let (status, _) = call(f, path, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

async fn fixing_the_thing_empties_the_list(f: &Fixture) {
    // The property that makes these worklists rather than queues: nothing is recorded, so an asset leaves the
    // list the moment somebody fixes what put it there. A queue would need a row deleting.
    let before = call(f, "/worklists", Some(&f.key)).await.1;
    assert_eq!(count_of(&before, "expired"), 1);

    sqlx::query("UPDATE assets SET status = 'archived' WHERE expires_at IS NOT NULL")
        .execute(&f.acme)
        .await
        .expect("archive it");

    let after = call(f, "/worklists", Some(&f.key)).await.1;
    assert_eq!(
        count_of(&after, "expired"),
        0,
        "dealt with, so off the list"
    );
    assert_eq!(
        count_of(&after, "uncategorised"),
        1,
        "and an archived asset is not somebody's filing job either"
    );
}

#[tokio::test]
async fn the_worklist_contract_holds() {
    let f = fixture().await;

    every_worklist_is_listed_with_a_sentence(&f).await;
    the_rights_lists_agree_with_the_badge_on_the_asset(&f).await;
    a_page_carries_the_grids_own_rows(&f).await;
    a_count_and_its_page_are_the_callers_own(&f).await;
    an_unknown_worklist_is_not_found(&f).await;
    a_worklist_needs_a_credential(&f).await;
    // Last: it changes the fixture's data.
    fixing_the_thing_empties_the_list(&f).await;

    assert!(!f.global.is_closed());
}
