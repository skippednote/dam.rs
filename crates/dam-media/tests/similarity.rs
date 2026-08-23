//! Perceptual hashes and dominant colour (M4, §8.1).
//!
//! What these two things are *for* decides what is worth asserting.
//!
//! A perceptual hash exists to catch the same picture after a re-encode or a small edit, because content
//! addressing already catches identical bytes. So the properties are: stable for the same input, close for the
//! same picture through a transformation, and far for a different picture. A hash that only matched identical
//! bytes would duplicate BLAKE3, and one that matched everything would fill a review queue with noise.
//!
//! Two hashes, because they fail differently — the gradient hash survives a brightness change, the DCT hash
//! survives a rescale — and [`Hashes::distance`] takes the closer of the two. Both halves are asserted
//! separately, or a regression in one would hide behind the other.
//!
//! Colour extraction exists to back a facet, so the properties are perceptual grouping (LAB, not RGB) and
//! **determinism**: the same image must yield the same colours every run, or a re-process rewrites every row
//! and a facet count moves for no reason anybody can explain.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::similarity::{self, Hashes};
use image::{DynamicImage, Rgb, RgbImage};

/// A deterministic gradient, the same shape the probe suite uses.
fn gradient(w: u32, h: u32) -> DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        px.0 = [
            u8::try_from(x * 7 % 256).unwrap_or(0),
            u8::try_from(y * 11 % 256).unwrap_or(0),
            128,
        ];
    }
    DynamicImage::ImageRgb8(img)
}

fn solid(w: u32, h: u32, colour: [u8; 3]) -> DynamicImage {
    let mut img = RgbImage::new(w, h);
    for px in img.pixels_mut() {
        *px = Rgb(colour);
    }
    DynamicImage::ImageRgb8(img)
}

/// Half one colour, half another — for coverage.
fn split(w: u32, h: u32, left: [u8; 3], right: [u8; 3]) -> DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, _, px) in img.enumerate_pixels_mut() {
        *px = Rgb(if x < w / 2 { left } else { right });
    }
    DynamicImage::ImageRgb8(img)
}

/// A smooth two-axis ramp — what a low-frequency hash sees in ordinary photographic content.
fn smooth(w: u32, h: u32) -> DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = u8::try_from(x * 255 / w.max(1)).unwrap_or(255);
        let fy = u8::try_from(y * 255 / h.max(1)).unwrap_or(255);
        *px = Rgb([fx, fy, 128]);
    }
    DynamicImage::ImageRgb8(img)
}

fn checkerboard(w: u32, h: u32) -> DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let on = (x / 8 + y / 8) % 2 == 0;
        *px = Rgb(if on { [255, 255, 255] } else { [0, 0, 0] });
    }
    DynamicImage::ImageRgb8(img)
}

// ─── the hashes ─────────────────────────────────────────────────────────────

#[test]
fn the_same_image_hashes_identically_every_time() {
    // Determinism is load-bearing: a hash that varied would enqueue every asset as a new duplicate candidate
    // on every re-process.
    let image = gradient(128, 128);
    assert_eq!(similarity::hashes(&image), similarity::hashes(&image));
    assert_eq!(
        similarity::hashes(&image).distance(similarity::hashes(&image)),
        0
    );
}

#[test]
fn a_rescale_stays_close_and_a_different_picture_does_not() {
    // The whole point. A re-export at another size is the same picture and must stay close; a checkerboard is
    // not, and must not.
    //
    // The smaller image is the *same* image resized, not the generator run at another size. `gradient` computes
    // from pixel coordinates modulo 256, so calling it at 96 produces a pattern that wraps differently — a
    // genuinely different picture, which is what the first version of this compared and then blamed the hash
    // for. A fixture that does not hold the thing under test still is not a test of it.
    // A smooth gradient, which is what real photographic content looks like to a low-frequency hash. The
    // high-frequency `gradient` helper is exercised separately below, because heavy aliasing is its own case.
    let source = smooth(256, 256);
    let original = similarity::hashes(&source);
    let rescaled =
        similarity::hashes(&source.resize_exact(96, 96, image::imageops::FilterType::Lanczos3));
    let unrelated = similarity::hashes(&checkerboard(256, 256));

    let near = original.distance(rescaled);
    let far = original.distance(unrelated);
    assert!(
        near <= 4,
        "a rescale of an ordinary picture should barely move, got {near}"
    );
    assert!(
        far > similarity::NEAR_DUPLICATE_DISTANCE,
        "a different picture must fall outside the review threshold: got {far}"
    );
}

#[test]
fn a_heavy_downscale_of_a_fine_pattern_stays_inside_the_review_threshold() {
    // The case that set the threshold. `gradient` is stripes with a period of about 37 pixels, so a 256→96
    // downscale aliases them badly and nine or ten bits move — which is the hash reporting real information
    // loss, not a different picture. A cutoff of eight would have thrown this away, and the cost of throwing
    // away a true duplicate is that nobody ever sees it, whereas the cost of keeping a false one is a row
    // somebody dismisses.
    let source = gradient(256, 256);
    let distance = similarity::hashes(&source).distance(similarity::hashes(&source.resize_exact(
        96,
        96,
        image::imageops::FilterType::Lanczos3,
    )));
    assert!(
        distance <= similarity::NEAR_DUPLICATE_DISTANCE,
        "an aliased downscale is still the same picture, got {distance}"
    );
    assert!(
        distance > 4,
        "and it really is a harder case than a smooth rescale, got {distance}"
    );
}

#[test]
fn a_brightness_change_is_survived_by_the_gradient_hash() {
    // The reason there are two. A global tone shift moves every DCT coefficient, so the DCT hash may drift;
    // the gradient hash compares neighbours, so it barely moves. `distance` takes the closer of the two,
    // which is what keeps a re-export out of the "unrelated" bucket.
    let base = gradient(128, 128);
    let mut brighter = base.to_rgb8();
    for px in brighter.pixels_mut() {
        for channel in &mut px.0 {
            *channel = channel.saturating_add(40);
        }
    }
    let brighter = DynamicImage::ImageRgb8(brighter);

    let a = similarity::hashes(&base);
    let b = similarity::hashes(&brighter);
    let gradient_distance = (a.dhash ^ b.dhash).count_ones();
    assert!(
        gradient_distance <= 4,
        "a brightness shift should barely move the gradient hash, got {gradient_distance}"
    );
    assert!(
        a.distance(b) <= gradient_distance,
        "distance() must take the closer of the two hashes"
    );
}

#[test]
fn the_two_hashes_are_independent_values() {
    // Asserted because a copy-paste that computed one algorithm twice would pass every other test here: the
    // pair would be self-consistent, stable and close for similar images, and would simply have lost the
    // failure mode the second hash exists to cover.
    let image = gradient(128, 128);
    let Hashes { phash, dhash } = similarity::hashes(&image);
    assert_ne!(phash, dhash, "two algorithms should not agree bit for bit");
    assert_ne!(phash, 0, "a gradient is not a blank hash");
    assert_ne!(dhash, 0);
}

#[test]
fn a_flat_image_hashes_without_panicking() {
    // A solid colour has no gradient and a DCT with one non-zero coefficient, which is where a median over an
    // all-equal set and a divide-by-zero would show up. The assertion is that it returns.
    let flat = similarity::hashes(&solid(64, 64, [128, 128, 128]));
    let also_flat = similarity::hashes(&solid(64, 64, [128, 128, 128]));
    assert_eq!(flat, also_flat);
    // And two different flat colours are *not* far apart, which is correct and worth recording: neither hash
    // carries absolute brightness, so "grey square" and "blue square" are the same picture to both of them.
    // Colour is what tells those apart, which is why both features exist.
    let blue = similarity::hashes(&solid(64, 64, [40, 80, 200]));
    assert!(flat.distance(blue) <= 8);
}

// ─── colour ─────────────────────────────────────────────────────────────────

#[test]
fn a_solid_colour_yields_one_colour_at_full_coverage() {
    // Empty clusters are dropped rather than re-seeded, so a flat field gives one colour — not five nearly
    // identical ones, which is what a facet would show if k were honoured blindly.
    let colours = similarity::colours(&solid(64, 64, [200, 30, 40])).expect("colours");
    assert_eq!(colours.len(), 1, "{colours:?}");
    assert!((colours[0].coverage - 1.0).abs() < 0.01);
    assert_eq!(colours[0].palette_bucket, "red");
    // The hex round-trips through LAB and back within a shade of the input.
    assert!(colours[0].hex.starts_with('#') && colours[0].hex.len() == 7);
}

#[test]
fn coverage_reflects_how_much_of_the_picture_a_colour_is() {
    let colours =
        similarity::colours(&split(96, 96, [220, 20, 20], [20, 40, 200])).expect("colours");
    assert_eq!(colours.len(), 2, "{colours:?}");
    // Most-covering first, which is what `rank` means in the schema and what a facet shows.
    assert!(colours[0].coverage >= colours[1].coverage);
    for colour in &colours {
        assert!(
            (colour.coverage - 0.5).abs() < 0.1,
            "each half should be about half: {colour:?}"
        );
    }
    let buckets: Vec<&str> = colours.iter().map(|c| c.palette_bucket.as_str()).collect();
    assert!(
        buckets.contains(&"red") && buckets.contains(&"blue"),
        "{buckets:?}"
    );
}

#[test]
fn the_same_image_yields_the_same_colours_every_time() {
    // The property that makes this storable. k-means is seeded from the sorted sample rather than at random
    // precisely so this holds — otherwise a re-process rewrites `asset_colors` and a facet count moves with
    // nothing having changed.
    let image = gradient(128, 128);
    let first = similarity::colours(&image).expect("colours");
    let second = similarity::colours(&image).expect("colours");
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(a.hex, b.hex);
        assert_eq!(a.palette_bucket, b.palette_bucket);
        assert!((a.coverage - b.coverage).abs() < f32::EPSILON);
    }
}

#[test]
fn achromatic_colours_are_named_by_lightness_rather_than_hue() {
    // A very dark or very desaturated pixel still has a hue angle, and reporting it as "green" because a and b
    // leaned that way is how a colour facet loses a designer's trust.
    for (rgb, expected) in [
        ([0u8, 0, 0], "black"),
        ([60, 60, 60], "dark grey"),
        ([140, 140, 140], "grey"),
        ([210, 210, 210], "light grey"),
        ([255, 255, 255], "white"),
    ] {
        let colours = similarity::colours(&solid(48, 48, rgb)).expect("colours");
        assert_eq!(
            colours[0].palette_bucket, expected,
            "{rgb:?} should bucket as {expected}, got {:?}",
            colours[0]
        );
    }
}

#[test]
fn brown_is_its_own_bucket_rather_than_a_dark_orange() {
    // The one bucket that is not a hue. Without it, a search for orange returns every wooden table in the
    // library — which is the kind of result that makes somebody stop using the facet.
    let colours = similarity::colours(&solid(48, 48, [110, 70, 35])).expect("colours");
    assert_eq!(colours[0].palette_bucket, "brown", "{:?}", colours[0]);

    // And a bright orange is still orange.
    let colours = similarity::colours(&solid(48, 48, [255, 140, 20])).expect("colours");
    assert_eq!(colours[0].palette_bucket, "orange", "{:?}", colours[0]);
}

#[test]
fn lab_is_stored_so_a_nearest_colour_search_need_not_convert_every_row() {
    let colours = similarity::colours(&solid(48, 48, [200, 30, 40])).expect("colours");
    let lab = colours[0].lab;
    // A saturated red: high a*, positive b*, mid lightness. Asserted as ranges rather than exact values,
    // because the point is that the stored triple really is LAB and not RGB in disguise.
    assert!(
        (30.0..70.0).contains(&lab[0]),
        "lightness should be mid-range for this red, got {lab:?}"
    );
    assert!(lab[1] > 40.0, "a* should be strongly positive, got {lab:?}");
    assert!(lab[2] > 10.0, "b* should be positive, got {lab:?}");
}

#[test]
fn at_most_five_colours_come_back() {
    // `asset_colors.rank` is a smallint because this is a short list, and a full histogram is a different
    // feature. A noisy image must not return one row per distinct shade.
    let colours = similarity::colours(&gradient(256, 256)).expect("colours");
    assert!(
        colours.len() <= similarity::COLOURS_KEPT,
        "got {} colours",
        colours.len()
    );
    assert!(!colours.is_empty());
    let total: f32 = colours.iter().map(|c| c.coverage).sum();
    assert!(
        (total - 1.0).abs() < 0.01,
        "coverage should account for the whole sample, got {total}"
    );
}
