//! Image probing (task 1.7): the technical facts that fill the `assets` columns.
//!
//! Two things here are easy to get wrong and expensive to discover later:
//!
//! - **EXIF orientation.** A phone stores a portrait photo as 4000×3000 pixels with
//!   `orientation = 6`. Report those numbers as-is and every grid cell, aspect ratio and
//!   thumbnail is sideways — the most common visible bug in a DAM.
//! - **Reading dimensions without decoding.** A header claiming 60000×60000 is a request to
//!   allocate ten gigabytes. The probe must answer from the header alone.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::probe::{self, Probe};
use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use std::io::Cursor;

/// Encodes an image to bytes in `format`.
fn encode(img: &image::DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).expect("encode");
    out.into_inner()
}

fn rgb(w: u32, h: u32) -> image::DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = Rgb([(x * 7 % 256) as u8, (y * 11 % 256) as u8, 128]);
    }
    image::DynamicImage::ImageRgb8(img)
}

fn rgba_with_transparency(w: u32, h: u32) -> image::DynamicImage {
    let mut img = RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = Rgba([200, 100, 50, if (x + y) % 2 == 0 { 0 } else { 255 }]);
    }
    image::DynamicImage::ImageRgba8(img)
}

/// Splices a minimal EXIF APP1 segment carrying `orientation` into a JPEG.
///
/// Hand-built rather than taken from a fixture file so the test states exactly which bytes
/// produce the behaviour — and so the suite carries no binary blobs whose provenance nobody
/// can check.
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "expected a JPEG SOI");

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II\x2A\x00"); // little-endian TIFF magic
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
    tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]); // pad the 4-byte value field
    tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

    let mut payload = Vec::new();
    payload.extend_from_slice(b"Exif\0\0");
    payload.extend_from_slice(&tiff);

    let mut out = Vec::new();
    out.extend_from_slice(&jpeg[..2]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn probe(bytes: &[u8]) -> Probe {
    probe::image(bytes).expect("probe")
}

/// CRC-32/IEEE, as PNG chunks use. Twelve lines beats a dependency for one fixture.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
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
}

/// A real PNG whose IHDR has been rewritten to claim `width` x `height`.
///
/// Built by patching a valid file rather than hand-assembling one: only the claimed size differs
/// from something the encoder produced, so the test cannot pass because of an unrelated malformation.
/// The CRC is recomputed, since PNG readers reject a chunk whose checksum does not match.
fn png_claiming(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = encode(&rgb(4, 4), ImageFormat::Png);
    // 8-byte signature, then the IHDR chunk: 4-byte length, 4-byte type, 13-byte data, 4-byte CRC.
    bytes[16..20].copy_from_slice(&width.to_be_bytes());
    bytes[20..24].copy_from_slice(&height.to_be_bytes());
    let crc = crc32(&bytes[12..29]); // type + data
    bytes[29..33].copy_from_slice(&crc.to_be_bytes());
    bytes
}

#[test]
fn a_plain_image_reports_its_dimensions_and_colour() {
    let bytes = encode(&rgb(40, 20), ImageFormat::Png);
    let p = probe(&bytes);

    assert_eq!(p.stored_width, Some(40));
    assert_eq!(p.stored_height, Some(20));
    assert_eq!(p.display_width, Some(40));
    assert_eq!(p.display_height, Some(20));
    assert_eq!(p.has_alpha, Some(false));
    assert_eq!(p.color_space.as_deref(), Some("srgb"));
}

#[test]
fn transparency_is_detected_so_a_jpeg_derivative_can_be_matted() {
    // A PNG logo flattened onto black instead of white is a support ticket from a brand team,
    // and the only signal that it needed matting is this flag.
    let png = encode(&rgba_with_transparency(8, 8), ImageFormat::Png);
    assert_eq!(probe(&png).has_alpha, Some(true));

    let jpeg = encode(&rgb(8, 8), ImageFormat::Jpeg);
    assert_eq!(probe(&jpeg).has_alpha, Some(false), "JPEG has no alpha");
}

#[test]
fn a_rotated_photo_reports_display_dimensions_with_the_axes_swapped() {
    // The bug this prevents: a 4000x3000 portrait photo shown landscape in every grid cell.
    let landscape = encode(&rgb(40, 20), ImageFormat::Jpeg);
    let rotated = with_exif_orientation(&landscape, 6); // rotate 90° clockwise

    let p = probe(&rotated);
    assert_eq!(
        p.orientation,
        Some(6),
        "the original transform is preserved"
    );
    assert_eq!(
        (p.stored_width, p.stored_height),
        (Some(40), Some(20)),
        "the stored pixels are unchanged — this is what the file contains"
    );
    assert_eq!(
        (p.display_width, p.display_height),
        (Some(20), Some(40)),
        "and the display axes are swapped, which is what a layout needs"
    );
    assert!(p.needs_rotation(), "the derivative step has work to do");
}

#[test]
fn every_orientation_value_swaps_the_axes_only_when_it_should() {
    // 1-4 are identity or mirror/flip: same axes. 5-8 involve a quarter turn: swapped. Getting
    // the boundary wrong shows up as *some* photos being sideways, which reads as random.
    let landscape = encode(&rgb(40, 20), ImageFormat::Jpeg);
    for orientation in 1..=8u16 {
        let p = probe(&with_exif_orientation(&landscape, orientation));
        let swapped = matches!(orientation, 5..=8);
        let expected = if swapped {
            (Some(20), Some(40))
        } else {
            (Some(40), Some(20))
        };
        assert_eq!(
            (p.display_width, p.display_height),
            expected,
            "orientation {orientation} should {} the axes",
            if swapped { "swap" } else { "keep" }
        );
        assert_eq!(p.needs_rotation(), orientation != 1);
    }
}

#[test]
fn an_absent_orientation_tag_is_reported_as_absent_not_as_one() {
    // `None` and `Some(1)` mean different things: the first is a file with no EXIF at all, the
    // second a file that explicitly says "upright". The derivative pipeline treats them the
    // same; a provenance record should not conflate them.
    let bytes = encode(&rgb(10, 10), ImageFormat::Png);
    let p = probe(&bytes);
    assert_eq!(p.orientation, None);
    assert!(!p.needs_rotation());
}

#[test]
fn an_out_of_range_orientation_is_ignored_rather_than_trusted() {
    // EXIF orientation is 1-8. Cameras have written 0 and 9; applying an unknown transform
    // would corrupt the derivative, so it is dropped and the image treated as upright.
    let landscape = encode(&rgb(40, 20), ImageFormat::Jpeg);
    for bogus in [0u16, 9, 255] {
        let p = probe(&with_exif_orientation(&landscape, bogus));
        assert_eq!(
            (p.display_width, p.display_height),
            (Some(40), Some(20)),
            "orientation {bogus} is not a valid transform and must not swap the axes"
        );
        assert!(!p.needs_rotation());
    }
}

#[test]
fn a_header_claiming_enormous_dimensions_is_read_without_decoding_it() {
    // A GIF logical screen descriptor is 13 bytes and carries no checksum, so this is a
    // 65000x65000 image in 13 bytes — an instruction to allocate about 12 GB. The probe answers
    // from the header, so it neither allocates nor refuses.
    // 65535 x 65535 in a file of a few hundred bytes: about 12 gigapixels, or ~50 GB decoded.
    let bomb = png_claiming(65_535, 65_535);
    let p = probe::image(&bomb).expect("a header read must succeed");
    assert_eq!(p.stored_width, Some(65_535));
    assert_eq!(p.stored_height, Some(65_535));
    assert_eq!(
        p.pixel_count(),
        Some(65_535u64 * 65_535),
        "the caller needs the pixel count to decide whether to decode at all"
    );
    assert!(
        p.exceeds_pixel_budget(100_000_000),
        "and to be told when it is past a sane budget"
    );
}

#[test]
fn a_normal_image_is_within_the_pixel_budget() {
    let bytes = encode(&rgb(100, 100), ImageFormat::Png);
    let p = probe(&bytes);
    assert!(!p.exceeds_pixel_budget(100_000_000));
    assert_eq!(p.pixel_count(), Some(10_000));
}

#[test]
fn corrupt_or_truncated_bytes_produce_an_error_rather_than_a_panic() {
    let png = encode(&rgb(20, 20), ImageFormat::Png);
    for bad in [
        &[][..],
        &[0u8; 4][..],
        &png[..8],  // signature only
        &png[..20], // partial IHDR
        &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0][..],
    ] {
        // The assertion is that this returns rather than unwinding — a panic in a worker takes
        // the whole job runner down, and one malformed upload should not.
        let _ = probe::image(bad);
    }
}

#[test]
fn a_perceptual_hash_is_stable_and_close_for_similar_images() {
    // The point of a perceptual hash is that a re-save, a re-compression or a small crop stays
    // close while a different picture does not. A hash that only matched identical bytes would
    // duplicate what BLAKE3 already does.
    let original = rgb(64, 64);
    let png = encode(&original, ImageFormat::Png);
    let jpeg = encode(&original, ImageFormat::Jpeg);

    let a = probe::perceptual_hash(&png).expect("hash png");
    let b = probe::perceptual_hash(&png).expect("hash png again");
    assert_eq!(a, b, "the same bytes must hash identically every time");

    let recompressed = probe::perceptual_hash(&jpeg).expect("hash jpeg");
    let distance = probe::hash_distance(&a, &recompressed).expect("distance");
    assert!(
        distance <= 8,
        "a JPEG re-encode of the same picture should stay close, got {distance}"
    );

    let mut different = RgbImage::new(64, 64);
    for (x, y, px) in different.enumerate_pixels_mut() {
        *px = Rgb([if (x / 8 + y / 8) % 2 == 0 { 255 } else { 0 }, 0, 0]);
    }
    let unrelated = probe::perceptual_hash(&encode(
        &image::DynamicImage::ImageRgb8(different),
        ImageFormat::Png,
    ))
    .expect("hash");
    let far = probe::hash_distance(&a, &unrelated).expect("distance");
    assert!(
        far > distance,
        "an unrelated picture must be further away than a re-encode: {far} vs {distance}"
    );
}

#[test]
fn a_hash_of_a_bomb_sized_header_is_refused_rather_than_attempted() {
    // Hashing requires decoding, so unlike a probe it cannot be done from the header. The guard
    // has to be here rather than in the caller, because "hash everything on ingest" is exactly
    // what a worker will do.
    let err = probe::perceptual_hash(&png_claiming(65_535, 65_535))
        .expect_err("must refuse to decode a bomb");
    assert!(
        format!("{err}").contains("pixel"),
        "the error must name the reason: {err}"
    );
}

#[test]
fn an_icc_profile_is_detected_because_d11_says_it_must_be_preserved() {
    // D11: profiles are preserved end to end and CMYK converts at delivery, never at ingest.
    // Detecting the profile is what makes it possible to notice a master that has one — and to
    // flag one that does not, since an untagged CMYK file cannot be converted correctly later.
    let plain = encode(&rgb(10, 10), ImageFormat::Jpeg);
    assert!(!probe(&plain).has_icc_profile);

    // A JPEG APP2 segment tagged ICC_PROFILE, spliced in the same way as the EXIF one.
    let mut payload = Vec::new();
    payload.extend_from_slice(b"ICC_PROFILE\0");
    payload.extend_from_slice(&[1, 1]); // chunk 1 of 1
    payload.extend_from_slice(&[0u8; 16]); // a stand-in for the profile body
    let mut tagged = Vec::new();
    tagged.extend_from_slice(&plain[..2]);
    tagged.extend_from_slice(&[0xFF, 0xE2]);
    tagged.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    tagged.extend_from_slice(&payload);
    tagged.extend_from_slice(&plain[2..]);

    assert!(
        probe(&tagged).has_icc_profile,
        "an embedded profile must be noticed"
    );
}
