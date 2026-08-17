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
