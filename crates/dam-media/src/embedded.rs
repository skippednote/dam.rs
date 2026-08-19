//! The metadata a camera and an editor leave inside the file (Q.4).
//!
//! ## What this is for
//!
//! Auto-import has two halves. The tenant's *mapping* — "put `exif.artist` into my `photographer` field" — is
//! configuration and lives in the database. This is the other half: turning EXIF and XMP into a flat set of
//! named values a mapping can point at.
//!
//! ## Everything here is attacker-controlled
//!
//! A file arrives from outside and these bytes are whatever it says they are, so the rules are defensive rather
//! than thorough:
//!
//! - **Absence is not failure.** [`read`] returns a map, never a `Result`. Most files carry nothing, and an
//!   error would make "no metadata" indistinguishable from "corrupt file" at the one moment that distinction
//!   matters — during an ingest that should still succeed.
//! - **Values are bounded** at [`MAX_VALUE_CHARS`]. A crafted file can put a megabyte in one tag, and a caption
//!   that long would fail the *database write* instead — an error a mile from anywhere somebody could act on.
//! - **Control characters are stripped.** These values end up in JSON, in search text and on a page. A newline
//!   in a title is untidy; a NUL or an ANSI escape is the kind of thing that breaks a consumer downstream, and
//!   the boundary where it enters the system is the honest place to deal with it.
//! - **Empty is omitted.** Cameras write blank tags routinely, and importing one would overwrite a real value
//!   with nothing the next time an import ran — invisible, automatic data loss.
//! - **Nothing is typed.** Every value is text; the tenant's field definition decides what an `int` field does
//!   with `"1/125"`, and the validator refuses what does not fit. Guessing types here would be a second,
//!   quieter validator. The one exception is [`iso_timestamp`], which *transcribes* EXIF's specified
//!   `YYYY:MM:DD` into the interchange spelling — a fixed format converted, not a type inferred.
//!
//! ## Why XMP is scanned rather than parsed
//!
//! XMP is RDF/XML. The fields anybody maps are a handful of simple elements, and pointing a full XML parser at
//! attacker-controlled bytes buys entity expansion, namespace trickery and a much larger dependency for no gain.
//! So this looks for the specific elements it knows and refuses to be an XML processor. The cost is that exotic
//! XMP shapes are missed, which is the right way to be wrong: a field that fails to import is visible, while a
//! parser that can be made to hang is not.

use std::collections::BTreeMap;
use std::io::Cursor;

/// The longest value any single tag contributes.
///
/// Generous for a caption and far below anything that would trouble a JSONB column. The bound exists so a
/// hostile file fails *here*, where the truncation is documented, rather than at a database write.
pub const MAX_VALUE_CHARS: usize = 2_000;

/// How many values one file may contribute at all.
///
/// A file can declare thousands of tags. A mapping points at a handful, so the rest is weight — and an unbounded
/// map is an unbounded allocation from an untrusted header.
pub const MAX_VALUES: usize = 200;

/// The EXIF tags worth naming, and the name each gets.
///
/// A fixed list rather than "everything the parser found", because these names are *configuration surface*: a
/// tenant's mapping refers to them, so they have to be stable and legible. Adding one is additive; renaming one
/// silently stops an import.
const EXIF_TAGS: &[(exif::Tag, &str)] = &[
    (exif::Tag::Artist, "exif.artist"),
    (exif::Tag::Copyright, "exif.copyright"),
    (exif::Tag::ImageDescription, "exif.description"),
    (exif::Tag::Make, "exif.make"),
    (exif::Tag::Model, "exif.model"),
    (exif::Tag::Software, "exif.software"),
    (exif::Tag::DateTimeOriginal, "exif.taken_at"),
    (exif::Tag::OffsetTimeOriginal, "exif.taken_at_offset"),
    (exif::Tag::LensModel, "exif.lens"),
    (exif::Tag::FNumber, "exif.aperture"),
    (exif::Tag::ExposureTime, "exif.shutter"),
    (exif::Tag::PhotographicSensitivity, "exif.iso"),
    (exif::Tag::FocalLength, "exif.focal_length"),
];

/// The XMP elements worth reading, and the name each gets.
const XMP_ELEMENTS: &[(&str, &str)] = &[
    ("dc:title", "xmp.title"),
    ("dc:description", "xmp.description"),
    ("dc:creator", "xmp.creator"),
    ("dc:rights", "xmp.rights"),
    ("dc:subject", "xmp.subject"),
    ("photoshop:Credit", "xmp.credit"),
    ("photoshop:Headline", "xmp.headline"),
    ("Iptc4xmpCore:Location", "xmp.location"),
];

/// Every named value this file carries, as text.
///
/// Never fails: see the module docs. An unreadable file, an absent block and a malformed packet all read as
/// "nothing here", because that is what an ingest needs to hear in order to carry on.
#[must_use]
pub fn read(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    read_exif(bytes, &mut found);
    read_xmp(bytes, &mut found);
    found
}

fn read_exif(bytes: &[u8], into: &mut BTreeMap<String, String>) {
    let mut cursor = Cursor::new(bytes);
    let Ok(reader) = exif::Reader::new().read_from_container(&mut cursor) else {
        return;
    };

    for (tag, name) in EXIF_TAGS {
        if into.len() >= MAX_VALUES {
            return;
        }
        // Both IFDs: a camera writes some tags in the primary directory and some in the thumbnail's, and which
        // is which varies by make. Primary wins where both carry the tag.
        let field = reader
            .get_field(*tag, exif::In::PRIMARY)
            .or_else(|| reader.get_field(*tag, exif::In::THUMBNAIL));
        let Some(field) = field else { continue };

        // Text tags are read from their *bytes*; everything else is rendered.
        //
        // `display_value` is a debug rendering, not a value: it wraps a string in quotes and escapes control
        // characters into literal `\x1b` sequences — so importing through it would store `"Ada Lovelace"`,
        // quotes included, and turn a hostile NUL into four visible characters that no sanitiser would strip.
        // For a rational or a date it is exactly right, rendering `1/125` where the raw value is a pair of
        // integers, so the two cases are split rather than one being forced through the other.
        let rendered = match &field.value {
            exif::Value::Ascii(parts) => parts
                .iter()
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
                .join(" "),
            _ => field.display_value().with_unit(&reader).to_string(),
        };
        if let Some(clean) = sanitise(&rendered) {
            into.insert((*name).to_owned(), clean);
        }
    }

    // EXIF writes a timestamp as `YYYY:MM:DD HH:MM:SS`, which is not a date in any interchange format, so a
    // mapping onto a date field could never fire. Transcribing a fixed wire format into the standard one is not
    // the type-guessing this module refuses — the shape is specified, and the alternative is a name that is
    // documented as mappable and never works.
    if let Some(stamp) = into.get("exif.taken_at").cloned()
        && let Some(iso) =
            iso_timestamp(&stamp, into.get("exif.taken_at_offset").map(String::as_str))
    {
        into.insert("exif.taken_at".to_owned(), iso);
    }
}

/// An EXIF timestamp as ISO 8601, with the camera's own UTC offset when it recorded one.
///
/// The offset is *not* invented. EXIF's original timestamp is local wall-clock time with no zone, and 2.31 added
/// a separate tag for the offset — so a file that carries one describes an instant and a file that does not
/// describes a reading on a clock. Appending `Z` to the second kind would silently move a photograph by up to a
/// day, in whichever direction the photographer happened to be travelling. Without an offset this returns the
/// local form, which a `date` field accepts and a `datetime` field refuses: refusal is the correct answer,
/// because there is no instant to store.
fn iso_timestamp(raw: &str, offset: Option<&str>) -> Option<String> {
    let (date, time) = raw.split_once(' ')?;
    let mut parts = date.split(':');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    if !date.chars().all(|c| c.is_ascii_digit() || c == ':') {
        return None;
    }
    // The time is left exactly as written: EXIF spells it with colons already, which is also how ISO spells it.
    let local = format!("{year}-{month}-{day}T{time}");
    match offset {
        // `+05:30` and `Z` are both valid tails; anything else is a tag we do not understand, and guessing at it
        // would be the same mistake as inventing one.
        Some(offset) if offset == "Z" || is_offset(offset) => Some(format!("{local}{offset}")),
        _ => Some(local),
    }
}

/// Whether `text` is an offset EXIF is allowed to have written: `+HH:MM` or `-HH:MM`.
fn is_offset(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 6
        && (bytes[0] == b'+' || bytes[0] == b'-')
        && bytes[3] == b':'
        && bytes[1..3]
            .iter()
            .chain(&bytes[4..6])
            .all(u8::is_ascii_digit)
}

/// Reads the XMP packet by scanning for known elements. See the module docs for why this is not an XML parse.
fn read_xmp(bytes: &[u8], into: &mut BTreeMap<String, String>) {
    let Some(packet) = xmp_packet(bytes) else {
        return;
    };

    for (element, name) in XMP_ELEMENTS {
        if into.len() >= MAX_VALUES {
            return;
        }
        if let Some(value) = element_text(packet, element)
            && let Some(clean) = sanitise(&value)
        {
            into.insert((*name).to_owned(), clean);
        }
    }
}

/// The XMP packet's text, located by its delimiters.
///
/// Bounded by what the slice actually contains, so a truncated packet yields whatever is there rather than
/// reading past the end. `from_utf8_lossy` because a mangled packet should degrade to replacement characters —
/// which `sanitise` then handles — instead of discarding metadata that is otherwise fine.
fn xmp_packet(bytes: &[u8]) -> Option<&str> {
    const OPEN: &[u8] = b"<x:xmpmeta";
    const CLOSE: &[u8] = b"</x:xmpmeta>";
    // A packet longer than this is not a description of a photograph; it is a payload.
    const MAX_PACKET: usize = 512 * 1024;

    let start = find(bytes, OPEN)?;
    let window = &bytes[start..bytes.len().min(start + MAX_PACKET)];
    let end = find(window, CLOSE).map_or(window.len(), |at| at + CLOSE.len());
    std::str::from_utf8(&window[..end]).ok()
}

/// The text of the first `<element>` in `packet`, unwrapping one layer of `rdf:li` when present.
///
/// `dc:title` is an `rdf:Alt` of language alternatives and `dc:creator` an `rdf:Seq`, so the value a person
/// means is inside an `rdf:li`. Only the first is taken: a mapping targets one field, and joining alternatives
/// would put two languages in one value.
fn element_text(packet: &str, element: &str) -> Option<String> {
    let open = format!("<{element}");
    let close = format!("</{element}>");
    let start = packet.find(&open)?;
    // Past the opening tag's own `>`, so attributes are skipped rather than read as content.
    let after_open = packet[start..].find('>')? + start + 1;
    let end = packet[after_open..].find(&close)? + after_open;
    let inner = &packet[after_open..end];

    let text = match inner.find("<rdf:li") {
        Some(li) => {
            let after_li = inner[li..].find('>')? + li + 1;
            let li_end = inner[after_li..].find("</rdf:li>")? + after_li;
            &inner[after_li..li_end]
        }
        // No list wrapper: the element's own text, with any nested markup left in place for `sanitise` to
        // reject or keep. Stripping tags here would be the beginning of writing an XML parser.
        None => inner,
    };
    Some(unescape(text))
}

/// The five predefined XML entities. Nothing else — a numeric or custom entity is left as written, because
/// expanding entities from an untrusted document is the class of bug this module exists to avoid.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Last, so an escaped ampersand cannot produce a second entity.
        .replace("&amp;", "&")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A value fit to store, or `None` when there is nothing worth storing.
///
/// Control characters go; a newline or tab becomes a space so two words do not run together; runs of space
/// collapse; and the result is bounded. An empty or whitespace-only value is `None` — see the module docs on why
/// importing a blank is worse than importing nothing.
fn sanitise(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(MAX_VALUE_CHARS));
    let mut last_was_space = false;
    for ch in raw.chars() {
        let ch = if ch == '\n' || ch == '\r' || ch == '\t' {
            ' '
        } else if ch.is_control() {
            // Dropped entirely rather than replaced: a NUL or an escape introducer is not standing in for a
            // character somebody typed.
            continue;
        } else {
            ch
        };
        if ch == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        if out.chars().count() >= MAX_VALUE_CHARS {
            break;
        }
        out.push(ch);
    }

    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Every name [`read`] can produce, in the order a picker should show them.
///
/// Exported because a mapping's left-hand side is *configuration*, and a screen that made an administrator type
/// `exif.artist` from memory would produce rules that look right and never fire. The names come from the same
/// two tables the extractor reads, so the list cannot drift from what is actually available.
#[must_use]
pub fn sources() -> Vec<&'static str> {
    EXIF_TAGS
        .iter()
        .map(|(_, name)| *name)
        .chain(XMP_ELEMENTS.iter().map(|(_, name)| *name))
        .collect()
}
