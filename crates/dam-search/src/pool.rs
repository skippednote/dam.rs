//! The per-tenant index pool (2.6), ARCHITECTURE §19.
//!
//! One index per tenant is the right isolation — it is D2 applied to search, and it means a query can
//! never reach another tenant's documents because they are not in the file the searcher opened. But 1,000
//! simultaneously-open Tantivy indexes is not viable: each carries file handles and segment-reader heap,
//! and the file-handle limit alone is reached long before the memory is.
//!
//! So the pool keeps the *active working set* open and opens cold tenants on demand. §19 names the
//! consequence to watch — cold-open latency sits on the p99 — and this module is where it is measurable.
//!
//! ## Writers are scarcer than readers
//!
//! Tantivy permits **one writer per index**, and a writer holds a memory arena sized in tens of megabytes.
//! So writers are pooled separately and much more tightly than readers: a reader is cheap and shared, a
//! writer is exclusive and expensive. Holding both in one cache would size the whole thing to the writer
//! and evict readers that cost nothing to keep.
//!
//! ## Eviction is not deletion
//!
//! An evicted index is closed, not removed. Reopening it is the cold-open path, and the only thing lost is
//! the warm segment readers. That is worth being explicit about because the alternative reading — that
//! eviction loses data — would make the whole design look reckless: the durable state is on disk, and a
//! writer is committed before it can be dropped.

use crate::schema::IndexSchema;
use crate::{Error, Result};
use dam_core::TenantSlug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tantivy::{Index, IndexReader, IndexWriter};

/// How the pool is sized.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Indexes kept open. §19's "active working set".
    ///
    /// The default is deliberately far below any plausible tenant count: the pool exists because holding
    /// them all open is the thing that does not work, so a default sized to "most deployments" would
    /// quietly reintroduce the problem for the deployments that matter.
    pub max_open_indexes: u64,
    /// Writers kept open. Much smaller — see the module docs.
    pub max_open_writers: u64,
    /// Arena per writer, in bytes. Tantivy's minimum is 15 MB.
    pub writer_memory_bytes: usize,
    /// Where each tenant's index lives.
    pub root: PathBuf,
}

impl PoolConfig {
    /// A configuration rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            max_open_indexes: 64,
            max_open_writers: 8,
            writer_memory_bytes: 50 * 1024 * 1024,
            root: root.into(),
        }
    }

    #[must_use]
    pub fn with_max_open_indexes(mut self, max: u64) -> Self {
        self.max_open_indexes = max;
        self
    }

    #[must_use]
    pub fn with_max_open_writers(mut self, max: u64) -> Self {
        self.max_open_writers = max;
        self
    }

    /// Sets the per-writer arena. Tantivy's floor is 15 MB.
    #[must_use]
    pub fn with_writer_memory_bytes(mut self, bytes: usize) -> Self {
        self.writer_memory_bytes = bytes;
        self
    }
}

/// An open index and its reader.
#[derive(Clone)]
pub struct OpenIndex {
    index: Index,
    reader: IndexReader,
}

impl std::fmt::Debug for OpenIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenIndex").finish_non_exhaustive()
    }
}

impl OpenIndex {
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// A searcher over the last committed state.
    ///
    /// Reload is on-commit, so a searcher taken here reflects everything committed before it was taken and
    /// nothing after. A search that silently included uncommitted documents would make the differential
    /// test against SQL non-deterministic.
    pub fn searcher(&self) -> tantivy::Searcher {
        self.reader.searcher()
    }

    /// Picks up commits made since the last searcher.
    pub fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }
}

/// The LRU-pooled set of open indexes.
pub struct IndexPool {
    indexes: moka::future::Cache<String, Arc<OpenIndex>>,
    writers: moka::future::Cache<String, Arc<tokio::sync::Mutex<IndexWriter>>>,
    config: PoolConfig,
    /// How many times an index has been opened from disk.
    ///
    /// The pool's own instrumentation rather than a test hook: cold-open count over time is how a
    /// deployment discovers its working set does not fit, and §19 lists that as an unknown to measure.
    cold_opens: AtomicUsize,
}

impl std::fmt::Debug for IndexPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexPool")
            .field("open_indexes", &self.indexes.entry_count())
            .field("max_open_indexes", &self.config.max_open_indexes)
            .field("cold_opens", &self.cold_opens.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl IndexPool {
    pub fn new(config: PoolConfig) -> Self {
        Self {
            indexes: moka::future::Cache::builder()
                .max_capacity(config.max_open_indexes)
                .build(),
            writers: moka::future::Cache::builder()
                .max_capacity(config.max_open_writers)
                .build(),
            config,
            cold_opens: AtomicUsize::new(0),
        }
    }

    /// The directory a tenant's index lives in.
    ///
    /// Built from `TenantSlug`, which has already restricted itself to lowercase ASCII, digits and
    /// underscore — so this cannot escape `root` however the slug arrived.
    pub fn directory(&self, tenant: &TenantSlug) -> PathBuf {
        self.config.root.join(tenant.as_str())
    }

    /// Indexes currently held open.
    ///
    /// `run_pending_tasks` first, because moka evicts asynchronously and the count is otherwise a
    /// lagging indicator — which would make any assertion about pool size flaky rather than wrong.
    pub async fn open_count(&self) -> u64 {
        self.indexes.run_pending_tasks().await;
        self.indexes.entry_count()
    }

    /// How many times an index has been opened from disk since this pool was created.
    pub fn cold_opens(&self) -> usize {
        self.cold_opens.load(Ordering::Relaxed)
    }

    /// Opens a tenant's index, creating it if it does not exist.
    ///
    /// Idempotent and safe to call per request: a hit costs a cache lookup, and a miss costs the cold
    /// open. `get_with` collapses concurrent misses for the same tenant into one open, which matters
    /// because a cold tenant's first traffic tends to arrive as several requests at once rather than one.
    pub async fn get(&self, tenant: &TenantSlug, schema: &IndexSchema) -> Result<Arc<OpenIndex>> {
        let key = tenant.as_str().to_owned();
        let directory = self.directory(tenant);
        let schema = schema.clone();
        let slug = key.clone();

        // `try_get_with`, not get-then-insert. Get-then-insert reads as if it collapses concurrent misses
        // and does not: sixteen requests all miss, all open, and fifteen opens are discarded. It *looks*
        // correct on a single-threaded runtime because the open is synchronous and therefore atomic there,
        // which is how a test for this can pass while the property does not hold in a server.
        self.indexes
            .try_get_with(key, async move {
                // `spawn_blocking`, because opening an index is file I/O and this runs on the request
                // path. A synchronous open on an async worker stalls every other request sharing that
                // thread — and the cold-open path is exactly when the tenant is already slowest.
                let opened =
                    tokio::task::spawn_blocking(move || open_or_create(&directory, &schema))
                        .await
                        .map_err(|e| {
                            Error::ColdOpen(slug.clone(), format!("open task failed: {e}"))
                        })?
                        .map_err(|e| Error::ColdOpen(slug, e.to_string()))?;
                self.cold_opens.fetch_add(1, Ordering::Relaxed);
                Ok(Arc::new(opened))
            })
            .await
            .map_err(|e: Arc<Error>| Error::Tantivy(e.to_string()))
    }

    /// A tenant's writer, held exclusively for the duration of the guard.
    ///
    /// Tantivy permits one writer per index, so this is a `Mutex` rather than a clone: two concurrent
    /// writers would be a Tantivy error at best, and the mutex turns that into ordinary contention.
    pub async fn writer(
        &self,
        tenant: &TenantSlug,
        schema: &IndexSchema,
    ) -> Result<Arc<tokio::sync::Mutex<IndexWriter>>> {
        let key = tenant.as_str().to_owned();
        if let Some(writer) = self.writers.get(&key).await {
            return Ok(writer);
        }

        let open = self.get(tenant, schema).await?;
        let writer = open
            .index()
            .writer::<tantivy::TantivyDocument>(self.config.writer_memory_bytes)?;
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        self.writers.insert(key, Arc::clone(&writer)).await;
        Ok(writer)
    }

    /// Drops a tenant's cached index and writer.
    ///
    /// For a tenant that has just been deprovisioned, and for a schema change that needs a fresh open.
    /// Nothing is deleted from disk: eviction closes, it does not destroy.
    pub async fn evict(&self, tenant: &TenantSlug) {
        let key = tenant.as_str().to_owned();
        self.writers.invalidate(&key).await;
        self.indexes.invalidate(&key).await;
    }
}

/// Opens the index at `directory`, creating it if absent.
fn open_or_create(directory: &Path, schema: &IndexSchema) -> Result<OpenIndex> {
    std::fs::create_dir_all(directory)?;

    // `open_in_dir` first, then create. The other order would need a "does a meta.json exist" check,
    // which is the same test with a race in it.
    let index = match Index::open_in_dir(directory) {
        Ok(index) => index,
        Err(_) => Index::create_in_dir(directory, schema.schema().clone())?,
    };

    let reader = index
        .reader_builder()
        // On commit rather than on a timer: a timer makes a read-after-write flaky, and a search that
        // sometimes reflects the last write is worse to debug than one that never does.
        .reload_policy(tantivy::ReloadPolicy::OnCommitWithDelay)
        .try_into()?;

    Ok(OpenIndex { index, reader })
}
