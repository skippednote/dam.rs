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

/// PNG bytes, for the entry point that takes them.
fn encode(image: &DynamicImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

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

/// Texture at a spatial frequency low enough to survive a downscale — a stand-in for photographic content.
///
/// Generated from coordinates *normalised to the image size*, so the same call at two sizes is the same
/// picture rather than a differently-wrapped pattern. That distinction cost two debugging rounds: a fixture
/// computed from raw pixel indices is a different image at every size, and blaming the hash for that is the
/// easiest mistake here to make twice.
fn textured(w: u32, h: u32) -> DynamicImage {
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        let value = ((fx * 9.0).sin() * 60.0 + (fy * 7.0).cos() * 50.0 + 128.0).clamp(0.0, 255.0);
        let v = value as u8;
        *px = Rgb([v, v.wrapping_add(20), 200u8.saturating_sub(v / 2)]);
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
        Some(0)
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
    // A textured image, which is what real photographic content looks like to a DCT hash. Not the smooth ramp:
    // that has all its energy in two coefficients, so the median comparison is near-arbitrary and a rescale
    // moves twenty-two bits — a genuine limit of the algorithm, recorded in its own test rather than papered
    // over here. Measured, this fixture rescales to distance 0.
    let source = textured(256, 256);
    let original = similarity::hashes(&source);
    let rescaled =
        similarity::hashes(&source.resize_exact(96, 96, image::imageops::FilterType::Lanczos3));
    let unrelated = similarity::hashes(&checkerboard(256, 256));

    let near = original.distance(rescaled).expect("both have structure");
    let far = original.distance(unrelated).expect("both have structure");
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
    let distance = similarity::hashes(&source)
        .distance(similarity::hashes(&source.resize_exact(
            96,
            96,
            image::imageops::FilterType::Lanczos3,
        )))
        .expect("both have structure");
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
        a.distance(b).expect("both have structure") <= gradient_distance,
        "distance() must take the closer of the two comparable hashes"
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
}

#[test]
fn an_image_with_no_tonal_variation_gets_no_hash_at_all() {
    // The defence that matters, and the second attempt at it. The first was a population-count band on the
    // hash, which cannot work: the DCT hash compares each coefficient against the *median* of the set, so
    // about half its bits are set for a photograph and a blank page alike. Measured, a solid grey square and
    // a solid blue one both hashed to thirty-one bits and came out **11 apart** — inside the twelve-bit review
    // threshold, so two unrelated flat colours would have been queued as duplicates of each other.
    //
    // So the test is on the image, not the hash: below `MIN_LUMA_DEVIATION` there is no hash stored, which
    // excludes the asset from both directions with no column to record it and no filter to remember.
    let flat = encode(&solid(96, 96, [128, 128, 128]));
    let analysed = similarity::analyse(&flat).expect("analyse");
    assert!(
        analysed.hashes.is_none(),
        "a flat wash matches every other flat wash, so it gets no hash"
    );
    // Its colours are still recorded — and that is the point. Colour is exactly what distinguishes the images
    // a perceptual hash cannot.
    assert_eq!(analysed.colours.len(), 1);
    assert_eq!(analysed.colours[0].palette_bucket, "grey");

    let blue = similarity::analyse(&encode(&solid(96, 96, [40, 80, 200]))).expect("analyse");
    assert!(blue.hashes.is_none());
    assert_eq!(blue.colours[0].palette_bucket, "blue");

    // A textured image keeps its hash.
    let busy = similarity::analyse(&encode(&checkerboard(96, 96))).expect("analyse");
    assert!(
        busy.hashes.is_some(),
        "a checkerboard has plenty of variation"
    );

    // And the deviation measure is what decides, on the scale it claims: 0 for a flat field, well above the
    // floor for a checkerboard.
    assert!(similarity::luma_deviation(&solid(96, 96, [128, 128, 128])) < 1.0);
    assert!(similarity::luma_deviation(&checkerboard(96, 96)) > similarity::MIN_LUMA_DEVIATION);
}

#[test]
fn a_smooth_ramp_keeps_its_hash_and_is_honestly_unreliable() {
    // A gradient is the awkward middle case, and this records it rather than pretending otherwise. It has
    // plenty of tonal variation, so it is hashed; its *gradient* hash is all zeroes, because a ramp has no
    // pixel-to-pixel differences at the 8×8 scale; and its DCT hash is unstable across a rescale — measured at
    // 22 bits for a 256→96 downscale, well outside the review threshold.
    //
    // The consequence is that two rescaled copies of the same gradient will not be found as duplicates. That
    // is a known limit of a DCT hash on an image whose energy sits in two coefficients, not something this
    // module can fix by choosing better constants — and a test that asserted otherwise would be a test of a
    // wish.
    let ramp = smooth(256, 256);
    let analysed = similarity::analyse(&encode(&ramp)).expect("analyse");
    let hashes = analysed.hashes.expect("a ramp has tonal variation");
    assert_eq!(
        hashes.dhash.count_ones(),
        0,
        "no local differences to record"
    );
    assert!(
        similarity::discriminative(hashes.phash),
        "but the DCT hash carries the shape"
    );

    // It is not paired with a flat colour, because the flat colour has no hash to pair with.
    assert!(
        similarity::analyse(&encode(&solid(96, 96, [128, 128, 128])))
            .expect("analyse")
            .hashes
            .is_none()
    );
}

#[test]
fn a_collapsed_hash_is_left_out_of_the_comparison() {
    // The bug a real library found: a 932-byte test pattern paired with an MP4 at distance 0. Both had
    // `dhash = 0` — correctly, since neither picture has any pixel-to-pixel variation — and taking the
    // minimum of the two distances blindly let that useless hash decide, while the DCT hashes were saying the
    // pictures were nothing alike.
    let ramp = similarity::hashes(&smooth(128, 128));
    let flat = similarity::hashes(&solid(128, 128, [90, 90, 90]));
    assert_eq!(ramp.dhash.count_ones(), 0);
    assert_eq!(flat.dhash.count_ones(), 0);
    assert!(
        ramp.distance(flat)
            .is_none_or(|d| d > similarity::NEAR_DUPLICATE_DISTANCE),
        "a ramp and a flat wash must not be reported alike through a hash that is all zeroes: {:?}",
        ramp.distance(flat)
    );

    // And a hash that is entirely set is as uninformative as one that is entirely clear.
    assert!(!similarity::discriminative(0));
    assert!(!similarity::discriminative(u64::MAX));
    assert!(similarity::discriminative(1));
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
