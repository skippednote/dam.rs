//! The scrub, against a real store.
//!
//! Every case here is a shape a load run actually produced. Filling the disk under a single-node
//! SeaweedFS and letting it be killed left 608 objects gone and around 80 present-but-empty, and the
//! database went on reporting all of them as `active` at their recorded size. These are those two
//! shapes, plus the two that decide whether the report is worth reading: a store that cannot be
//! reached must not be recorded as loss, and a placement that gets fixed must be able to come back.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::{StorageClass, TenantSlug};
use dam_db::{integrity, migrate, testing::PostgresHarness};
use dam_store::testing::SeaweedfsHarness;
use dam_store::{BlobStore, Key, ResumableStore};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    _s3: SeaweedfsHarness,
    global: PgPool,
    tenant: PgPool,
    store: Arc<dyn ResumableStore>,
    slug: TenantSlug,
    tenant_id: Uuid,
    pool_id: Uuid,
    bucket: String,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let s3 = SeaweedfsHarness::start().await.expect("start seaweedfs");
    let store: Arc<dyn ResumableStore> = Arc::new(s3.store());

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant row");

    let pool_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.storage_pools \
         (id, tenant_id, name, driver, bucket, credentials_ref, latency_class) \
         VALUES (gen_random_uuid(), $1, 'hot', 's3', $2, 'test', 'instant') RETURNING id",
    )
    .bind(tenant_id)
    .bind(s3.bucket())
    .fetch_one(&global)
    .await
    .expect("storage pool");

    let bucket = s3.bucket().to_owned();
    Fixture {
        _pg: pg,
        _s3: s3,
        global,
        tenant,
        store,
        slug: TenantSlug::new("acme").expect("slug"),
        tenant_id,
        pool_id,
        bucket,
    }
}

/// Puts real bytes in the store and the placement row that claims them.
async fn place(f: &Fixture, hash: &str, bytes: &[u8]) -> Key {
    let key = Key::original(f.tenant_id, hash).expect("key");
    f.store
        .put(
            &key,
            bytes::Bytes::from(bytes.to_vec()),
            StorageClass::Standard,
        )
        .await
        .expect("put");

    // `placements_owner` insists on exactly one owner, so the asset comes first. A placement without
    // one is not a thing the schema will hold, which is the constraint doing its job.
    let asset_id: Uuid = sqlx::query_scalar(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES (gen_random_uuid(), $1, $2, 'application/octet-stream', $3, gen_random_uuid()) \
         RETURNING id",
    )
    .bind(hash)
    .bind(format!("{hash}.bin"))
    .bind(i64::try_from(bytes.len()).expect("size"))
    .fetch_one(&f.tenant)
    .await
    .expect("asset");

    sqlx::query(
        "INSERT INTO object_placements \
           (object_key, pool_id, asset_id, size_bytes, checksum, storage_class, state) \
         VALUES ($1, $2, $3, $4, $5, 'STANDARD', 'present')",
    )
    .bind(key.as_str())
    .bind(f.pool_id)
    .bind(asset_id)
    .bind(i64::try_from(bytes.len()).expect("size"))
    .bind(hash)
    .execute(&f.tenant)
    .await
    .expect("placement");

    key
}

async fn state_of(f: &Fixture, key: &Key) -> (String, Option<chrono::DateTime<chrono::Utc>>) {
    sqlx::query_as("SELECT state, last_verified_at FROM object_placements WHERE object_key = $1")
        .bind(key.as_str())
        .fetch_one(&f.tenant)
        .await
        .expect("read the placement back")
}

fn hash_of(n: u8) -> String {
    std::iter::repeat_n(format!("{n:02x}"), 32).collect()
}

#[tokio::test]
async fn the_scrub_holds() {
    // One fixture, several cases: a Postgres and a SeaweedFS container each take seconds to start, and
    // these cases share every part of that setup. Ordered, because the last one depends on the state
    // the third leaves behind.
    let f = fixture().await;

    an_object_that_matches_its_row_is_verified(&f).await;
    an_object_the_store_has_lost_is_recorded_missing(&f).await;
    an_object_that_disagrees_with_its_size_is_recorded_corrupt(&f).await;
    a_store_that_cannot_be_reached_is_not_recorded_as_loss(&f).await;
    a_placement_that_was_put_back_returns_to_present(&f).await;
    an_object_listed_at_full_size_that_serves_nothing_is_corrupt(&f).await;
    the_standing_report_counts_what_the_passes_found(&f).await;
}

async fn an_object_that_matches_its_row_is_verified(f: &Fixture) {
    let key = place(f, &hash_of(1), b"the bytes the row claims").await;

    let scrubbed = dam_pipeline::integrity::scrub(
        &f.global,
        f.store.as_ref() as &dyn BlobStore,
        &f.slug,
        chrono::Utc::now(),
    )
    .await
    .expect("scrub");

    assert_eq!(scrubbed.verified, 1);
    assert_eq!(scrubbed.findings(), 0);

    let (state, verified_at) = state_of(f, &key).await;
    assert_eq!(state, "present");
    assert!(
        verified_at.is_some(),
        "the stamp is the point of the pass even when the answer is 'still fine' — without it \
         `due` cannot advance and the scrub re-checks the same page forever"
    );
}

async fn an_object_the_store_has_lost_is_recorded_missing(f: &Fixture) {
    let key = place(f, &hash_of(2), b"bytes that are about to go away").await;
    f.store.delete(&key).await.expect("delete the object");

    let scrubbed = dam_pipeline::integrity::scrub(
        &f.global,
        f.store.as_ref() as &dyn BlobStore,
        &f.slug,
        chrono::Utc::now(),
    )
    .await
    .expect("scrub");

    assert_eq!(scrubbed.missing, 1, "{scrubbed:?}");
    let (state, _) = state_of(f, &key).await;
    assert_eq!(
        state, "missing",
        "the state `PlacementState` documents as needing a re-replication, written for the first \
         time by anything"
    );
}

async fn an_object_that_disagrees_with_its_size_is_recorded_corrupt(f: &Fixture) {
    // The shape the killed writer produced: the object is there, `head` answers for it, and it is not
    // what the row says it is. A scrub that only looked for absence would call this healthy.
    let hash = hash_of(3);
    let key = place(f, &hash, b"the full original contents").await;
    f.store
        .put(&key, bytes::Bytes::from_static(b""), StorageClass::Standard)
        .await
        .expect("truncate it");

    let scrubbed = dam_pipeline::integrity::scrub(
        &f.global,
        f.store.as_ref() as &dyn BlobStore,
        &f.slug,
        chrono::Utc::now(),
    )
    .await
    .expect("scrub");

    assert_eq!(scrubbed.corrupt, 1, "{scrubbed:?}");
    let (state, _) = state_of(f, &key).await;
    assert_eq!(state, "corrupt");
}

async fn a_store_that_cannot_be_reached_is_not_recorded_as_loss(f: &Fixture) {
    // The property that decides whether the report is believable. An unreachable backend says nothing
    // about whether the bytes exist, and recording it as `missing` would fill the report with noise on
    // every blip — the same misreading `finalise` used to make in the other direction.
    let hash = hash_of(4);
    let key = place(f, &hash, b"present, and about to be unreachable").await;

    let unreachable = dam_store::S3Store::seaweedfs("http://127.0.0.1:1", &f.bucket, "k", "s");
    let scrubbed = dam_pipeline::integrity::scrub(
        &f.global,
        &unreachable as &dyn BlobStore,
        &f.slug,
        chrono::Utc::now(),
    )
    .await
    .expect("a scrub against a dead backend is not itself a failure");

    assert_eq!(scrubbed.missing, 0, "{scrubbed:?}");
    assert_eq!(scrubbed.corrupt, 0, "{scrubbed:?}");
    assert!(scrubbed.unreachable > 0, "{scrubbed:?}");

    let (state, _) = state_of(f, &key).await;
    assert_eq!(
        state, "present",
        "an unreachable store leaves every verdict exactly as it found it"
    );
}

async fn a_placement_that_was_put_back_returns_to_present(f: &Fixture) {
    // Depends on the `missing` case above. A verdict is re-derived every pass rather than latched,
    // because an operator who repairs something has to be able to see that they did.
    let key = Key::original(f.tenant_id, &hash_of(2)).expect("key");
    let (before, _) = state_of(f, &key).await;
    assert_eq!(before, "missing", "the premise, from the earlier case");

    f.store
        .put(
            &key,
            bytes::Bytes::from_static(b"bytes that are about to go away"),
            StorageClass::Standard,
        )
        .await
        .expect("put it back");

    dam_pipeline::integrity::scrub(
        &f.global,
        f.store.as_ref() as &dyn BlobStore,
        &f.slug,
        chrono::Utc::now(),
    )
    .await
    .expect("scrub");

    let (after, _) = state_of(f, &key).await;
    assert_eq!(
        after, "present",
        "a flagged placement whose object came back must clear, or the scrub can only ever \
         report worse news"
    );
}

/// A store that answers `head` truthfully and serves nothing — for one key.
///
/// The shape a killed writer leaves and the reason the scrub asks for a byte: no real backend can be
/// talked into reporting a size it will not serve, so the only way to hold this case still is to build
/// it. Scoped to a single key rather than the whole store, because a backend with one damaged object
/// is the case being tested and a backend that serves nothing at all is a different one.
struct ServesNothing<'a> {
    inner: &'a dyn BlobStore,
    damaged: String,
}

#[async_trait::async_trait]
impl BlobStore for ServesNothing<'_> {
    fn driver(&self) -> &'static str {
        self.inner.driver()
    }
    fn capabilities(&self) -> dam_store::Capabilities {
        self.inner.capabilities()
    }
    fn latency_class(&self) -> dam_core::LatencyClass {
        self.inner.latency_class()
    }
    async fn put(
        &self,
        key: &Key,
        body: bytes::Bytes,
        class: StorageClass,
    ) -> dam_store::Result<dam_store::Placement> {
        self.inner.put(key, body, class).await
    }
    async fn get(
        &self,
        key: &Key,
        range: Option<dam_store::ByteRange>,
    ) -> dam_store::Result<dam_store::GetOutcome> {
        if key.as_str() == self.damaged {
            return Ok(dam_store::GetOutcome::Bytes(bytes::Bytes::new()));
        }
        self.inner.get(key, range).await
    }
    async fn head(&self, key: &Key) -> dam_store::Result<dam_store::ObjectState> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &Key) -> dam_store::Result<()> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str, limit: usize) -> dam_store::Result<Vec<Key>> {
        self.inner.list(prefix, limit).await
    }
    async fn transition(&self, key: &Key, to: StorageClass) -> dam_store::Result<()> {
        self.inner.transition(key, to).await
    }
    async fn copy(
        &self,
        from: &Key,
        to: &Key,
        size: u64,
        class: StorageClass,
    ) -> dam_store::Result<()> {
        self.inner.copy(from, to, size, class).await
    }
    async fn restore(
        &self,
        key: &Key,
        tier: dam_core::RestoreTier,
        keep_for: std::time::Duration,
    ) -> dam_store::Result<dam_store::RestoreTicket> {
        self.inner.restore(key, tier, keep_for).await
    }
    async fn presign_get(&self, key: &Key, ttl: std::time::Duration) -> dam_store::Result<String> {
        self.inner.presign_get(key, ttl).await
    }
    async fn presign_put(&self, key: &Key, ttl: std::time::Duration) -> dam_store::Result<String> {
        self.inner.presign_put(key, ttl).await
    }
}

async fn an_object_listed_at_full_size_that_serves_nothing_is_corrupt(f: &Fixture) {
    // The 80 the load run produced that a metadata-only pass called healthy: `head` reports 331,390
    // bytes, the download returns zero, and on a backend that reports no checksum there is nothing
    // else in the response to disagree with.
    let key = place(f, &hash_of(5), b"listed in full, served not at all").await;

    let lying = ServesNothing {
        inner: f.store.as_ref() as &dyn BlobStore,
        damaged: key.as_str().to_owned(),
    };
    let scrubbed = dam_pipeline::integrity::scrub(
        &f.global,
        &lying as &dyn BlobStore,
        &f.slug,
        chrono::Utc::now(),
    )
    .await
    .expect("scrub");

    assert!(scrubbed.corrupt >= 1, "{scrubbed:?}");
    let (state, _) = state_of(f, &key).await;
    assert_eq!(
        state, "corrupt",
        "a byte was asked for, the store said yes, and there was no byte — the one answer no \
         working backend can give"
    );
}

async fn the_standing_report_counts_what_the_passes_found(f: &Fixture) {
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("begin");
    let standing = integrity::standing(conn.executor())
        .await
        .expect("standing");
    let findings = integrity::findings(conn.executor(), 100)
        .await
        .expect("findings");
    conn.commit().await.expect("commit");

    // Two corrupt: the truncated object, and the one listed at full size that served nothing.
    assert_eq!(standing.corrupt, 2, "{standing:?}");
    assert_eq!(standing.missing, 0, "{standing:?}");
    assert_eq!(
        standing.unverified, 0,
        "every placement has been reached by a pass: {standing:?}"
    );
    assert_eq!(standing.findings(), 2);
    assert_eq!(
        findings.len(),
        2,
        "the list an operator reads, not just the count"
    );
    assert_eq!(findings[0].state, dam_core::PlacementState::Corrupt);
}
