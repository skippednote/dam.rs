//! The OpenAPI document (F.3): one source of truth for the wire vocabulary.
//!
//! §14.1 puts it plainly — "OpenAPI → TS generation from `utoipa`. One source of truth; drift becomes
//! a build error." This suite is the Rust half of that gate. The frontend half is a type-level check
//! in `web/`, and together they mean a backend enum losing a variant cannot reach a deployed UI that
//! still renders it.
//!
//! The document is **checked in** rather than generated on demand, for the same reason a lockfile is:
//! a reviewer should see the wire contract change in the diff, not discover it at runtime.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_api::openapi;

/// Variant lists as the database CHECK constraints define them.
///
/// Hard-coded on purpose. Deriving them from the Rust enums would make this test tautological — it
/// would pass however far either drifted from the schema, which is the failure it exists to prevent.
const RIGHTS_STATES: &[&str] = &["allowed", "expiring", "denied", "unknown"];
const PROVENANCE_STATES: &[&str] = &["none", "valid", "invalid", "untrusted"];
const STORAGE_CLASSES: &[&str] = &[
    "STANDARD",
    "STANDARD_IA",
    "ONEZONE_IA",
    "INTELLIGENT_TIERING",
    "GLACIER_IR",
    "GLACIER",
    "DEEP_ARCHIVE",
];
const PLACEMENT_STATES: &[&str] = &[
    "uploading",
    "present",
    "transitioning",
    "missing",
    "corrupt",
    "deleting",
];

fn json() -> String {
    openapi::document_json().expect("the document must serialise")
}

fn document() -> serde_json::Value {
    serde_json::from_str(&json()).expect("the document must be valid JSON")
}

fn schema_enum(doc: &serde_json::Value, name: &str) -> Vec<String> {
    let schema = doc
        .pointer(&format!("/components/schemas/{name}"))
        .unwrap_or_else(|| panic!("no schema named {name}; found {:?}", schema_names(doc)));
    schema
        .get("enum")
        .and_then(|e| e.as_array())
        .unwrap_or_else(|| panic!("{name} is not an enum schema: {schema}"))
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_owned())
        .collect()
}

fn schema_names(doc: &serde_json::Value) -> Vec<String> {
    doc.pointer("/components/schemas")
        .and_then(|s| s.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn the_document_is_valid_openapi_3_with_a_title_and_version() {
    let doc = document();
    let version = doc
        .get("openapi")
        .and_then(|v| v.as_str())
        .expect("an openapi version");
    assert!(version.starts_with("3."), "got {version}");
    assert!(
        doc.pointer("/info/title")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.contains("damrs")),
        "the title identifies the API to whoever generates a client from it"
    );
}

#[test]
fn every_wire_enum_matches_its_database_check_constraint() {
    // The whole point of generating the client: a variant the database can store but the API cannot
    // name is a value the UI receives and cannot render.
    let doc = document();
    assert_eq!(schema_enum(&doc, "RightsState"), RIGHTS_STATES);
    assert_eq!(schema_enum(&doc, "ProvenanceState"), PROVENANCE_STATES);
    assert_eq!(schema_enum(&doc, "StorageClass"), STORAGE_CLASSES);
    assert_eq!(schema_enum(&doc, "PlacementState"), PLACEMENT_STATES);
}

#[test]
fn the_document_is_byte_identical_across_emissions() {
    // The drift check regenerates and diffs, so a document whose key order or formatting varied
    // between runs would fail CI at random and be disabled within a week.
    assert_eq!(json(), json());
}

#[test]
fn the_checked_in_document_matches_what_the_code_emits() {
    // The gate itself, in the Rust suite rather than only in CI, so `mise run check` catches drift
    // before a push — which is where it is cheap to fix.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi.json");
    let checked_in = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("openapi.json is missing ({e}); run `cargo run -p damctl -- openapi --write`")
    });
    assert_eq!(
        checked_in,
        json(),
        "openapi.json is stale. Regenerate with `cargo run -p damctl -- openapi --write` and commit \
         it — the wire contract belongs in the diff, not in a runtime surprise."
    );
}

#[test]
fn the_document_ends_with_a_newline_so_it_is_a_well_formed_text_file() {
    // Without one, every future diff touches the last line and buries the actual change.
    assert!(json().ends_with('\n'));
}

/// Every documented path is actually served by the app router.
///
/// The guard that was missing. Each endpoint's own suite builds *its* router directly, so a module can be fully
/// tested and still not be mounted — which is exactly what happened to `/upload-profiles`: seven passing API
/// cases, a correct OpenAPI entry, and a 404 from the running server, because the merge into `app::router` was
/// never applied. The document and the router are two lists that must agree, and nothing compared them.
///
/// Probed with each path's own documented method, and with no credentials. A mounted route refuses with 401 or
/// 403 because authentication runs first; an unmounted path is 404 from axum's routing table. That is the whole
/// signal, and it needs no database — no handler runs.
///
/// OPTIONS would have been the obvious probe and is the wrong one: the CORS layer answers a preflight for any
/// path at all, so every probe came back 200 and the guard passed while three routers were removed. Found by
/// mutation-testing the guard itself.
/// The connector-facing endpoints must not receive the deployment-wide CORS policy.
///
/// This is a regression test for a bug that every endpoint test missed and one `curl` found. `/browse` and
/// `/oembed` set `Access-Control-Allow-Origin` themselves, per connector, because a browse token lives in a
/// browser and the only origin that should be able to read with it is that connector's own `site_url`.
/// `tower_http`'s `CorsLayer` **overwrites** that header — so while those two were mounted under the global
/// layer, the deployed answer was `*` in development and whatever `server.allowed_origins` listed in
/// production. Their own tests drove the routers in isolation, where no global layer exists, so they passed.
///
/// Asserted structurally rather than by driving a request: with nothing connected, `/browse` answers 401 before
/// reaching the code that sets the header, so what is observable is exactly the thing that was wrong — whether
/// the global layer touched the response at all.
#[tokio::test]
async fn the_connector_endpoints_keep_their_own_cors_policy() {
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt as _;

    let app = dam_api::app::router(
        &dam_core::config::Config::default(),
        route_inspection_deps(),
    );

    let allow_origin = async |path: &str| {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .header(header::ORIGIN, "https://somebody.example")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router");
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };

    // The ordinary API keeps the deployment-wide policy — permissive in development, configured in production.
    // Asserted so this test fails if the global layer is ever removed by accident rather than silently
    // passing for the wrong reason.
    assert_eq!(
        allow_origin("/assets").await.as_deref(),
        Some("*"),
        "the global CORS layer should still apply to the ordinary API",
    );

    for path in ["/browse", "/oembed?url=x"] {
        assert_eq!(
            allow_origin(path).await,
            None,
            "{path} must be mounted outside the global CORS layer, or its per-connector \
             Access-Control-Allow-Origin is overwritten",
        );
    }
}

#[tokio::test]
async fn every_documented_path_is_mounted_on_the_app_router() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    let doc = document();
    let app = dam_api::app::router(
        &dam_core::config::Config::default(),
        route_inspection_deps(),
    );

    let mut probed = 0usize;
    let mut unmounted = Vec::new();
    for (path, methods) in doc["paths"].as_object().expect("paths") {
        let method = methods
            .as_object()
            .expect("methods")
            .keys()
            .find(|name| {
                matches!(
                    name.as_str(),
                    "get" | "post" | "put" | "patch" | "delete" | "head"
                )
            })
            .cloned()
            .expect("every documented path has a method");

        // Template parameters get a syntactically valid stand-in. It is never dereferenced: the request is
        // refused at authentication, long before a handler reads a path parameter.
        let concrete = path
            .split('/')
            .map(|segment| {
                if segment.starts_with('{') {
                    "00000000-0000-4000-8000-000000000000"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.to_uppercase().as_str())
                    .uri(&concrete)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        probed += 1;
        if response.status() == StatusCode::NOT_FOUND {
            unmounted.push(format!("{} {path}", method.to_uppercase()));
        }
    }

    assert!(
        probed > 20,
        "the document should describe a real API: {probed} paths"
    );
    assert!(
        unmounted.is_empty(),
        "documented but not mounted on the app router: {unmounted:?}"
    );
}

/// Dependencies for a router that is only ever asked whether a path exists.
///
/// Nothing here is connected. `oneshot` with OPTIONS is answered by axum's routing table before any handler
/// runs, so a lazily-built pool that would fail on first use is never used — which is what keeps this guard
/// free of Docker and fast enough to run on every push.
fn route_inspection_deps() -> dam_api::app::AppDeps {
    let lazy = |name: &str| {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy(&format!("postgres://unused:unused@127.0.0.1:1/{name}"))
            .expect("a lazy pool connects to nothing")
    };
    dam_api::app::AppDeps {
        global: lazy("global"),
        delivery_pool: lazy("delivery"),
        store: std::sync::Arc::new(dam_store::FakeS3Store::with_test_clock().0),
        delivery_store: std::sync::Arc::new(dam_store::FakeS3Store::with_test_clock().0),
        indexes: std::sync::Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
            std::path::Path::new("/tmp/damrs-route-inspection"),
        ))),
        keyring: dam_core::signed_url::Keyring::single(
            "k1",
            dam_core::Secret::new("route-inspection".to_owned()),
        ),
        delivery_tenant: uuid::Uuid::nil(),
        delivery_tenant_slug: dam_core::TenantSlug::new("acme").expect("a slug"),
        // Never called: OPTIONS is answered by the routing table before any handler runs.
        model_transport: std::sync::Arc::new(dam_ai::testing::Recorded::always(
            200,
            serde_json::json!({}),
        )),
        // No protocol routers: this suite inspects the REST surface, and MCP frames its own requests rather
        // than answering OPTIONS.
        protocols: None,
    }
}
