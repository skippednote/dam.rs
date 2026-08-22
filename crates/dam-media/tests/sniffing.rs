//! Magic-byte sniffing (task 1.6): never trust a client `Content-Type`.
//!
//! `assets.mime` is documented in the schema as "sniffed, never client-supplied". The
//! reason is not tidiness: a browser will happily render whatever the stored MIME says, so
//! a client that declares `image/png` for an HTML file is describing a stored XSS payload,
//! and a client that declares `image/jpeg` for a Mach-O binary is describing a malware
//! distribution endpoint on the customer's own domain.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::sniff::{self, MediaClass, SNIFF_PREFIX};

/// A minimal but genuine header for each format — real magic bytes, not placeholders.
mod samples {
    pub const JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1,
    ];
    pub const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    pub const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00";
    pub const PDF: &[u8] = b"%PDF-1.7\n1 0 obj\n<< >>\n";
    pub const MACH_O: &[u8] = &[0xCF, 0xFA, 0xED, 0xFE, 0x0C, 0, 0, 1, 0, 0, 0, 0];
    pub const ELF: &[u8] = &[0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0];
    pub const TIFF: &[u8] = &[b'I', b'I', 0x2A, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0];
    /// A minimal zip local file header — what a docx looks like before the content
    /// inspection that tells the two apart.
    pub const ZIP: &[u8] = &[
        0x50, 0x4B, 0x03, 0x04, 0x14, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0,
    ];
    pub const SVG: &[u8] = br#"<?xml version="1.0"?>
<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
  <script>alert(document.domain)</script>
</svg>"#;
    pub const SVG_NO_PROLOG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#;
    pub const HTML: &[u8] = b"<!DOCTYPE html>\n<html><body><script>x()</script></body></html>";
    pub const WEBVTT: &[u8] = b"WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nHello\n";
    pub const SRT: &[u8] = b"1\n00:00:01,000 --> 00:00:04,000\nHello\n\n";
}

#[test]
fn a_jpeg_named_png_is_stored_as_a_jpeg() {
    let got = sniff::sniff(samples::JPEG, Some("image/png"), Some("logo.png"));
    assert_eq!(got.mime, "image/jpeg", "the bytes decide, not the name");
    assert_eq!(got.ext.as_deref(), Some("jpg"));
    assert_eq!(got.class, MediaClass::Image);
    assert_eq!(
        got.declared_mismatch.as_deref(),
        Some("image/png"),
        "the disagreement must be reported so it can be logged, never silently dropped"
    );
}

#[test]
fn a_matching_declaration_produces_no_mismatch() {
    let got = sniff::sniff(samples::PNG, Some("image/png"), Some("logo.png"));
    assert_eq!(got.mime, "image/png");
    assert!(got.declared_mismatch.is_none());
}

#[test]
fn a_filename_never_overrides_the_bytes() {
    for (bytes, expected) in [
        (samples::PDF, "application/pdf"),
        (samples::GIF, "image/gif"),
        (samples::TIFF, "image/tiff"),
    ] {
        let got = sniff::sniff(bytes, Some("image/jpeg"), Some("photo.jpg"));
        assert_eq!(got.mime, expected, "sniffed type must win over .jpg");
    }
}

#[test]
fn an_executable_is_classified_as_one_however_it_is_named() {
    for bytes in [samples::MACH_O, samples::ELF] {
        let got = sniff::sniff(bytes, Some("image/jpeg"), Some("holiday.jpg"));
        assert_eq!(
            got.class,
            MediaClass::Executable,
            "an executable stored as an image is a malware distribution endpoint on the \
             customer's own domain"
        );
        assert!(
            got.is_dangerous(),
            "and must be refusable by the upload path"
        );
        assert!(!got.is_processable());
    }
}

#[test]
fn svg_is_detected_even_though_it_has_no_magic_bytes() {
    // SVG is XML text, so `infer` cannot see it. Left undetected it would fall through to
    // application/octet-stream and lose its previewability — or worse, be trusted from the
    // client's declaration.
    for bytes in [samples::SVG, samples::SVG_NO_PROLOG] {
        let got = sniff::sniff(bytes, None, Some("icon.svg"));
        assert_eq!(got.mime, "image/svg+xml");
        assert_eq!(got.class, MediaClass::Image);
        assert!(
            got.carries_active_content(),
            "an SVG can contain <script>, so it must never be served inline unsanitised"
        );
    }
}

#[test]
fn html_is_detected_and_flagged_as_active_content() {
    // HTML5 creatives are legitimate DAM assets, so this is not a refusal — but serving one
    // inline from the asset domain hands an attacker the DAM's origin.
    let got = sniff::sniff(samples::HTML, Some("image/png"), Some("banner.png"));
    assert_eq!(got.mime, "text/html");
    assert!(got.carries_active_content());
    assert!(
        !got.is_dangerous(),
        "active content is a delivery constraint, not a refusal — an HTML5 banner is a \
         real asset"
    );
}

#[test]
fn subtitle_formats_are_recognised_from_their_content() {
    let vtt = sniff::sniff(samples::WEBVTT, None, Some("captions.txt"));
    assert_eq!(vtt.mime, "text/vtt");
    assert_eq!(vtt.class, MediaClass::Subtitle);

    let srt = sniff::sniff(samples::SRT, None, Some("captions.dat"));
    assert_eq!(srt.mime, "application/x-subrip");
    assert_eq!(srt.class, MediaClass::Subtitle);
}

#[test]
fn a_zip_is_reported_as_a_zip_and_not_guessed_into_an_office_format() {
    // docx, xlsx and pptx are all zips. Claiming one from the container alone would put the
    // wrong renderer on the file; the distinction needs the archive's content list, which
    // this layer does not read.
    let got = sniff::sniff(samples::ZIP, Some("application/vnd.ms-word"), None);
    assert_eq!(got.mime, "application/zip");
    assert_eq!(got.class, MediaClass::Archive);
}

#[test]
fn an_unrecognised_type_is_accepted_as_opaque_bytes() {
    // A DAM is a store: an unknown format is kept, just not processed. Refusing would lose
    // the customer's file over our inability to preview it.
    let got = sniff::sniff(
        &[0x00, 0x01, 0x02, 0x03, 0xAA, 0xBB],
        None,
        Some("thing.xyz"),
    );
    assert_eq!(got.mime, "application/octet-stream");
    assert_eq!(got.class, MediaClass::Unknown);
    assert!(!got.is_dangerous());
    assert!(!got.is_processable());
}

#[test]
fn an_empty_or_truncated_header_does_not_panic() {
    for bytes in [&[][..], &[0xFF][..], &samples::PNG[..3]] {
        let got = sniff::sniff(bytes, Some("image/png"), Some("x.png"));
        assert_eq!(
            got.mime, "application/octet-stream",
            "a partial header must not be guessed at from the client's declaration"
        );
    }
}

#[test]
fn sniffing_the_first_chunk_gives_the_same_answer_as_the_whole_file() {
    // This is what lets a streaming upload be sniffed from its first chunk instead of being
    // buffered whole — the §18.3 requirement.
    let mut whole = samples::JPEG.to_vec();
    whole.extend(std::iter::repeat_n(0x42u8, 5 * 1024 * 1024));

    let from_prefix = sniff::sniff(&whole[..SNIFF_PREFIX.min(whole.len())], None, None);
    let from_whole = sniff::sniff(&whole, None, None);
    assert_eq!(from_prefix.mime, from_whole.mime);
    assert_eq!(from_prefix.class, from_whole.class);
}

#[test]
fn a_shell_script_is_treated_as_a_program_not_as_a_document() {
    // `infer` reports text/x-shellscript with a Text matcher, so a naive text mapping files it
    // as a document. It is not a browser-delivery risk the way an SVG is, but it is a
    // malware-distribution one and is never a creative asset.
    let got = sniff::sniff(
        b"#!/bin/sh\nrm -rf /\n",
        Some("text/plain"),
        Some("notes.txt"),
    );
    assert_eq!(got.class, MediaClass::Executable);
    assert!(got.is_dangerous());
}

#[test]
fn an_ole_container_stays_an_archive_because_doc_and_msi_are_indistinguishable_here() {
    // The same container holds legacy .doc/.xls and .msi installers, and telling them apart
    // needs the directory inside. Document would point a renderer at an installer;
    // Executable would refuse the customer's legacy Word files.
    let ole = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0, 0, 0];
    let got = sniff::sniff(&ole, None, Some("report.doc"));
    assert_eq!(got.class, MediaClass::Archive);
    assert!(!got.is_dangerous(), "must not refuse legacy Office files");
}

#[test]
fn plain_text_is_identified_rather_than_left_opaque() {
    // `infer` returns nothing for plain text. Leaving it as octet-stream would make an
    // indexable text asset unsearchable.
    let got = sniff::sniff(
        b"Shot list for the spring campaign.\nLocation: Lisbon.\n",
        None,
        None,
    );
    assert_eq!(got.mime, "text/plain");
    assert_eq!(got.class, MediaClass::Document);
}

#[test]
fn binary_bytes_that_happen_to_decode_as_utf8_are_not_called_text() {
    // Without a printable-ratio check, a run of control bytes decodes cleanly as UTF-8 and
    // would be indexed as prose.
    let mut buf = vec![0x01u8; 64];
    buf.extend_from_slice(&[0x02, 0x03, 0x04]);
    assert_eq!(
        sniff::sniff(&buf, None, None).mime,
        "application/octet-stream"
    );
}

#[test]
fn a_generic_xml_file_stays_xml_when_it_is_not_an_svg() {
    let got = sniff::sniff(
        br#"<?xml version="1.0"?><catalog><item id="1"/></catalog>"#,
        None,
        Some("catalog.xml"),
    );
    assert_eq!(got.mime, "text/xml");
    assert_eq!(got.class, MediaClass::Document);
    assert!(!got.carries_active_content());
}

#[test]
fn a_declared_type_is_never_used_as_a_fallback_when_sniffing_fails() {
    // The whole point. If an undetected type fell back to the declaration, every check
    // above could be bypassed by sending bytes we do not recognise.
    let got = sniff::sniff(&[0xDE, 0xAD, 0xBE, 0xEF], Some("image/png"), Some("a.png"));
    assert_eq!(got.mime, "application/octet-stream");
    assert_eq!(
        got.declared_mismatch.as_deref(),
        Some("image/png"),
        "the declaration is recorded as a mismatch, not adopted"
    );
}
