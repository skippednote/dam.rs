//! Rendering through libvips (1.7): the primary path, and where D11 lives.
//!
//! §18.1 calls ICC handling "non-negotiable for any brand or print library": masters keep their profile
//! and colour space, delivery converts to sRGB with a stated rendering intent, and `derivatives.op_hash`
//! includes the profile and intent so the cache cannot serve a wrongly-converted rendition. The
//! pure-Rust path cannot do any of that — `image` has no colour management — so this is the half of the
//! renderer that makes D11 real rather than aspirational.
//!
//! The colour tests assert on **pixels, not on metadata**. An embedded profile proves only that a
//! profile is embedded; whether the transform actually ran shows up in the numbers. Checking the profile
//! bytes instead would have compared identical ICC *headers* and passed regardless.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::derive::{Fit, OutputFormat};
use dam_media::vips::{self, Intent, RenderSpec, Toolchain};
use std::path::Path;

fn tools() -> Toolchain {
    Toolchain::discover().unwrap_or_else(|e| {
        panic!(
            "these tests need vips on PATH; run `mise run check` or `mise exec -- cargo test`: {e}"
        )
    })
}

fn spec(width: u32, height: u32) -> RenderSpec {
    RenderSpec {
        width,
        height,
        format: OutputFormat::Png,
        quality: 82,
        fit: Fit::Contain,
        output_profile: None,
        intent: Intent::Relative,
    }
}

/// A saturated red square, tagged with Display P3 — the shape that makes a colour transform visible.
///
/// Black would be useless: no colour transform changes it, so an earlier version of this fixture proved
/// nothing at all.
async fn p3_red(tools: &Toolchain, dir: &Path) -> std::path::PathBuf {
    let flat = dir.join("flat.png");
    let red = dir.join("red.png");
    let tagged = dir.join("red-p3.tif");
    vips::run_raw(
        tools,
        &["black", &flat.to_string_lossy(), "8", "8", "--bands", "3"],
    )
    .await
    .expect("make a canvas");
    vips::run_raw(
        tools,
        &[
            "linear",
            &flat.to_string_lossy(),
            &red.to_string_lossy(),
            "0",
            "255 0 0",
        ],
    )
    .await
    .expect("make it red");
    vips::run_raw(
        tools,
        &[
            "icc_export",
            &red.to_string_lossy(),
            &tagged.to_string_lossy(),
            "--output-profile",
            "p3",
        ],
    )
    .await
    .expect("tag it p3");
    tagged
}

/// The same red, tagged CMYK — a LUT-based profile, which is what makes rendering intents diverge.
async fn cmyk_red(tools: &Toolchain, dir: &Path) -> std::path::PathBuf {
    let flat = dir.join("cmyk-flat.png");
    let red = dir.join("cmyk-red.png");
    let tagged = dir.join("red-cmyk.tif");
    vips::run_raw(
        tools,
        &["black", &flat.to_string_lossy(), "8", "8", "--bands", "3"],
    )
    .await
    .expect("canvas");
    vips::run_raw(
        tools,
        &[
            "linear",
            &flat.to_string_lossy(),
            &red.to_string_lossy(),
            "0",
            "255 0 0",
        ],
    )
    .await
    .expect("red");
    vips::run_raw(
        tools,
        &[
            "icc_export",
            &red.to_string_lossy(),
            &tagged.to_string_lossy(),
            "--output-profile",
            "cmyk",
        ],
    )
    .await
    .expect("tag cmyk");
    tagged
}

async fn pixel(tools: &Toolchain, path: &Path) -> Vec<f64> {
    vips::pixel_at(tools, path, 0, 0)
        .await
        .expect("read a pixel")
}

#[tokio::test]
async fn converting_to_srgb_actually_changes_the_pixels() {
    // D11's delivery half. The source is Display P3 red; converted to sRGB with a relative intent the
    // numbers must move, because the same stimulus needs different RGB in a smaller gamut. If they did
    // not move, the profile would have been swapped without the pixels being transformed — which looks
    // correct in every metadata check and is wrong on screen.
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = p3_red(&tools, dir.path()).await;
    let before = pixel(&tools, &source).await;

    let out = dir.path().join("srgb.png");
    vips::render(
        &tools,
        &source,
        &out,
        &RenderSpec {
            output_profile: Some("srgb".to_owned()),
            ..spec(8, 8)
        },
    )
    .await
    .expect("render");

    let after = pixel(&tools, &out).await;
    assert_ne!(
        before, after,
        "P3 {before:?} and sRGB {after:?} must differ, or no transform happened"
    );
    // Red stays red — this is a gamut mapping, not a corruption.
    assert!(after[0] > after[1] && after[0] > after[2], "got {after:?}");
}

#[tokio::test]
async fn omitting_an_output_profile_preserves_the_source_colour() {
    // D11's other half: a master keeps its profile. Converting at ingest is lossy and irreversible, and
    // the customer's press-ready file would be gone.
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = p3_red(&tools, dir.path()).await;
    let before = pixel(&tools, &source).await;

    let out = dir.path().join("kept.png");
    vips::render(&tools, &source, &out, &spec(8, 8))
        .await
        .expect("render");

    assert_eq!(
        before,
        pixel(&tools, &out).await,
        "with no output profile the pixels must be untouched"
    );
}

#[tokio::test]
async fn the_rendering_intent_changes_the_result_for_a_cmyk_source() {
    // The intent is in `op_hash`, so two intents are two different derivatives. That is only justified if
    // the intent actually changes the pixels.
    //
    // It does — but **only for a LUT-based profile**. A first version of this test used the P3 source
    // above and found all four intents identical, which is correct ICC behaviour rather than a bug:
    // P3 and sRGB are both matrix/TRC profiles sharing a D65 white point, so every intent reduces to the
    // same matrix. Intents diverge for table-based profiles, and CMYK is one — which is precisely the
    // case D11 exists for, since "CMYK is converted at delivery, never at ingest" is about print masters.
    //
    // Measured here: relative 232/31/42, perceptual 232/0/0, absolute 223/28/41.
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = cmyk_red(&tools, dir.path()).await;

    let mut rendered = Vec::new();
    for intent in [Intent::Relative, Intent::Perceptual, Intent::Absolute] {
        let out = dir.path().join(format!("{}.png", intent.as_str()));
        vips::render(
            &tools,
            &source,
            &out,
            &RenderSpec {
                output_profile: Some("srgb".to_owned()),
                intent,
                ..spec(8, 8)
            },
        )
        .await
        .expect("render");
        rendered.push((intent.as_str(), pixel(&tools, &out).await));
    }

    let distinct: std::collections::BTreeSet<String> =
        rendered.iter().map(|(_, px)| format!("{px:?}")).collect();
    assert!(
        distinct.len() > 1,
        "intents must differ on a CMYK source, else the field in op_hash means nothing: {rendered:?}"
    );
}

#[tokio::test]
async fn intent_is_a_no_op_between_matrix_profiles_which_is_why_the_test_above_uses_cmyk() {
    // Recorded as a test rather than a comment, because the next person to look at intent handling will
    // reasonably expect P3 to behave like CMYK and conclude the code is broken when it does not. It also
    // says something real about the cache: for matrix-profile conversions, `op_hash` including the intent
    // stores duplicate derivatives. Wasteful, not wrong — and it is the price of one hash covering the
    // case where the intent does matter.
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = p3_red(&tools, dir.path()).await;

    let mut seen = std::collections::BTreeSet::new();
    for intent in [Intent::Relative, Intent::Perceptual, Intent::Saturation] {
        let out = dir.path().join(format!("p3-{}.png", intent.as_str()));
        vips::render(
            &tools,
            &source,
            &out,
            &RenderSpec {
                output_profile: Some("srgb".to_owned()),
                intent,
                ..spec(8, 8)
            },
        )
        .await
        .expect("render");
        seen.insert(format!("{:?}", pixel(&tools, &out).await));
    }
    assert_eq!(
        seen.len(),
        1,
        "P3 and sRGB are both matrix profiles with a D65 white point, so every intent is the same \
         transform: {seen:?}"
    );
}

#[tokio::test]
async fn a_source_smaller_than_the_request_is_never_upscaled() {
    // The divergence a differential test exists to catch. `vipsthumbnail --size 2048x2048` upscales by
    // default — measured: a 64x48 source came back 2048x1536 — while the pure-Rust path caps at the
    // source. Two renderers disagreeing on every small asset is the kind of bug nobody notices until
    // they compare two derivatives of the same file.
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("small.png");
    vips::run_raw(
        &tools,
        &[
            "black",
            &source.to_string_lossy(),
            "64",
            "48",
            "--bands",
            "3",
        ],
    )
    .await
    .expect("make a small source");

    let out = dir.path().join("out.png");
    vips::render(&tools, &source, &out, &spec(2048, 2048))
        .await
        .expect("render");

    let probed = vips::probe(&tools, &out).await.expect("probe");
    assert_eq!(
        (probed.width, probed.height),
        (64, 48),
        "the source dimensions are the ceiling, exactly as in the pure-Rust path"
    );
}

#[tokio::test]
async fn contain_preserves_the_aspect_ratio_and_cover_fills_the_box() {
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("wide.png");
    vips::run_raw(
        &tools,
        &[
            "black",
            &source.to_string_lossy(),
            "200",
            "100",
            "--bands",
            "3",
        ],
    )
    .await
    .expect("make a wide source");

    let contained = dir.path().join("contain.png");
    vips::render(&tools, &source, &contained, &spec(64, 64))
        .await
        .expect("contain");
    let probed = vips::probe(&tools, &contained).await.expect("probe");
    assert_eq!((probed.width, probed.height), (64, 32));

    let covered = dir.path().join("cover.png");
    vips::render(
        &tools,
        &source,
        &covered,
        &RenderSpec {
            fit: Fit::Cover,
            ..spec(64, 64)
        },
    )
    .await
    .expect("cover");
    let probed = vips::probe(&tools, &covered).await.expect("probe");
    assert_eq!((probed.width, probed.height), (64, 64));
}

#[tokio::test]
async fn quality_changes_the_encoded_size() {
    // Otherwise the quality field is decorative, and `op_hash` is again carrying something that means
    // nothing.
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("noise.png");
    // Noise, not flat colour: a flat image compresses to almost nothing at any quality, so the
    // comparison would be between two numbers that are both essentially the header size.
    vips::run_raw(
        &tools,
        &["gaussnoise", &source.to_string_lossy(), "200", "200"],
    )
    .await
    .expect("make noise");

    let mut sizes = Vec::new();
    for quality in [40u8, 95] {
        let out = dir.path().join(format!("q{quality}.jpg"));
        vips::render(
            &tools,
            &source,
            &out,
            &RenderSpec {
                format: OutputFormat::Jpeg,
                quality,
                ..spec(200, 200)
            },
        )
        .await
        .expect("render");
        sizes.push(std::fs::metadata(&out).expect("stat").len());
    }
    assert!(
        sizes[0] < sizes[1],
        "q40 must be smaller than q95, got {sizes:?}"
    );
}

#[tokio::test]
async fn an_unreadable_source_carries_vipss_own_diagnosis() {
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("nonsense.bin");
    std::fs::write(&source, b"not an image").expect("write");

    let err = vips::render(&tools, &source, &dir.path().join("out.png"), &spec(64, 64))
        .await
        .expect_err("must fail");
    assert!(
        format!("{err}").len() > 20,
        "the error must carry vips's stderr rather than a bare code: {err}"
    );
}

#[tokio::test]
async fn rendering_is_bounded_by_the_sandbox() {
    // Same property as the probe: there must be no path to a decoder that skips the limits.
    let tools = tools();
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("small.png");
    vips::run_raw(
        &tools,
        &[
            "black",
            &source.to_string_lossy(),
            "16",
            "16",
            "--bands",
            "3",
        ],
    )
    .await
    .expect("source");

    let outcome = vips::render_with_limits(
        &tools,
        &source,
        &dir.path().join("out.png"),
        &spec(16, 16),
        dam_media::sandbox::Limits {
            wall_clock: std::time::Duration::from_millis(1),
            ..Default::default()
        },
    )
    .await;
    assert!(outcome.is_err(), "a 1ms wall clock must stop the render");
}
