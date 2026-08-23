//! The near-duplicate review queue and the colour facet (M4, §8.1).
//!
//! `dam_db` proves the pair ordering, the dismissal rule and the facet arithmetic. What lives only here are
//! four decisions about the surface, and the first is the one that matters:
//!
//! - **Both halves of a pair must be visible.** A pair names two assets. Showing one where the caller can see
//!   only one side would disclose that the other exists — the same existence oracle every other endpoint in
//!   this codebase closes, arriving through a feature whose whole job is to show two things side by side.
//! - **`merged` records a decision and merges nothing.** 0003: "auto-merging a crop that is actually a
//!   different licensed deliverable is a rights problem, so a human decides." What a merge *means* is not
//!   something this endpoint can decide, and a button that silently picked would be the worst place to.
//! - **Read to look, Manage to resolve.** Reading the queue tells a reader nothing they could not work out
//!   from the assets; recording a verdict changes what somebody else sees next.
//! - **The colour facet is deliberately unscoped**, which is the one exception to §7's counts-are-disclosures
//!   rule in this file — and it is called out rather than left to be noticed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::duplicates::{DuplicateState, router};
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
    /// Sees `mine` and `also_mine`, and nothing else.
    scoped_key: String,
    mine: Uuid,
    also_mine: Uuid,
    theirs: Uuid,
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

    // Three assets with controlled ids, so `asset_id < other_id` is predictable.
    let mine = asset(
        &acme,
        Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
        "mine",
    )
    .await;
    let also_mine = asset(
        &acme,
        Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
        "also-mine",
    )
    .await;
    let theirs = asset(
        &acme,
        Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap(),
        "theirs",
    )
    .await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    for id in [mine, also_mine] {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(group)
            .bind(id)
            .execute(&acme)
            .await
            .expect("member");
    }
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped', 'Scoped', '{asset:read,asset:manage}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let curator = identity(&global, "curator@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{scoped}', false)",
    )
    .bind(tenant_id)
    .bind(curator)
    .execute(&global)
    .await
    .expect("membership");

    Fixture {
        _pg: pg,
        app: router(DuplicateState {
            global: global.clone(),
            // No delivery: these assets have no rendered thumbnail, so a link would 404. `None` is the honest
            // shape and what a build without delivery configured produces.
            delivery: None,
        }),
        key: issue(&global, tenant_id, Some(admin), &[]).await,
        read_only_key: issue(&global, tenant_id, Some(admin), &["asset:read"]).await,
        scoped_key: issue(&global, tenant_id, Some(curator), &[]).await,
        global,
        acme,
        mine,
        also_mine,
        theirs,
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
            .map(|s| (*s).to_owned())
            .collect::<Vec<String>>(),
    )
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn asset(pool: &PgPool, id: Uuid, name: &str) -> Uuid {
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

/// Records a candidate pair directly, so a test can choose which assets it names.
async fn pair(f: &Fixture, a: Uuid, b: Uuid, hamming: i16) {
    dam_db::similarity::record_candidates(
        &mut f.acme.acquire().await.expect("conn"),
        a,
        &[(b, hamming)],
    )
    .await
    .expect("record");
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

async fn a_pair_needs_both_sides_visible(f: &Fixture) {
    // The property this file exists for. `mine`/`also_mine` is fully visible to the scoped curator;
    // `mine`/`theirs` is half visible, and showing it would tell them `theirs` exists.
    pair(f, f.mine, f.also_mine, 1).await;
    pair(f, f.mine, f.theirs, 2).await;

    let (status, wide) = call(f, "GET", "/duplicates", Some(&f.key), None).await;
    assert_eq!(status, StatusCode::OK, "{wide}");
    assert_eq!(
        wide.as_array().expect("array").len(),
        2,
        "the admin sees both"
    );

    let (status, narrow) = call(f, "GET", "/duplicates", Some(&f.scoped_key), None).await;
    assert_eq!(status, StatusCode::OK, "{narrow}");
    let rows = narrow.as_array().expect("array");
    assert_eq!(
        rows.len(),
        1,
        "only the pair whose both halves they can see: {narrow}"
    );
    let ids = [
        rows[0]["left"]["asset_id"].as_str().expect("left"),
        rows[0]["right"]["asset_id"].as_str().expect("right"),
    ];
    assert!(!ids.contains(&f.theirs.to_string().as_str()), "{ids:?}");

    // And the half-visible pair cannot be resolved either — a write outside their scope, and a way to learn
    // the pair exists.
    let hidden = wide
        .as_array()
        .expect("array")
        .iter()
        .find(|row| {
            row["left"]["asset_id"] == f.theirs.to_string()
                || row["right"]["asset_id"] == f.theirs.to_string()
        })
        .expect("the half-visible pair");
    let (status, _) = call(
        f,
        "POST",
        &format!("/duplicates/{}", hidden["id"].as_str().expect("id")),
        Some(&f.scoped_key),
        Some(json!({ "state": "dismissed" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn a_pair_carries_what_a_reviewer_needs_to_compare(f: &Fixture) {
    let (_, listed) = call(f, "GET", "/duplicates", Some(&f.key), None).await;
    let closest = &listed.as_array().expect("array")[0];
    // Most alike first: a reviewer working down the list should see the likely ones before the marginal ones.
    assert_eq!(closest["hamming"], 1);
    assert_eq!(closest["relation"], "near_identical");
    // No cosine: that needs an embedding, which is the model-dependent half of M4. Absent rather than zero.
    assert!(closest["cosine"].is_null());
    for side in ["left", "right"] {
        assert!(closest[side]["filename"].as_str().is_some(), "{closest}");
        assert_eq!(closest[side]["mime"], "image/jpeg");
        assert_eq!(closest[side]["bytes"], 4096);
        // Nothing rendered a thumbnail, so no link rather than one that 404s.
        assert!(closest[side]["thumbnail_url"].is_null());
    }
}

async fn looking_needs_read_and_deciding_needs_manage(f: &Fixture) {
    let (status, _) = call(f, "GET", "/duplicates", Some(&f.read_only_key), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "reading the queue is reading the library"
    );

    let (_, listed) = call(f, "GET", "/duplicates", Some(&f.key), None).await;
    let id = listed.as_array().expect("array")[0]["id"]
        .as_str()
        .expect("id");
    let (status, _) = call(
        f,
        "POST",
        &format!("/duplicates/{id}"),
        Some(&f.read_only_key),
        Some(json!({ "state": "dismissed" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a verdict changes what somebody else sees next"
    );

    let (status, _) = call(f, "GET", "/duplicates", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn a_verdict_is_recorded_and_the_pair_leaves_the_queue(f: &Fixture) {
    let (_, listed) = call(f, "GET", "/duplicates", Some(&f.key), None).await;
    let before = listed.as_array().expect("array").len();
    let id = listed.as_array().expect("array")[0]["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let (status, _) = call(
        f,
        "POST",
        &format!("/duplicates/{id}"),
        Some(&f.key),
        Some(json!({ "state": "dismissed" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, after) = call(f, "GET", "/duplicates", Some(&f.key), None).await;
    assert_eq!(after.as_array().expect("array").len(), before - 1);

    // Twice is a 404, not a second success: the first reviewer's judgement stands.
    let (status, _) = call(
        f,
        "POST",
        &format!("/duplicates/{id}"),
        Some(&f.key),
        Some(json!({ "state": "confirmed" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn merged_records_a_decision_and_merges_nothing(f: &Fixture) {
    // 0003's rule: auto-merging a crop that turns out to be a separately licensed deliverable is a rights
    // problem. So `merged` is a note about what a person decided, and both assets are still there afterwards.
    pair(f, f.also_mine, f.theirs, 4).await;
    let (_, listed) = call(f, "GET", "/duplicates", Some(&f.key), None).await;
    let id = listed.as_array().expect("array")[0]["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let (status, _) = call(
        f,
        "POST",
        &format!("/duplicates/{id}"),
        Some(&f.key),
        Some(json!({ "state": "merged" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let alive: i64 = sqlx::query_scalar("SELECT count(*) FROM assets WHERE deleted_at IS NULL")
        .fetch_one(&f.acme)
        .await
        .expect("count");
    assert_eq!(alive, 3, "a merge verdict deletes nothing");
    let state: String = sqlx::query_scalar("SELECT state FROM duplicate_candidates WHERE id = $1")
        .bind(Uuid::parse_str(&id).expect("uuid"))
        .fetch_one(&f.acme)
        .await
        .expect("state");
    assert_eq!(state, "merged", "the decision is recorded");
}

async fn an_invented_verdict_is_refused_by_name(f: &Fixture) {
    pair(f, f.mine, f.also_mine, 7).await;
    let (_, listed) = call(f, "GET", "/duplicates", Some(&f.key), None).await;
    let open = listed.as_array().expect("array");
    if open.is_empty() {
        return;
    }
    let id = open[0]["id"].as_str().expect("id");

    let (status, refused) = call(
        f,
        "POST",
        &format!("/duplicates/{id}"),
        Some(&f.key),
        Some(json!({ "state": "deleted" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let reason = refused["reason"].as_str().expect("reason");
    assert!(
        reason.contains("confirmed") && reason.contains("dismissed") && reason.contains("merged"),
        "the refusal lists the verdicts that work: {reason}"
    );
}

async fn the_colour_facet_counts_primary_colours(f: &Fixture) {
    for (id, bucket) in [(f.mine, "blue"), (f.also_mine, "blue"), (f.theirs, "red")] {
        dam_db::similarity::record_colours(
            &mut f.acme.acquire().await.expect("conn"),
            id,
            &[
                dam_db::similarity::Colour {
                    hex: "#0000ff".to_owned(),
                    lab: [30.0, 50.0, -80.0],
                    coverage: 0.8,
                    palette_bucket: bucket.to_owned(),
                },
                // A secondary colour, which must not be counted — or the numbers would sum to more than the
                // library and put every asset in a bucket it is only incidentally in.
                dam_db::similarity::Colour {
                    hex: "#00ff00".to_owned(),
                    lab: [80.0, -70.0, 60.0],
                    coverage: 0.2,
                    palette_bucket: "green".to_owned(),
                },
            ],
        )
        .await
        .expect("colours");
    }

    let (status, buckets) = call(f, "GET", "/colours", Some(&f.read_only_key), None).await;
    assert_eq!(status, StatusCode::OK, "{buckets}");
    let rows = buckets.as_array().expect("array");
    assert_eq!(
        rows.len(),
        2,
        "no green, because green is nobody's primary: {buckets}"
    );
    assert_eq!(rows[0]["bucket"], "blue");
    assert_eq!(rows[0]["count"], 2);
    assert_eq!(rows[1]["bucket"], "red");
    assert_eq!(rows[1]["count"], 1);

    // Unscoped, deliberately — see the endpoint's own docs. The scoped curator sees the same numbers, and the
    // *results* of clicking a bucket are scoped like every other search.
    let (_, narrow) = call(f, "GET", "/colours", Some(&f.scoped_key), None).await;
    assert_eq!(
        narrow, buckets,
        "the facet is a number with nothing behind it"
    );
}

#[tokio::test]
async fn the_duplicate_contract_holds() {
    let f = fixture().await;

    a_pair_needs_both_sides_visible(&f).await;
    a_pair_carries_what_a_reviewer_needs_to_compare(&f).await;
    looking_needs_read_and_deciding_needs_manage(&f).await;
    a_verdict_is_recorded_and_the_pair_leaves_the_queue(&f).await;
    merged_records_a_decision_and_merges_nothing(&f).await;
    an_invented_verdict_is_refused_by_name(&f).await;
    the_colour_facet_counts_primary_colours(&f).await;

    assert!(!f.global.is_closed());
}
