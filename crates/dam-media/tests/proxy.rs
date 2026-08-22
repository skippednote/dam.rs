//! The master proxy (task 1.8) — the §2 invariant.
//!
//! §2: only the original master tiers to cold storage, and the proxy is what makes that safe. It
//! is "a deliberately generous derivative (2048px JPEG / 720p H.264 / extracted text) good enough
//! to serve every future preview *and* to re-run every future AI model. When the tagging model is
//! upgraded, we re-embed the entire library off proxies and issue **zero restores**."
//!
//! The failure mode is quiet. Nothing breaks the day a stage starts reading originals; the bill
//! arrives at the next model upgrade, as a restore storm across the whole archive. `used_original`
//! is the alarm — but an alarm nobody has wired up is a comment, so this suite is about making the
//! invariant hold *structurally*: an enrichment stage takes a type that cannot be built from an
//! original, and the escape hatch records itself.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use dam_media::proxy::{self, EnrichmentSource, PROXY_LONG_EDGE};
use dam_store::Key;
use image::{ImageFormat, Rgb, RgbImage};
use std::io::Cursor;
use uuid::Uuid;

const HASH: &str = "9f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

fn tenant() -> Uuid {
    Uuid::from_u128(0x0da3_0000_0000_0000_0000_0000_0000_0008)
}

/// Photo-like bytes: correlated noise over a gradient, encoded as JPEG.
///
/// A smooth synthetic gradient is the wrong fixture for anything that reasons about *size* — PNG
/// compresses it to a fraction of a real photograph, which made an earlier version of the
/// size-reduction test compare the proxy against a 187 KB "master" and fail for a reason that had
/// nothing to do with the code.
fn photo(w: u32, h: u32) -> Vec<u8> {
    let mut img = RgbImage::new(w, h);
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for (x, y, px) in img.enumerate_pixels_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let noise = (seed % 48) as i32 - 24;
        let base = ((x as i32 * 255 / w.max(1) as i32) + (y as i32 * 120 / h.max(1) as i32)) / 2;
        let channel = |offset: i32| (base + noise + offset).clamp(0, 255) as u8;
        *px = Rgb([channel(10), channel(0), channel(-15)]);
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, ImageFormat::Jpeg)
        .expect("encode");
    out.into_inner()
}

/// A PNG source, for the cases that care about pixels rather than bytes.
fn photo_png(w: u32, h: u32) -> Vec<u8> {
    let decoded = image::load_from_memory(&photo(w, h)).expect("decode");
    let mut out = Cursor::new(Vec::new());
    decoded
        .write_to(&mut out, ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

fn proxy_key() -> Key {
    Key::proxy(tenant(), HASH, "jpg").expect("key")
}

// ─── the structural invariant ───────────────────────────────────────────────

#[test]
fn an_enrichment_source_cannot_be_built_from_an_original() {
    // This is the whole point. A comment saying "read the proxy" is a convention, and conventions
    // decay one commit at a time. A constructor that refuses the original key is a rule the
    // compiler helps enforce — a stage cannot be handed the master by accident.
    let original = Key::original(tenant(), HASH).expect("key");
    let err = EnrichmentSource::from_proxy(&original, Bytes::from_static(b"master bytes"))
        .expect_err("an original must be refused");
    assert!(
        format!("{err}").contains("proxy"),
        "the error must say what was expected: {err}"
    );
}

#[test]
fn only_the_proxy_namespace_counts_as_a_proxy() {
    // A thumbnail is hot and cheap too, but it is 400px — re-running an embedding model against it
    // would silently degrade every vector in the library. "Not the original" is not the test;
    // "is the proxy" is.
    let bytes = Bytes::from_static(b"x");
    for wrong in [
        Key::thumbnail(tenant(), HASH, 400).expect("key"),
        Key::derivative(tenant(), HASH, "abc", "jpg").expect("key"),
        Key::manifest(tenant(), HASH).expect("key"),
        Key::staging(tenant(), "upload-1").expect("key"),
    ] {
        assert!(
            EnrichmentSource::from_proxy(&wrong, bytes.clone()).is_err(),
            "{wrong} is not a proxy"
        );
    }
    assert!(EnrichmentSource::from_proxy(&proxy_key(), bytes).is_ok());
}

#[test]
fn a_source_built_from_the_proxy_reports_that_it_did_not_use_the_original() {
    let source =
        EnrichmentSource::from_proxy(&proxy_key(), Bytes::from_static(b"proxy bytes")).expect("ok");
    assert!(!source.used_original());
    assert_eq!(source.original_read_reason(), None);
    assert_eq!(&source.bytes()[..], b"proxy bytes");
}

#[test]
fn reading_the_original_is_possible_but_records_itself() {
    // Some stages legitimately need the master while it is still hot — C2PA verification at ingest
    // reads the bytes it is attesting to. So the escape hatch exists, and using it sets the alarm
    // and demands a reason. What it must never be is *convenient*.
    let original = Key::original(tenant(), HASH).expect("key");
    let source = EnrichmentSource::from_original_with_reason(
        &original,
        Bytes::from_static(b"master bytes"),
        "c2pa verification at ingest",
    )
    .expect("the escape hatch works");

    assert!(source.used_original(), "the alarm must be set");
    assert_eq!(
        source.original_read_reason(),
        Some("c2pa verification at ingest"),
        "and the reason recorded, because a flag with no reason cannot be triaged"
    );
}

#[test]
fn the_escape_hatch_demands_a_non_empty_reason() {
    let original = Key::original(tenant(), HASH).expect("key");
    for blank in ["", "   ", "\t\n"] {
        assert!(
            EnrichmentSource::from_original_with_reason(&original, Bytes::from_static(b"x"), blank)
                .is_err(),
            "a blank reason would make the alarm untriageable"
        );
    }
}

// ─── building the proxy ─────────────────────────────────────────────────────

#[test]
fn an_image_proxy_is_generous_enough_to_re_run_a_model_against() {
    // §2 says 2048px. The number matters: at 512px an image embedding loses fine detail, and the
    // whole promise is that a future model can be re-run off proxies with zero restores.
    let source = photo(4000, 3000);
    let built = proxy::build_image(&source).expect("build");

    let img = image::load_from_memory(&built.bytes).expect("decode");
    assert_eq!(
        img.width().max(img.height()),
        PROXY_LONG_EDGE,
        "the long edge must be exactly the proxy size"
    );
    assert_eq!(
        (img.width(), img.height()),
        (2048, 1536),
        "and the aspect ratio preserved"
    );
    assert_eq!(built.extension, "jpg");
}

#[test]
fn a_proxy_is_much_smaller_than_the_master_it_stands_in_for() {
    // The economics of §2: the hot footprint is ~0.5 MB per asset regardless of asset size. A
    // proxy that came out the same size as the master would defeat the entire design.
    let source = photo(4000, 3000);
    let built = proxy::build_image(&source).expect("build");
    assert!(
        built.bytes.len() * 4 < source.len(),
        "proxy is {} bytes against a {}-byte master — not a meaningful reduction",
        built.bytes.len(),
        source.len()
    );
    // And bounded in absolute terms, because §2's economics are per-asset rather than a ratio: the
    // hot footprint has to stay near half a megabyte whatever the master's size.
    assert!(
        built.bytes.len() < 700_000,
        "the proxy for a 12-megapixel master is {} bytes, which does not fit §2's footprint",
        built.bytes.len()
    );
}

#[test]
fn a_small_source_is_not_upscaled_into_its_proxy() {
    // A 300px original does not become a blurry 2048px proxy. It is already small enough to keep
    // hot forever, which is the only thing the proxy exists to guarantee.
    let source = photo_png(300, 200);
    let built = proxy::build_image(&source).expect("build");
    let img = image::load_from_memory(&built.bytes).expect("decode");
    assert_eq!((img.width(), img.height()), (300, 200));
}

#[test]
fn a_text_proxy_is_the_extracted_text_itself() {
    // For a document the proxy is not an image: §2 lists "extracted text" alongside the JPEG and
    // the H.264. It is what a future embedding model re-reads, so it is stored verbatim.
    let text = "Shot list for the spring campaign.\nLocation: Lisbon.\n";
    let built = proxy::build_text(text).expect("build");
    assert_eq!(&built.bytes[..], text.as_bytes());
    assert_eq!(built.extension, "txt");
}

#[test]
fn an_empty_text_proxy_is_refused() {
    // An empty proxy would read as "this asset has no text" forever, and nothing would ever
    // re-extract it. Better to have no proxy row than a proxy that lies.
    assert!(proxy::build_text("").is_err());
    assert!(proxy::build_text("   \n\t ").is_err());
}

#[test]
fn a_proxy_key_is_exempt_from_tiering_even_if_a_policy_asks() {
    // The mechanical half of §2, already enforced by `Key`, asserted here because this is the
    // invariant task: a lifecycle policy that tries to tier a proxy must be clamped, not obeyed.
    let key = proxy_key();
    assert!(key.is_tier_exempt());
    assert_eq!(
        key.permitted_class(dam_core::StorageClass::DeepArchive),
        dam_core::StorageClass::Standard,
        "the AI substrate must stay hot whatever the policy says"
    );
}

#[test]
fn the_proxy_for_a_bomb_is_refused_rather_than_attempted() {
    let mut bytes = photo_png(4, 4);
    bytes[16..20].copy_from_slice(&65_535u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&65_535u32.to_be_bytes());
    let crc = {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in &bytes[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    };
    bytes[29..33].copy_from_slice(&crc.to_be_bytes());

    assert!(
        proxy::build_image(&bytes).is_err(),
        "the proxy builder runs on every upload, so it needs the same guard as the renderer"
    );
}

#[test]
fn a_rotated_master_produces_an_upright_proxy() {
    // The proxy is what every preview and every model sees. If it is sideways, the entire library
    // is sideways downstream — and re-generating proxies later means reading originals, which is
    // the restore storm this design exists to avoid.
    let landscape = {
        let mut out = Cursor::new(Vec::new());
        let mut img = RgbImage::new(80, 40);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, 10]);
        }
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, ImageFormat::Jpeg)
            .expect("encode");
        out.into_inner()
    };

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II\x2A\x00");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&6u16.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_le_bytes());
    let mut payload = Vec::new();
    payload.extend_from_slice(b"Exif\0\0");
    payload.extend_from_slice(&tiff);
    let mut rotated = Vec::new();
    rotated.extend_from_slice(&landscape[..2]);
    rotated.extend_from_slice(&[0xFF, 0xE1]);
    rotated.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    rotated.extend_from_slice(&payload);
    rotated.extend_from_slice(&landscape[2..]);

    let built = proxy::build_image(&rotated).expect("build");
    let img = image::load_from_memory(&built.bytes).expect("decode");
    assert_eq!(
        (img.width(), img.height()),
        (40, 80),
        "the proxy must be upright, not carry the rotation forward"
    );
}
