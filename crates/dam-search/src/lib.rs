//! Tantivy index, pgvector, hybrid query planner, facets, query DSL.
//!
//! One index per tenant (D2's isolation, applied to search), held open by an LRU pool so a thousand
//! tenants do not mean a thousand open indexes — see [`pool`] and ARCHITECTURE §19.

pub mod document;
pub mod eval_run;
pub mod pool;
pub mod query;
pub mod reindex;
pub mod schema;

pub use document::AssetDocument;
pub use pool::{IndexPool, PoolConfig};
pub use schema::IndexSchema;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("tantivy: {0}")]
    Tantivy(String),

    #[error("index for tenant {0} could not be opened: {1}")]
    ColdOpen(String, String),

    /// A clause this back end cannot express. Refused rather than dropped: dropping a filter clause
    /// returns more than the caller asked for.
    #[error("this query cannot be answered by the index: {0}")]
    Unsupported(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<tantivy::TantivyError> for Error {
    fn from(error: tantivy::TantivyError) -> Self {
        Self::Tantivy(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
