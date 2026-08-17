//! Domain types, metadata schema engine, ABAC policy, errors.
//!
//! Depends on nothing internal. Everything else in the workspace depends on this,
//! so it stays free of infrastructure: no sqlx, no aws-sdk, no axum.

#![forbid(unsafe_code)]
// Same relaxation as the integration tests, for inline `mod tests`.
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)
)]

pub mod config;
pub mod error;
pub mod secret;
pub mod tenant;

pub use config::Config;
pub use error::{Error, ResourceKind, Result};
pub use secret::Secret;
pub use tenant::TenantSlug;
