//! axum router, auth, OpenAPI, webhooks.
//!
//! [`openapi`] is the wire contract, generated from `utoipa` and checked in as `openapi.json`.

pub mod ai;
pub mod app;
pub mod assets;
pub mod attachments;
pub mod auto_import;
pub mod bulk;
pub mod caller;
pub mod categories;
pub mod comments;
pub mod conversions;
pub mod csv_export;
pub mod dashboard;
pub mod delivery;
pub mod downloads;
pub mod dto;
pub mod engagement;
pub mod history;
pub mod openapi;
pub mod orders;
pub mod portals;
pub mod schema;
pub mod search;
pub mod shares;
pub mod tus;
pub mod upload_profiles;
pub mod versions;
