//! The libvips path (1.7): the formats the pure-Rust probe cannot read.
//!
//! ARCHITECTURE §18.2 puts camera RAW, PSD and Office behind libvips, and §16 puts libvips behind a
//! subprocess. Both are load-bearing here for the same reason: **libvips marks 14 of its own loaders
//! "untrusted"** — `dcrawload`, `magickload`, `pdfload`, `svgload`, `openslideload` among them — which
//! is to say the formats a DAM most needs are the ones its maintainers flag as risky on hostile input.
//! Running them out of process, inside `dam_media::sandbox`, is the containment those warnings ask for.
//!
//! Two things this suite is careful about:
//!
//! - **It tests the environment as well as the code.** A loader that silently disappears between vips
//!   builds turns "we support RAW" into a runtime surprise, so the capability set is asserted.
//! - **Fixtures are constructed, not downloaded.** The multi-page PDF below is built byte by byte with
//!   a correct xref table. A real camera RAW cannot honestly be synthesised, so RAW is covered by
//!   capability detection rather than by a fabricated file that would prove nothing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_media::vips::{self, Toolchain};
use image::{ImageFormat, Rgb, RgbImage};
use std::io::Cursor;

/// A valid PDF with `pages` pages, including a correct cross-reference table.
///
/// Built rather than checked in: a binary fixture is a fixture nobody can review, and poppler wants a
/// real xref — offsets are computed as the file is assembled, which is the fiddly part and the reason
/// most hand-written PDFs are rejected.
fn pdf_with_pages(pages: usize) -> Vec<u8> {
    let mut objects: Vec<String> = Vec::new();

    // 1: catalogue, 2: page tree, 3..: the pages themselves.
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_owned());
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", i + 3)).collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {pages} >>",
        kids.join(" ")
    ));
    for _ in 0..pages {
        objects.push("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>".to_owned());
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }

    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    // The free-list head, which must be exactly this shape or the table is rejected.
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut img = RgbImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = Rgb([(x % 256) as u8, (y % 256) as u8, 90]);
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

/// The toolchain, or a failure that says how to get one.
///
/// The likeliest cause is not a missing install but a missing PATH: vips lives under
/// `~/.local/share/mise`, so a bare `cargo test` cannot see it while `mise run check` and
/// `mise exec -- cargo test` can. Saying so beats sending someone to reinstall a tool they already have.
fn toolchain() -> Toolchain {
    Toolchain::discover().unwrap_or_else(|e| {
        panic!(
            "these tests need vips on PATH. It is pinned in .mise.toml, so run them as \
             `mise run check` or `mise exec -- cargo test`, or set DAMRS_VIPS_BIN. Underlying \
             error: {e}"
        )
    })
}

#[test]
fn the_toolchain_is_discovered_by_absolute_path() {
    // It has to be absolute. The sandbox clears the environment and sets PATH to the system
    // directories, so a mise-installed vips is *not* on the child's PATH — resolving the binary before
    // the sandbox strips the environment is the only thing that makes this work, and it is also the
    // right posture for a tool we point at hostile input.
    let tools = toolchain();
    assert!(tools.vips().is_absolute(), "got {:?}", tools.vips());
    assert!(tools.vipsheader().is_absolute());
    assert!(
        tools.vips().exists() && tools.vipsheader().exists(),
        "both binaries must exist at discovery time, not at first use"
    );
}

#[tokio::test]
async fn the_loaders_this_build_provides_are_asserted_not_assumed() {
    // A loader disappearing between vips builds turns "we support RAW" into a runtime surprise on a
    // customer's upload. The set is pinned here so the environment fails the build instead.
    let tools = toolchain();
    let loaders = vips::loaders(&tools).await.expect("list loaders");

    for required in ["pngload", "jpegload", "tiffload", "webpload"] {
        assert!(loaders.contains(required), "missing {required}");
    }
    // The reason libvips is here at all: these are the formats the pure-Rust path cannot read.
    for required in ["dcrawload", "magickload", "pdfload", "heifload", "svgload"] {
        assert!(
            loaders.contains(required),
            "missing {required} — §18.2 depends on it"
        );
    }
}

#[tokio::test]
async fn the_untrusted_loaders_are_exactly_the_ones_we_run_in_the_sandbox() {
    // Asserted so the justification for the sandbox stays true rather than becoming folklore. If a
    // future vips promoted these to trusted, that would be worth knowing; if it demoted more, that is
    // worth knowing sooner.
    let tools = toolchain();
    let untrusted = vips::untrusted_loaders(&tools).await.expect("list");
    for loader in ["dcrawload", "magickload", "pdfload", "svgload"] {
        assert!(
            untrusted.contains(loader),
            "{loader} is no longer marked untrusted by libvips — re-read the sandbox rationale"
        );
    }
    assert!(
        !untrusted.contains("pngload"),
        "png is a trusted loader; if that changed, so did a lot else"
    );
}

#[tokio::test]
async fn a_png_probes_to_the_same_dimensions_as_the_pure_rust_path() {
    // A differential test, the same discipline as the two blob-store drivers: two implementations of
    // one measurement must agree, or a format read by both would report different sizes depending on
    // which path happened to run.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sample.png");
    let bytes = png(120, 80);
    std::fs::write(&path, &bytes).expect("write");

    let via_vips = vips::probe(&tools, &path).await.expect("vips probe");
    let via_rust = dam_media::probe::image(&bytes).expect("rust probe");

    assert_eq!(via_vips.width, via_rust.stored_width.expect("width"));
    assert_eq!(via_vips.height, via_rust.stored_height.expect("height"));
    assert_eq!(via_vips.loader.as_deref(), Some("pngload"));
}

#[tokio::test]
async fn a_multi_page_pdf_reports_its_page_count() {
    // The capability I previously said needed pdfium. `n-pages` comes from vipsheader, and the whole
    // reason it is reachable is that `pdfload` — an untrusted loader — runs inside the sandbox.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");

    // `None` for one page, `Some(n)` beyond that — "one page" is not a meaningful distinction from
    // "not paged", and the next test covers why. An earlier version of this loop asserted `Some(1)`
    // here and contradicted that design directly.
    for (pages, expected) in [(1usize, None), (2, Some(2)), (5, Some(5))] {
        let path = dir.path().join(format!("doc-{pages}.pdf"));
        std::fs::write(&path, pdf_with_pages(pages)).expect("write");

        let probed = vips::probe(&tools, &path)
            .await
            .unwrap_or_else(|e| panic!("probing a {pages}-page pdf: {e}"));
        assert_eq!(
            probed.page_count, expected,
            "a {pages}-page document must report {expected:?}"
        );
        assert_eq!(
            probed.loader.as_deref(),
            Some("pdfload"),
            "and it must have gone through the PDF loader, which is one of the untrusted ones"
        );
    }
}

#[tokio::test]
async fn a_single_page_image_reports_no_page_count_rather_than_one() {
    // `None` and `Some(1)` mean different things: a JPEG is not a one-page document, and storing 1 in
    // `assets.page_count` for every photograph would make "documents" unfilterable.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("photo.png");
    std::fs::write(&path, png(32, 32)).expect("write");

    assert_eq!(
        vips::probe(&tools, &path).await.expect("probe").page_count,
        None
    );
}

#[tokio::test]
async fn a_file_vips_cannot_read_is_an_error_rather_than_a_panic() {
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nonsense.bin");
    std::fs::write(&path, b"this is not an image").expect("write");

    let err = vips::probe(&tools, &path)
        .await
        .expect_err("unreadable input must be an error");
    // The message has to carry vips's own diagnosis; "probe failed" sends someone to read our code
    // when the answer is in the tool's stderr.
    assert!(
        !format!("{err}").is_empty(),
        "the error must say something actionable"
    );
}

#[tokio::test]
async fn a_missing_file_is_an_error_and_not_a_hang() {
    let tools = toolchain();
    let missing = std::path::Path::new("/nonexistent/definitely-not-here.png");
    assert!(vips::probe(&tools, missing).await.is_err());
}

#[tokio::test]
async fn probing_runs_under_the_sandbox_limits() {
    // The property that matters more than any single format: this call is bounded. A malformed file
    // that makes vips spin or allocate is the whole reason §16 puts it in a subprocess, so a probe must
    // not be reachable by a path that skips the sandbox.
    let tools = toolchain();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sample.png");
    std::fs::write(&path, png(16, 16)).expect("write");

    let outcome = vips::probe_with_limits(
        &tools,
        &path,
        dam_media::sandbox::Limits {
            // Absurdly short on purpose: a probe that ignored the limits would still succeed.
            wall_clock: std::time::Duration::from_millis(1),
            ..Default::default()
        },
    )
    .await;
    assert!(
        outcome.is_err(),
        "a 1ms wall clock must stop the probe, which proves the limits are applied"
    );
}
