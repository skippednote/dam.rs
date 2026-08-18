//! The ingest pipeline: what turns an upload into an asset, and an asset into its derivatives.
//!
//! This is 0.9's remainder and the reason the grid had no thumbnails. Every piece it needs already existed —
//! the resumable engine, the session repository, the probe, the render profiles, the derivative table, the
//! delivery chokepoint — and nothing connected them, so an upload landed in staging and stopped there.
//!
//! ## Why this is a library and not the worker binary
//!
//! Both stages touch a real object store and a real database, which means they need the testcontainers
//! harness — and an integration test cannot reach a binary's private modules. Putting the logic here also
//! means `damctl` can run a stage by hand, which is what makes a stuck asset recoverable without a queue.
//!
//! ## Every stage is idempotent, because the queue is at-least-once
//!
//! `jobs::claim` leases rather than deletes, so a worker that dies mid-job has its work re-run. A stage that
//! is not idempotent turns that into duplicate assets or double-charged storage. So: finalisation is keyed on
//! the upload session's own state, derivation on `(asset_id, op_hash)`, and both check before they write.

pub mod derive;
pub mod finalise;
pub mod worker;

/// Everything that can go wrong in a stage.
///
/// Distinguishes **permanent** from **transient**, because the queue's retry is the difference between a
/// self-healing failure and one that burns five attempts and lands in `dead`. A malformed file will never
/// parse however often it is retried; an S3 timeout usually will.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Retrying will not help: the input is what it is.
    #[error("permanent: {0}")]
    Permanent(String),

    /// Retrying might. The queue backs off and tries again.
    #[error("transient: {0}")]
    Transient(String),

    #[error("database: {0}")]
    Db(#[from] dam_db::Error),

    #[error("store: {0}")]
    Store(#[from] dam_store::Error),

    #[error("ingest: {0}")]
    Ingest(#[from] dam_media::ingest::Error),

    #[error("render: {0}")]
    Render(#[from] dam_media::derive::Error),
}

impl Error {
    /// Whether the queue should retry.
    ///
    /// A database or store error is assumed transient — a connection reset, a leader election, a timeout —
    /// and the attempt counter is what stops an infinite loop over a genuinely broken one. Guessing the other
    /// way would mean one flaky read permanently kills an asset's derivatives.
    pub fn is_transient(&self) -> bool {
        !matches!(self, Self::Permanent(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
