//! axum router, auth, OpenAPI, webhooks.
//!
//! [`openapi`] is the wire contract, generated from `utoipa` and checked in as `openapi.json`.

pub mod app;
pub mod assets;
pub mod bulk;
pub mod caller;
pub mod delivery;
pub mod dto;
pub mod openapi;
pub mod schema;
pub mod search;
pub mod shares;
pub mod tus;
