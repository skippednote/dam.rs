//! Probing: the technical facts that fill the `assets` columns.
//!
//! ## Dimensions are reported twice, on purpose
//!
//! A phone stores a portrait photo as 4000×3000 pixels with `orientation = 6`. Reporting those
//! numbers as the image's size makes every grid cell, aspect ratio and thumbnail sideways — the
//! most visible bug a DAM can have. Reporting only the rotated size loses what the file actually
//! contains, which the derivative pipeline needs.
//!
//! So both are named explicitly: [`Probe::stored_width`] is what the pixels are, and
//! [`Probe::display_width`] is what a person sees. Nothing is called `width`, because a bare
//! `width` is the field somebody uses without thinking about which one they wanted.
//!
//! ## The header is read without decoding
//!
//! A 13-byte GIF header can claim 65000×65000 — an instruction to allocate about 12 GB. Every
//! dimension here comes from the header alone, and [`Probe::exceeds_pixel_budget`] is how a
//! caller decides whether decoding is worth attempting at all. Anything that *must* decode —
//! the perceptual hash — checks that budget first.

use image::ImageReader;
use std::io::Cursor;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("decoding: {0}")]
    Decode(String),

    /// Refused before decoding. Its own variant because it is a property of the *file*, worth
    /// recording against the asset, rather than a transient failure worth retrying.
    #[error("{width}x{height} is {pixels} pixels, past the {budget}-pixel budget")]
    PixelBudget {
        width: u32,
        height: u32,
        pixels: u64,
        budget: u64,
    },
}

type Result<T> = std::result::Result<T, Error>;

/// Default ceiling on what will be decoded: 100 megapixels.
///
/// Above a 12000×8000 medium-format frame, which is the largest thing a photo library plausibly
/// holds, and far below the point where decoding threatens the host.
pub const DEFAULT_PIXEL_BUDGET: u64 = 100_000_000;

/// The budget must clear a 12000x8000 medium-format frame — the largest a photo library
/// plausibly holds — and stay well under the gigapixel headers that arrive from fuzzing.
/// Checked at compile time rather than in a test, so it cannot be skipped.
const _: () = assert!(
    DEFAULT_PIXEL_BUDGET > 12_000 * 8_000 && DEFAULT_PIXEL_BUDGET < 4_000_000_000,
    "the pixel budget must admit a real medium-format frame and refuse a bomb"
);

/// What the file says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// Pixel dimensions as stored, before any orientation transform.
    pub stored_width: Option<u32>,
    pub stored_height: Option<u32>,
    /// Dimensions as a viewer sees them — the axes swapped when the orientation is a quarter
    /// turn. This is what a layout, an aspect ratio and a thumbnail request want.
    pub display_width: Option<u32>,
    pub display_height: Option<u32>,
    /// EXIF orientation, 1–8, when the file carries a valid one. `None` means no tag at all,
    /// which is distinct from `Some(1)` ("explicitly upright") for provenance purposes even
    /// though both mean no work for the derivative pipeline.
    pub orientation: Option<u16>,
    /// `srgb`, `gray`, or `cmyk` where it can be told. Not a colour *profile* — see
    /// `has_icc_profile`.
    pub color_space: Option<String>,
    pub has_alpha: Option<bool>,
    /// Whether an ICC profile is embedded (D11: profiles are preserved end to end, and CMYK
    /// converts at delivery). Detected so a master that has one can be handled correctly — and
    /// so an untagged CMYK file, which cannot be converted correctly later, can be flagged.
    pub has_icc_profile: bool,
    /// Pages, for paged formats. `None` here always: counting PDF or Office pages needs a
    /// renderer, which is the libvips/LibreOffice path rather than this one.
    pub page_count: Option<u32>,
    pub duration_ms: Option<i64>,
}

impl Probe {
    /// Total pixels as stored.
    pub fn pixel_count(&self) -> Option<u64> {
        Some(u64::from(self.stored_width?) * u64::from(self.stored_height?))
    }

    /// Whether decoding this would exceed `budget` pixels.
    ///
    /// An unknown size counts as *not* exceeding: refusing a file whose dimensions could not be
    /// read would reject every format this probe does not understand, and a DAM stores those.
    pub fn exceeds_pixel_budget(&self, budget: u64) -> bool {
        self.pixel_count().is_some_and(|p| p > budget)
    }

    /// Whether a derivative has to apply a transform.
    ///
    /// True for every orientation except 1 — mirrors and flips (2, 4) need work too, even though
    /// they do not change the axes.
    pub fn needs_rotation(&self) -> bool {
        self.orientation.is_some_and(|o| o != 1)
    }

    /// Whether the orientation is a quarter turn, which swaps the axes.
    fn swaps_axes(orientation: Option<u16>) -> bool {
        matches!(orientation, Some(5..=8))
    }
}

/// Reads what the header says, without decoding pixels.
pub fn image(bytes: &[u8]) -> Result<Probe> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::Decode(format!("guessing the format: {e}")))?;
    let format = reader.format();

    let (stored_width, stored_height) =
        reader
            .into_dimensions()
            .map(|(w, h)| (Some(w), Some(h)))
            .map_err(|e| Error::Decode(format!("reading dimensions: {e}")))?;

    // EXIF is parsed from the bytes rather than through the decoder: `image` does not expose it,
    // and it exists in JPEG, TIFF, PNG and WebP alike.
    let orientation = exif_orientation(bytes);

    let (display_width, display_height) = if Probe::swaps_axes(orientation) {
        (stored_height, stored_width)
    } else {
        (stored_width, stored_height)
    };

    let (color_space, has_alpha) = colour_of(format, bytes);

    Ok(Probe {
        stored_width,
        stored_height,
        display_width,
        display_height,
        orientation,
        color_space,
        has_alpha,
        has_icc_profile: has_icc_profile(bytes),
        page_count: None,
        duration_ms: None,
    })
}

/// A perceptual hash, for near-duplicate detection.
///
/// Unlike BLAKE3 this survives a re-encode, a re-save and a small crop — which is the whole point:
/// content addressing already catches identical bytes, and this catches the same *picture*.
///
/// Decoding is unavoidable here, so the pixel budget is enforced first. "Hash everything on
/// ingest" is exactly what a worker will do, so the guard belongs here rather than at the call
/// site.
pub fn perceptual_hash(bytes: &[u8]) -> Result<Vec<u8>> {
    perceptual_hash_within(bytes, DEFAULT_PIXEL_BUDGET)
}

pub fn perceptual_hash_within(bytes: &[u8], budget: u64) -> Result<Vec<u8>> {
    let probed = image(bytes)?;
    if let (Some(width), Some(height), Some(pixels)) = (
        probed.stored_width,
        probed.stored_height,
        probed.pixel_count(),
    ) && pixels > budget
    {
        return Err(Error::PixelBudget {
            width,
            height,
            pixels,
            budget,
        });
    }

    let decoded = image::load_from_memory(bytes)
        .map_err(|e| Error::Decode(format!("decoding for a perceptual hash: {e}")))?;
    let hasher = image_hasher::HasherConfig::new()
        // Gradient (dHash) rather than the mean: it survives brightness and contrast changes,
        // which a re-export from a design tool routinely applies.
        .hash_alg(image_hasher::HashAlg::Gradient)
        .to_hasher();
    Ok(hasher.hash_image(&decoded).as_bytes().to_vec())
}

/// Hamming distance between two perceptual hashes.
///
/// Errors on a length mismatch rather than comparing what it can: two hashes from different
/// algorithms or sizes are not comparable, and a silently-small distance would read as a
/// duplicate.
pub fn hash_distance(a: &[u8], b: &[u8]) -> Result<u32> {
    if a.len() != b.len() {
        return Err(Error::Decode(format!(
            "cannot compare a {}-byte hash with a {}-byte one",
            a.len(),
            b.len()
        )));
    }
    Ok(a.iter()
        .zip(b)
        .map(|(x, y)| (x ^ y).count_ones())
        .sum::<u32>())
}

/// Reads the EXIF orientation tag, if the file has a valid one.
///
/// Out-of-range values are dropped. EXIF defines 1–8; cameras have written 0 and 9, and applying
/// an unknown transform would corrupt the derivative — so an unrecognised value is treated as no
/// tag at all rather than guessed at.
fn exif_orientation(bytes: &[u8]) -> Option<u16> {
    let mut cursor = Cursor::new(bytes);
    let reader = exif::Reader::new().read_from_container(&mut cursor).ok()?;
    let field = reader.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    let value = field.value.get_uint(0)?;
    let value = u16::try_from(value).ok()?;
    (1..=8).contains(&value).then_some(value)
}

/// Colour space and alpha, from the header where the format exposes it.
fn colour_of(format: Option<image::ImageFormat>, bytes: &[u8]) -> (Option<String>, Option<bool>) {
    use image::ImageFormat;
    match format {
        // JPEG has no alpha channel, and its colour model is in the SOF component count: 1 is
        // greyscale, 3 YCbCr (reported as sRGB, which is what it is once converted), 4 CMYK or
        // YCCK — the case D11 cares about, and the one `image` cannot decode at all.
        Some(ImageFormat::Jpeg) => (jpeg_colour(bytes), Some(false)),
        Some(ImageFormat::Png) => png_colour(bytes),
        // Everything else: report alpha from the decoded colour type only if it is cheap to
        // know. Guessing would be worse than saying nothing, since a wrong `has_alpha` makes a
        // logo get matted onto the wrong background.
        _ => (Some("srgb".to_owned()), None),
    }
}

fn jpeg_colour(bytes: &[u8]) -> Option<String> {
    // Walk the segment headers to the first SOF. Cheap, and it avoids decoding.
    let mut i = 2usize;
    while i + 3 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // SOF0..SOF15 except the DHT/JPG/DAC markers interleaved in that range.
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let components = *bytes.get(i + 9)?;
            return Some(
                match components {
                    1 => "gray",
                    3 => "srgb",
                    4 => "cmyk",
                    _ => return None,
                }
                .to_owned(),
            );
        }
        let length = u16::from_be_bytes([*bytes.get(i + 2)?, *bytes.get(i + 3)?]) as usize;
        i += 2 + length;
    }
    None
}

fn png_colour(bytes: &[u8]) -> (Option<String>, Option<bool>) {
    // IHDR is always the first chunk: 8-byte signature, 4-byte length, 4-byte type, then
    // width, height, bit depth, colour type.
    let colour_type = bytes.get(25);
    match colour_type {
        Some(0) => (Some("gray".to_owned()), Some(false)),
        Some(2) => (Some("srgb".to_owned()), Some(false)),
        // 3 is palette; a palette can carry a tRNS chunk, so alpha is not knowable from IHDR.
        Some(3) => (
            Some("srgb".to_owned()),
            Some(find_chunk(bytes, b"tRNS").is_some()),
        ),
        Some(4) => (Some("gray".to_owned()), Some(true)),
        Some(6) => (Some("srgb".to_owned()), Some(true)),
        _ => (None, None),
    }
}

/// Whether an ICC profile is embedded.
fn has_icc_profile(bytes: &[u8]) -> bool {
    // JPEG carries it in an APP2 segment tagged `ICC_PROFILE`; PNG in an `iCCP` chunk; TIFF in
    // tag 34675. Searching for the marker is enough to answer "is there one", which is what the
    // ingest record needs — extracting it is the derivative pipeline's job.
    contains(bytes, b"ICC_PROFILE") || find_chunk(bytes, b"iCCP").is_some()
}

fn find_chunk(bytes: &[u8], kind: &[u8; 4]) -> Option<usize> {
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut i = 8usize;
    while i + 8 <= bytes.len() {
        let length = u32::from_be_bytes(bytes.get(i..i + 4)?.try_into().ok()?) as usize;
        let name = bytes.get(i + 4..i + 8)?;
        if name == kind {
            return Some(i);
        }
        if name == b"IDAT" {
            // Stop at the pixel data: a marker found beyond it is a coincidence inside
            // compressed bytes, not a chunk header.
            return None;
        }
        i = i.checked_add(12)?.checked_add(length)?;
    }
    None
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_size_does_not_count_against_the_budget() {
        // Refusing a file whose dimensions could not be read would reject every format this
        // probe does not understand — and a DAM stores those.
        let unknown = Probe::default();
        assert!(!unknown.exceeds_pixel_budget(1));
        assert_eq!(unknown.pixel_count(), None);
    }

    #[test]
    fn only_a_quarter_turn_swaps_the_axes() {
        for o in 1..=4u16 {
            assert!(!Probe::swaps_axes(Some(o)), "{o}");
        }
        for o in 5..=8u16 {
            assert!(Probe::swaps_axes(Some(o)), "{o}");
        }
        assert!(!Probe::swaps_axes(None));
    }

    #[test]
    fn hashes_of_different_lengths_are_not_comparable() {
        assert!(hash_distance(&[0, 0], &[0]).is_err());
        assert_eq!(hash_distance(&[0b1010], &[0b0101]).expect("distance"), 4);
    }
}
