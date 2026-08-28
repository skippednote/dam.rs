//! Migrating real files off a real disk into the library.
//!
//! Against a real Postgres and a real S3 server, and against real files in a real temporary directory,
//! because that combination is the entire argument for making the filesystem the first source connector: a
//! vendor API could only ever have been driven against its own fake, and a migration verified against a fake
//! is discovered to be wrong while somebody's library is half-moved.
//!
//! The properties here are the ones a migration cannot get wrong:
//!
//! - **It goes through the real ingest.** Not "an asset row appears" — the content hash, the sniffed type
//!   and the placement all have to be what an ordinary upload would have produced, because the whole design
//!   turns on transfer having no ingest of its own.
//! - **Running it twice does not move anything twice.** A migration is resumed by re-running it, so the
//!   second pass has to skip rather than duplicate. This is the failure that costs a customer their library.
//! - **One bad record is one bad record.** A file that is missing, or a path that tries to leave the export,
//!   fails that record and lets the rest of the run continue.
//! - **Deduplication still points somewhere.** Two source assets with identical bytes are one asset, and the
//!   second record has to be marked migrated against it rather than left looking failed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::TenantSlug;
use dam_db::{migrate, testing::PostgresHarness};
use dam_pipeline::source::Filesystem;
use dam_pipeline::transfer::{self, Outcome};
use dam_store::ResumableStore;
use dam_store::testing::SeaweedfsHarness;
use image::{ImageFormat, RgbImage};
use sqlx::PgPool;
use std::io::Cursor;
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
    root: tempfile::TempDir,
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

    sqlx::query(
        "INSERT INTO dam_global.storage_pools \
         (id, tenant_id, name, driver, bucket, credentials_ref, latency_class) \
         VALUES (gen_random_uuid(), $1, 'hot', 's3', $2, 'test', 'instant')",
    )
    .bind(tenant_id)
    .bind(s3.bucket())
    .execute(&global)
    .await
    .expect("storage pool");

    Fixture {
        _pg: pg,
        _s3: s3,
        global,
        tenant,
        store,
        slug: TenantSlug::new("acme").expect("slug"),
        tenant_id,
        root: tempfile::tempdir().expect("tempdir"),
    }
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut img = RgbImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        px.0 = [(x % 256) as u8, (y % 256) as u8, 128];
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, ImageFormat::Jpeg)
        .expect("encode");
    out.into_inner()
}

/// Writes a file into the export, creating whatever folders it names.
fn export(f: &Fixture, relative: &str, bytes: &[u8]) {
    let path = f.root.path().join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, bytes).expect("write");
}

async fn import_job(f: &Fixture) -> Uuid {
    let mut conn = f.tenant.acquire().await.expect("conn");
    dam_db::imports::create(
        &mut conn,
        &dam_db::imports::NewImport {
            id: Uuid::now_v7(),
            source: "filesystem",
            label: "the old shared drive",
            config: serde_json::json!({}),
            batch_size: 1000,
            created_by: None,
        },
    )
    .await
    .expect("import")
}

fn record(id: &str, file: &str) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::json!({ "id": id, "file": file }) {
        serde_json::Value::Object(map) => map,
        _ => unreachable!(),
    }
}

fn source(f: &Fixture) -> Filesystem {
    Filesystem::rooted(f.root.path(), "file").expect("source")
}

#[allow(clippy::too_many_arguments)]
async fn transfer_one(
    f: &Fixture,
    job: Uuid,
    source_id: &str,
    rec: &serde_json::Map<String, serde_json::Value>,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> Outcome {
    transfer::one(
        &f.global,
        f.store.as_ref(),
        &source(f),
        &f.slug,
        f.tenant_id,
        job,
        source_id,
        rec,
        metadata,
        // No scanner: clamd is not in this suite's container set, and `finalise` treats `None` as "not
        // configured" rather than "clean" — the same thing an operator without clamd gets.
        None,
    )
    .await
    .expect("transfer")
}

async fn state_of(f: &Fixture, job: Uuid, source_id: &str) -> Option<String> {
    let mut conn = f.tenant.acquire().await.expect("conn");
    dam_db::imports::state_of(&mut conn, job, source_id)
        .await
        .expect("state")
}

#[tokio::test]
async fn transferring_a_folder_of_files_holds() {
    let f = fixture().await;
    a_file_on_disk_becomes_an_asset_through_the_real_ingest(&f).await;
    a_second_run_skips_what_already_arrived(&f).await;
    a_file_larger_than_one_chunk_arrives_whole(&f).await;
    identical_bytes_become_one_object_and_two_migrated_records(&f).await;
    a_path_leaving_the_export_fails_that_record_only(&f).await;
    a_file_that_is_not_there_fails_that_record_only(&f).await;
    a_record_naming_no_file_fails_rather_than_guessing(&f).await;
}

/// The whole point: the bytes arrive the way an upload's would.
async fn a_file_on_disk_becomes_an_asset_through_the_real_ingest(f: &Fixture) {
    let bytes = jpeg(64, 48);
    export(f, "photos/first.jpg", &bytes);
    let job = import_job(f).await;

    let metadata = match serde_json::json!({"caption": "from the old drive"}) {
        serde_json::Value::Object(map) => map,
        _ => unreachable!(),
    };
    let outcome = transfer_one(
        f,
        job,
        "src-1",
        &record("src-1", "photos/first.jpg"),
        &metadata,
    )
    .await;

    let Outcome::Migrated { asset_id, created } = outcome else {
        panic!("expected a migration, got {outcome:?}");
    };
    assert!(created, "the library was empty, so this is a new asset");

    // Through `finalise`, not around it. The type was sniffed from the bytes rather than taken from the
    // record — the record never said what this was — and the hash is over the content.
    let (mime, hash, filename): (String, String, String) =
        sqlx::query_as("SELECT mime, content_hash, filename FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("asset");
    assert_eq!(mime, "image/jpeg", "the ingest sniffed the bytes");
    assert_eq!(hash, blake3::hash(&bytes).to_hex().to_string());
    assert_eq!(filename, "first.jpg", "the export's own name for it");

    // And it is really in the store, under a placement the rest of the system can resolve.
    let placements: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM object_placements WHERE asset_id = $1 AND state = 'present'",
    )
    .bind(asset_id)
    .fetch_one(&f.tenant)
    .await
    .expect("placements");
    assert_eq!(placements, 1, "one placement, as an upload would have made");

    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("metadata");
    assert_eq!(stored["caption"], "from the old drive");

    assert_eq!(state_of(f, job, "src-1").await.as_deref(), Some("migrated"));

    // And the rest of the pipeline is queued. `finalise` does not do this — its production caller does —
    // so a transfer that stopped at `finalise` would leave an asset with no proxy, no thumbnail and nothing
    // in the index. The first real run did exactly that, which is why this assertion exists.
    let (kind, priority): (String, i16) = sqlx::query_as(
        "SELECT kind, priority FROM dam_global.jobs WHERE payload->>'asset_id' = $1",
    )
    .bind(asset_id.to_string())
    .fetch_one(&f.global)
    .await
    .expect("a queued job");
    assert_eq!(kind, "derive", "the chain starts with derive");
    assert!(
        priority >= 50,
        "a migration's renders must not sit in the interactive band ahead of real uploads: {priority}"
    );
}

/// The failure a migration must not have: a resumed run doubling the library.
async fn a_second_run_skips_what_already_arrived(f: &Fixture) {
    export(f, "photos/second.jpg", &jpeg(32, 32));
    let job = import_job(f).await;
    let rec = record("src-2", "photos/second.jpg");
    let empty = serde_json::Map::new();

    let first = transfer_one(f, job, "src-2", &rec, &empty).await;
    let Outcome::Migrated { asset_id, .. } = first else {
        panic!("expected a migration, got {first:?}");
    };

    let again = transfer_one(f, job, "src-2", &rec, &empty).await;
    assert_eq!(again, Outcome::Skipped, "the record was already migrated");

    let assets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assets WHERE content_hash = \
         (SELECT content_hash FROM assets WHERE id = $1) AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_one(&f.tenant)
    .await
    .expect("count");
    assert_eq!(
        assets, 1,
        "re-running a migration must not double the library"
    );
}

/// A file bigger than one read chunk, because the chunking loop is where an off-by-one would hide.
///
/// Everything else in this suite uses files of a few kilobytes, so the loop in `ingest` runs exactly once and
/// the interesting case — a second `patch` at a non-zero offset, and the resumable engine assembling more
/// than one part — never happens. A migration's large masters are precisely where that would surface, and it
/// would surface as a corrupted asset rather than an error.
///
/// Sized just over the 8 MiB chunk so the loop runs twice with an uneven remainder, which also puts the
/// first part over S3's 5 MiB multipart minimum.
async fn a_file_larger_than_one_chunk_arrives_whole(f: &Fixture) {
    // Compressible but not uniform, so a truncated or doubled chunk changes the hash rather than landing on
    // the same bytes by luck.
    let mut bytes = Vec::with_capacity(9 * 1024 * 1024 + 1234);
    let mut x: u32 = 0x9E37_79B9;
    while bytes.len() < 9 * 1024 * 1024 + 1234 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    export(f, "big/master.bin", &bytes);
    let job = import_job(f).await;
    let empty = serde_json::Map::new();

    let outcome = transfer_one(f, job, "big-1", &record("big-1", "big/master.bin"), &empty).await;
    let Outcome::Migrated { asset_id, .. } = outcome else {
        panic!("a large file must transfer: {outcome:?}");
    };

    let (stored, hash): (i64, String) =
        sqlx::query_as("SELECT bytes, content_hash FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("asset");
    assert_eq!(
        stored,
        i64::try_from(bytes.len()).expect("fits"),
        "every chunk has to land exactly once"
    );
    assert_eq!(
        hash,
        blake3::hash(&bytes).to_hex().to_string(),
        "and in the right order"
    );
}

/// Two source assets, the same bytes: one stored object, two assets, both records migrated.
///
/// The bytes are shared and the assets are not, which is the right answer for a library — two records can
/// point at the same file and differ in rights, metadata and history, and collapsing them would lose the
/// second one's. What a migration must not do is leave the second record looking failed because its bytes
/// were already there.
async fn identical_bytes_become_one_object_and_two_migrated_records(f: &Fixture) {
    let bytes = jpeg(20, 20);
    export(f, "dupes/a.jpg", &bytes);
    export(f, "dupes/b.jpg", &bytes);
    let job = import_job(f).await;
    let empty = serde_json::Map::new();

    let first = transfer_one(f, job, "dupe-a", &record("dupe-a", "dupes/a.jpg"), &empty).await;
    let second = transfer_one(f, job, "dupe-b", &record("dupe-b", "dupes/b.jpg"), &empty).await;

    let (Outcome::Migrated { asset_id: one, .. }, Outcome::Migrated { asset_id: two, .. }) =
        (first.clone(), second.clone())
    else {
        panic!("expected two migrations, got {first:?} and {second:?}");
    };
    assert_ne!(one, two, "two source assets are two assets");

    let hash = blake3::hash(&bytes).to_hex().to_string();
    let assets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assets WHERE content_hash = $1 AND deleted_at IS NULL",
    )
    .bind(&hash)
    .fetch_one(&f.tenant)
    .await
    .expect("count");
    assert_eq!(assets, 2);

    // Stored once, though. The ingest is content-addressed, so both assets resolve to one object.
    let objects: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT object_key) FROM object_placements WHERE asset_id IN ($1, $2)",
    )
    .bind(one)
    .bind(two)
    .fetch_one(&f.tenant)
    .await
    .expect("objects");
    assert_eq!(objects, 1, "identical bytes are stored once");

    // And the second record is migrated, not failed: an operator reading the report has to see that the
    // source asset arrived.
    assert_eq!(
        state_of(f, job, "dupe-b").await.as_deref(),
        Some("migrated")
    );
}

/// The export is untrusted input: it came out of somebody else's system.
async fn a_path_leaving_the_export_fails_that_record_only(f: &Fixture) {
    let job = import_job(f).await;
    let empty = serde_json::Map::new();

    let outcome = transfer_one(
        f,
        job,
        "escape",
        &record("escape", "../../etc/passwd"),
        &empty,
    )
    .await;
    let Outcome::Failed(reason) = outcome else {
        panic!("a path leaving the root must not transfer: {outcome:?}");
    };
    assert!(
        reason.contains("leaves the source root"),
        "and the record has to say why: {reason}"
    );
    assert_eq!(state_of(f, job, "escape").await.as_deref(), Some("failed"));

    // The run carries on: the next record still arrives.
    export(f, "photos/after.jpg", &jpeg(16, 16));
    let next = transfer_one(
        f,
        job,
        "after",
        &record("after", "photos/after.jpg"),
        &empty,
    )
    .await;
    assert!(
        matches!(next, Outcome::Migrated { .. }),
        "one refused record must not stop the migration: {next:?}"
    );
}

async fn a_file_that_is_not_there_fails_that_record_only(f: &Fixture) {
    let job = import_job(f).await;
    let empty = serde_json::Map::new();

    let outcome = transfer_one(f, job, "gone", &record("gone", "photos/nope.jpg"), &empty).await;
    assert!(
        matches!(outcome, Outcome::Failed(_)),
        "a missing file is a failed record: {outcome:?}"
    );
    assert_eq!(state_of(f, job, "gone").await.as_deref(), Some("failed"));
}

/// A record with no path is a mapping problem, and it says so rather than transferring nothing quietly.
async fn a_record_naming_no_file_fails_rather_than_guessing(f: &Fixture) {
    let job = import_job(f).await;
    let empty = serde_json::Map::new();
    let rec = match serde_json::json!({ "id": "no-file" }) {
        serde_json::Value::Object(map) => map,
        _ => unreachable!(),
    };

    let outcome = transfer_one(f, job, "no-file", &rec, &empty).await;
    let Outcome::Failed(reason) = outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert!(reason.contains("file"), "{reason}");
}
