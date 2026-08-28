//! Magic-byte sniffing. The client's `Content-Type` is evidence, never authority.
//!
//! `assets.mime` is "sniffed, never client-supplied" in the schema, and the reason is
//! delivery: a browser renders according to the MIME we stored. A client declaring
//! `image/png` for an HTML file is describing a stored XSS payload on the customer's own
//! asset domain; one declaring `image/jpeg` for a Mach-O binary is describing a malware
//! distribution endpoint. Both are cheap to prevent here and expensive to discover later.
//!
//! ## What this layer does and does not decide
//!
//! It reports what the bytes are and what handling they imply. It does not refuse anything —
//! the upload path does that, because the policy differs by tenant and by intent. Two
//! distinctions carry that policy:
//!
//! - [`Sniffed::is_dangerous`] — an executable. Not an asset under any reading, and a
//!   refusal by default.
//! - [`Sniffed::carries_active_content`] — SVG and HTML. Legitimate assets (HTML5 creatives,
//!   icon sets) that must never be served inline unsanitised, because they execute with the
//!   privileges of whatever origin serves them.
//!
//! ## Formats magic bytes cannot see
//!
//! SVG, WebVTT and SubRip are text. `infer` returns nothing for them, and leaving them
//! undetected would either lose their previewability or — much worse — invite a fallback to
//! the client's declaration. They are sniffed from their content here. A filename extension
//! is never consulted: it is attacker-controlled in exactly the same way the declared MIME
//! type is.

use std::fmt;

/// Bytes needed to sniff any format this layer claims.
///
/// A streaming upload is sniffed from its first chunk rather than being buffered whole
/// (§18.3), so this must exceed the longest signature — including the text scans, which look
/// past an XML prolog and any leading whitespace.
pub const SNIFF_PREFIX: usize = 8192;

/// A sniff window smaller than the longest scan would make detection depend on how the first
/// chunk happened to be sized — the SVG search alone looks 1 KiB in. Checked at compile time
/// rather than in a test, so it cannot be skipped.
const _: () = assert!(
    SNIFF_PREFIX >= 4096,
    "SNIFF_PREFIX must cover the text scans"
);

/// How an asset is handled downstream.
///
/// Coarser than a MIME type on purpose: the probe and derivative pipeline branches on class,
/// and a per-MIME match would need editing for every new format in §18.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaClass {
    Image,
    Video,
    Audio,
    /// PDF, Office, ebooks — anything that rasterises to pages.
    Document,
    Archive,
    Font,
    Subtitle,
    /// HTML and other markup that executes when served. A real asset class (HTML5
    /// creatives), but a delivery constraint.
    ActiveContent,
    /// A program. Never a creative asset.
    Executable,
    /// Stored, but not processable. A DAM is a store first.
    Unknown,
}

impl MediaClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Font => "font",
            Self::Subtitle => "subtitle",
            Self::ActiveContent => "active_content",
            Self::Executable => "executable",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for MediaClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the bytes turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sniffed {
    /// Never empty: `application/octet-stream` when nothing matched.
    pub mime: String,
    /// Canonical extension for the sniffed type — not the uploaded filename's. The original
    /// filename is preserved separately in `assets.filename`.
    pub ext: Option<String>,
    pub class: MediaClass,
    /// The client's declared type, when it disagreed with the bytes.
    ///
    /// Recorded rather than acted on. A mismatch is usually a careless client, occasionally
    /// an attempt, and either way it is worth an audit entry — dropping it means the only
    /// evidence of a deliberate attempt disappears.
    pub declared_mismatch: Option<String>,
}

impl Sniffed {
    /// An executable. Refused by default at the upload path.
    pub fn is_dangerous(&self) -> bool {
        matches!(self.class, MediaClass::Executable)
    }

    /// Executes when served: SVG and HTML. Stored, but never inline without sanitisation.
    pub fn carries_active_content(&self) -> bool {
        matches!(self.class, MediaClass::ActiveContent) || self.mime == "image/svg+xml"
    }

    /// Whether a proxy and derivatives can be produced.
    pub fn is_processable(&self) -> bool {
        matches!(
            self.class,
            MediaClass::Image
                | MediaClass::Video
                | MediaClass::Audio
                | MediaClass::Document
                | MediaClass::Subtitle
        )
    }

    fn known(mime: &str, ext: Option<&str>, class: MediaClass) -> Self {
        Self {
            mime: mime.to_owned(),
            ext: ext.map(str::to_owned),
            class,
            declared_mismatch: None,
        }
    }

    fn opaque() -> Self {
        Self {
            mime: "application/octet-stream".to_owned(),
            ext: None,
            class: MediaClass::Unknown,
            declared_mismatch: None,
        }
    }
}

/// Identifies `prefix`, which should be the first [`SNIFF_PREFIX`] bytes of the object.
///
/// `declared_mime` and `filename` are recorded and compared, never trusted: a declaration
/// that disagrees with the bytes is reported in [`Sniffed::declared_mismatch`], and one that
/// cannot be corroborated at all is *still* not adopted. A fallback to the declaration would
/// make every check here bypassable by sending unrecognised bytes.
pub fn sniff(prefix: &[u8], declared_mime: Option<&str>, filename: Option<&str>) -> Sniffed {
    let _ = filename; // Deliberately unused — see the module docs.

    let mut out = match infer::get(prefix) {
        // `text/xml` and `text/plain` are containers, not formats: an SVG with an XML prolog
        // is detected as XML, and stopping there would lose every prologued SVG's
        // previewability and its active-content flag. So a generic text answer is refined by
        // content before it is accepted.
        // Refinement may only *specialise*. `sniff_text` alone would answer `text/plain` for a
        // catalogue XML file and replace a more specific type with a less specific one, so the
        // plain-text fallback is deliberately not part of this branch.
        Some(t) if is_generic_text(t.mime_type()) => {
            sniff_text_specific(prefix).unwrap_or_else(|| from_infer(t))
        }
        Some(t) => from_infer(t),
        // No signature matched: signatures `infer` lacks, then specific text formats, then
        // plain text, then opaque.
        None => explicit_signature(prefix)
            .or_else(|| sniff_text_specific(prefix))
            .or_else(|| sniff_plain_text(prefix))
            .unwrap_or_else(Sniffed::opaque),
    };

    if let Some(declared) = declared_mime {
        let declared = declared
            .split(';')
            .next()
            .unwrap_or(declared)
            .trim()
            .to_ascii_lowercase();
        if !declared.is_empty() && declared != out.mime {
            out.declared_mismatch = Some(declared);
        }
    }
    out
}

fn from_infer(t: infer::Type) -> Sniffed {
    Sniffed::known(t.mime_type(), Some(t.extension()), class_of(t))
}

/// Whether an `infer` answer is a container rather than a format, and worth refining.
fn is_generic_text(mime: &str) -> bool {
    matches!(mime, "text/xml" | "text/plain")
}

/// Signatures `infer` 0.22 does not carry.
///
/// ELF is the notable one: `infer` detects Mach-O, PE, wasm and Java class files but **not
/// ELF**, so a Linux binary would fall through to `application/octet-stream` with class
/// `Unknown` — meaning [`Sniffed::is_dangerous`] would be false and the upload path would
/// store it happily. Verified by probing the crate directly rather than assumed.
fn explicit_signature(prefix: &[u8]) -> Option<Sniffed> {
    if prefix.starts_with(&[0x7F, b'E', b'L', b'F']) {
        return Some(Sniffed::known(
            "application/x-executable",
            Some("elf"),
            MediaClass::Executable,
        ));
    }
    None
}

/// Specific formats with no magic bytes, identified from their leading text.
///
/// Only formats this can name with confidence; the plain-text catch-all lives in
/// [`sniff_plain_text`] so a refinement can never widen an answer.
fn sniff_text_specific(prefix: &[u8]) -> Option<Sniffed> {
    // Only the head is examined, and only as UTF-8-ish text. `from_utf8_lossy` over a bounded
    // window keeps a binary file with an accidental "<svg" deep inside it from matching.
    let window = &prefix[..prefix.len().min(SNIFF_PREFIX)];
    let text = String::from_utf8_lossy(window);
    let head = text.trim_start();
    if head.is_empty() {
        return None;
    }
    let lower = head.to_ascii_lowercase();

    // WebVTT's signature is mandated by the spec as the literal first bytes.
    if lower.starts_with("webvtt") {
        return Some(Sniffed::known(
            "text/vtt",
            Some("vtt"),
            MediaClass::Subtitle,
        ));
    }
    if looks_like_subrip(head) {
        return Some(Sniffed::known(
            "application/x-subrip",
            Some("srt"),
            MediaClass::Subtitle,
        ));
    }

    // SVG may or may not carry an XML prolog, a doctype, or comments before the root element,
    // so the tag is searched for near the start rather than expected at offset zero.
    //
    // Truncated to a char boundary, not to byte 1024. `lower` came through `from_utf8_lossy`, which
    // turns every invalid sequence into a three-byte replacement character — so on binary input the
    // 1024th byte is very often the middle of one, and slicing there panicked. Reachable from any
    // upload of a file that is not text, which is most of them: the worker died with a panic message
    // where a sniff verdict belonged. Found by transferring a nine-megabyte binary.
    let cap = lower.len().min(1024);
    let cap = (0..=cap)
        .rev()
        .find(|&index| lower.is_char_boundary(index))
        .unwrap_or(0);
    let leading = &lower[..cap];
    if leading.contains("<svg") {
        return Some(Sniffed::known(
            "image/svg+xml",
            Some("svg"),
            // An SVG is an image, and `carries_active_content` is what constrains delivery.
            // Classing it as ActiveContent instead would send it down the wrong pipeline and
            // lose thumbnails for entire icon libraries.
            MediaClass::Image,
        ));
    }
    if leading.starts_with("<!doctype html") || leading.starts_with("<html") {
        return Some(Sniffed::known(
            "text/html",
            Some("html"),
            MediaClass::ActiveContent,
        ));
    }

    None
}

/// The plain-text catch-all, used only when nothing more specific matched.
///
/// Worth identifying rather than leaving opaque: text is indexable, and
/// `application/octet-stream` sends it nowhere. Requires the window to be valid UTF-8 with no
/// NULs and mostly printable — a binary run that happens to decode would otherwise be indexed
/// as prose.
fn sniff_plain_text(prefix: &[u8]) -> Option<Sniffed> {
    let window = &prefix[..prefix.len().min(SNIFF_PREFIX)];
    if window.is_empty() || window.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(window).ok()?;
    if !is_mostly_printable(text) {
        return None;
    }
    Some(Sniffed::known(
        "text/plain",
        Some("txt"),
        MediaClass::Document,
    ))
}

/// Whether the window reads as text rather than as bytes that happen to decode.
fn is_mostly_printable(text: &str) -> bool {
    let mut printable = 0usize;
    let mut total = 0usize;
    for c in text.chars().take(4096) {
        total += 1;
        if !c.is_control() || c == '\n' || c == '\r' || c == '\t' {
            printable += 1;
        }
    }
    total > 0 && printable * 100 / total >= 95
}

/// A SubRip cue block: an index line, then a `-->` timing line with comma decimals.
///
/// Both lines are required. "Starts with a digit" alone would match a CSV, a log, or half
/// the plain-text files in existence.
fn looks_like_subrip(head: &str) -> bool {
    let mut lines = head.lines();
    let Some(index) = lines.next() else {
        return false;
    };
    if index.trim().is_empty() || !index.trim().chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    lines
        .next()
        .is_some_and(|timing| timing.contains("-->") && timing.contains(','))
}

fn class_of(t: infer::Type) -> MediaClass {
    use infer::MatcherType;
    match t.matcher_type() {
        MatcherType::Image => MediaClass::Image,
        MatcherType::Video => MediaClass::Video,
        MatcherType::Audio => MediaClass::Audio,
        MatcherType::Doc | MatcherType::Book => MediaClass::Document,
        MatcherType::Font => MediaClass::Font,
        MatcherType::Archive => archive_or_document(t.mime_type()),
        // `App` covers executables and shared libraries — and also wasm and a few
        // installers. All of them are programs.
        MatcherType::App => MediaClass::Executable,
        MatcherType::Text => text_class(t.mime_type()),
        MatcherType::Custom => MediaClass::Unknown,
    }
}

/// PDF and EPUB arrive as `Archive` from some matchers but rasterise to pages.
///
/// OLE storage deliberately stays `Archive`: the same container holds legacy `.doc`/`.xls`
/// **and** `.msi` installers, and telling them apart needs the directory inside. Claiming
/// `Document` would point a renderer at an installer; claiming `Executable` would refuse the
/// customer's legacy Word files.
fn archive_or_document(mime: &str) -> MediaClass {
    match mime {
        "application/pdf" | "application/epub+zip" | "application/x-mobipocket-ebook" => {
            MediaClass::Document
        }
        _ => MediaClass::Archive,
    }
}

fn text_class(mime: &str) -> MediaClass {
    match mime {
        "text/html" | "application/xhtml+xml" => MediaClass::ActiveContent,
        "image/svg+xml" => MediaClass::Image,
        // A script is a program. Not a browser-delivery risk the way an SVG is, but a
        // malware-distribution one, and never a creative asset — so it is refused by default
        // and a tenant that genuinely needs to store one turns the refusal off.
        "text/x-shellscript" | "text/x-python" | "text/x-perl" | "text/x-php" => {
            MediaClass::Executable
        }
        _ => MediaClass::Document,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sniffing binary content must not panic, whatever the bytes are.
    ///
    /// It did. The SVG search truncated the lowercased head at byte 1024, and that head comes through
    /// `from_utf8_lossy`, which replaces every invalid sequence with a three-byte replacement character —
    /// so on binary input byte 1024 is very often inside one, and a `&str` slice there panics. Reachable
    /// from any upload of a non-text file, and in the worker it meant a dead process with a panic message
    /// where a verdict belonged.
    ///
    /// The loop walks offsets rather than picking one shape of input, because which byte lands on the
    /// boundary depends on the content: a single hand-picked blob would pass over a fix that was still
    /// wrong by one.
    #[test]
    fn sniffing_binary_content_never_panics_on_a_char_boundary() {
        for offset in 0..48usize {
            let mut bytes = vec![b'a'; offset];
            // Invalid UTF-8, so each becomes a three-byte replacement character and the byte offsets of
            // the boundaries shift with `offset`.
            bytes.extend(std::iter::repeat_n(0xF8u8, 2048));
            let got = sniff(&bytes, None, None);
            assert!(
                !got.mime.is_empty(),
                "offset {offset} produced no verdict at all"
            );
        }
    }

    #[test]
    fn a_declared_type_with_parameters_is_compared_on_the_type_alone() {
        // Browsers send `text/html; charset=utf-8`; comparing the whole string would report a
        // mismatch on every correctly-declared upload and make the signal useless.
        let html = b"<!DOCTYPE html><html></html>";
        let got = sniff(html, Some("text/html; charset=UTF-8"), None);
        assert!(
            got.declared_mismatch.is_none(),
            "got {:?}",
            got.declared_mismatch
        );
    }

    #[test]
    fn a_declared_type_is_compared_case_insensitively() {
        let got = sniff(b"%PDF-1.4\n", Some("APPLICATION/PDF"), None);
        assert!(got.declared_mismatch.is_none());
    }

    #[test]
    fn a_binary_file_mentioning_svg_far_from_the_start_is_not_an_svg() {
        // The bounded window is what makes this safe: an unbounded search would let any file
        // claim to be an SVG by embedding the string anywhere.
        let mut buf = vec![0x00u8; 4096];
        buf.extend_from_slice(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>");
        assert_eq!(sniff(&buf, None, None).mime, "application/octet-stream");
    }

    #[test]
    fn a_csv_starting_with_a_number_is_not_mistaken_for_a_subtitle() {
        let csv = b"1,2,3\n4,5,6\n";
        assert_ne!(
            sniff(csv, None, Some("data.csv")).mime,
            "application/x-subrip"
        );
    }

    #[test]
    fn a_class_renders_as_the_name_the_pipeline_branches_on() {
        assert_eq!(MediaClass::ActiveContent.to_string(), "active_content");
        assert_eq!(MediaClass::Image.to_string(), "image");
    }
}
