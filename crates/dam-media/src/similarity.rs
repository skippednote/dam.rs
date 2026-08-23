//! Perceptual hashes and dominant colour: the model-free half of M4 (§8.1).
//!
//! Both of these are pure computation over the master proxy — no ONNX runtime, no model files, no GPU. That is
//! why they are the first part of M4 to exist: everything else in §8.1 needs a model download measured in
//! hundreds of megabytes, and these two already back user-visible features. `asset_phashes`, `asset_colors`
//! and `duplicate_candidates` have been in migration 0003 since the start with nothing writing to them.
//!
//! ## Two hashes, because they fail differently
//!
//! The schema asks for both, and the reason is worth stating. **dHash** (gradient) compares each pixel with
//! its neighbour, so it survives brightness and contrast changes — a re-export from a design tool routinely
//! applies those. **pHash** (DCT) keeps the low-frequency structure, so it survives a rescale and a re-encode
//! but is more easily fooled by a global tone shift. A crop that defeats one often does not defeat the other,
//! and a candidate pair is worth surfacing when *either* is close.
//!
//! ## Near-duplicates are a review queue, never an automatic merge
//!
//! 0003 says why: "auto-merging a crop that is actually a different licensed deliverable is a rights problem,
//! so a human decides". This module produces distances and a guess at the relation; it decides nothing.
//!
//! ## Colour is clustered in LAB, not RGB
//!
//! Also 0003's reasoning: Euclidean distance in RGB does not match perceived similarity, so RGB clustering
//! produces facets that look wrong to a designer. The conversion is done here rather than pulled in as a
//! dependency because it is thirty lines of well-specified arithmetic and the alternative was a crate for it.

use crate::probe::Error;
use image::DynamicImage;

type Result<T> = std::result::Result<T, Error>;

/// The side length the hashes are computed at.
///
/// Eight, giving 64 bits — which is what `asset_phashes.phash bigint` holds. Larger hashes discriminate better
/// and stop fitting in a column the schema already chose, so the size is fixed here rather than configurable.
const HASH_SIDE: u32 = 8;

/// How much tonal variation an image needs before its hash means anything.
///
/// Standard deviation of luma, on 0..=255. Six is a little over two percent of the range — below that an image
/// is a flat wash, and *every* flat wash is a near-duplicate of every other by any perceptual hash.
///
/// Measured from the image rather than inferred from the hash, and that is the point. The obvious test —
/// population count of the hash — cannot work for the DCT hash: it compares each coefficient against the
/// *median* of the set, so about half the bits are set by construction, for a photograph and a blank page
/// alike. Running the numbers is what showed it: a solid grey square and a solid blue one both hash to
/// thirty-one bits, and their distance came out at 11 — inside the twelve-bit review threshold, so two
/// unrelated flat colours would have been queued as duplicates.
///
/// The gradient hash *does* degenerate visibly, to all zeroes, which is why [`discriminative`] still earns its
/// keep for that one.
pub const MIN_LUMA_DEVIATION: f32 = 6.0;

/// Whether one hash carries enough structure to compare against another.
///
/// True unless the hash is all zeroes or all ones. Useful for the **gradient** hash, which genuinely collapses
/// to zero on any image with no pixel-to-pixel variation — a flat colour, a smooth ramp, the black poster frame
/// of a video. Near-useless for the DCT hash, which is a median comparison and therefore always sits near
/// thirty-two bits; [`MIN_LUMA_DEVIATION`] is what catches that case.
#[must_use]
pub const fn discriminative(hash: u64) -> bool {
    let bits = hash.count_ones();
    bits > 0 && bits < 64
}

/// The Hamming distance at or below which two images are worth a human's attention.
///
/// Twelve of sixty-four bits. Chosen from what the hashes actually do rather than from a round number: a
/// re-encode moves one or two bits, a rescale of a photograph two to four, and a heavy downscale of a
/// high-frequency pattern nine or ten — that last case is aliasing, not a different picture, and a threshold
/// of eight would have discarded it. Above about sixteen, unrelated images start appearing.
///
/// It is a *queue* threshold, not a verdict. 0003 is explicit that near-duplicates are reviewed rather than
/// merged, because "auto-merging a crop that is actually a different licensed deliverable is a rights
/// problem" — so the cost of being generous here is a row a person dismisses, and the cost of being strict is
/// a duplicate nobody ever sees.
pub const NEAR_DUPLICATE_DISTANCE: u32 = 12;

/// How many dominant colours to keep per asset.
///
/// Five. A designer looking for "the orange one" is served by the top few; a full histogram is a different
/// feature and a much larger table. `asset_colors.rank` is a smallint precisely because this is a short list.
pub const COLOURS_KEPT: usize = 5;

/// Everything one decode of an image yields.
///
/// One entry point taking bytes, because decoding is the expensive part and the caller wants both halves —
/// asking for hashes and colours separately would decode twice for every asset in the library. It also keeps
/// image decoding inside this crate: the pipeline orchestrates and does not need an `image` dependency of its
/// own to do it.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    /// `None` when the image has too little tonal variation for a hash to mean anything.
    ///
    /// Not stored at all in that case, which is the whole mechanism: an image with no hash cannot be found by
    /// a search and cannot find anything, so it is excluded from both directions without a column to record it
    /// or a filter to remember. See [`MIN_LUMA_DEVIATION`].
    pub hashes: Option<Hashes>,
    /// Always present. Colour is precisely what distinguishes the images a hash cannot: a grey square and a
    /// blue square are the same picture to any perceptual hash, and obviously different to a person.
    pub colours: Vec<Colour>,
}

/// Hashes and colours one encoded image.
///
/// The pixel budget is enforced by the decode itself here rather than probed first: this runs on a *proxy*,
/// which the derive pass has already bounded, so the guard `probe::perceptual_hash` needs for arbitrary
/// uploaded bytes is already upstream.
pub fn analyse(bytes: &[u8]) -> Result<Analysis> {
    let image = image::load_from_memory(bytes)
        .map_err(|error| Error::Decode(format!("decoding for similarity: {error}")))?;
    Ok(Analysis {
        hashes: (luma_deviation(&image) >= MIN_LUMA_DEVIATION).then(|| hashes(&image)),
        colours: colours(&image)?,
    })
}

/// Standard deviation of luma over a small sample, on 0..=255.
///
/// Sampled at the same size the colour pass uses, so a flat wash is cheap to spot and a photograph is measured
/// on enough pixels to be representative. A single pass, using the sum-of-squares form: two accumulators
/// rather than two traversals.
#[must_use]
pub fn luma_deviation(image: &DynamicImage) -> f32 {
    const SAMPLE_SIDE: u32 = 96;
    let small = image
        .resize_exact(
            SAMPLE_SIDE,
            SAMPLE_SIDE,
            image::imageops::FilterType::Triangle,
        )
        .to_luma8();

    let mut sum = 0f64;
    let mut squares = 0f64;
    let mut count = 0u32;
    for pixel in small.pixels() {
        let value = f64::from(pixel.0[0]);
        sum += value;
        squares += value * value;
        count += 1;
    }
    if count == 0 {
        return 0.0;
    }
    let n = f64::from(count);
    let mean = sum / n;
    // Clamped at zero: floating-point cancellation can make this a hair negative for a genuinely flat image,
    // and a NaN from `sqrt` would propagate into a comparison that then answers neither true nor false.
    let variance = (squares / n - mean * mean).max(0.0);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a deviation on 0..=255, well inside f32"
    )]
    {
        variance.sqrt() as f32
    }
}

/// Both perceptual hashes of one image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hashes {
    /// DCT-based. Survives rescaling and re-encoding.
    pub phash: u64,
    /// Gradient-based. Survives brightness and contrast changes.
    pub dhash: u64,
}

impl Hashes {
    /// Whether either hash carries enough structure to be worth comparing.
    ///
    /// An image failing this — a flat colour, a smooth gradient, a black video frame — is at distance zero from
    /// every other such image, so hashing it is fine and *searching* with it is not.
    #[must_use]
    pub const fn is_comparable(self) -> bool {
        discriminative(self.phash) || discriminative(self.dhash)
    }

    /// Hamming distance to another pair, taking the *closer* of the two comparable hashes.
    ///
    /// The minimum rather than the mean, because the two algorithms fail on different transformations: a crop
    /// that defeats the DCT hash often leaves the gradient hash intact, and averaging would bury that.
    ///
    /// But a structureless hash is excluded from that minimum, and the correction came from real data: a smooth
    /// gradient and the black poster frame of a video both hash to `dhash = 0`, because neither has any
    /// pixel-to-pixel variation for the gradient hash to record. Taking the minimum blindly then reported them
    /// as the same picture while the DCT hashes were correctly saying otherwise.
    ///
    /// `None` when no pair of hashes is comparable — no answer, rather than an answer of zero.
    #[must_use]
    pub fn distance(self, other: Self) -> Option<u32> {
        let mut best: Option<u32> = None;
        for (mine, theirs) in [(self.phash, other.phash), (self.dhash, other.dhash)] {
            if !discriminative(mine) || !discriminative(theirs) {
                continue;
            }
            let distance = (mine ^ theirs).count_ones();
            best = Some(best.map_or(distance, |current| current.min(distance)));
        }
        best
    }
}

/// Computes both hashes.
///
/// The caller passes a decoded image rather than bytes, because the pipeline has already decoded the proxy for
/// the colour pass and decoding twice is the expensive part.
#[must_use]
pub fn hashes(image: &DynamicImage) -> Hashes {
    Hashes {
        phash: dct_hash(image),
        dhash: gradient_hash(image),
    }
}

/// The gradient (difference) hash: is each pixel brighter than the one to its right?
///
/// Computed on a `(side+1) x side` greyscale thumbnail, so each row yields exactly `side` comparisons and the
/// result is `side * side` bits with no wasted work.
fn gradient_hash(image: &DynamicImage) -> u64 {
    let small = image
        .resize_exact(
            HASH_SIDE + 1,
            HASH_SIDE,
            image::imageops::FilterType::Triangle,
        )
        .to_luma8();

    let mut bits = 0u64;
    for y in 0..HASH_SIDE {
        for x in 0..HASH_SIDE {
            let left = small.get_pixel(x, y).0[0];
            let right = small.get_pixel(x + 1, y).0[0];
            bits <<= 1;
            if left > right {
                bits |= 1;
            }
        }
    }
    bits
}

/// The DCT hash: which of the low-frequency coefficients are above their median?
///
/// The standard construction — a 32×32 greyscale, a 2-D DCT-II, the top-left 8×8 block excluding the DC term,
/// compared against the median. The DC term is dropped because it is overall brightness: keeping it would make
/// every hash sensitive to exposure, which is what the gradient hash is *for*.
fn dct_hash(image: &DynamicImage) -> u64 {
    const SIDE: usize = 32;
    let small = image
        .resize_exact(
            u32::try_from(SIDE).unwrap_or(32),
            u32::try_from(SIDE).unwrap_or(32),
            image::imageops::FilterType::Triangle,
        )
        .to_luma8();

    let mut rows = [[0f32; SIDE]; SIDE];
    for (y, row) in rows.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            let pixel =
                small.get_pixel(u32::try_from(x).unwrap_or(0), u32::try_from(y).unwrap_or(0));
            *cell = f32::from(pixel.0[0]);
        }
    }

    // Separable: rows then columns, which is O(2 n³) rather than O(n⁴) for the naive 2-D form. At n = 32 both
    // are fast, but the separable version is also the one whose correctness is easy to see.
    let rows = dct_rows(&rows);
    let transposed = transpose(&rows);
    let columns = dct_rows(&transposed);
    let coefficients = transpose(&columns);

    // The low-frequency block, skipping [0][0] — the DC term is overall brightness, and keeping it would make
    // every hash sensitive to exposure, which is what the gradient hash is for.
    let side = HASH_SIDE as usize;
    let low: Vec<f32> = coefficients
        .iter()
        .take(side)
        .enumerate()
        .flat_map(|(y, row)| {
            row.iter()
                .take(side)
                .enumerate()
                .filter(move |(x, _)| !(*x == 0 && y == 0))
                .map(|(_, value)| *value)
        })
        .collect();

    let median = median_of(&low);
    let mut bits = 0u64;
    for value in &low {
        bits <<= 1;
        if *value > median {
            bits |= 1;
        }
    }
    // 63 comparisons, so the top bit is always clear. Shifted once more to make that explicit rather than
    // leaving a hash whose range is half of u64 for a reason nobody can see from the value.
    bits << 1
}

/// A DCT-II across each row.
fn dct_rows<const N: usize>(input: &[[f32; N]; N]) -> [[f32; N]; N] {
    let mut out = [[0f32; N]; N];
    #[expect(
        clippy::cast_precision_loss,
        reason = "N is 32; every index is exact in f32"
    )]
    let n = N as f32;
    for (y, row) in input.iter().enumerate() {
        for (k, cell) in out[y].iter_mut().enumerate() {
            let mut sum = 0f32;
            for (x, value) in row.iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "indices below 32, exact in f32")]
                let angle = (std::f32::consts::PI / n) * (x as f32 + 0.5) * (k as f32);
                sum += value * angle.cos();
            }
            *cell = sum;
        }
    }
    out
}

fn transpose<const N: usize>(input: &[[f32; N]; N]) -> [[f32; N]; N] {
    let mut out = [[0f32; N]; N];
    for (y, row) in input.iter().enumerate() {
        for (x, value) in row.iter().enumerate() {
            out[x][y] = *value;
        }
    }
    out
}

/// The median, by sorting a copy.
///
/// `sort_unstable_by` with a total order rather than `partial_cmp().unwrap()`: a NaN from a degenerate image
/// would panic inside a worker, and `total_cmp` orders NaN rather than refusing to.
fn median_of(values: &[f32]) -> f32 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

/// One dominant colour.
#[derive(Debug, Clone, PartialEq)]
pub struct Colour {
    /// Lowercase `#rrggbb`, the cluster centre converted back to sRGB.
    pub hex: String,
    /// The centre in CIELAB, as stored — so a nearest-colour search can compare perceptually without
    /// converting every row on every query.
    pub lab: [f32; 3],
    /// Fraction of sampled pixels in this cluster, 0..=1.
    pub coverage: f32,
    /// A coarse name, so facet counts group into something a person would click.
    pub palette_bucket: String,
}

/// Extracts up to [`COLOURS_KEPT`] dominant colours, most-covering first.
///
/// Clustered in LAB. `k` starts at [`COLOURS_KEPT`] and empty clusters are dropped rather than re-seeded, so a
/// flat colour field yields one colour at full coverage instead of five nearly identical ones.
pub fn colours(image: &DynamicImage) -> Result<Vec<Colour>> {
    // A fixed small sample rather than every pixel. k-means over eight megapixels is seconds of CPU for a
    // result that does not visibly differ from one over a hundred-and-something-thousand — and this runs on
    // every asset in a library.
    const SAMPLE_SIDE: u32 = 96;
    let small = image
        .resize_exact(
            SAMPLE_SIDE,
            SAMPLE_SIDE,
            image::imageops::FilterType::Triangle,
        )
        .to_rgb8();

    let points: Vec<[f32; 3]> = small
        .pixels()
        .map(|pixel| rgb_to_lab(pixel.0[0], pixel.0[1], pixel.0[2]))
        .collect();
    if points.is_empty() {
        return Err(Error::Decode("an image with no pixels".to_owned()));
    }

    let clusters = kmeans(&points, COLOURS_KEPT);
    #[expect(
        clippy::cast_precision_loss,
        reason = "a sample of at most 96*96 points"
    )]
    let total = points.len() as f32;

    let mut out: Vec<Colour> = clusters
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(centre, count)| {
            #[expect(clippy::cast_precision_loss, reason = "counts below the sample size")]
            let coverage = count as f32 / total;
            let (r, g, b) = lab_to_rgb(centre);
            Colour {
                hex: format!("#{r:02x}{g:02x}{b:02x}"),
                lab: centre,
                coverage,
                palette_bucket: bucket_of(centre, r, g, b),
            }
        })
        .collect();

    // Most-covering first, which is what `rank` means and what a facet shows.
    out.sort_by(|a, b| b.coverage.total_cmp(&a.coverage));
    out.truncate(COLOURS_KEPT);
    Ok(out)
}

/// Lloyd's algorithm, seeded deterministically.
///
/// Deterministic seeding matters more than cluster quality here: the same image must produce the same colours
/// on every run, or a re-process rewrites every row and a facet count moves for no reason. So the seeds are
/// evenly spaced through the sorted sample rather than random — k-means++ would be better clustering and would
/// need a random source this must not have.
fn kmeans(points: &[[f32; 3]], k: usize) -> Vec<([f32; 3], usize)> {
    // Bounded: ten passes is well past where the centres stop moving visibly on a 96×96 sample, and it keeps
    // this a fixed cost per asset rather than a loop that runs until it feels like stopping.
    const PASSES: usize = 10;

    let mut sorted: Vec<[f32; 3]> = points.to_vec();
    sorted.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    let k = k.min(sorted.len()).max(1);
    let mut centres: Vec<[f32; 3]> = (0..k)
        .map(|index| sorted[index * sorted.len() / k])
        .collect();

    let mut counts = vec![0usize; k];
    for _ in 0..PASSES {
        let mut sums = vec![[0f32; 3]; k];
        counts = vec![0usize; k];
        for point in points {
            let nearest = centres
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| squared(point, a).total_cmp(&squared(point, b)))
                .map_or(0, |(index, _)| index);
            for axis in 0..3 {
                sums[nearest][axis] += point[axis];
            }
            counts[nearest] += 1;
        }
        for (index, centre) in centres.iter_mut().enumerate() {
            if counts[index] == 0 {
                // Left where it is rather than re-seeded. A re-seed would make the result depend on iteration
                // order, and an empty cluster is dropped by the caller anyway.
                continue;
            }
            #[expect(clippy::cast_precision_loss, reason = "counts below the sample size")]
            let n = counts[index] as f32;
            for axis in 0..3 {
                centre[axis] = sums[index][axis] / n;
            }
        }
    }

    centres.into_iter().zip(counts).collect()
}

fn squared(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    (0..3).map(|i| (a[i] - b[i]).powi(2)).sum()
}

/// sRGB to CIELAB, via linear RGB and XYZ under D65.
///
/// Written out rather than taken as a dependency: it is well-specified arithmetic, and the alternative was a
/// crate whose whole surface is this function.
fn rgb_to_lab(r: u8, g: u8, b: u8) -> [f32; 3] {
    let linear = |channel: u8| {
        let c = f32::from(channel) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(r), linear(g), linear(b));

    // sRGB primaries under D65, then normalised by the white point.
    let x = (0.412_456 * r + 0.357_576 * g + 0.180_437 * b) / 0.950_47;
    let y = 0.212_673 * r + 0.715_152 * g + 0.072_175 * b;
    let z = (0.019_334 * r + 0.119_192 * g + 0.950_304 * b) / 1.088_83;

    let f = |t: f32| {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIELAB back to sRGB, for the hex a facet shows.
fn lab_to_rgb(lab: [f32; 3]) -> (u8, u8, u8) {
    let fy = (lab[0] + 16.0) / 116.0;
    let fx = fy + lab[1] / 500.0;
    let fz = fy - lab[2] / 200.0;

    let inverse = |t: f32| {
        if t.powi(3) > 0.008_856 {
            t.powi(3)
        } else {
            (t - 16.0 / 116.0) / 7.787
        }
    };
    let x = inverse(fx) * 0.950_47;
    let y = inverse(fy);
    let z = inverse(fz) * 1.088_83;

    let r = 3.240_454 * x - 1.537_138 * y - 0.498_531 * z;
    let g = -0.969_266 * x + 1.876_011 * y + 0.041_556 * z;
    let b = 0.055_643 * x - 0.204_026 * y + 1.057_225 * z;

    let encode = |channel: f32| {
        let c = if channel <= 0.003_130_8 {
            12.92 * channel
        } else {
            1.055 * channel.powf(1.0 / 2.4) - 0.055
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 0..=255 first"
        )]
        {
            (c.clamp(0.0, 1.0) * 255.0).round() as u8
        }
    };
    (encode(r), encode(g), encode(b))
}

/// A coarse name for a colour, so facet counts group into something clickable.
///
/// Hue from the LAB chroma angle, and lightness and chroma decide the achromatic cases first — a very dark or
/// very desaturated pixel has a hue, and reporting it as "green" because a and b happened to lean that way is
/// how a colour facet loses a designer's trust. Eleven buckets, which is about as many as a facet rail can
/// show without becoming a colour picker.
fn bucket_of(lab: [f32; 3], r: u8, g: u8, b: u8) -> String {
    let (lightness, a, bb) = (lab[0], lab[1], lab[2]);
    let chroma = (a * a + bb * bb).sqrt();

    // Achromatic first. The thresholds are in LAB units: 10 is about where a tint stops reading as a colour.
    if chroma < 10.0 {
        return if lightness < 20.0 {
            "black"
        } else if lightness < 45.0 {
            "dark grey"
        } else if lightness < 75.0 {
            "grey"
        } else if lightness < 92.0 {
            "light grey"
        } else {
            "white"
        }
        .to_owned();
    }

    // Brown is the one bucket that is not a hue: it is a dark, low-chroma orange, and calling it "orange"
    // makes a search for orange return every wooden table in the library.
    if (0.0..45.0).contains(&lightness) && bb > 0.0 && a > 0.0 && chroma < 40.0 {
        return "brown".to_owned();
    }

    // The boundaries are measured LAB hue angles, not the HSV wheel — and the difference is large enough that
    // guessing them produces a facet that calls orange "yellow". Under D65:
    //
    //   red 32–40 · orange 63–73 · yellow 103 · green 136 · cyan 196 · blue 303–306 · magenta 328
    //
    // The gap between yellow and green is wide because LAB spends a lot of angle on greens, and the gap
    // between green and blue is wider still — there is no hue between 200 and 300 that any sRGB colour
    // reaches, so "blue" owns that whole arc.
    let hue = bb.atan2(a).to_degrees().rem_euclid(360.0);

    // Pink before the hue table, because pink is *not* a hue in LAB: (255,150,180) sits at 2°, right on top of
    // red, and what separates them is lightness — 74 against red's 47. Reading it off the angle would call
    // every pink "red", which is the sort of thing that makes somebody stop trusting a colour facet.
    if (0.0..50.0).contains(&hue) && lightness > 65.0 && chroma < 60.0 {
        return "pink".to_owned();
    }

    let name = match hue {
        h if h < 50.0 => "red",
        h if h < 88.0 => "orange",
        h if h < 115.0 => "yellow",
        h if h < 165.0 => "green",
        h if h < 225.0 => "teal",
        h if h < 315.0 => "blue",
        h if h < 345.0 => "purple",
        _ => "red",
    };
    // The sRGB values are unused in the decision and taken only so a caller cannot pass a mismatched pair.
    let _ = (r, g, b);
    name.to_owned()
}
