//! Postgres access: migration runner, tenant provisioning, `TenantConn`, job queue.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)
)]

pub mod migrate;
pub mod provision;
pub mod tenant_conn;

#[cfg(feature = "testing")]
pub mod testing;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(String),

    /// Test-harness failures. Their own variant so a container problem is never
    /// mistaken for a defect in the code under test.
    #[error("test harness: {0}")]
    Harness(String),

    /// The tenant's schema does not exist. Its own variant because "not provisioned"
    /// and "query failed" need different handling: the first is a 404 or a
    /// provisioning bug, the second is an incident.
    #[error("tenant schema `{0}` does not exist — tenant not provisioned")]
    TenantNotProvisioned(String),

    #[error(transparent)]
    Core(#[from] dam_core::Error),
}

pub use tenant_conn::TenantConn;

pub type Result<T, E = Error> = std::result::Result<T, E>;
