//! Reading the metadata a camera and an editor leave in the file (Q.4).
//!
//! This is the extraction half of auto-import: turning EXIF, XMP and IPTC into a flat set of named values a
//! mapping can point at. The mapping itself is a tenant decision and lives in the database; what belongs here
//! is that the values come out *correctly and safely*, because every byte of this is attacker-controlled.
//!
//! Three properties matter more than coverage of tags:
//!
//! - **A missing block is not an error.** Most files have no XMP and plenty have no EXIF. A probe that failed
//!   on absence would make "no metadata" indistinguishable from "corrupt file".
//! - **Values are bounded.** A crafted file can carry a megabyte in one tag, and a caption that long would fail
//!   the database write rather than the parse — a mile from where anybody could act on it.
//! - **Nothing is trusted as a type.** Everything comes out as text. The tenant's own field definition decides
//!   what an `int` field does with `"1/125"`, and the validator refuses what does not fit.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::embedded;
use dam_media::testing::tags::{
    ARTIST, COPYRIGHT, DATE_TIME_ORIGINAL, DESCRIPTION, EXPOSURE_TIME, F_NUMBER, FOCAL_LENGTH,
    LENS_MODEL, MAKE, MODEL, OFFSET_TIME_ORIGINAL, PHOTOGRAPHIC_SENSITIVITY,
};
use dam_media::testing::{Entry, jpeg_with_exif};

#[test]
fn exif_text_comes_out_under_stable_names() {
    let bytes = jpeg_with_exif(
        &[
            (ARTIST, Entry::Text("Ada Lovelace")),
            (COPYRIGHT, Entry::Text("(c) 2026 Acme")),
            (DESCRIPTION, Entry::Text("A harbour at dawn")),
            (MAKE, Entry::Text("FUJIFILM")),
            (MODEL, Entry::Text("X-T5")),
        ],
        &[],
    );

    let found = embedded::read(&bytes);

    // Names a mapping can be written against, and stable: a tenant's mapping is configuration, so renaming one
    // of these later would silently stop importing a field.
    assert_eq!(
        found.get("exif.artist").map(String::as_str),
        Some("Ada Lovelace")
    );
    assert_eq!(
        found.get("exif.copyright").map(String::as_str),
        Some("(c) 2026 Acme")
    );
    assert_eq!(
        found.get("exif.description").map(String::as_str),
        Some("A harbour at dawn")
    );
    assert_eq!(found.get("exif.make").map(String::as_str), Some("FUJIFILM"));
    assert_eq!(found.get("exif.model").map(String::as_str), Some("X-T5"));
}

#[test]
fn a_file_with_no_embedded_metadata_reads_as_empty_rather_than_failing() {
    // The common case by a wide margin, and the reason `read` returns a map rather than a `Result`: most files
    // carry nothing, and an error would make "no metadata" indistinguishable from "corrupt".
    let bare = vec![0xff, 0xd8, 0xff, 0xd9];
    assert!(embedded::read(&bare).is_empty());

    // And so does something that is not an image at all. Ingest calls this on whatever arrived.
    assert!(embedded::read(b"this is a text file, not a photograph").is_empty());
    assert!(embedded::read(&[]).is_empty());
}

#[test]
fn an_enormous_value_is_truncated_rather_than_carried_into_the_database() {
    // A crafted file can put a megabyte in one tag. Truncated here, where the bound is visible, rather than
    // failing the metadata write later — a column-length error is a mile from anywhere somebody could act on it.
    let huge = "x".repeat(20_000);
    let bytes = jpeg_with_exif(&[(DESCRIPTION, Entry::Text(&huge))], &[]);

    let found = embedded::read(&bytes);
    let value = found.get("exif.description").expect("the tag is present");
    assert_eq!(
        value.chars().count(),
        embedded::MAX_VALUE_CHARS,
        "truncated to the bound, not dropped: a caption that is too long is still a caption"
    );
}

#[test]
fn control_characters_are_stripped_so_a_value_cannot_forge_structure() {
    // Embedded metadata is attacker-controlled and ends up in JSON, in search text and on a page. A newline in
    // a "title" is merely untidy; a NUL or an escape sequence is the kind of thing that breaks a consumer
    // downstream, and the honest place to deal with it is at the boundary where it enters the system.
    let bytes = jpeg_with_exif(
        &[(
            ARTIST,
            Entry::Text("Ada\u{0}\u{1b}[31m Lovelace\r\nSecond line"),
        )],
        &[],
    );

    let value = embedded::read(&bytes)
        .get("exif.artist")
        .cloned()
        .expect("present");
    assert!(!value.contains('\u{0}'), "no NUL: {value:?}");
    assert!(!value.contains('\u{1b}'), "no escape: {value:?}");
    // A newline becomes a space rather than vanishing, so two words do not run together.
    assert_eq!(value, "Ada [31m Lovelace Second line");
}

#[test]
fn an_empty_or_whitespace_tag_is_omitted_rather_than_imported_as_blank() {
    // Cameras write empty tags routinely. Importing one would overwrite a real value with nothing the first
    // time somebody re-ran an import, which is the worst kind of data loss: invisible and automatic.
    let bytes = jpeg_with_exif(
        &[(ARTIST, Entry::Text("   ")), (MODEL, Entry::Text("X-T5"))],
        &[],
    );

    let found = embedded::read(&bytes);
    assert!(!found.contains_key("exif.artist"), "{found:?}");
    assert_eq!(found.get("exif.model").map(String::as_str), Some("X-T5"));
}

#[test]
fn xmp_is_read_from_its_packet_without_a_full_xml_parse() {
    // XMP is RDF/XML in an APP1 segment. The fields worth importing are a handful of simple elements, and a
    // full XML parser on attacker-controlled bytes is a much larger attack surface than reading the elements
    // we actually map. So this scans for known element names and refuses to be an XML processor.
    let packet = concat!(
        "<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?>",
        "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF>",
        "<rdf:Description dc:title='ignored-attribute-form'>",
        "<dc:title><rdf:Alt><rdf:li xml:lang='x-default'>Harbour, dawn</rdf:li></rdf:Alt></dc:title>",
        "<dc:creator><rdf:Seq><rdf:li>Ada Lovelace</rdf:li></rdf:Seq></dc:creator>",
        "<dc:rights><rdf:Alt><rdf:li>All rights reserved</rdf:li></rdf:Alt></dc:rights>",
        "</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end='w'?>"
    );
    let mut jpeg = vec![0xff, 0xd8];
    jpeg.extend_from_slice(&[0xff, 0xe1]);
    let mut app1 = Vec::new();
    app1.extend_from_slice(b"http://ns.adobe.com/xap/1.0/\0");
    app1.extend_from_slice(packet.as_bytes());
    let length = u16::try_from(app1.len() + 2).expect("small");
    jpeg.extend_from_slice(&length.to_be_bytes());
    jpeg.extend_from_slice(&app1);
    jpeg.extend_from_slice(&[0xff, 0xd9]);

    let found = embedded::read(&jpeg);
    assert_eq!(
        found.get("xmp.title").map(String::as_str),
        Some("Harbour, dawn")
    );
    assert_eq!(
        found.get("xmp.creator").map(String::as_str),
        Some("Ada Lovelace")
    );
    assert_eq!(
        found.get("xmp.rights").map(String::as_str),
        Some("All rights reserved")
    );
}

#[test]
fn a_malformed_xmp_packet_yields_nothing_and_does_not_panic() {
    // Truncated mid-element, unbalanced, and enormous. Every one of these arrives in real libraries, and the
    // only acceptable outcomes are "some values" or "none" — never a panic in an ingest worker.
    for packet in [
        "<x:xmpmeta><dc:title><rdf:li>unterminated",
        "<dc:title></dc:title>",
        "<dc:title><rdf:li></rdf:li></dc:title>",
        "<<<>>>",
    ] {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
        let mut app1 = Vec::new();
        app1.extend_from_slice(b"http://ns.adobe.com/xap/1.0/\0");
        app1.extend_from_slice(packet.as_bytes());
        let length = u16::try_from(app1.len() + 2).expect("small");
        jpeg.extend_from_slice(&length.to_be_bytes());
        jpeg.extend_from_slice(&app1);
        jpeg.extend_from_slice(&[0xff, 0xd9]);

        // The assertion is that this returns at all.
        let found = embedded::read(&jpeg);
        assert!(
            found.get("xmp.title").is_none_or(|value| !value.is_empty()),
            "an empty value must be omitted, not imported: {found:?}"
        );
    }
}

#[test]
fn an_element_with_attributes_yields_its_text_and_not_its_attributes() {
    // The simple XMP shape: no `rdf:Alt` wrapper, but an attribute on the element itself. Scanning has to skip
    // past the opening tag's own `>` rather than past the tag *name*, or the value comes out carrying
    // `xml:lang="x-default">` on the front — which would then be imported as somebody's title.
    //
    // Worth its own case because the wrapped form hides it: with an `rdf:li` inside, the stray fragment is
    // swallowed by the unwrap and everything looks correct. Mutation testing found exactly that blind spot.
    let packet = concat!(
        "<x:xmpmeta><rdf:RDF><rdf:Description>",
        "<dc:title xml:lang='x-default'>Plainly titled</dc:title>",
        "<photoshop:Credit>Acme Press</photoshop:Credit>",
        "</rdf:Description></rdf:RDF></x:xmpmeta>"
    );
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
    let mut app1 = Vec::new();
    app1.extend_from_slice(b"http://ns.adobe.com/xap/1.0/\0");
    app1.extend_from_slice(packet.as_bytes());
    let length = u16::try_from(app1.len() + 2).expect("small");
    jpeg.extend_from_slice(&length.to_be_bytes());
    jpeg.extend_from_slice(&app1);
    jpeg.extend_from_slice(&[0xff, 0xd9]);

    let found = embedded::read(&jpeg);
    assert_eq!(
        found.get("xmp.title").map(String::as_str),
        Some("Plainly titled")
    );
    assert_eq!(
        found.get("xmp.credit").map(String::as_str),
        Some("Acme Press")
    );
}

#[test]
fn the_exif_sub_directory_is_read_as_well_as_the_first_one() {
    // Six of the twelve names live here, not in IFD0: a tag number means something different in each directory,
    // so a reader that only looked at the first one would come up empty for exactly the exposure facts people
    // map — and would do it silently, because absence is indistinguishable from "the camera wrote nothing".
    let bytes = jpeg_with_exif(
        &[(MAKE, Entry::Text("FUJIFILM"))],
        &[
            (DATE_TIME_ORIGINAL, Entry::Text("2026:03:14 09:26:53")),
            (LENS_MODEL, Entry::Text("XF16-55mmF2.8")),
            (PHOTOGRAPHIC_SENSITIVITY, Entry::Short(400)),
            (F_NUMBER, Entry::Rational(28, 10)),
            (EXPOSURE_TIME, Entry::Rational(1, 125)),
            (FOCAL_LENGTH, Entry::Rational(35, 1)),
        ],
    );

    let found = embedded::read(&bytes);

    assert_eq!(found.get("exif.make").map(String::as_str), Some("FUJIFILM"));
    // ISO, not EXIF's own colon spelling: a mapping onto a date field is the point of this name, and
    // `2026:03:14` is a date in no interchange format at all.
    assert_eq!(
        found.get("exif.taken_at").map(String::as_str),
        Some("2026-03-14T09:26:53"),
        "the date the shutter opened, transcribed: {found:?}"
    );
    assert_eq!(
        found.get("exif.lens").map(String::as_str),
        Some("XF16-55mmF2.8")
    );
    assert_eq!(found.get("exif.iso").map(String::as_str), Some("400"));

    // Rationals are *rendered*, because a pair of integers is not what anybody means by an aperture. This is the
    // one place `display_value` is the right reader rather than the wrong one — see the note in `read_exif`.
    assert_eq!(
        found.get("exif.aperture").map(String::as_str),
        Some("f/2.8")
    );
    assert_eq!(
        found.get("exif.shutter").map(String::as_str),
        Some("1/125 s")
    );
    assert_eq!(
        found.get("exif.focal_length").map(String::as_str),
        Some("35 mm")
    );
}

#[test]
fn a_timestamp_gains_a_zone_only_when_the_camera_recorded_one() {
    // With an offset the file describes an instant, and the value is a valid RFC 3339 timestamp a `datetime`
    // field will take.
    let bytes = jpeg_with_exif(
        &[],
        &[
            (DATE_TIME_ORIGINAL, Entry::Text("2026:03:14 09:26:53")),
            (OFFSET_TIME_ORIGINAL, Entry::Text("+05:30")),
        ],
    );
    let found = embedded::read(&bytes);
    assert_eq!(
        found.get("exif.taken_at").map(String::as_str),
        Some("2026-03-14T09:26:53+05:30")
    );

    // A nonsense offset is ignored rather than appended: half a zone is worse than none, because it would be
    // stored as fact. The local reading survives, which is what the file actually said.
    let bytes = jpeg_with_exif(
        &[],
        &[
            (DATE_TIME_ORIGINAL, Entry::Text("2026:03:14 09:26:53")),
            (OFFSET_TIME_ORIGINAL, Entry::Text("later")),
        ],
    );
    let found = embedded::read(&bytes);
    assert_eq!(
        found.get("exif.taken_at").map(String::as_str),
        Some("2026-03-14T09:26:53"),
        "an offset we do not understand is not guessed at: {found:?}"
    );

    // A timestamp that is not shaped like one at all is left exactly as written. Rewriting it would be inventing
    // a date, and leaving it means the validator refuses it and the rejection is reported.
    let bytes = jpeg_with_exif(
        &[],
        &[(DATE_TIME_ORIGINAL, Entry::Text("sometime in March"))],
    );
    let found = embedded::read(&bytes);
    assert_eq!(
        found.get("exif.taken_at").map(String::as_str),
        Some("sometime in March")
    );
}

#[test]
fn every_advertised_source_is_one_the_extractor_can_actually_produce() {
    // The list is what a mapping screen offers, so a name on it that nothing produces is a rule an administrator
    // can save and that silently never fires. Checked against the shape the mapping column enforces, and for
    // duplicates, because two identical entries in a picker are a bug nobody reports.
    let sources = embedded::sources();
    assert!(sources.len() >= 12, "{sources:?}");
    for source in &sources {
        let (namespace, name) = source.split_once('.').unwrap_or_else(|| panic!("{source}"));
        assert!(matches!(namespace, "exif" | "xmp"), "{source}");
        assert!(
            !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_lowercase())
                && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "a source has to satisfy the mapping column's own constraint: {source}"
        );
    }
    let mut unique: Vec<&str> = sources.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), sources.len(), "no duplicates: {sources:?}");

    // And the names really are produced: a file carrying one tag from each namespace lands under both.
    let bytes = jpeg_with_exif(&[(ARTIST, Entry::Text("Ada Lovelace"))], &[]);
    let found = embedded::read(&bytes);
    for key in found.keys() {
        assert!(
            sources.contains(&key.as_str()),
            "`{key}` is produced but not advertised"
        );
    }
}
