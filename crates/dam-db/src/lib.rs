//! Postgres access: migration runner, tenant provisioning, `TenantConn`, job queue.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)
)]

pub mod access;
pub mod ai_credentials;
pub mod assets;
pub mod attachments;
pub mod auth;
pub mod auto_import;
pub mod bulk;
pub mod categories;
pub mod collections;
pub mod comments;
pub mod conversions;
pub mod derivatives;
pub mod engagement;
pub mod events;
pub mod facets;
pub mod fields;
pub mod jobs;
pub mod judgements;
pub mod metadata_types;
pub mod migrate;
pub mod orders;
pub mod paths;
pub mod provenance;
pub mod provision;
pub mod query_sql;
pub mod restores;
pub mod rights;
pub mod saved_searches;
pub mod shares;
pub mod taxonomy;
pub mod tenant_conn;
pub mod upload_profiles;
pub mod uploads;
pub mod usage;
pub mod versions;

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

    /// A worker tried to extend or finish a job it no longer holds — its lease was
    /// reclaimed and another worker may now own the job. Its own variant because the
    /// worker must stop work immediately rather than retry.
    #[error("lease lost for job {job_id} (worker {worker}); another worker may own it")]
    LeaseLost { job_id: uuid::Uuid, worker: String },

    /// A caller tried to advance an upload session the database has no record of. Its own
    /// variant because the caller believes it holds a live session: continuing would append to
    /// an upload that does not exist, so it must stop rather than retry.
    #[error("upload session `{0}` does not exist")]
    UploadGone(String),

    /// A configuration the code cannot yet act on. Its own variant because the honest response is to
    /// refuse rather than to approximate — see `access::check_groups_are_renderable`.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A row contradicts itself — a part count that disagrees with the part list, a status
    /// outside the vocabulary. Its own variant because it is neither a query failure nor a
    /// missing row: resuming from it would assemble the wrong bytes under a content-addressed
    /// key, which produces an object that looks canonical. Refusing is the only safe move.
    #[error("inconsistent row: {0}")]
    Inconsistent(String),

    #[error(transparent)]
    Core(#[from] dam_core::Error),
}

pub use tenant_conn::TenantConn;

pub type Result<T, E = Error> = std::result::Result<T, E>;
