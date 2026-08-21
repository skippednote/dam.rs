//! The archival endpoints over HTTP (§6.4, §6.5).
//!
//! `dam-pipeline`'s suite proves the machinery — the planner's verdicts, the transitions, the restore
//! timeline. What only exists here is the interface, and it makes four decisions worth pinning:
//!
//! - **A plan is Manage; asking for a restore is Download.** A restore is the first half of taking a copy, so
//!   whoever may download an asset may ask for its bytes to become fetchable. Reading a tiering plan is
//!   administration of the library's costs and is not.
//! - **Approving is Manage, and never the requester's own act.** The threshold exists so a large spend needs
//!   somebody other than the person who asked; a self-approval would make it a confirmation dialog.
//! - **The asset gate is first.** An asset outside the caller's scope is 404 before anything is planned,
//!   because a cost estimate discloses roughly how large a file is and an ETA discloses its storage class.
//! - **A refusal is the domain's own sentence.** "Deep Archive has no expedited tier" is something a user can
//!   act on, and flattening it to "invalid request" throws away the only useful part.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::archival::{ArchivalState, router};
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
    /// A tenant admin: Manage and Download.
    key: String,
    /// Download but not Manage, so the approval gate has somebody to refuse.
    downloader_key: String,
    /// Read only, so "may look" and "may ask for a restore" stay separate.
    reader_key: String,
    /// A machine key: Manage, and nobody behind it.
    machine_key: String,
    pool_id: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("acme");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("acme pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant");

    let admin = identity(&global, "ada@example.com").await;
    member(&global, tenant_id, admin, &[], true).await;
    let key = issue(&global, tenant_id, Some(admin)).await;

    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'downloader', 'Downloader', '{asset:read,asset:download}', '{}', true), \
                (gen_random_uuid(), 'reader', 'Reader', '{asset:read}', '{}', true)",
    )
    .execute(&acme)
    .await
    .expect("roles");

    let dee = identity(&global, "dee@example.com").await;
    member(&global, tenant_id, dee, &["downloader"], false).await;
    let downloader_key = issue(&global, tenant_id, Some(dee)).await;

    let ray = identity(&global, "ray@example.com").await;
    member(&global, tenant_id, ray, &["reader"], false).await;
    let reader_key = issue(&global, tenant_id, Some(ray)).await;

    // No identity: a machine key, which may manage and may not approve a spend.
    let machine_key = issue(&global, tenant_id, None).await;

    let pool_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.storage_pools \
           (id, tenant_id, name, driver, bucket, credentials_ref, latency_class, \
            cost_per_gb_retrieval, cost_per_gb_retrieval_expedited, cost_per_gb_retrieval_bulk, \
            cost_per_1k_requests) \
         VALUES (gen_random_uuid(), $1, 'hot', 's3', 'b', 'test', 'instant', \
                 0.01, 0.03, 0.0025, 0.05) RETURNING id",
    )
    .bind(tenant_id)
    .fetch_one(&global)
    .await
    .expect("pool");

    let app = router(ArchivalState {
        global: global.clone(),
    });

    Fixture {
        _pg: pg,
        app,
        global,
        acme,
        key,
        downloader_key,
        reader_key,
        machine_key,
        pool_id,
    }
}

#[tokio::test]
async fn the_archival_http_contract_holds() {
    let f = fixture().await;

    a_plan_needs_manage(&f).await;
    a_plan_reads_a_policy_that_is_not_enabled_yet(&f).await;
    a_plan_says_why_each_candidate_was_left_alone(&f).await;
    the_policy_list_shows_which_rules_have_never_moved_anything(&f).await;
    a_run_is_queued_rather_than_performed(&f).await;

    a_restore_needs_download_not_read(&f).await;
    an_asset_the_caller_cannot_see_is_absent_before_it_is_priced(&f).await;
    a_restore_of_something_hot_is_refused_in_the_domains_words(&f).await;
    expedited_deep_archive_is_refused_by_name(&f).await;
    an_archived_asset_is_planned_priced_and_recorded(&f).await;
    a_second_caller_joins_the_first_request(&f).await;
    a_quote_prices_every_tier_without_asking_for_one(&f).await;
    a_machine_key_is_refused_at_the_door(&f).await;
    an_expensive_restore_waits_for_an_approver(&f).await;
}

// ─── policies and plans ─────────────────────────────────────────────────────

async fn a_plan_needs_manage(f: &Fixture) {
    let id = policy(f, "needs manage", true, "GLACIER_IR", 90).await;
    let (status, _) = call(
        f,
        "POST",
        &format!("/lifecycle/policies/{id}/plan"),
        &f.downloader_key,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "reading what a tiering rule would do is administration of the library's costs",
    );
    let (status, _) = call(f, "GET", "/lifecycle/policies", &f.downloader_key, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A disabled policy plans, and that is the point.
///
/// Refusing until it is switched on would mean the only way to find out what a rule does is to turn it on —
/// over a library where the mistake costs a 90-day minimum charge per object and a 48-hour wait to undo.
async fn a_plan_reads_a_policy_that_is_not_enabled_yet(f: &Fixture) {
    let id = policy(f, "not yet on", true, "GLACIER_IR", 90).await;
    sqlx::query("UPDATE lifecycle_policies SET enabled = false WHERE id = $1")
        .bind(id)
        .execute(&f.acme)
        .await
        .expect("disable");

    let (status, body) = call(
        f,
        "POST",
        &format!("/lifecycle/policies/{id}/plan"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["policy_name"], "not yet on");
    assert_eq!(
        body["dry_run"], true,
        "and it must say it would not have moved anything either way",
    );
}

/// The skips are the plan. "Why did nothing happen?" is the only question anybody asks.
async fn a_plan_says_why_each_candidate_was_left_alone(f: &Fixture) {
    let held = asset(f, "under-hold").await;
    placement(
        f,
        &format!("acme/o/aa/bb/{held}"),
        held,
        4_000_000,
        "STANDARD",
        300,
    )
    .await;
    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = $1")
        .bind(held)
        .execute(&f.acme)
        .await
        .expect("hold");

    let fresh = asset(f, "brand-new").await;
    placement(
        f,
        &format!("acme/o/cc/dd/{fresh}"),
        fresh,
        4_000_000,
        "STANDARD",
        1,
    )
    .await;

    let id = policy(f, "explains itself", true, "GLACIER_IR", 90).await;
    let (status, body) = call(
        f,
        "POST",
        &format!("/lifecycle/policies/{id}/plan"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let skips = body["skipped"].as_array().expect("skipped");
    let reasons: Vec<&str> = skips.iter().filter_map(|s| s["reason"].as_str()).collect();
    assert!(
        reasons.contains(&"pinned"),
        "a legal hold must appear as a pin with its reason, not as an omission: {body}",
    );
    assert!(
        reasons.contains(&"not_yet_eligible"),
        "and an object that is simply too young must say so: {body}",
    );
    let pinned = skips
        .iter()
        .find(|s| s["reason"] == "pinned")
        .expect("the pinned entry");
    assert_eq!(
        pinned["detail"], "the asset is under legal hold",
        "the detail is what makes a plan of ten thousand readable: {pinned}",
    );
}

async fn the_policy_list_shows_which_rules_have_never_moved_anything(f: &Fixture) {
    let (status, body) = call(f, "GET", "/lifecycle/policies", &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rules = body.as_array().expect("a list");
    assert!(!rules.is_empty(), "the fixture created some: {body}");
    assert!(
        rules.iter().all(|r| r["dry_run"] == true),
        "every policy starts in dry run, and the list must show it — a rule nobody has taken off dry run \
         has never moved anything, which is the most important thing on the row: {body}",
    );
    assert!(
        rules.iter().all(|r| r["last_run_at"].is_null()),
        "and none of them has run: {body}",
    );
}

/// The button that runs a sweep must not be the button that executes a plan.
async fn a_run_is_queued_rather_than_performed(f: &Fixture) {
    let (status, body) = call(f, "POST", "/lifecycle/runs", &f.key, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(body["job_id"].is_string(), "{body}");
    assert!(
        body["policies_in_dry_run"].as_u64().unwrap() > 0,
        "restated on the response, because \"I pressed run and nothing moved\" is otherwise a support \
         ticket rather than a policy still in dry run: {body}",
    );

    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.jobs WHERE kind = 'tier_sweep' AND state = 'queued'",
    )
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(
        queued, 1,
        "the sweep is a job, not a request held open for hours"
    );
}

// ─── restores ───────────────────────────────────────────────────────────────

async fn a_restore_needs_download_not_read(f: &Fixture) {
    let id = archived(f, "read-only-cannot-ask", "GLACIER").await;
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{id}/restore"),
        &f.reader_key,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a restore is the first half of taking a copy",
    );
    // Reading the state of one is Read, though: a person who may see the asset may see that it is thawing.
    let (status, _) = call(
        f,
        "GET",
        &format!("/assets/{id}/restore"),
        &f.reader_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn an_asset_the_caller_cannot_see_is_absent_before_it_is_priced(f: &Fixture) {
    let (status, _) = call(
        f,
        "POST",
        &format!("/assets/{}/restore", Uuid::now_v7()),
        &f.key,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an estimate discloses roughly how large a file is, and an ETA discloses its class",
    );
}

async fn a_restore_of_something_hot_is_refused_in_the_domains_words(f: &Fixture) {
    let id = archived(f, "already-hot", "STANDARD").await;
    let (status, body) = call(f, "POST", &format!("/assets/{id}/restore"), &f.key, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.as_str()
            .unwrap_or_default()
            .contains("instantly readable")
            || body.to_string().contains("instantly readable"),
        "the refusal says the object needs no restore, which is a fact the caller can act on: {body}",
    );
}

async fn expedited_deep_archive_is_refused_by_name(f: &Fixture) {
    let id = archived(f, "deep-and-slow", "DEEP_ARCHIVE").await;
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{id}/restore?tier=expedited"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.to_string().contains("no expedited tier"),
        "silently substituting Standard would answer a request for minutes with twelve hours: {body}",
    );
}

async fn an_archived_asset_is_planned_priced_and_recorded(f: &Fixture) {
    let id = archived(f, "frozen-and-wanted", "GLACIER").await;
    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{id}/restore?tier=bulk"),
        &f.downloader_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "queued", "{body}");
    assert_eq!(body["tier"], "bulk");
    assert_eq!(body["joined_existing"], false);
    assert!(body["eta_at"].is_string(), "an ETA is the point: {body}");
    assert!(
        body["est_cost_cents"].as_i64().unwrap() >= 0,
        "and so is a price: {body}",
    );

    // And the poll chain is started, because there is now something to poll for.
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dam_global.jobs WHERE kind = 'restore_poll'")
            .fetch_one(&f.global)
            .await
            .expect("count");
    assert_eq!(
        queued, 1,
        "a deployment where nobody has ever archived anything should run no polling at all",
    );

    let (status, body) = call(f, "GET", &format!("/assets/{id}/restore"), &f.key, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "queued", "and it reads back: {body}");
}

/// Two people asking for the same archived asset share one retrieval and one charge.
async fn a_second_caller_joins_the_first_request(f: &Fixture) {
    let id = archived(f, "wanted-twice", "GLACIER").await;
    let first = call(f, "POST", &format!("/assets/{id}/restore"), &f.key, None)
        .await
        .1;
    let second = call(
        f,
        "POST",
        &format!("/assets/{id}/restore"),
        &f.downloader_key,
        None,
    )
    .await
    .1;

    assert_eq!(first["id"], second["id"], "one request, not two");
    assert_eq!(
        second["joined_existing"], true,
        "and the second caller is told, so the UI does not offer them a button that would do nothing: \
         {second}",
    );
}

/// §6.5 wants the estimate before the confirmation, which means a way to price without asking.
///
/// Before this endpoint there was none: the only thing that produced a plan was the POST, which records the
/// request — so a screen could show a price or ask for a restore, and showing the price meant having asked.
async fn a_quote_prices_every_tier_without_asking_for_one(f: &Fixture) {
    let id = asset(f, "quotable").await;
    placement(
        f,
        &format!("acme/o/gg/hh/{id}"),
        id,
        50 * 1024 * 1024 * 1024,
        "GLACIER",
        300,
    )
    .await;

    let (status, body) = call(
        f,
        "GET",
        &format!("/assets/{id}/restore/quote"),
        &f.reader_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a quote is Read: {body}");

    let options = body["options"].as_array().expect("options");
    assert_eq!(options.len(), 3, "all three tiers in one response: {body}");
    let cents: Vec<u64> = options
        .iter()
        .map(|o| o["est_cost_cents"].as_u64().unwrap())
        .collect();
    assert!(
        cents[0] > cents[2],
        "the comparison is the whole reason to show a number — Expedited above Bulk: {body}",
    );
    assert!(
        options.iter().all(|o| o["available"] == true),
        "Glacier offers all three: {body}",
    );

    // And nothing was recorded. A quote that left a row behind would mean opening a detail panel started a
    // retrieval.
    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM restore_requests WHERE asset_id = $1")
            .bind(id)
            .fetch_one(&f.acme)
            .await
            .expect("count");
    assert_eq!(requests, 0, "a quote asks for nothing");

    // Deep Archive has no Expedited, and the option is present-and-refused rather than absent: a chooser
    // showing two options where another asset shows three invites "why is this one different".
    let deep = archived(f, "quote-deep", "DEEP_ARCHIVE").await;
    let (_, body) = call(
        f,
        "GET",
        &format!("/assets/{deep}/restore/quote"),
        &f.key,
        None,
    )
    .await;
    let expedited = body["options"]
        .as_array()
        .expect("options")
        .iter()
        .find(|o| o["tier"] == "expedited")
        .expect("an expedited entry");
    assert_eq!(expedited["available"], false, "{body}");
    assert!(
        expedited["unavailable_because"]
            .as_str()
            .unwrap_or_default()
            .contains("no expedited tier"),
        "and it says why: {expedited}",
    );
}

/// A machine key gets no further than the door, on every one of these routes.
///
/// Written first as "a machine key cannot approve a spend", asserting the handler's own identity check — and a
/// mutation sweep showed that check is unreachable: `authorize` refuses an identity-less key before any
/// handler runs, so the case passed for a reason it was not testing. The property that is actually true is
/// broader and worth stating as such, and the handler's guard is now documented as unobservable rather than
/// left looking covered.
async fn a_machine_key_is_refused_at_the_door(f: &Fixture) {
    for (method, path) in [
        ("POST", format!("/restores/{}/approve", Uuid::now_v7())),
        ("GET", "/lifecycle/policies".to_owned()),
        ("POST", "/lifecycle/runs".to_owned()),
        ("POST", format!("/assets/{}/restore", Uuid::now_v7())),
    ] {
        let (status, body) = call(f, method, &path, &f.machine_key, None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {path}: a key with nobody behind it has no membership and so no grants: {body}",
        );
    }
}

/// Over the threshold, a restore is held rather than refused: a large restore is often legitimate, and the
/// answer is "somebody senior confirms".
async fn an_expensive_restore_waits_for_an_approver(f: &Fixture) {
    // Forty terabytes at three cents a gigabyte on Expedited is comfortably past the $50 default.
    let id = asset(f, "the-whole-shoot").await;
    placement(
        f,
        &format!("acme/o/ee/ff/{id}"),
        id,
        40 * 1024 * 1024 * 1024 * 1024,
        "GLACIER",
        300,
    )
    .await;

    let (status, body) = call(
        f,
        "POST",
        &format!("/assets/{id}/restore?tier=expedited"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["state"], "awaiting_approval",
        "held, not refused — the answer to an expensive request is a person, not a no: {body}",
    );

    let request_id = body["id"].as_str().expect("an id");
    let (status, _) = call(
        f,
        "POST",
        &format!("/restores/{request_id}/approve"),
        &f.downloader_key,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "approving is Manage: the threshold exists so a large spend needs somebody other than the asker",
    );

    let (status, body) = call(
        f,
        "POST",
        &format!("/restores/{request_id}/approve"),
        &f.key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["state"], "queued",
        "released for the worker to issue: {body}"
    );
}

// ─── plumbing ───────────────────────────────────────────────────────────────

async fn policy(f: &Fixture, name: &str, dry_run: bool, target: &str, idle_days: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO lifecycle_policies \
           (id, name, applies_to, idle_days, action, target_pool_id, target_class, dry_run, enabled) \
         VALUES (gen_random_uuid(), $1, 'original', $2, 'transition', $3, $4, $5, true) RETURNING id",
    )
    .bind(name)
    .bind(idle_days)
    .bind(f.pool_id)
    .bind(target)
    .bind(dry_run)
    .fetch_one(&f.acme)
    .await
    .expect("policy")
}

async fn asset(f: &Fixture, label: &str) -> Uuid {
    let id = Uuid::now_v7();
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
    id
}

async fn placement(f: &Fixture, key: &str, asset_id: Uuid, bytes: i64, class: &str, days_old: i64) {
    sqlx::query(
        "INSERT INTO object_placements \
           (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state, placed_at) \
         VALUES ($1, $2, $3, $4, 'x', $5, 'present', now() - make_interval(days => $6::int))",
    )
    .bind(key)
    .bind(f.pool_id)
    .bind(asset_id)
    .bind(bytes)
    .bind(class)
    .bind(i32::try_from(days_old).unwrap())
    .execute(&f.acme)
    .await
    .expect("placement");
}

async fn archived(f: &Fixture, label: &str, class: &str) -> Uuid {
    let id = asset(f, label).await;
    placement(
        f,
        &format!("acme/o/{}/x", &label[..2]),
        id,
        4_000_000,
        class,
        300,
    )
    .await;
    id
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

async fn member(global: &PgPool, tenant: Uuid, identity: Uuid, roles: &[&str], admin: bool) {
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant)
    .bind(identity)
    .bind(roles.iter().map(|r| (*r).to_owned()).collect::<Vec<String>>())
    .bind(admin)
    .execute(global)
    .await
    .expect("membership");
}

async fn issue(global: &PgPool, tenant: Uuid, identity: Option<Uuid>) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
           (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant)
    .bind(identity)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(global)
    .await
    .expect("key");
    api_key.plaintext().to_owned()
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
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}
