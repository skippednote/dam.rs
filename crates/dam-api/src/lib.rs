//! axum router, auth, OpenAPI, webhooks.
//!
//! [`openapi`] is the wire contract, generated from `utoipa` and checked in as `openapi.json`.

// The same allowance every other crate in the workspace carries. This crate had no unit tests until the
// throttle got some — its whole suite lives in `tests/`, where the lint does not apply — so the attribute was
// never needed here before.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod ai;
pub mod app;
pub mod archival;
pub mod assets;
pub mod attachments;
pub mod auto_import;
pub mod branding;
pub mod browse;
pub mod bulk;
pub mod caller;
pub mod categories;
pub mod collections;
pub mod comments;
pub mod connectors;
pub mod conversions;
pub mod csv_export;
pub mod dashboard;
pub mod delivery;
pub mod downloads;
pub mod dto;
pub mod duplicates;
pub mod engagement;
pub mod governance;
pub mod history;
pub mod insights;
pub mod observability;
pub mod oembed;
pub mod openapi;
pub mod orders;
pub mod portals;
pub mod proofing;
pub mod quotas;
pub mod references;
pub mod schema;
pub mod search;
pub mod shares;
pub mod throttle;
pub mod tus;
pub mod upload_profiles;
pub mod versions;
pub mod vocabularies;
pub mod webhooks;
pub mod worklists;
