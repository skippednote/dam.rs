//! Files built to carry specific metadata, for tests that need a real one.
//!
//! Behind the `testing` feature, and shared rather than copied because more than one crate needs it: the
//! extractor's own suite and the ingest suite both have to produce a JPEG whose EXIF is *shaped like a camera's*.
//! The TIFF layout is fiddly enough that two hand-written copies would drift, and a fixture that is subtly wrong
//! in the same way as the code it tests proves nothing.
//!
//! ## Why the sub-directory matters
//!
//! EXIF is two directories, not one. IFD0 holds facts about the file — Artist, Copyright, Make — and the *Exif
//! sub-directory*, reached through a pointer tag, holds facts about the exposure: ISO, aperture, shutter, lens,
//! and the date the shutter actually opened. Half of the tags anybody maps live in the second one, and a tag
//! number in the wrong directory is a *different tag* as far as a reader is concerned. So this builder writes
//! both, and a fixture that only ever wrote IFD0 would silently pass while the sub-directory went unread.

/// One EXIF value, in the type its tag uses on the wire.
///
/// Types are not interchangeable: a reader looks up `(directory, number)` and then interprets the bytes by the
/// declared type, so writing a sensitivity as text produces something a real reader will not recognise as
/// sensitivity at all.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    /// A NUL-terminated ASCII string (type 2) — Artist, Copyright, LensModel, DateTimeOriginal.
    Text(&'a str),
    /// A 16-bit unsigned integer (type 3) — PhotographicSensitivity.
    Short(u16),
    /// A pair of 32-bit integers (type 5), numerator then denominator — FNumber, ExposureTime, FocalLength.
    Rational(u32, u32),
}

impl Entry<'_> {
    fn type_code(self) -> u16 {
        match self {
            Self::Text(_) => 2,
            Self::Short(_) => 3,
            Self::Rational(_, _) => 5,
        }
    }

    /// The value's bytes, or `None` for the types that always fit in the entry's own four.
    fn heap_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Text(text) => {
                let mut bytes = text.as_bytes().to_vec();
                bytes.push(0);
                (bytes.len() > 4).then_some(bytes)
            }
            Self::Short(_) => None,
            // A rational is eight bytes and never fits, so it is always out of line.
            Self::Rational(numerator, denominator) => {
                let mut bytes = numerator.to_le_bytes().to_vec();
                bytes.extend_from_slice(&denominator.to_le_bytes());
                Some(bytes)
            }
        }
    }

    fn count(self) -> u32 {
        match self {
            Self::Text(text) => u32::try_from(text.len() + 1).unwrap_or(u32::MAX),
            Self::Short(_) | Self::Rational(_, _) => 1,
        }
    }

    /// The four bytes written in the entry itself when the value fits there.
    fn inline_bytes(self) -> [u8; 4] {
        match self {
            Self::Text(text) => {
                let mut bytes = text.as_bytes().to_vec();
                bytes.push(0);
                bytes.resize(4, 0);
                [bytes[0], bytes[1], bytes[2], bytes[3]]
            }
            Self::Short(number) => {
                let [low, high] = number.to_le_bytes();
                [low, high, 0, 0]
            }
            Self::Rational(_, _) => [0; 4],
        }
    }
}

/// The tag numbers this module's callers use, so a test reads as the thing it is testing.
pub mod tags {
    /// IFD0.
    pub const ARTIST: u16 = 0x013b;
    /// IFD0.
    pub const COPYRIGHT: u16 = 0x8298;
    /// IFD0.
    pub const DESCRIPTION: u16 = 0x010e;
    /// IFD0.
    pub const MAKE: u16 = 0x010f;
    /// IFD0.
    pub const MODEL: u16 = 0x0110;
    /// IFD0.
    pub const SOFTWARE: u16 = 0x0131;
    /// IFD0. 1-8; 5-8 are the quarter turns that swap the axes, and 6 is what an iPhone writes for a
    /// portrait photograph.
    pub const ORIENTATION: u16 = 0x0112;
    /// Exif sub-directory.
    pub const DATE_TIME_ORIGINAL: u16 = 0x9003;
    /// Exif sub-directory.
    pub const EXPOSURE_TIME: u16 = 0x829a;
    /// Exif sub-directory.
    pub const F_NUMBER: u16 = 0x829d;
    /// Exif sub-directory.
    pub const PHOTOGRAPHIC_SENSITIVITY: u16 = 0x8827;
    /// Exif sub-directory.
    pub const FOCAL_LENGTH: u16 = 0x920a;
    /// Exif sub-directory.
    pub const LENS_MODEL: u16 = 0xa434;
    /// Exif sub-directory, and only since EXIF 2.31 — which is why a timestamp may have no zone at all.
    pub const OFFSET_TIME_ORIGINAL: u16 = 0x9011;
    /// IFD0, and the reason the sub-directory is reachable at all.
    const EXIF_IFD_POINTER: u16 = 0x8769;

    pub(super) const POINTER: u16 = EXIF_IFD_POINTER;
}

/// A TIFF block: IFD0 with `primary`, plus an Exif sub-directory with `sub` when `sub` is non-empty.
fn tiff(primary: &[(u16, Entry<'_>)], sub: &[(u16, Entry<'_>)]) -> Vec<u8> {
    // Little-endian, IFD0 at offset 8 — the layout every camera writes.
    let mut out = b"II\x2a\x00".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());

    let with_pointer = !sub.is_empty();
    let primary_count = primary.len() + usize::from(with_pointer);

    // Offsets have to be known before any entry is written, because an out-of-line value is referenced by
    // absolute position. So the directories are measured first and then filled.
    let ifd0_at = 8usize;
    let ifd0_size = 2 + primary_count * 12 + 4;
    let ifd0_heap_at = ifd0_at + ifd0_size;
    let ifd0_heap_len: usize = primary
        .iter()
        .filter_map(|(_, value)| value.heap_bytes().map(|bytes| bytes.len()))
        .sum();
    let sub_at = ifd0_heap_at + ifd0_heap_len;
    let sub_size = 2 + sub.len() * 12 + 4;
    let sub_heap_at = sub_at + sub_size;

    let mut heap = Vec::new();
    out.extend_from_slice(
        &u16::try_from(primary_count)
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    for (tag, value) in primary {
        write_entry(&mut out, *tag, *value, ifd0_heap_at, &mut heap);
    }
    if with_pointer {
        // Type 4 (LONG), one value: the absolute offset of the sub-directory.
        out.extend_from_slice(&tags::POINTER.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&u32::try_from(sub_at).unwrap_or(0).to_le_bytes());
    }
    // No IFD1: a thumbnail directory would only add a second place for a reader to look.
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&heap);

    if with_pointer {
        let mut sub_heap = Vec::new();
        out.extend_from_slice(&u16::try_from(sub.len()).unwrap_or(u16::MAX).to_le_bytes());
        for (tag, value) in sub {
            write_entry(&mut out, *tag, *value, sub_heap_at, &mut sub_heap);
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&sub_heap);
    }

    out
}

/// One 12-byte directory entry, pushing any out-of-line value onto `heap`.
fn write_entry(out: &mut Vec<u8>, tag: u16, value: Entry<'_>, heap_at: usize, heap: &mut Vec<u8>) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&value.type_code().to_le_bytes());
    out.extend_from_slice(&value.count().to_le_bytes());
    match value.heap_bytes() {
        Some(bytes) => {
            let at = u32::try_from(heap_at + heap.len()).unwrap_or(0);
            out.extend_from_slice(&at.to_le_bytes());
            heap.extend_from_slice(&bytes);
        }
        None => out.extend_from_slice(&value.inline_bytes()),
    }
}

/// A JPEG carrying an EXIF APP1 block and nothing else worth decoding.
///
/// Minimal on purpose: the body is just the end-of-image marker, which is enough for anything that *scans* the
/// header. Use [`decodable_jpeg_with_exif`] where something also has to decode the image.
#[must_use]
pub fn jpeg_with_exif(primary: &[(u16, Entry<'_>)], sub: &[(u16, Entry<'_>)]) -> Vec<u8> {
    let mut jpeg = vec![0xff, 0xd8];
    jpeg.extend_from_slice(&app1(primary, sub));
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    jpeg
}

/// The APP1 segment, ready to splice in after a JPEG's start-of-image marker.
#[must_use]
pub fn app1(primary: &[(u16, Entry<'_>)], sub: &[(u16, Entry<'_>)]) -> Vec<u8> {
    let block = tiff(primary, sub);
    let mut segment = vec![0xff, 0xe1];
    // Length covers itself and the `Exif\0\0` prefix, hence the eight.
    segment.extend_from_slice(
        &u16::try_from(block.len() + 8)
            .unwrap_or(u16::MAX)
            .to_be_bytes(),
    );
    segment.extend_from_slice(b"Exif\0\0");
    segment.extend_from_slice(&block);
    segment
}

/// An encoded image of `width` by `height` with the EXIF spliced in, so both readers have something to work with.
///
/// Ingest runs two passes over the same header — the probe decodes it for dimensions and the extractor scans it
/// for tags — so a fixture for ingest has to be a real image *and* carry real metadata.
#[must_use]
pub fn decodable_jpeg_with_exif(
    width: u32,
    height: u32,
    primary: &[(u16, Entry<'_>)],
    sub: &[(u16, Entry<'_>)],
) -> Vec<u8> {
    let mut body = Vec::new();
    let mut image = image::RgbImage::new(width.max(1), height.max(1));
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        pixel.0 = [
            u8::try_from(x % 256).unwrap_or(0),
            u8::try_from(y % 256).unwrap_or(0),
            128,
        ];
    }
    let encoded = image::DynamicImage::ImageRgb8(image).write_to(
        &mut std::io::Cursor::new(&mut body),
        image::ImageFormat::Jpeg,
    );
    if encoded.is_err() || body.len() < 2 {
        // Falls back to the header-only file rather than panicking: a fixture builder that can fail turns an
        // encoder problem into a mystery in whichever test happened to call it.
        return jpeg_with_exif(primary, sub);
    }

    // Immediately after the start-of-image marker, which is where a camera writes APP1.
    let mut out = body[..2].to_vec();
    out.extend_from_slice(&app1(primary, sub));
    out.extend_from_slice(&body[2..]);
    out
}
