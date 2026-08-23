//! Insights over HTTP (M6c).
//!
//! `dam_db` proves the date spine, the ledger-not-feed source, the class mapping and the predicate scoping.
//! What lives only here:
//!
//! - **One request answers the whole screen.** The chart and the lists beneath it come from one transaction, so
//!   they cannot disagree about the instant they describe.
//! - **The window it answers with is the window it used.** A request for ten years comes back saying 366, so a
//!   screen can label the chart honestly rather than repeating what was asked for.
//! - **`Read`, not `Manage`.** Every number is already narrowed to what the caller can see, so gating it behind
//!   administration would only stop a curator seeing their own library's activity.
//! - **A scoped reader gets their own numbers, not the library's.** The same rule as the dashboard, and the
//!   whole reason this endpoint cannot report a tenant-wide total.
//! - **An export is the same query.** Same predicate, same window — a file that disagreed with the page that
//!   offered it would be worse than no export.
//! - **A contributor whose identity no longer resolves is dropped, not shown as a uuid.**

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::insights::{InsightsState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    app: axum::Router,
    /// Sees everything.
    key: String,
    /// Scoped to one group, which holds `mine` only.
    scoped_key: String,
    mine: Uuid,
    theirs: Uuid,
    ada: Uuid,
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
    member(&global, tenant_id, ada, "{}", true).await;

    let mine = asset(&acme, "mine", "image/jpeg", 4_000_000, 40).await;
    let theirs = asset(&acme, "theirs", "video/mp4", 900_000_000, 40).await;

    // Two downloads of the invisible asset, one of the visible one, so a wide reader and a narrow one cannot
    // accidentally agree.
    download(&acme, mine, 1, Some(ada)).await;
    download(&acme, theirs, 1, None).await;
    download(&acme, theirs, 2, None).await;
    event(&acme, "upload", mine, Some(ada), 3).await;
    event(&acme, "upload", theirs, Some(ada), 3).await;
    // A contributor who is not an identity in the control plane: a person deleted since. Must not appear as a
    // uuid on a list whose entire content is names.
    event(&acme, "upload", mine, Some(Uuid::new_v4()), 2).await;

    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&acme)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(mine)
        .execute(&acme)
        .await
        .expect("member");
    sqlx::query(
        "INSERT INTO roles (id, key, label, permissions, asset_group_ids, all_asset_groups) \
         VALUES (gen_random_uuid(), 'scoped', 'Scoped', '{asset:read}', ARRAY[$1], false)",
    )
    .bind(group)
    .execute(&acme)
    .await
    .expect("role");
    let cara = identity(&global, "cara@example.com", "Cara").await;
    member(&global, tenant_id, cara, "{scoped}", false).await;

    Fixture {
        _pg: pg,
        app: router(InsightsState {
            global: global.clone(),
        }),
        key: issue(&global, tenant_id, Some(ada)).await,
        scoped_key: issue(&global, tenant_id, Some(cara)).await,
        mine,
        theirs,
        ada,
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

async fn member(global: &PgPool, tenant: Uuid, who: Uuid, roles: &str, admin: bool) {
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, $3::text[], $4)",
    )
    .bind(tenant)
    .bind(who)
    .bind(roles)
    .bind(admin)
    .execute(global)
    .await
    .expect("membership");
}

async fn issue(global: &PgPool, tenant: Uuid, who: Option<Uuid>) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}') ",
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

async fn asset(pool: &PgPool, name: &str, mime: &str, bytes: i64, age_days: i64) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, created_at) \
         VALUES ($1, $2, $3, $4, $5, $1, now() - ($6 || ' days')::interval)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.bin"))
    .bind(mime)
    .bind(bytes)
    .bind(age_days)
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn download(pool: &PgPool, asset_id: Uuid, days_ago: i64, who: Option<Uuid>) {
    sqlx::query(
        "INSERT INTO rights_usage (id, asset_id, downloads, source, recorded_by, recorded_at) \
         VALUES (gen_random_uuid(), $1, 1, 'download', $2, now() - ($3 || ' days')::interval)",
    )
    .bind(asset_id)
    .bind(who)
    .bind(days_ago)
    .execute(pool)
    .await
    .expect("download");
}

async fn event(pool: &PgPool, kind: &str, asset_id: Uuid, actor: Option<Uuid>, days_ago: i64) {
    sqlx::query(
        "INSERT INTO events (id, occurred_at, kind, asset_id, actor_id, actor_kind) \
         VALUES (gen_random_uuid(), now() - ($1 || ' days')::interval, $2, $3, $4, 'user')",
    )
    .bind(days_ago)
    .bind(kind)
    .bind(asset_id)
    .bind(actor)
    .execute(pool)
    .await
    .expect("event");
}

async fn get(f: &Fixture, path: &str, key: Option<&str>) -> (StatusCode, Vec<u8>) {
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
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

async fn json(f: &Fixture, path: &str, key: Option<&str>) -> (StatusCode, Value) {
    let (status, bytes) = get(f, path, key).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn text(f: &Fixture, path: &str, key: &str) -> (StatusCode, String) {
    let (status, bytes) = get(f, path, Some(key)).await;
    (status, String::from_utf8(bytes).expect("utf-8"))
}

async fn one_request_answers_the_whole_screen(f: &Fixture) {
    let (status, body) = json(f, "/insights?days=7", Some(&f.key)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["days"], 7);

    // The spine, so a chart has no holes to draw a straight line across.
    let series = body["series"].as_array().expect("series");
    assert_eq!(series.len(), 7);
    let downloads: i64 = series
        .iter()
        .map(|day| day["downloads"].as_i64().unwrap_or_default())
        .sum();
    assert_eq!(downloads, 3, "all three, for a reader who sees everything");

    // Both lists, the storage breakdown and the people, from the same call.
    assert_eq!(body["most_downloaded"].as_array().expect("top").len(), 2);
    let classes = body["by_class"].as_array().expect("classes");
    assert_eq!(classes.len(), 2);
    assert_eq!(classes[0]["class"], "video", "largest by bytes first");
    assert_eq!(classes[0]["bytes"], 900_000_000i64);
    assert!(!body["contributors"].as_array().expect("people").is_empty());

    // The total, not the page length. Both assets here have been downloaded, so it is zero — and the field
    // exists because a capped list of unused assets reads as the whole problem.
    assert_eq!(body["never_downloaded_total"], 0);
}

async fn the_window_it_answers_with_is_the_window_it_used(f: &Fixture) {
    // A screen has to be able to label the chart. Echoing the request rather than the clamp would put "last
    // 3650 days" over a year of data.
    let (status, body) = json(f, "/insights?days=3650", Some(&f.key)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["days"], 366);
    assert_eq!(body["series"].as_array().expect("series").len(), 366);

    // And the low end. Zero days is one day, not an empty chart with nothing to say why.
    let (_, body) = json(f, "/insights?days=0", Some(&f.key)).await;
    assert_eq!(body["days"], 1);
    assert_eq!(body["series"].as_array().expect("series").len(), 1);

    // Absent is a sensible month rather than a refusal: the screen's own default.
    let (status, body) = json(f, "/insights", Some(&f.key)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["days"], 30);
}

async fn a_scoped_reader_gets_their_own_numbers(f: &Fixture) {
    let (status, body) = json(f, "/insights?days=7", Some(&f.scoped_key)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let downloads: i64 = body["series"]
        .as_array()
        .expect("series")
        .iter()
        .map(|day| day["downloads"].as_i64().unwrap_or_default())
        .sum();
    // One, not "three of which one is yours". A total with their share beneath it would tell them exactly how
    // much of the library they cannot reach.
    assert_eq!(downloads, 1);

    let top = body["most_downloaded"].as_array().expect("top");
    assert_eq!(top.len(), 1);
    assert_eq!(top[0]["asset_id"], f.mine.to_string());

    // The storage breakdown too: no video row at all, so the byte total is theirs.
    let classes = body["by_class"].as_array().expect("classes");
    assert_eq!(classes.len(), 1);
    assert_eq!(classes[0]["class"], "image");
    assert_eq!(classes[0]["bytes"], 4_000_000);

    // And Ada uploaded two, of which this reader sees one — which is why the screen must not present this as a
    // performance measure.
    let people = body["contributors"].as_array().expect("people");
    assert_eq!(people.len(), 1);
    assert_eq!(people[0]["person"]["id"], f.ada.to_string());
    assert_eq!(people[0]["uploads"], 1);
}

async fn a_contributor_with_no_identity_left_is_dropped_not_shown_as_a_uuid(f: &Fixture) {
    let (_, body) = json(f, "/insights?days=7", Some(&f.key)).await;
    let people = body["contributors"].as_array().expect("people");
    // Two actors uploaded; only one of them still resolves to a person.
    assert_eq!(people.len(), 1, "{people:?}");
    assert_eq!(people[0]["person"]["name"], "Ada");
    // The email is there for the same reason the comment picker carries it: two colleagues can share a name.
    assert_eq!(people[0]["person"]["email"], "ada@example.com");
}

async fn an_export_is_the_same_query_with_the_same_scope(f: &Fixture) {
    let (status, csv) = text(f, "/insights/export.csv?report=activity&days=7", &f.key).await;
    assert_eq!(status, StatusCode::OK, "{csv}");
    assert!(
        csv.starts_with("day,uploads,downloads,edits,comments,shares\n"),
        "{csv}"
    );
    assert_eq!(csv.lines().count(), 8, "a header and seven days");

    let (status, csv) = text(
        f,
        "/insights/export.csv?report=most-downloaded&days=7",
        &f.key,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(csv.contains("theirs.bin"), "{csv}");

    // The same file for the scoped reader carries their rows, not a wider set. An export that ignored the
    // predicate would be the most complete disclosure on the whole surface.
    let (status, csv) = text(
        f,
        "/insights/export.csv?report=most-downloaded&days=7",
        &f.scoped_key,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(csv.contains("mine.bin"), "{csv}");
    assert!(!csv.contains("theirs.bin"), "{csv}");
    let _ = f.theirs;

    // Never-downloaded has no per-row count column, because there is no count to put in it.
    let (status, csv) = text(f, "/insights/export.csv?report=never-downloaded", &f.key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(csv.lines().next(), Some("filename,mime,asset_id"));

    let (status, csv) = text(f, "/insights/export.csv?report=storage", &f.key).await;
    assert_eq!(status, StatusCode::OK);
    assert!(csv.contains("video,1,900000000"), "{csv}");

    let (status, csv) = text(f, "/insights/export.csv?report=contributors&days=7", &f.key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        csv.lines().next(),
        Some("person,email,uploads,edits,comments")
    );
    assert!(csv.contains("Ada,ada@example.com,2,0,0"), "{csv}");
}

async fn an_export_is_a_file_rather_than_a_wall_of_commas(f: &Fixture) {
    let mut request = Request::builder()
        .method("GET")
        .uri("/insights/export.csv?report=storage");
    request = request.header(header::AUTHORIZATION, format!("Bearer {}", f.key));
    let response = f
        .app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let headers = response.headers();
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/csv; charset=utf-8")
    );
    assert_eq!(
        headers
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok()),
        Some("attachment; filename=\"storage-by-class.csv\"")
    );
}

async fn a_report_this_build_does_not_know_is_refused(f: &Fixture) {
    // A closed set, so a typo is a refusal rather than an empty file that looks like "no activity".
    let (status, _) = json(f, "/insights/export.csv?report=everything", Some(&f.key)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // And no report at all is the same: there is no sensible default file.
    let (status, _) = json(f, "/insights/export.csv", Some(&f.key)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn an_unauthenticated_caller_sees_nothing(f: &Fixture) {
    let (status, _) = json(f, "/insights", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = json(f, "/insights/export.csv?report=storage", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_insights_contract_holds() {
    let f = fixture().await;

    one_request_answers_the_whole_screen(&f).await;
    the_window_it_answers_with_is_the_window_it_used(&f).await;
    a_scoped_reader_gets_their_own_numbers(&f).await;
    a_contributor_with_no_identity_left_is_dropped_not_shown_as_a_uuid(&f).await;
    an_export_is_the_same_query_with_the_same_scope(&f).await;
    an_export_is_a_file_rather_than_a_wall_of_commas(&f).await;
    a_report_this_build_does_not_know_is_refused(&f).await;
    an_unauthenticated_caller_sees_nothing(&f).await;
}
