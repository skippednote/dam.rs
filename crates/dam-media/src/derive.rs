//! Rendering derivatives: resize, convert, matte.
//!
//! The `image` + `fast_image_resize` path, which ARCHITECTURE calls the fallback to libvips. It
//! covers JPEG, PNG, WebP, TIFF, GIF and AVIF; RAW, PSD and Office formats need the primary path.
//!
//! Four things here are the difference between a rendition and a support ticket:
//!
//! - **Orientation is applied exactly once**, and the output carries no orientation tag. Rotate on
//!   the way in and leave the tag, and the viewer rotates again.
//! - **Alpha is premultiplied before resizing.** Otherwise every edge pixel averages the
//!   transparent *black* around a logo into the white inside it, and the result has a grey fringe
//!   — subtle enough to pass review, obvious enough for a brand team to reject.
//! - **Transparency is matted when the target cannot carry it.** A transparent PNG flattened to
//!   JPEG without matting is black where it should be white.
//! - **Nothing is upscaled.** A 2048px rendition of a 64px source looks like a defect.

use crate::probe;
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ImageEncoder};
use std::io::Cursor;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("rendering: {0}")]
    Render(String),

    #[error(transparent)]
    Probe(#[from] probe::Error),
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    Jpeg,
    Png,
    WebP,
    Avif,
}

impl OutputFormat {
    /// Whether the format can carry an alpha channel. Drives matting.
    pub fn supports_alpha(self) -> bool {
        !matches!(self, Self::Jpeg)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Avif => "avif",
        }
    }

    /// The filename extension a stored rendition gets.
    ///
    /// `jpg` rather than `jpeg`, which is the one place this differs from [`Self::as_str`] — and the reason
    /// they are separate methods rather than one: the object key uses the conventional extension while the
    /// hash and the logs use the format's name.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Avif => "avif",
        }
    }

    /// The media type to serve it as.
    ///
    /// Stored on the derivative row rather than guessed at delivery: a browser handed `application/octet-
    /// stream` for a WebP downloads it instead of showing it, and the derivative is the one place that knows.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::WebP => "image/webp",
            Self::Avif => "image/avif",
        }
    }
}

/// How the source is fitted into the requested box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fit {
    /// Fit inside, preserving aspect ratio. The result may be smaller than the box in one axis.
    Contain,
    /// Fill the box, cropping the overflow from the centre. What a fixed-size grid cell needs.
    Cover,
}

/// A rendition profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rendition {
    pub width: u32,
    pub height: u32,
    pub format: OutputFormat,
    /// 1–100, for the lossy formats. Ignored by PNG.
    pub quality: u8,
    pub fit: Fit,
    /// What transparency is flattened onto when the target cannot carry it.
    pub background: [u8; 3],
}

/// Renders `bytes` to the rendition.
pub fn render(bytes: &[u8], spec: &Rendition) -> Result<Vec<u8>> {
    if spec.width == 0 || spec.height == 0 {
        return Err(Error::Render(format!(
            "{}x{} is not a rendition",
            spec.width, spec.height
        )));
    }

    // The budget is checked from the header, before anything is allocated. Rendering has to decode,
    // so this guard belongs here as well as in the probe.
    let probed = probe::image(bytes)?;
    if let (Some(width), Some(height), Some(pixels)) = (
        probed.stored_width,
        probed.stored_height,
        probed.pixel_count(),
    ) && pixels > probe::DEFAULT_PIXEL_BUDGET
    {
        return Err(Error::Probe(probe::Error::PixelBudget {
            width,
            height,
            pixels,
            budget: probe::DEFAULT_PIXEL_BUDGET,
        }));
    }

    let decoded = image::load_from_memory(bytes)
        .map_err(|e| Error::Render(format!("decoding the source: {e}")))?;

    // Once, here, and never recorded in the output — see `encode`.
    let upright = apply_orientation(decoded, probed.orientation);

    let (target_w, target_h) = target_size(upright.width(), upright.height(), spec);
    let resized = resize(&upright, target_w, target_h)?;

    let cropped = match spec.fit {
        Fit::Contain => resized,
        Fit::Cover => crop_centre(resized, spec.width.min(target_w), spec.height.min(target_h)),
    };

    let final_image = if cropped.color().has_alpha() && !spec.format.supports_alpha() {
        matte(&cropped, spec.background)
    } else {
        cropped
    };

    encode(&final_image, spec)
}

/// A stable hash of everything that affects the output bytes.
///
/// §18.1 requires the colour profile and rendering intent to be part of it, so the cache cannot
/// serve a wrongly-converted rendition — the same pixels at the same size in the same format are
/// still a *different derivative* if the profile differs. Anything omitted here becomes a stale
/// derivative served forever.
pub fn op_hash(spec: &Rendition, profile: &str, intent: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"damrs-rendition-v1\0");
    hasher.update(&spec.width.to_le_bytes());
    hasher.update(&spec.height.to_le_bytes());
    hasher.update(spec.format.as_str().as_bytes());
    hasher.update(&[0]);
    hasher.update(&[spec.quality]);
    hasher.update(match spec.fit {
        Fit::Contain => b"contain",
        Fit::Cover => b"cover",
    });
    hasher.update(&[0]);
    hasher.update(&spec.background);
    // Length-prefixed rather than concatenated: "srgb" + "perceptual" and "srgbper" + "ceptual"
    // would otherwise hash the same, and a hash collision here serves the wrong colour.
    hasher.update(&(profile.len() as u32).to_le_bytes());
    hasher.update(profile.as_bytes());
    hasher.update(&(intent.len() as u32).to_le_bytes());
    hasher.update(intent.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Applies an EXIF orientation, so the pixels are upright.
///
/// All eight values, including the four that mirror: a mirrored derivative is as wrong as a rotated
/// one, and only 1 means no work.
fn apply_orientation(img: DynamicImage, orientation: Option<u16>) -> DynamicImage {
    match orientation {
        Some(2) => img.fliph(),
        Some(3) => img.rotate180(),
        Some(4) => img.flipv(),
        Some(5) => img.rotate90().fliph(),
        Some(6) => img.rotate90(),
        Some(7) => img.rotate270().fliph(),
        Some(8) => img.rotate270(),
        // 1, absent, or an out-of-range value the probe already rejected.
        _ => img,
    }
}

/// The size to resize to, never larger than the source.
fn target_size(src_w: u32, src_h: u32, spec: &Rendition) -> (u32, u32) {
    let (box_w, box_h) = (spec.width, spec.height);
    let scale_w = f64::from(box_w) / f64::from(src_w.max(1));
    let scale_h = f64::from(box_h) / f64::from(src_h.max(1));
    let scale = match spec.fit {
        // Contain: the smaller ratio, so both axes fit inside the box.
        Fit::Contain => scale_w.min(scale_h),
        // Cover: the larger, so the box is filled and the excess is cropped.
        Fit::Cover => scale_w.max(scale_h),
    }
    // Never above 1: upscaling produces a blurry rendition that reads as a defect, so the source
    // is the ceiling and the caller decides whether that is usable.
    .min(1.0);

    let w = ((f64::from(src_w) * scale).round() as u32).max(1);
    let h = ((f64::from(src_h) * scale).round() as u32).max(1);
    (w, h)
}

/// Lanczos3 resize with alpha premultiplied.
fn resize(src: &DynamicImage, width: u32, height: u32) -> Result<DynamicImage> {
    if src.width() == width && src.height() == height {
        return Ok(src.clone());
    }
    let mut dst = DynamicImage::new(width, height, src.color());
    let options = ResizeOptions::new()
        // Lanczos3 is the standard choice for downscaling photography: sharper than bilinear
        // without the ringing of a wider kernel.
        .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
        // The halo fix. Without it, a transparent-black border averages into the visible pixels
        // next to it and every edge darkens.
        .use_alpha(true);

    Resizer::new()
        .resize(src, &mut dst, &options)
        .map_err(|e| Error::Render(format!("resizing to {width}x{height}: {e}")))?;
    Ok(dst)
}

/// Centre-crops to `width` x `height`, which is what `Fit::Cover` needs after the resize.
fn crop_centre(img: DynamicImage, width: u32, height: u32) -> DynamicImage {
    let width = width.min(img.width());
    let height = height.min(img.height());
    let x = (img.width() - width) / 2;
    let y = (img.height() - height) / 2;
    img.crop_imm(x, y, width, height)
}

/// Flattens transparency onto a solid colour.
///
/// Composited per pixel rather than by drawing the image over a filled canvas, because the latter
/// depends on the blend mode the library happens to use — and getting it wrong yields a black
/// background, which is the bug this exists to prevent.
fn matte(img: &DynamicImage, background: [u8; 3]) -> DynamicImage {
    let src = img.to_rgba8();
    let mut out = image::RgbImage::new(src.width(), src.height());
    for (x, y, px) in src.enumerate_pixels() {
        let alpha = f32::from(px[3]) / 255.0;
        let blend = |channel: usize| {
            (f32::from(px[channel]) * alpha + f32::from(background[channel]) * (1.0 - alpha))
                .round()
                .clamp(0.0, 255.0) as u8
        };
        out.put_pixel(x, y, image::Rgb([blend(0), blend(1), blend(2)]));
    }
    DynamicImage::ImageRgb8(out)
}

/// Encodes to the target format.
///
/// No EXIF is written, deliberately: the pixels are already upright, so an orientation tag on the
/// output would make a viewer rotate them a second time.
fn encode(img: &DynamicImage, spec: &Rendition) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    match spec.format {
        OutputFormat::Jpeg => {
            // The encoder is deterministic for a given quality, which matters because derivatives
            // are content-addressed: a non-deterministic encoder would store a new object on every
            // render and the cache would never hit.
            let rgb = img.to_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, spec.quality)
                .write_image(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| Error::Render(format!("encoding JPEG: {e}")))?;
        }
        OutputFormat::Png => {
            img.write_to(&mut out, image::ImageFormat::Png)
                .map_err(|e| Error::Render(format!("encoding PNG: {e}")))?;
        }
        OutputFormat::WebP => {
            img.write_to(&mut out, image::ImageFormat::WebP)
                .map_err(|e| Error::Render(format!("encoding WebP: {e}")))?;
        }
        OutputFormat::Avif => {
            img.write_to(&mut out, image::ImageFormat::Avif)
                .map_err(|e| Error::Render(format!("encoding AVIF: {e}")))?;
        }
    }
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Rendition {
        Rendition {
            width: 100,
            height: 100,
            format: OutputFormat::Jpeg,
            quality: 82,
            fit: Fit::Contain,
            background: [255, 255, 255],
        }
    }

    #[test]
    fn only_jpeg_lacks_an_alpha_channel() {
        assert!(!OutputFormat::Jpeg.supports_alpha());
        for f in [OutputFormat::Png, OutputFormat::WebP, OutputFormat::Avif] {
            assert!(f.supports_alpha(), "{f:?}");
        }
    }

    #[test]
    fn contain_fits_inside_and_cover_fills() {
        // 400x200 into 100x100.
        assert_eq!(target_size(400, 200, &spec()), (100, 50));
        assert_eq!(
            target_size(
                400,
                200,
                &Rendition {
                    fit: Fit::Cover,
                    ..spec()
                }
            ),
            (200, 100),
            "cover scales until the short axis fills the box, and the rest is cropped"
        );
    }

    #[test]
    fn a_small_source_is_left_alone() {
        assert_eq!(target_size(64, 48, &spec()), (64, 48));
    }

    #[test]
    fn a_one_pixel_result_never_rounds_to_zero() {
        // A 10000x1 panorama into a 100x100 box scales to 0.01, and a zero-height image is not
        // encodable — so the floor is one pixel rather than an error.
        let (w, h) = target_size(10_000, 1, &spec());
        assert!(w >= 1 && h >= 1, "got {w}x{h}");
    }

    #[test]
    fn the_profile_and_intent_are_length_prefixed_in_the_hash() {
        // Concatenation would make ("srgbper", "ceptual") and ("srgb", "perceptual") collide, and a
        // collision here serves the wrong colour from cache.
        assert_ne!(
            op_hash(&spec(), "srgbper", "ceptual"),
            op_hash(&spec(), "srgb", "perceptual")
        );
    }
}
