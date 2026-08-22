//! Derivative rendering (task 1.7).
//!
//! Four bugs this suite exists to prevent, all of which ship silently:
//!
//! - **Double rotation.** The source is rotated once on the way in; if the output keeps an
//!   orientation tag, the viewer rotates it again.
//! - **Dark halos.** Resizing RGBA without premultiplying alpha averages transparent black into
//!   every edge pixel, so a logo on transparency comes out with a grey fringe.
//! - **Black backgrounds.** A transparent PNG flattened to JPEG without matting is black where it
//!   should be white, which a brand team notices immediately.
//! - **Blurry upscales.** Asking for 2048px from a 100px source produces something that looks
//!   like a bug rather than a rendition.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::derive::{self, Fit, OutputFormat, Rendition};
use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use std::io::Cursor;

fn encode(img: &image::DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).expect("encode");
    out.into_inner()
}

fn photo(w: u32, h: u32) -> image::DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = Rgb([(x * 3 % 256) as u8, (y * 5 % 256) as u8, 90]);
    }
    image::DynamicImage::ImageRgb8(img)
}

/// An opaque white square surrounded by fully-transparent **black** — the arrangement that
/// produces a halo if alpha is not premultiplied before resizing.
///
/// The inset is deliberately not a multiple of any likely scale factor. An earlier version used
/// `size / 4`, which put the square's edge exactly on a block boundary when scaling 64 to 16: no
/// output pixel straddled the edge, so there was no halo to detect and the test passed for the
/// wrong reason.
fn logo_on_transparency(size: u32) -> image::DynamicImage {
    let mut img = RgbaImage::new(size, size);
    let inset = size / 4 + 3;
    for (x, y, px) in img.enumerate_pixels_mut() {
        let inside = x >= inset && y >= inset && x < size - inset && y < size - inset;
        *px = if inside {
            Rgba([255, 255, 255, 255])
        } else {
            Rgba([0, 0, 0, 0])
        };
    }
    image::DynamicImage::ImageRgba8(img)
}

fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II\x2A\x00");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_le_bytes());

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

fn decode(bytes: &[u8]) -> image::DynamicImage {
    image::load_from_memory(bytes).expect("decode the derivative")
}

fn spec(width: u32, height: u32, format: OutputFormat) -> Rendition {
    Rendition {
        width,
        height,
        format,
        quality: 82,
        fit: Fit::Contain,
        background: [255, 255, 255],
    }
}

#[test]
fn a_contained_rendition_fits_inside_the_box_and_keeps_its_aspect_ratio() {
    let source = encode(&photo(400, 200), ImageFormat::Png);
    let out = derive::render(&source, &spec(100, 100, OutputFormat::Png)).expect("render");
    let img = decode(&out);

    assert_eq!(
        (img.width(), img.height()),
        (100, 50),
        "a 2:1 source in a 100x100 box is 100x50, not stretched to fill"
    );
}

#[test]
fn a_covering_rendition_fills_the_box_and_crops_the_overflow() {
    // Cover is what a fixed-size grid cell needs: no letterboxing, and the subject centred.
    let source = encode(&photo(400, 200), ImageFormat::Png);
    let out = derive::render(
        &source,
        &Rendition {
            fit: Fit::Cover,
            ..spec(100, 100, OutputFormat::Png)
        },
    )
    .expect("render");
    let img = decode(&out);
    assert_eq!((img.width(), img.height()), (100, 100));
}

#[test]
fn a_source_smaller_than_the_request_is_never_upscaled() {
    // A blurry 2048px rendition of a 64px source looks like a defect. DAM convention is to cap at
    // the source size and let the caller decide whether that is usable.
    let source = encode(&photo(64, 48), ImageFormat::Png);
    let out = derive::render(&source, &spec(2048, 2048, OutputFormat::Png)).expect("render");
    let img = decode(&out);
    assert_eq!(
        (img.width(), img.height()),
        (64, 48),
        "the source dimensions are the ceiling"
    );
}

#[test]
fn a_rotated_source_is_uprighted_once_and_carries_no_orientation_tag() {
    // The double-rotation bug: rotate on the way in, leave the tag, and the viewer rotates again.
    let landscape = encode(&photo(80, 40), ImageFormat::Jpeg);
    let rotated = with_exif_orientation(&landscape, 6); // 90° clockwise

    let out = derive::render(&rotated, &spec(200, 200, OutputFormat::Jpeg)).expect("render");
    let img = decode(&out);
    assert_eq!(
        (img.width(), img.height()),
        (40, 80),
        "the derivative is upright, so its axes are the display ones"
    );

    let mut cursor = Cursor::new(&out);
    let exif = exif::Reader::new().read_from_container(&mut cursor);
    let orientation = exif.ok().and_then(|r| {
        r.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
    });
    assert!(
        orientation.is_none_or(|o| o == 1),
        "an upright derivative must not tell a viewer to rotate it again, got {orientation:?}"
    );
}

#[test]
fn resizing_transparency_does_not_leave_a_dark_halo() {
    // Without premultiplying alpha, every edge pixel averages the transparent *black* around the
    // logo into the white inside it, and the result has a grey fringe. It is subtle enough to pass
    // review and obvious enough that a brand team rejects it.
    let source = encode(&logo_on_transparency(64), ImageFormat::Png);
    // 15, not 16: a scale factor that does not divide the source guarantees output pixels that
    // straddle the square's edge, which is where a halo appears.
    let out = derive::render(&source, &spec(15, 15, OutputFormat::Png)).expect("render");
    let img = decode(&out).to_rgba8();

    let mut edge_pixels = 0;
    for px in img.pixels() {
        // Partially transparent pixels are the interesting ones — they are the edge. A fully
        // transparent pixel's colour is meaningless, and a fully opaque one never mixed.
        if (32..250).contains(&px[3]) {
            edge_pixels += 1;
            assert!(
                px[0] > 200 && px[1] > 200 && px[2] > 200,
                "an edge pixel darkened toward the transparent black around it: {px:?}"
            );
        }
    }
    assert!(
        edge_pixels > 0,
        "no partially-transparent pixels were produced, so this test proves nothing — the \
         geometry must make the square's edge fall between output pixels"
    );
}

#[test]
fn flattening_transparency_to_jpeg_mattes_onto_the_requested_background() {
    // A transparent PNG converted to JPEG without matting is black where it should be white.
    let source = encode(&logo_on_transparency(32), ImageFormat::Png);

    let white = derive::render(&source, &spec(32, 32, OutputFormat::Jpeg)).expect("render");
    let corner = decode(&white).to_rgb8().get_pixel(0, 0).0;
    assert!(
        corner.iter().all(|c| *c > 230),
        "the transparent corner must be matted white, got {corner:?}"
    );

    let black = derive::render(
        &source,
        &Rendition {
            background: [0, 0, 0],
            ..spec(32, 32, OutputFormat::Jpeg)
        },
    )
    .expect("render");
    let corner = decode(&black).to_rgb8().get_pixel(0, 0).0;
    assert!(
        corner.iter().all(|c| *c < 25),
        "and a black background must be honoured, got {corner:?}"
    );
}

#[test]
fn transparency_survives_a_format_that_supports_it() {
    let source = encode(&logo_on_transparency(32), ImageFormat::Png);
    let out = derive::render(&source, &spec(16, 16, OutputFormat::Png)).expect("render");
    let img = decode(&out).to_rgba8();
    assert!(
        img.pixels().any(|px| px[3] < 255),
        "a PNG derivative of a transparent source must keep its alpha"
    );
}

#[test]
fn the_same_source_and_spec_produce_byte_identical_output() {
    // Derivatives are content-addressed too, so a non-deterministic encoder would store a new
    // object for every render of the same rendition and the cache would never hit.
    let source = encode(&photo(120, 90), ImageFormat::Png);
    let spec = spec(60, 60, OutputFormat::Jpeg);
    let first = derive::render(&source, &spec).expect("render");
    let second = derive::render(&source, &spec).expect("render");
    assert_eq!(first, second, "rendering must be deterministic");
}

#[test]
fn the_operation_hash_covers_every_input_that_changes_the_output() {
    // §18.1: op_hash includes the colour profile and rendering intent, so the cache cannot serve a
    // wrongly-converted rendition. Anything that changes the bytes must change the hash, or a
    // stale derivative is served forever.
    let base = spec(100, 100, OutputFormat::Jpeg);
    let reference = derive::op_hash(&base, "srgb", "relative_colorimetric");
    assert_eq!(
        reference,
        derive::op_hash(&base, "srgb", "relative_colorimetric"),
        "the same operation must hash the same"
    );

    let variants: Vec<(&str, String)> = vec![
        (
            "width",
            derive::op_hash(
                &Rendition { width: 101, ..base },
                "srgb",
                "relative_colorimetric",
            ),
        ),
        (
            "height",
            derive::op_hash(
                &Rendition {
                    height: 101,
                    ..base
                },
                "srgb",
                "relative_colorimetric",
            ),
        ),
        (
            "format",
            derive::op_hash(
                &Rendition {
                    format: OutputFormat::WebP,
                    ..base
                },
                "srgb",
                "relative_colorimetric",
            ),
        ),
        (
            "quality",
            derive::op_hash(
                &Rendition {
                    quality: 90,
                    ..base
                },
                "srgb",
                "relative_colorimetric",
            ),
        ),
        (
            "fit",
            derive::op_hash(
                &Rendition {
                    fit: Fit::Cover,
                    ..base
                },
                "srgb",
                "relative_colorimetric",
            ),
        ),
        (
            "background",
            derive::op_hash(
                &Rendition {
                    background: [0, 0, 0],
                    ..base
                },
                "srgb",
                "relative_colorimetric",
            ),
        ),
        (
            "profile",
            derive::op_hash(&base, "display-p3", "relative_colorimetric"),
        ),
        ("intent", derive::op_hash(&base, "srgb", "perceptual")),
    ];

    for (what, hash) in variants {
        assert_ne!(
            hash, reference,
            "changing the {what} must change the operation hash"
        );
    }
}

#[test]
fn a_decompression_bomb_is_refused_before_it_is_decoded() {
    // Rendering must decode, so the budget guard belongs here as well as in the probe.
    let mut bytes = encode(&photo(4, 4), ImageFormat::Png);
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

    let err = derive::render(&bytes, &spec(100, 100, OutputFormat::Jpeg))
        .expect_err("a bomb must be refused");
    assert!(
        format!("{err}").contains("pixel"),
        "the error must name the reason: {err}"
    );
}

#[test]
fn a_zero_sized_request_is_refused() {
    let source = encode(&photo(40, 40), ImageFormat::Png);
    for (w, h) in [(0, 100), (100, 0), (0, 0)] {
        assert!(
            derive::render(&source, &spec(w, h, OutputFormat::Png)).is_err(),
            "{w}x{h} is not a rendition"
        );
    }
}

#[test]
fn corrupt_input_produces_an_error_rather_than_a_panic() {
    for bad in [&[][..], &[0xFF, 0xD8][..], b"not an image at all"] {
        let _ = derive::render(bad, &spec(10, 10, OutputFormat::Png));
    }
}
