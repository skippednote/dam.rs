//! The ingest pipeline end to end: an upload becomes an asset, and an asset gets a thumbnail.
//!
//! Both stages against a real Postgres and a real S3 server, because both are the point: finalisation is
//! mostly about what the object store does with a staged object, and derivation is about bytes coming back out
//! of it. A fake would test the parts that were never in doubt.
//!
//! The properties that matter here are the ones an at-least-once queue makes load-bearing:
//!
//! - **Running a stage twice must not produce two assets** or two derivative rows.
//! - **A permanent failure must be reported as permanent,** because the alternative is five retries and twenty
//!   minutes of backoff on a file that will never parse.
//! - **The staging object is deleted only after the asset row exists,** so a crash leaves something the reaper
//!   can clean rather than bytes nothing points at.
//!
//! One container pair per driver, with the cases as functions over a borrowed fixture — nineteen containers in
//! one suite took a run from 12 s to 231 s and then failed on connection timeouts.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::{StorageClass, TenantSlug};
use dam_db::{migrate, testing::PostgresHarness};
use dam_media::testing::{Entry, tags};
use dam_store::testing::SeaweedfsHarness;
use dam_store::{BlobStore, Key, ResumableStore};
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

    // A placement needs a pool, and finalisation refuses without one rather than inventing an id — see
    // `finalise::default_pool`.
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
    }
}

/// A real JPEG, so the probe and the renderer both have something to work with.
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

/// Stages an upload: a session row plus the bytes at the staging key, exactly as TUS leaves them.
async fn stage(f: &Fixture, upload_id: &str, filename: &str, bytes: &[u8]) {
    stage_as(f, upload_id, filename, "image/jpeg", bytes).await;
}

/// Stages an upload whose client-declared type is not a JPEG.
///
/// Declared, not sniffed: finalisation sniffs the bytes and records the mismatch, which is the whole point of
/// the field. A video staged as `image/jpeg` would be a test of nothing.
async fn stage_as(f: &Fixture, upload_id: &str, filename: &str, declared_mime: &str, bytes: &[u8]) {
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("tenant conn");
    let mut session = dam_db::uploads::create(
        conn.executor(),
        f.tenant_id,
        upload_id,
        Some(bytes.len() as u64),
        Some(filename),
        Some(declared_mime),
        None,
        // No profile named on the session, deliberately: the finalisation case marks its profile as the
        // tenant's fallback, so this also exercises the resolution path an ordinary upload takes.
        None,
    )
    .await
    .expect("session");
    conn.commit().await.expect("commit");

    // Through the resumable engine rather than a bare `put`, so the session's part bookkeeping is what the
    // real path produces. A hand-written staging object would test a shape TUS never creates.
    let outcome = dam_store::resumable::patch(
        f.store.as_ref(),
        &mut session,
        0,
        bytes::Bytes::from(bytes.to_vec()),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    assert!(matches!(
        outcome,
        dam_store::resumable::PatchOutcome::Accepted { .. }
    ));

    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("tenant conn");
    dam_db::uploads::save(conn.executor(), &session)
        .await
        .expect("save");
    conn.commit().await.expect("commit");
}

fn blob(f: &Fixture) -> &dyn BlobStore {
    f.store.as_ref() as &dyn BlobStore
}

// ─── finalisation ───────────────────────────────────────────────────────────

async fn a_staged_upload_becomes_an_asset(f: &Fixture) {
    // A metadata type claiming the image class, defined before the upload: ingest should pick it from the
    // sniffed mime without being told, so an asset arrives carrying the form it should have (Q.1). Without a
    // type present this assertion is vacuous, which is why the type is created here rather than in the fixture.
    let image_type = dam_db::metadata_types::define(
        &f.tenant,
        dam_db::metadata_types::NewType {
            key: "image".to_owned(),
            label: "Image".to_owned(),
            applies_to: vec!["image".to_owned()],
            is_default: false,
            field_keys: vec![],
        },
    )
    .await
    .expect("define image type");

    // A field for the profile to default, and a *second* metadata type the profile names — so "the profile's
    // choice wins over the mime's class" is observable rather than coincidental. Without the second type both
    // paths would pick `image` and the precedence would be untested.
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, display_order) \
         VALUES (gen_random_uuid(), 'credit', 'Credit', 'text', 1) ON CONFLICT (key) DO NOTHING",
    )
    .execute(&f.tenant)
    .await
    .expect("field");
    let press_type = dam_db::metadata_types::define(
        &f.tenant,
        dam_db::metadata_types::NewType {
            key: "press".to_owned(),
            label: "Press".to_owned(),
            applies_to: vec![],
            is_default: false,
            field_keys: vec!["credit".to_owned()],
        },
    )
    .await
    .expect("define press type");
    let profile = dam_db::upload_profiles::create(
        &f.tenant,
        dam_db::upload_profiles::NewProfile {
            key: "press".to_owned(),
            label: "Press delivery".to_owned(),
            metadata_type_id: Some(press_type.id),
            defaults: serde_json::json!({ "credit": "Acme Press Office" }),
            require_complete: false,
            ai_tags_enabled: false,
            is_default: true,
        },
    )
    .await
    .expect("profile");

    let bytes = jpeg(640, 480);
    stage(f, "finalise001", "harbour.jpg", &bytes).await;

    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "finalise001",
        None,
    )
    .await
    .expect("finalise");

    assert!(finalised.created);
    assert_eq!(finalised.mime, "image/jpeg", "sniffed from the bytes");
    assert_eq!(finalised.bytes, bytes.len() as i64);
    assert_eq!(
        finalised.content_hash,
        blake3::hash(&bytes).to_hex().to_string(),
        "the content hash is BLAKE3 of the bytes, which is what makes the key content-addressed"
    );

    let row: (String, String, Option<i32>, Option<i32>, String) =
        sqlx::query_as("SELECT filename, mime, width, height, status FROM assets WHERE id = $1")
            .bind(finalised.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("asset row");
    assert_eq!(
        row.0, "harbour.jpg",
        "the uploader's own filename, verbatim"
    );
    assert_eq!(row.1, "image/jpeg");
    assert_eq!(
        (row.2, row.3),
        (Some(640), Some(480)),
        "dimensions come from the header, read as a ranged prefix rather than a download"
    );
    assert_eq!(row.4, "active");

    // The profile's own answers reached the asset (Q.3): its metadata type won over the mime's class, its
    // defaults are real validated metadata on the row, and the profile id is recorded so enrichment can still
    // ask "was machine tagging permitted" after the session row is reaped.
    let (profile_on_asset, values): (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT a.upload_profile_id, coalesce(m.values, '{}'::jsonb) \
         FROM assets a LEFT JOIN asset_metadata m ON m.asset_id = a.id WHERE a.id = $1",
    )
    .bind(finalised.asset_id)
    .fetch_one(&f.tenant)
    .await
    .expect("asset and metadata");
    assert_eq!(profile_on_asset, Some(profile.id));
    assert_eq!(
        values["credit"], "Acme Press Office",
        "the profile's default is metadata, not a note: {values}"
    );

    let assigned: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT metadata_type_id FROM assets WHERE id = $1")
            .bind(finalised.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("metadata type");
    assert_eq!(
        assigned,
        Some(press_type.id),
        "the profile's type wins over the mime's class: a profile is a statement, a class is a guess"
    );
    assert_ne!(
        assigned,
        Some(image_type.id),
        "and the class would have chosen differently, which is what makes the precedence observable"
    );

    // The bytes are at the content-addressed key, and the staging object is gone.
    let original = Key::original(f.tenant_id, &finalised.content_hash).expect("key");
    assert!(
        blob(f).head(&original).await.is_ok(),
        "the original is stored"
    );
    let staging = Key::staging(f.tenant_id, "finalise001").expect("key");
    assert!(
        blob(f).head(&staging).await.is_err(),
        "and staging is unstaged, so the reaper has nothing left to find"
    );

    // The placement is what makes the tier derivable rather than defaulted.
    let placement: (String, String) =
        sqlx::query_as("SELECT storage_class, state FROM object_placements WHERE asset_id = $1")
            .bind(finalised.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("placement");
    assert_eq!(placement, ("STANDARD".to_owned(), "present".to_owned()));
}

async fn embedded_metadata_is_imported_and_beats_a_blanket_default(f: &Fixture) {
    // Two fields and two ways to fill them, so the precedence is observable rather than assumed:
    // `photographer` is claimed by both the file and the profile, and `shot_iso` only by the file.
    for (key, kind) in [("photographer", "text"), ("shot_iso", "int")] {
        sqlx::query(
            "INSERT INTO field_defs (id, key, label, kind, display_order) \
             VALUES (gen_random_uuid(), $1, $1, $2, 9) ON CONFLICT (key) DO NOTHING",
        )
        .bind(key)
        .bind(kind)
        .execute(&f.tenant)
        .await
        .expect("field");
    }
    for (source, field) in [("exif.artist", "photographer"), ("exif.iso", "shot_iso")] {
        dam_db::auto_import::create(
            &f.tenant,
            dam_db::auto_import::NewMapping {
                source: source.to_owned(),
                field_key: field.to_owned(),
                priority: 0,
                overwrite: false,
                enabled: true,
            },
        )
        .await
        .expect("mapping");
    }

    // A profile whose default claims `photographer` too. It is the *blanket* answer for this intake, and the
    // file's own answer is the specific one — so the import has to win, or one default would silently defeat
    // every per-asset import from that source.
    let profile = dam_db::upload_profiles::create(
        &f.tenant,
        dam_db::upload_profiles::NewProfile {
            key: "agency".to_owned(),
            label: "Agency feed".to_owned(),
            metadata_type_id: None,
            defaults: serde_json::json!({ "photographer": "The Agency" }),
            require_complete: false,
            ai_tags_enabled: false,
            is_default: true,
        },
    )
    .await
    .expect("profile");

    let bytes = dam_media::testing::decodable_jpeg_with_exif(
        200,
        150,
        &[(tags::ARTIST, Entry::Text("Ada Lovelace"))],
        // In the Exif sub-directory and as a SHORT, which is where and how a camera writes sensitivity. A
        // fixture that put it in IFD0 as text would be a different tag entirely, and would prove nothing.
        &[(tags::PHOTOGRAPHIC_SENSITIVITY, Entry::Short(400))],
    );
    stage(f, "finalise-exif", "dawn.jpg", &bytes).await;
    dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "finalise-exif",
        None,
    )
    .await
    .expect("finalise");

    let values: serde_json::Value = sqlx::query_scalar(
        "SELECT m.values FROM asset_metadata m JOIN assets a ON a.id = m.asset_id \
         WHERE a.filename = 'dawn.jpg'",
    )
    .fetch_one(&f.tenant)
    .await
    .expect("metadata");

    assert_eq!(
        values.get("photographer").and_then(|v| v.as_str()),
        Some("Ada Lovelace"),
        "the file's own answer beats the profile's blanket one: {values}"
    );
    // Coerced on the way in, because EXIF renders sensitivity as text and the field is an int — a mapping into a
    // numeric field that could never fire would rule out most of what people actually import.
    assert_eq!(
        values.get("shot_iso").and_then(|v| v.as_i64()),
        Some(400),
        "a numeric field is filled, not refused: {values}"
    );

    // Cleaned up so the cases after this one see the tenant they expect: this profile is the default, and a
    // leftover default would silently apply to every later upload.
    dam_db::upload_profiles::remove(&f.tenant, profile.id)
        .await
        .expect("remove profile");
}

async fn finalising_twice_produces_one_asset(f: &Fixture) {
    // The property an at-least-once queue makes load-bearing. A worker that dies after the insert is asked to
    // do this again, and a second asset for one upload is a duplicate nobody can tell apart.
    let bytes = jpeg(64, 64);
    stage(f, "finalise002", "twice.jpg", &bytes).await;

    let first = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "finalise002",
        None,
    )
    .await
    .expect("first");
    let second = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "finalise002",
        None,
    )
    .await
    .expect("second");

    assert!(first.created);
    assert!(
        !second.created,
        "the second run reports that it did nothing"
    );
    assert_eq!(first.asset_id, second.asset_id);
    assert_eq!(first.content_hash, second.content_hash);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM assets WHERE filename = 'twice.jpg'")
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(count, 1);
}

async fn two_uploads_of_one_file_share_an_object_and_are_two_assets(f: &Fixture) {
    // D1. Deduplication falls out of content addressing rather than being a feature: the same bytes produce
    // the same key. Two *assets* still exist, because they have different filenames, metadata and rights.
    let bytes = jpeg(48, 32);
    stage(f, "dedupe001", "first-name.jpg", &bytes).await;
    stage(f, "dedupe002", "second-name.jpg", &bytes).await;

    let one = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "dedupe001",
        None,
    )
    .await
    .expect("first");
    let two = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "dedupe002",
        None,
    )
    .await
    .expect("second");

    assert_ne!(one.asset_id, two.asset_id, "two assets");
    assert_eq!(
        one.content_hash, two.content_hash,
        "sharing one object, because the key is the digest"
    );

    let names: Vec<String> =
        sqlx::query_scalar("SELECT filename FROM assets WHERE content_hash = $1 ORDER BY filename")
            .bind(&one.content_hash)
            .fetch_all(&f.tenant)
            .await
            .expect("names");
    assert_eq!(names, vec!["first-name.jpg", "second-name.jpg"]);
}

async fn an_incomplete_upload_is_a_permanent_refusal_not_a_retry(f: &Fixture) {
    // The client declared more than it sent. The session deliberately stays active — the client may still send
    // the rest — so this must not burn five attempts and twenty minutes of backoff.
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("conn");
    let mut session = dam_db::uploads::create(
        conn.executor(),
        f.tenant_id,
        "short001",
        Some(10_000),
        Some("truncated.jpg"),
        Some("image/jpeg"),
        None,
        None,
    )
    .await
    .expect("session");
    conn.commit().await.expect("commit");

    dam_store::resumable::patch(
        f.store.as_ref(),
        &mut session,
        0,
        bytes::Bytes::from_static(b"only a few bytes"),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("conn");
    dam_db::uploads::save(conn.executor(), &session)
        .await
        .expect("save");
    conn.commit().await.expect("commit");

    let error = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "short001",
        None,
    )
    .await
    .expect_err("an incomplete upload cannot be finalised");
    assert!(
        !error.is_transient(),
        "an incomplete upload is not a transient failure: {error}"
    );
}

async fn an_upload_with_no_session_is_permanent(f: &Fixture) {
    let error = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "neverexisted",
        None,
    )
    .await
    .expect_err("no session");
    assert!(!error.is_transient(), "{error}");
}

async fn a_promotion_that_lost_its_asset_row_resumes(f: &Fixture) {
    // The gap between promoting an object and recording an asset cannot be one transaction — one is an object
    // store and the other is Postgres — so a crash in between leaves the bytes promoted, staging gone, and no
    // asset row. This is what that retry does.
    //
    // Found by running the real pipeline: the first attempt failed on a missing storage pool *after* promoting,
    // and the retry failed permanently with "object not found" on an upload whose bytes were safely stored.
    let bytes = jpeg(200, 150);
    stage(f, "resume001", "half-done.jpg", &bytes).await;

    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "resume001",
        None,
    )
    .await
    .expect("finalise");

    // Wind the session back to the state a crash between the two steps leaves: the digest recorded, the asset
    // forgotten. The staging object is genuinely gone, because promotion moved it.
    sqlx::query("UPDATE upload_sessions SET asset_id = NULL WHERE upload_id = 'resume001'")
        .execute(&f.tenant)
        .await
        .expect("forget the asset");
    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(finalised.asset_id)
        .execute(&f.tenant)
        .await
        .expect("delete the asset");
    let staging = Key::staging(f.tenant_id, "resume001").expect("key");
    assert!(
        blob(f).head(&staging).await.is_err(),
        "the premise: promotion already moved the bytes out of staging"
    );

    let resumed = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "resume001",
        None,
    )
    .await
    .expect("a promoted upload with no asset row must be recoverable");

    assert!(
        resumed.created,
        "it records the asset it never got to record"
    );
    assert_eq!(
        resumed.content_hash, finalised.content_hash,
        "and against the same object, because the digest was recorded before the crash"
    );
    assert_eq!(
        resumed.mime, "image/jpeg",
        "re-sniffed from the promoted object"
    );
    assert_eq!(resumed.bytes, bytes.len() as i64);
}

async fn a_promotion_whose_store_is_unreachable_retries_rather_than_dies(f: &Fixture) {
    // The sibling of the case above, and the one that actually cost an upload. There the object was
    // genuinely absent, and permanent was the right answer. Here the object is exactly where it should be
    // and the *backend* is unreachable for a moment — and the resume path used to read every `head`
    // failure as "the object is gone", retiring a finished upload over a network blip.
    //
    // Observed under load against SeaweedFS: one `IncompleteMessage` on one HEAD, job dead, bytes fine.
    let bytes = jpeg(120, 90);
    stage(f, "resume002", "blip.jpg", &bytes).await;

    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "resume002",
        None,
    )
    .await
    .expect("finalise");

    // The same wind-back as above: the digest is recorded, the asset is forgotten.
    sqlx::query("UPDATE upload_sessions SET asset_id = NULL WHERE upload_id = 'resume002'")
        .execute(&f.tenant)
        .await
        .expect("forget the asset");
    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(finalised.asset_id)
        .execute(&f.tenant)
        .await
        .expect("delete the asset");

    // Nothing listens on port 1, so the first store call the resume path makes — the `head` on the promoted
    // key — fails to dispatch. The same shape as a backend that dropped the connection mid-request.
    let unreachable = dam_store::S3Store::seaweedfs("http://127.0.0.1:1", f._s3.bucket(), "k", "s");
    let error = dam_pipeline::finalise::upload(
        &f.global,
        &unreachable,
        &f.slug,
        f.tenant_id,
        "resume002",
        None,
    )
    .await
    .expect_err("an unreachable store is not a finished finalisation");
    assert!(
        error.is_transient(),
        "an unreachable backend is a reason to retry, not a verdict on the upload: {error}"
    );

    // And the retry that classification buys does succeed, which is the entire point: the bytes were never
    // in doubt.
    let resumed = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "resume002",
        None,
    )
    .await
    .expect("the retry finds the object where the first attempt promoted it");
    assert!(resumed.created, "the asset row it never got to write");
    assert_eq!(resumed.content_hash, finalised.content_hash);
    assert_eq!(resumed.bytes, bytes.len() as i64);
}

// ─── derivation ─────────────────────────────────────────────────────────────

/// A tiny clip, for the measurement cases. ffmpeg is pinned in `.mise.toml`.
async fn clip(dir: &std::path::Path, name: &str, width: u32, height: u32) -> Vec<u8> {
    let tools = dam_media::avprobe::AvToolchain::discover().unwrap_or_else(|e| {
        panic!(
            "these cases need ffmpeg on PATH; it is pinned in .mise.toml, so run them as \
             `mise run check` or `mise exec -- cargo test`. Underlying error: {e}"
        )
    });
    let path = dir.join(name);
    dam_media::avprobe::run_ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={width}x{height}:rate=25:duration=1"),
            "-pix_fmt",
            "yuv420p",
            path.to_str().expect("utf-8 path"),
            "-y",
        ],
    )
    .await
    .expect("ffmpeg");
    tokio::fs::read(&path).await.expect("clip bytes")
}

/// A video is measured, and rendered from a frame of itself.
///
/// Two gaps found by uploading a real iPhone library. Every clip landed with no dimensions and no duration,
/// because finalisation measures with the pure-Rust probe and nothing else ever looked — `avprobe` had existed
/// and been tested in `dam-media` the whole time, and nothing called it. And every clip had no derivative at
/// all, because the image profiles cannot decode a container, so a grid of videos was a wall of grey
/// rectangles.
async fn a_video_is_measured_even_though_nothing_renders_it(f: &Fixture) {
    let dir = tempfile::tempdir().expect("temp dir");
    let bytes = clip(dir.path(), "measured.mp4", 640, 360).await;
    stage_as(f, "measure001", "measured.mp4", "video/mp4", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "measure001",
        None,
    )
    .await
    .expect("finalise");

    let derived = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("derive");

    // Rendered from a frame of itself. All three profiles, through exactly the machinery a photograph uses —
    // which is what makes the thumbnail deliverable without a second delivery path.
    let mut rendered = derived.rendered.clone();
    rendered.sort();
    assert_eq!(
        rendered,
        vec!["preview-1024", "thumb-256", "web-2048"],
        "a video must get a poster, refused: {:?}",
        derived.refused
    );
    assert!(derived.has_thumbnail());

    let (width, height, duration): (Option<i32>, Option<i32>, Option<i64>) =
        sqlx::query_as("SELECT width, height, duration_ms FROM assets WHERE id = $1")
            .bind(finalised.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("row");
    assert_eq!((width, height), (Some(640), Some(360)));
    // A duration, which is the field only a timed probe can supply — and the one a player needs before it can
    // draw a scrub bar.
    assert!(
        duration.is_some_and(|ms| ms > 0),
        "no duration: {duration:?}"
    );

    // A one-second clip is the case that catches a guessed seek: a Live Photo is about this long, there are
    // thousands of them in a phone library, and seeking a second into one lands past the last frame — where
    // ffmpeg exits 0 having written nothing at all.
    let brief = clip(dir.path(), "brief.mp4", 320, 240).await;
    stage_as(f, "measure006", "brief.mp4", "video/mp4", &brief).await;
    let short = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "measure006",
        None,
    )
    .await
    .expect("finalise");
    let derived = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        short.asset_id,
        None,
    )
    .await
    .expect("derive");
    assert!(
        derived.has_thumbnail(),
        "a one-second clip must still get a poster: {derived:?}"
    );
}

/// The measurement runs even when every derivative already exists.
///
/// This is the case the first version of the fix missed, and it is the only population that matters for a
/// backfill: an existing library has its thumbnails already, so a job that returned early on "nothing to
/// render" would measure exactly none of it.
async fn an_asset_whose_derivatives_all_exist_is_still_measured(f: &Fixture) {
    let bytes = jpeg(900, 600);
    stage(f, "measure002", "already-derived.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "measure002",
        None,
    )
    .await
    .expect("finalise");
    dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("derive");

    // Now forget the dimensions, as an asset ingested before the measurement existed would have them.
    sqlx::query("UPDATE assets SET width = NULL, height = NULL WHERE id = $1")
        .bind(finalised.asset_id)
        .execute(&f.tenant)
        .await
        .expect("forget");

    let again = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("derive");
    assert!(
        again.rendered.is_empty(),
        "nothing left to render: {again:?}"
    );

    let (width, height): (Option<i32>, Option<i32>) =
        sqlx::query_as("SELECT width, height FROM assets WHERE id = $1")
            .bind(finalised.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("row");
    assert_eq!(
        (width, height),
        (Some(900), Some(600)),
        "an already-derived asset must still be measured"
    );
}

/// A measured dimension is a *display* dimension: a quarter turn swaps the axes.
///
/// vips reports what is stored and rotates at render time, so the swap has to happen here or a portrait
/// photograph is recorded as a landscape one — which is the orientation facet answering wrong and a grid cell
/// laid out the wrong way round. The same rule `probe::image` applies to a JPEG, applied to the tool that can
/// read the formats it cannot.
async fn a_quarter_turn_swaps_the_axes_in_the_measurement(f: &Fixture) {
    // 6 is "rotate 90° clockwise", which is what an iPhone writes for a portrait photograph.
    let bytes = dam_media::testing::decodable_jpeg_with_exif(
        600,
        400,
        &[(tags::ORIENTATION, Entry::Short(6))],
        &[],
    );
    stage(f, "measure004", "turned.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "measure004",
        None,
    )
    .await
    .expect("finalise");

    // Forgotten, so the measurement is the thing under test rather than the cheap probe.
    sqlx::query("UPDATE assets SET width = NULL, height = NULL WHERE id = $1")
        .bind(finalised.asset_id)
        .execute(&f.tenant)
        .await
        .expect("forget");

    dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("derive");

    let (width, height): (Option<i32>, Option<i32>) =
        sqlx::query_as("SELECT width, height FROM assets WHERE id = $1")
            .bind(finalised.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("row");
    assert_eq!(
        (width, height),
        (Some(400), Some(600)),
        "a quarter turn must swap the axes, not record what is stored"
    );

    // A *mirror* is not a quarter turn. 2 flips left-for-right and leaves the axes alone, so a swap here
    // would record a landscape photograph as a portrait one for a transform that changes neither dimension.
    let mirrored = dam_media::testing::decodable_jpeg_with_exif(
        600,
        400,
        &[(tags::ORIENTATION, Entry::Short(2))],
        &[],
    );
    stage(f, "measure005", "mirrored.jpg", &mirrored).await;
    let flipped = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "measure005",
        None,
    )
    .await
    .expect("finalise");
    sqlx::query("UPDATE assets SET width = NULL, height = NULL WHERE id = $1")
        .bind(flipped.asset_id)
        .execute(&f.tenant)
        .await
        .expect("forget");
    dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        flipped.asset_id,
        None,
    )
    .await
    .expect("derive");
    let (width, height): (Option<i32>, Option<i32>) =
        sqlx::query_as("SELECT width, height FROM assets WHERE id = $1")
            .bind(flipped.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("row");
    assert_eq!(
        (width, height),
        (Some(600), Some(400)),
        "a mirror does not swap the axes"
    );
}

/// A measurement fills nulls and nothing else.
///
/// The cheap probe is authoritative where it worked, and a re-run of this job must not change an answer
/// somebody has already seen — a dimension that flips between runs is a grid that reflows for no reason.
async fn a_measurement_never_overwrites_one_that_already_exists(f: &Fixture) {
    let bytes = jpeg(800, 400);
    stage(f, "measure003", "keep-mine.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "measure003",
        None,
    )
    .await
    .expect("finalise");

    // A deliberately wrong height, present. Only the null is the measurement's business.
    sqlx::query("UPDATE assets SET width = NULL, height = 4242 WHERE id = $1")
        .bind(finalised.asset_id)
        .execute(&f.tenant)
        .await
        .expect("seed");

    dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("derive");

    let (width, height): (Option<i32>, Option<i32>) =
        sqlx::query_as("SELECT width, height FROM assets WHERE id = $1")
            .bind(finalised.asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("row");
    assert_eq!(width, Some(800), "the null was filled");
    assert_eq!(height, Some(4242), "the value that existed was left alone");
}

async fn an_asset_gets_a_thumbnail_a_preview_and_a_proxy(f: &Fixture) {
    let bytes = jpeg(1200, 800);
    stage(f, "derive001", "wide.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "derive001",
        None,
    )
    .await
    .expect("finalise");

    let derived = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("derive");

    let mut rendered = derived.rendered.clone();
    rendered.sort();
    assert_eq!(
        rendered,
        vec!["preview-1024", "thumb-256", "web-2048"],
        "all three built-in profiles, refused: {:?}",
        derived.refused
    );
    assert!(derived.has_thumbnail());

    // A row per profile, keyed on the op hash — which is what the delivery path resolves through, rather than
    // by name, so a redefined profile cannot serve yesterday's bytes.
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT role, profile, mime, bytes FROM derivatives WHERE asset_id = $1 ORDER BY profile",
    )
    .bind(finalised.asset_id)
    .fetch_all(&f.tenant)
    .await
    .expect("rows");
    assert_eq!(rows.len(), 3);
    let thumb = rows
        .iter()
        .find(|row| row.1 == "thumb-256")
        .expect("a thumbnail row");
    assert_eq!(thumb.0, "thumbnail");
    assert_eq!(thumb.2, "image/webp");
    assert!(thumb.3 > 0, "the thumbnail has bytes");

    // And the bytes are actually in the store, at the key the row names.
    let key: String = sqlx::query_scalar(
        "SELECT object_key FROM derivatives WHERE asset_id = $1 AND profile = 'thumb-256'",
    )
    .bind(finalised.asset_id)
    .fetch_one(&f.tenant)
    .await
    .expect("key");
    let stored = blob(f)
        .get(&Key::new(key).expect("valid key"), None)
        .await
        .expect("get")
        .into_bytes(&Key::new("x").expect("k"))
        .expect("bytes");
    assert!(!stored.is_empty());

    // A WebP, decodable, and no larger than the box it was fitted into.
    let decoded = image::load_from_memory(&stored).expect("the thumbnail decodes");
    assert!(
        decoded.width() <= 256 && decoded.height() <= 256,
        "fitted into the 256 box, got {}x{}",
        decoded.width(),
        decoded.height()
    );
}

async fn deriving_twice_records_one_row_per_profile(f: &Fixture) {
    let bytes = jpeg(300, 300);
    stage(f, "derive002", "again.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "derive002",
        None,
    )
    .await
    .expect("finalise");

    let first = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("first");
    let second = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("second");

    assert_eq!(first.rendered.len(), 3);
    assert!(
        second.rendered.is_empty(),
        "the second run renders nothing: {:?}",
        second.rendered
    );
    assert_eq!(
        second.already.len(),
        3,
        "and reports all three as already present"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM derivatives WHERE asset_id = $1")
        .bind(finalised.asset_id)
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(count, 3, "one row per profile, not two");
}

async fn deriving_a_deleted_asset_is_permanent(f: &Fixture) {
    // The queue is asynchronous, so a user deleting an asset moments after uploading it is ordinary rather
    // than exceptional — and retrying cannot un-delete it.
    let bytes = jpeg(32, 32);
    stage(f, "derive003", "gone.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "derive003",
        None,
    )
    .await
    .expect("finalise");
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(finalised.asset_id)
        .execute(&f.tenant)
        .await
        .expect("delete");

    let error = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect_err("a deleted asset has nothing to derive");
    assert!(!error.is_transient(), "{error}");
}

async fn a_file_no_renderer_can_read_is_not_a_failure(f: &Fixture) {
    // A text file finalises fine — a DAM stores documents — and has no image rendition. That is **not** a job
    // failure, and the first version of this got it wrong: it returned a transient error, so the queue would
    // have retried a `.txt` five times and dead-lettered it. A format nothing can read is reported and the
    // grid draws a placeholder.
    //
    // A *missing tool* is the case that does fail the job, because it is a deployment mistake rather than a
    // fact about the file. That distinction is what this asserts.
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("conn");
    let mut session = dam_db::uploads::create(
        conn.executor(),
        f.tenant_id,
        "textfile001",
        Some(11),
        Some("notes.txt"),
        Some("text/plain"),
        None,
        None,
    )
    .await
    .expect("session");
    conn.commit().await.expect("commit");
    dam_store::resumable::patch(
        f.store.as_ref(),
        &mut session,
        0,
        bytes::Bytes::from_static(b"hello world"),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("conn");
    dam_db::uploads::save(conn.executor(), &session)
        .await
        .expect("save");
    conn.commit().await.expect("commit");

    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "textfile001",
        None,
    )
    .await
    .expect("a text file is a perfectly good asset");
    assert_eq!(finalised.mime, "text/plain");

    let derived = dam_pipeline::derive::asset(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        None,
    )
    .await
    .expect("a document with no image rendition is not a failed job");

    assert!(
        derived.rendered.is_empty(),
        "nothing should have rendered from a text file: {:?}",
        derived.rendered
    );
    assert!(!derived.has_thumbnail());
    assert_eq!(
        derived.refused.len(),
        3,
        "each profile is refused, with a reason: {:?}",
        derived.refused
    );
    assert!(
        derived
            .refused
            .iter()
            .all(|(_, reason)| reason.contains("no renderer can read this")),
        "the reason must say the format is unreadable rather than blaming the worker: {:?}",
        derived.refused
    );

    // And nothing was recorded, so the delivery path answers "not rendered" rather than serving a stub.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM derivatives WHERE asset_id = $1")
        .bind(finalised.asset_id)
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(count, 0);
}

async fn indexing_twice_leaves_one_document(f: &Fixture, context: &dam_pipeline::worker::Context) {
    // Tantivy has no update, so a re-index that did not delete the asset's previous document would leave two —
    // the same asset returned twice in search results, and every facet count it contributes to doubled. The
    // delete-by-term is what prevents that, and nothing asserted it: a mutation removing it passed the suite.
    //
    // Re-indexing is ordinary rather than exceptional: the `index` job's dedupe key only holds while it is
    // queued, so every metadata edit legitimately queues another one.
    let bytes = jpeg(120, 90);
    stage(f, "reindex001", "twice-indexed.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "reindex001",
        None,
    )
    .await
    .expect("finalise");

    let job = |id| {
        dam_db::jobs::JobSpec::new(f.tenant_id, dam_pipeline::worker::kind::INDEX)
            .payload(serde_json::json!({ "asset_id": id }))
    };

    for _ in 0..2 {
        let id = dam_db::jobs::enqueue(&f.global, job(finalised.asset_id))
            .await
            .expect("enqueue");
        let claimed = dam_db::jobs::claim(
            &f.global,
            "reindex-worker",
            dam_db::jobs::ClaimOptions::default(),
        )
        .await
        .expect("claim");
        let this = claimed
            .iter()
            .find(|c| c.id == id)
            .expect("the index job just enqueued");
        dam_pipeline::worker::handle(context, this)
            .await
            .expect("index");
        dam_db::jobs::complete(&f.global, id)
            .await
            .expect("complete");
    }

    let schema = dam_search::IndexSchema::new(dam_db::fields::load(&f.tenant).await.expect("defs"));
    let open = context
        .indexes
        .get(&f.slug, &schema)
        .await
        .expect("open the index");
    open.reload().expect("reload");
    let found = open
        .searcher()
        .search(
            &tantivy::query::TermQuery::new(
                tantivy::Term::from_field_text(schema.asset_id(), &finalised.asset_id.to_string()),
                tantivy::schema::IndexRecordOption::Basic,
            ),
            &tantivy::collector::Count,
        )
        .expect("search");
    assert_eq!(
        found, 1,
        "two index runs must leave one document, or the asset appears twice in every search"
    );
}

// ─── drivers ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn finalisation_holds() {
    let f = fixture().await;
    a_staged_upload_becomes_an_asset(&f).await;
    embedded_metadata_is_imported_and_beats_a_blanket_default(&f).await;
    finalising_twice_produces_one_asset(&f).await;
    two_uploads_of_one_file_share_an_object_and_are_two_assets(&f).await;
    an_incomplete_upload_is_a_permanent_refusal_not_a_retry(&f).await;
    an_upload_with_no_session_is_permanent(&f).await;
    a_promotion_that_lost_its_asset_row_resumes(&f).await;
    a_promotion_whose_store_is_unreachable_retries_rather_than_dies(&f).await;
}

#[tokio::test]
async fn derivation_holds() {
    let f = fixture().await;
    an_asset_gets_a_thumbnail_a_preview_and_a_proxy(&f).await;
    deriving_twice_records_one_row_per_profile(&f).await;
    deriving_a_deleted_asset_is_permanent(&f).await;
    a_file_no_renderer_can_read_is_not_a_failure(&f).await;
    a_video_is_measured_even_though_nothing_renders_it(&f).await;
    an_asset_whose_derivatives_all_exist_is_still_measured(&f).await;
    a_measurement_never_overwrites_one_that_already_exists(&f).await;
    a_quarter_turn_swaps_the_axes_in_the_measurement(&f).await;
}

#[tokio::test]
async fn conversion_rendering_holds() {
    // Q.11c. The tenant-defined path, which shares this module's renderer and its vips fallback — see
    // `derive::Recipe` on why a signature changed for that.
    let f = fixture().await;
    a_named_format_is_rendered_and_recorded(&f).await;
    rendering_the_same_format_twice_records_one_row(&f).await;
    a_conversion_for_the_wrong_class_is_permanent(&f).await;
    an_unknown_conversion_is_permanent(&f).await;
    a_withdrawn_conversion_still_renders(&f).await;
}

/// A conversion row, returning its key.
async fn declare_conversion(
    f: &Fixture,
    key: &str,
    size: i32,
    format: &str,
    class: &str,
) -> String {
    sqlx::query(
        "INSERT INTO conversions \
         (id, key, label, description, media_class, max_width, max_height, format, quality, fit) \
         VALUES (gen_random_uuid(), $1, $1, 'A format.', $2, $3, $3, $4, 82, 'contain')",
    )
    .bind(key)
    .bind(class)
    .bind(size)
    .bind(format)
    .execute(&f.tenant)
    .await
    .expect("conversion");
    key.to_owned()
}

async fn a_named_format_is_rendered_and_recorded(f: &Fixture) {
    let bytes = jpeg(1200, 800);
    stage(f, "conv001", "poster.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "conv001",
        None,
    )
    .await
    .expect("finalise");

    let key = declare_conversion(f, "email-600", 600, "png", "image").await;
    let rendered = dam_pipeline::derive::conversion(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        &key,
        None,
    )
    .await
    .expect("render");
    assert!(rendered.rendered, "{rendered:?}");

    // Recorded under the role 0001 reserved for exactly this and which nothing had ever written, with the
    // conversion's key as the profile name — which is what a delivery token carries.
    let row: (String, String, String, i64) = sqlx::query_as(
        "SELECT role, profile, mime, bytes FROM derivatives WHERE asset_id = $1 AND profile = $2",
    )
    .bind(finalised.asset_id)
    .bind(&key)
    .fetch_one(&f.tenant)
    .await
    .expect("row");
    assert_eq!(row.0, "rendition");
    assert_eq!(row.1, "email-600");
    assert_eq!(row.2, "image/png", "the recipe's format, not the source's");
    assert!(row.3 > 0);

    // And the bytes are in the store, decodable, and no larger than the box the tenant asked for. This is the
    // half that proves the recipe reached the renderer rather than a default.
    let object: String = sqlx::query_scalar(
        "SELECT object_key FROM derivatives WHERE asset_id = $1 AND profile = $2",
    )
    .bind(finalised.asset_id)
    .bind(&key)
    .fetch_one(&f.tenant)
    .await
    .expect("key");
    let stored = blob(f)
        .get(&Key::new(object).expect("valid key"), None)
        .await
        .expect("get")
        .into_bytes(&Key::new("x").expect("k"))
        .expect("bytes");
    let decoded = image::load_from_memory(&stored).expect("the rendition decodes");
    assert!(
        decoded.width() <= 600 && decoded.height() <= 600,
        "fitted into the tenant's 600 box, got {}x{}",
        decoded.width(),
        decoded.height()
    );
}

async fn rendering_the_same_format_twice_records_one_row(f: &Fixture) {
    // At-least-once queue plus two people choosing the same format. The second call must not re-render or
    // insert a second row: the cache key is content-addressed on the recipe.
    let bytes = jpeg(400, 400);
    stage(f, "conv002", "again.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "conv002",
        None,
    )
    .await
    .expect("finalise");
    let key = declare_conversion(f, "small-200", 200, "jpeg", "image").await;

    let first = dam_pipeline::derive::conversion(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        &key,
        None,
    )
    .await
    .expect("render");
    let second = dam_pipeline::derive::conversion(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        &key,
        None,
    )
    .await
    .expect("render again");
    assert!(first.rendered);
    assert!(
        !second.rendered,
        "the second render did work it did not need to"
    );

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM derivatives WHERE asset_id = $1 AND profile = $2")
            .bind(finalised.asset_id)
            .bind(&key)
            .fetch_one(&f.tenant)
            .await
            .expect("count");
    assert_eq!(count, 1);
}

async fn a_conversion_for_the_wrong_class_is_permanent(f: &Fixture) {
    // A *document* asset with an image format named against it. That direction, not the reverse: the table's
    // CHECK constrains conversions to `image`, so an image conversion of a text file is the reachable mismatch —
    // and it is reachable, because a job payload is not a promise. It could have been enqueued before the asset
    // was replaced, or by anything that writes the queue directly.
    //
    // Permanent rather than transient: retrying cannot make a text file into a photograph.
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("conn");
    let mut session = dam_db::uploads::create(
        conn.executor(),
        f.tenant_id,
        "conv003",
        Some(11),
        Some("readme.txt"),
        Some("text/plain"),
        None,
        None,
    )
    .await
    .expect("session");
    conn.commit().await.expect("commit");
    dam_store::resumable::patch(
        f.store.as_ref(),
        &mut session,
        0,
        bytes::Bytes::from_static(b"plain words"),
        StorageClass::Standard,
    )
    .await
    .expect("patch");
    let mut conn = dam_db::TenantConn::begin(&f.global, &f.slug)
        .await
        .expect("conn");
    dam_db::uploads::save(conn.executor(), &session)
        .await
        .expect("save");
    conn.commit().await.expect("commit");
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "conv003",
        None,
    )
    .await
    .expect("finalise");
    let key = declare_conversion(f, "doc-800", 800, "png", "image").await;

    let refusal = dam_pipeline::derive::conversion(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        &key,
        None,
    )
    .await
    .expect_err("a class mismatch must refuse");
    // Permanent, and — the part that matters — refused *for the right reason*. Without the class check this
    // still fails: the renderer cannot read a text file either, so it comes back as "no renderer can read
    // this". Both are permanent, so the only observable difference is what an operator reads in a dead-lettered
    // job, which is why this asserts the sentence rather than only the variant. Mutation testing found the
    // weaker version of this case passing with the check removed.
    let dam_pipeline::Error::Permanent(reason) = &refusal else {
        panic!("a class mismatch must be permanent, got {refusal:?}");
    };
    assert!(
        reason.contains("applies to image") && reason.contains("document"),
        "the refusal does not say the format is for a different kind of thing: {reason}"
    );
}

async fn an_unknown_conversion_is_permanent(f: &Fixture) {
    let bytes = jpeg(300, 300);
    stage(f, "conv004", "unknown.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "conv004",
        None,
    )
    .await
    .expect("finalise");

    let refusal = dam_pipeline::derive::conversion(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        "no-such-format",
        None,
    )
    .await
    .expect_err("an unknown format must refuse");
    assert!(
        matches!(refusal, dam_pipeline::Error::Permanent(_)),
        "retrying cannot make a conversion exist: {refusal:?}"
    );
}

async fn a_withdrawn_conversion_still_renders(f: &Fixture) {
    // The key is what a delivery token carries, so a link issued while a format was offered must still
    // resolve. Refusing here would make withdrawing a format retroactively break links that were valid when
    // they were sent.
    let bytes = jpeg(500, 500);
    stage(f, "conv005", "withdrawn.jpg", &bytes).await;
    let finalised = dam_pipeline::finalise::upload(
        &f.global,
        f.store.as_ref(),
        &f.slug,
        f.tenant_id,
        "conv005",
        None,
    )
    .await
    .expect("finalise");
    let key = declare_conversion(f, "retired-300", 300, "jpeg", "image").await;
    sqlx::query("UPDATE conversions SET is_active = false WHERE key = $1")
        .bind(&key)
        .execute(&f.tenant)
        .await
        .expect("withdraw");

    let rendered = dam_pipeline::derive::conversion(
        &f.global,
        blob(f),
        &f.slug,
        f.tenant_id,
        finalised.asset_id,
        &key,
        None,
    )
    .await
    .expect("a withdrawn format still renders for links already issued");
    assert!(rendered.rendered);
}

#[tokio::test]
async fn the_whole_chain_runs_through_the_worker() {
    // The dispatch, not just the stages: finalisation enqueues derivation, derivation enqueues indexing, and
    // an unknown kind is permanent. Driving `handle` directly rather than the polling loop keeps the test
    // deterministic — the loop's own behaviour is a sleep and a claim, and asserting on those would be
    // asserting on timing.
    let f = fixture().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let context = dam_pipeline::worker::Context {
        // No hosted-model context: these suites are about the queue and the render stages.
        ai: None,
        scanner: None,
        signing_identity: None,
        global: f.global.clone(),
        store: Arc::clone(&f.store),
        indexes: Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
            dir.path(),
        ))),
        worker: "test-worker".to_owned(),
        // No webhook subscriptions in these fixtures, so nothing is ever dispatched. A default client
        // rather than a builder, because what these suites exercise is unrelated to how it is configured.
        http: reqwest::Client::new(),
    };

    // A field definition, so the index schema has something to carry.
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, display_order) \
         VALUES (gen_random_uuid(), 'caption', 'Caption', 'text', 1)",
    )
    .execute(&f.tenant)
    .await
    .expect("field def");

    let bytes = jpeg(500, 400);
    stage(&f, "chain001", "chained.jpg", &bytes).await;
    let finalise_id = dam_pipeline::worker::enqueue_finalise(&f.global, f.tenant_id, "chain001")
        .await
        .expect("enqueue");

    // Claimed and dispatched the way the loop would.
    let claimed = dam_db::jobs::claim(
        &f.global,
        "test-worker",
        dam_db::jobs::ClaimOptions::default(),
    )
    .await
    .expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, finalise_id);
    assert_eq!(claimed[0].kind, dam_pipeline::worker::kind::FINALISE_UPLOAD);
    dam_pipeline::worker::handle(&context, &claimed[0])
        .await
        .expect("finalise");
    dam_db::jobs::complete(&f.global, finalise_id)
        .await
        .expect("complete");

    // Finalisation queued the derive.
    let claimed = dam_db::jobs::claim(
        &f.global,
        "test-worker",
        dam_db::jobs::ClaimOptions::default(),
    )
    .await
    .expect("claim");
    assert_eq!(claimed.len(), 1, "finalisation must have queued a derive");
    assert_eq!(claimed[0].kind, dam_pipeline::worker::kind::DERIVE);
    dam_pipeline::worker::handle(&context, &claimed[0])
        .await
        .expect("derive");
    dam_db::jobs::complete(&f.global, claimed[0].id)
        .await
        .expect("complete");

    // Which queued the index and the similarity pass. Both, because a proxy now exists: indexing so an asset
    // reaching search already has a thumbnail to draw, and similarity because that is where the bytes it
    // hashes start existing. Run in whatever order the queue hands them over — neither depends on the other.
    let claimed = dam_db::jobs::claim(
        &f.global,
        "test-worker",
        dam_db::jobs::ClaimOptions::default(),
    )
    .await
    .expect("claim");
    let kinds: Vec<&str> = claimed.iter().map(|job| job.kind.as_str()).collect();
    assert!(
        kinds.contains(&dam_pipeline::worker::kind::INDEX),
        "derivation must have queued an index: {kinds:?}"
    );
    assert!(
        kinds.contains(&dam_pipeline::worker::kind::SIMILARITY),
        "and the similarity pass, which needs the proxy derivation just made: {kinds:?}"
    );
    for job in &claimed {
        dam_pipeline::worker::handle(&context, job)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", job.kind));
        dam_db::jobs::complete(&f.global, job.id)
            .await
            .expect("complete");
    }

    // The asset exists, has a thumbnail, and is searchable.
    let asset_id: Uuid = sqlx::query_scalar("SELECT id FROM assets WHERE filename = 'chained.jpg'")
        .fetch_one(&f.tenant)
        .await
        .expect("asset");
    let thumbs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM derivatives WHERE asset_id = $1 AND role = 'thumbnail'",
    )
    .bind(asset_id)
    .fetch_one(&f.tenant)
    .await
    .expect("count");
    assert_eq!(thumbs, 1);

    // And the similarity pass ran over the proxy the same derivation produced. Asserted here rather than in
    // its own test because the point is the *chain*: these rows exist because a real upload went through a
    // real worker, not because a fixture wrote them.
    let (phash, dhash): (i64, i64) =
        sqlx::query_as("SELECT phash, dhash FROM asset_phashes WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_one(&f.tenant)
            .await
            .expect("the asset was hashed");
    assert_ne!(phash, dhash, "two algorithms, two values");
    let colours: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_colors WHERE asset_id = $1")
        .bind(asset_id)
        .fetch_one(&f.tenant)
        .await
        .expect("count colours");
    assert!(
        (1..=5).contains(&colours),
        "between one and five dominant colours, got {colours}"
    );
    // Nothing else in this fixture, so nothing to be a duplicate of.
    let candidates: i64 = sqlx::query_scalar("SELECT count(*) FROM duplicate_candidates")
        .fetch_one(&f.tenant)
        .await
        .expect("count candidates");
    assert_eq!(candidates, 0, "one asset is a duplicate of nothing");

    let schema = dam_search::IndexSchema::new(dam_db::fields::load(&f.tenant).await.expect("defs"));
    let open = context
        .indexes
        .get(&f.slug, &schema)
        .await
        .expect("open the index");
    open.reload().expect("reload");
    let searcher = open.searcher();
    let found = searcher
        .search(
            &tantivy::query::TermQuery::new(
                tantivy::Term::from_field_text(schema.asset_id(), &asset_id.to_string()),
                tantivy::schema::IndexRecordOption::Basic,
            ),
            &tantivy::collector::Count,
        )
        .expect("search");
    assert_eq!(found, 1, "the asset is in the tenant's index");

    indexing_twice_leaves_one_document(&f, &context).await;

    // An unknown kind is **transient**, which is the opposite of what this asserted until Q.11. Version skew is
    // a rolling deploy, and the fleet contains workers that *do* understand the kind — so the retry is the whole
    // point. Marking it permanent collapsed the attempts and dead-lettered work a newer worker would have done,
    // which is exactly what happened live: an old dev worker claimed the first conversion render and killed it.
    let unknown = dam_db::jobs::enqueue(
        &f.global,
        dam_db::jobs::JobSpec::new(f.tenant_id, "no_such_kind"),
    )
    .await
    .expect("enqueue");
    let claimed = dam_db::jobs::claim(
        &f.global,
        "test-worker",
        dam_db::jobs::ClaimOptions::default(),
    )
    .await
    .expect("claim");
    let job = claimed
        .iter()
        .find(|job| job.id == unknown)
        .expect("the unknown job");
    let error = dam_pipeline::worker::handle(&context, job)
        .await
        .expect_err("an unknown kind cannot be handled");
    assert!(
        error.is_transient(),
        "an unknown kind must be retried so another worker can take it: {error}"
    );

    // And the retry is real: after `fail`, the job is queued again rather than dead, with an attempt spent. This
    // is the half the variant alone does not prove — `worker::fail` collapses the attempts of a *permanent*
    // error, so asserting only on `is_transient` would pass while the job still died.
    dam_pipeline::worker::record_failure(&context, job, &error)
        .await
        .expect("record");
    let (state, attempts): (String, i32) =
        sqlx::query_as("SELECT state, attempts FROM dam_global.jobs WHERE id = $1")
            .bind(unknown)
            .fetch_one(&f.global)
            .await
            .expect("row");
    assert_eq!(state, "queued", "an unknown kind was dead-lettered");
    assert!(attempts < 5, "the remaining attempts were collapsed");
}
