//! The per-tenant index pool (2.6), ARCHITECTURE §19.
//!
//! The test TASKS.md names is here: **1,000 tenants do not open 1,000 indexes.** One index per tenant is
//! the right isolation — a query cannot reach another tenant's documents because they are not in the file
//! the searcher opened — but each open index carries file handles and segment-reader heap, and the
//! file-handle limit is reached long before the memory is.
//!
//! What makes the test meaningful rather than decorative is the second half: the tenants that were evicted
//! must still be *searchable*, and their documents must still be there. A pool that satisfied the count by
//! losing data would pass a naive version of this.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::TenantSlug;
use dam_core::fields::{Constraints, FieldDef, FieldKind};
use dam_search::schema::{self, IndexSchema};
use dam_search::{IndexPool, PoolConfig};
use tantivy::TantivyDocument;
use tantivy::collector::Count;
use tantivy::query::AllQuery;
use tantivy::schema::Value as _;

fn def(key: &str, kind: FieldKind) -> FieldDef {
    FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued: false,
        required: false,
        read_only: false,
        ai_writable: false,
        constraints: Constraints::default(),
    }
}

fn index_schema() -> IndexSchema {
    IndexSchema::new(vec![
        def("brand", FieldKind::Text),
        def("year", FieldKind::Int),
    ])
}

fn slug(n: usize) -> TenantSlug {
    TenantSlug::new(&format!("t{n}")).expect("slug")
}

/// Writes one document naming the tenant, and commits.
async fn write_one(pool: &IndexPool, schema: &IndexSchema, tenant: &TenantSlug) {
    let writer = pool.writer(tenant, schema).await.expect("writer");
    let mut guard = writer.lock().await;
    let mut doc = TantivyDocument::new();
    doc.add_text(schema.asset_id(), format!("asset-of-{}", tenant.as_str()));
    doc.add_bool(schema.deleted(), false);
    doc.add_text(schema.text(), format!("belongs to {}", tenant.as_str()));
    guard.add_document(doc).expect("add");
    guard.commit().expect("commit");
}

#[tokio::test]
async fn a_thousand_tenants_do_not_open_a_thousand_indexes() {
    // The named test. Deliberately writing to each tenant rather than only opening them, so the pool is
    // exercised the way a deployment exercises it — and so the assertion below about surviving data has
    // something to be about.
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = index_schema();
    let pool = IndexPool::new(
        PoolConfig::new(dir.path())
            .with_max_open_indexes(32)
            .with_max_open_writers(4)
            // Tantivy's floor. The arena size is not what this test is about, and 50 MB × a thousand
            // commits is a minute of fsync for no extra coverage.
            .with_writer_memory_bytes(15 * 1024 * 1024),
    );

    const TENANTS: usize = 1_000;
    // Every tenant is opened — that is the claim. A hundred of them are also written to and committed,
    // which is what gives the survival assertion below something to be about; a commit per tenant would
    // add nine hundred fsyncs and no coverage.
    for n in 0..TENANTS {
        if !(50..TENANTS - 50).contains(&n) {
            write_one(&pool, &schema, &slug(n)).await;
        } else {
            pool.get(&slug(n), &schema).await.expect("open");
        }
    }

    let open = pool.open_count().await;
    assert!(
        open <= 32,
        "the pool must hold at most its configured 32 indexes open, got {open}"
    );
    assert!(
        pool.cold_opens() >= TENANTS,
        "each tenant must have been opened at least once, got {} cold opens",
        pool.cold_opens()
    );

    // The half that makes the count meaningful. An evicted index is closed, not destroyed — so a tenant
    // from the very beginning of the run must still be searchable, with its document intact.
    let first = slug(0);
    let reopened = pool.get(&first, &schema).await.expect("cold open");
    let searcher = reopened.searcher();
    assert_eq!(
        searcher.search(&AllQuery, &Count).expect("search"),
        1,
        "an evicted tenant's documents must survive: eviction closes the index, it does not delete it"
    );

    let doc: TantivyDocument = searcher
        .doc(
            searcher.segment_readers()[0]
                .doc_ids_alive()
                .next()
                .map(|id| tantivy::DocAddress::new(0, id))
                .expect("a live doc"),
        )
        .expect("doc");
    let stored = doc
        .get_first(schema.asset_id())
        .and_then(|value| value.as_str().map(str::to_owned))
        .expect("the stored asset id");
    assert_eq!(
        stored, "asset-of-t0",
        "and it must be *that tenant's* document, not one it inherited from a neighbour"
    );
}

#[tokio::test]
async fn each_tenant_sees_only_its_own_documents() {
    // D2's isolation applied to search. Not a property of the query layer at all: the documents are in
    // different files, so there is no filter to forget.
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = index_schema();
    let pool = IndexPool::new(PoolConfig::new(dir.path()));

    let acme = TenantSlug::new("acme").expect("slug");
    let globex = TenantSlug::new("globex").expect("slug");
    write_one(&pool, &schema, &acme).await;
    for _ in 0..3 {
        write_one(&pool, &schema, &globex).await;
    }

    for (tenant, expected) in [(&acme, 1), (&globex, 3)] {
        let open = pool.get(tenant, &schema).await.expect("open");
        open.reload().expect("reload");
        assert_eq!(
            open.searcher().search(&AllQuery, &Count).expect("search"),
            expected,
            "{} must see exactly its own documents",
            tenant.as_str()
        );
    }
}

#[tokio::test]
async fn a_warm_tenant_is_not_reopened() {
    // The pool's whole purpose. Without this the cache is a memory leak with extra steps.
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = index_schema();
    let pool = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("warm").expect("slug");

    pool.get(&tenant, &schema).await.expect("first");
    let after_first = pool.cold_opens();
    for _ in 0..50 {
        pool.get(&tenant, &schema).await.expect("cached");
    }
    assert_eq!(
        pool.cold_opens(),
        after_first,
        "a warm tenant must be served from the pool, not reopened per request"
    );
}

#[tokio::test]
async fn an_index_survives_the_pool_that_created_it() {
    // The cold-open path proper: a fresh process opens what a previous one wrote. This is also what
    // makes eviction safe to reason about — the durable state is the directory, not the cache.
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = index_schema();
    let tenant = TenantSlug::new("persisted").expect("slug");

    {
        let pool = IndexPool::new(PoolConfig::new(dir.path()));
        write_one(&pool, &schema, &tenant).await;
    }

    let fresh = IndexPool::new(PoolConfig::new(dir.path()));
    let open = fresh.get(&tenant, &schema).await.expect("cold open");
    assert_eq!(
        open.searcher().search(&AllQuery, &Count).expect("search"),
        1,
        "a committed document must be there after the pool is gone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_requests_for_a_cold_tenant_open_it_once() {
    // A cold tenant's first traffic arrives as several requests at once, not one. Opening per request
    // would multiply the cold-open cost by the concurrency exactly when the tenant is slowest.
    //
    // **Multi-threaded on purpose.** The default `#[tokio::test]` runtime is single-threaded, and the
    // open is synchronous — so under it the sixteen tasks never actually overlap and this test passes
    // whether or not the pool collapses anything. It did, before `try_get_with` replaced a
    // get-then-insert that had exactly that race in it.
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = index_schema();
    let pool = std::sync::Arc::new(IndexPool::new(PoolConfig::new(dir.path())));
    let tenant = TenantSlug::new("stampede").expect("slug");

    let mut handles = Vec::new();
    for _ in 0..16 {
        let pool = std::sync::Arc::clone(&pool);
        let schema = schema.clone();
        let tenant = tenant.clone();
        handles.push(tokio::spawn(async move {
            pool.get(&tenant, &schema).await.expect("open");
        }));
    }
    for handle in handles {
        handle.await.expect("join");
    }

    assert_eq!(
        pool.cold_opens(),
        1,
        "sixteen concurrent first requests must collapse into one open, got {}",
        pool.cold_opens()
    );
}

#[tokio::test]
async fn eviction_closes_the_index_without_losing_committed_documents() {
    // Stated separately from the thousand-tenant case because it is the property that makes eviction
    // acceptable at all, and it deserves a test that fails for one reason.
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = index_schema();
    let pool = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("evicted").expect("slug");

    write_one(&pool, &schema, &tenant).await;
    pool.evict(&tenant).await;
    assert_eq!(pool.open_count().await, 0);

    let reopened = pool.get(&tenant, &schema).await.expect("reopen");
    assert_eq!(
        reopened
            .searcher()
            .search(&AllQuery, &Count)
            .expect("search"),
        1
    );
}

#[tokio::test]
async fn a_tenant_directory_is_named_from_the_slug_and_cannot_escape_the_root() {
    // `TenantSlug` has already restricted itself to lowercase ASCII, digits and underscore, so this is a
    // structural property rather than a sanitising step — but a test pins it, because the day someone
    // widens the slug rules this is the thing that quietly stops being true.
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = IndexPool::new(PoolConfig::new(dir.path()));
    let tenant = TenantSlug::new("acme").expect("slug");

    let path = pool.directory(&tenant);
    assert!(path.starts_with(dir.path()), "got {path:?}");
    assert!(path.ends_with("acme"), "got {path:?}");
    assert!(TenantSlug::new("../etc").is_err());
    assert!(TenantSlug::new("a/b").is_err());
}

#[test]
fn the_fixed_field_names_are_stable() {
    // They are on disk. Renaming one makes every existing index unreadable, so the constants are asserted
    // to make that a deliberate change with a migration rather than a refactor.
    assert_eq!(schema::ASSET_ID, "asset_id");
    assert_eq!(schema::GROUP_IDS, "group_ids");
    assert_eq!(schema::DELETED, "deleted");
    assert_eq!(schema::TEXT_BLOB, "text");
    assert_eq!(schema::METADATA, "metadata");
}
