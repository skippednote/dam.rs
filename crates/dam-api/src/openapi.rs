//! The OpenAPI document — one source of truth for the wire contract (§14.1, F.3).
//!
//! The document is generated from `utoipa` annotations, checked into the repository as
//! `openapi.json`, and consumed by `openapi-typescript` to produce the frontend's types. Three
//! properties make that a gate rather than a convention:
//!
//! - **Emission is deterministic.** The drift check regenerates and diffs, so a document whose key
//!   order varied between runs would fail CI at random and be disabled within a week.
//! - **The checked-in copy is asserted to match.** A stale `openapi.json` fails `mise run check`,
//!   before a push rather than after.
//! - **The frontend imports the generated types.** A variant removed here becomes a TypeScript error
//!   there, which is the only version of this gate that survives contact with a deadline.
//!
//! Paths are absent for now: the HTTP surface is stopped pending the authentication decisions in
//! `NEEDS-REVIEW.md`. The wiring is what F.3 asked for, and endpoints flow through it as they land.

use utoipa::OpenApi;

/// The document root.
#[derive(Debug, OpenApi)]
#[openapi(
    info(
        title = "damrs API",
        description = "Digital asset management: ingest, search, rights, provenance.",
        license(name = "AGPL-3.0-or-later")
    ),
    components(schemas(
        crate::dto::AssetSummary,
        crate::dto::AssetPage,
        dam_core::AssetTier,
        dam_core::RightsState,
        dam_core::ProvenanceState,
        dam_core::StorageClass,
        dam_core::LatencyClass,
        dam_core::RestoreState,
        dam_core::RestoreTier,
        dam_core::PlacementState,
    ))
)]
pub struct ApiDoc;

/// The document as pretty-printed JSON, with a trailing newline.
///
/// Pretty-printed because it is reviewed in diffs: a single-line document turns every change into an
/// unreadable one. The trailing newline keeps it a well-formed text file, so a later change does not
/// touch the last line for no reason.
///
/// Fallible rather than panicking. Serialising a static document is not a realistic failure, but a
/// library that panics takes its host process with it — and this one is linked into `damd`, not just
/// into a build script.
pub fn document_json() -> Result<String, serde_json::Error> {
    let mut json = ApiDoc::openapi().to_pretty_json()?;
    if !json.ends_with('\n') {
        json.push('\n');
    }
    Ok(json)
}
