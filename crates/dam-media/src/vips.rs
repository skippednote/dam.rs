//! libvips, out of process.
//!
//! The formats the pure-Rust path cannot read — camera RAW, PSD, PDF, HEIF, SVG, whole-slide images —
//! all arrive through here. §18.2 puts them behind libvips and §16 puts libvips behind a subprocess,
//! and the second decision is not defensive habit:
//!
//! **libvips marks 14 of its own loaders "untrusted"**, and they are precisely the ones a DAM needs:
//! `dcrawload`, `magickload`, `pdfload`, `svgload`, `openslideload`. Its maintainers are saying that
//! these decoders should not be pointed at hostile input without containment. Every file damrs
//! processes was chosen by whoever uploaded it, so [`dam_media::sandbox`](crate::sandbox) *is* that
//! containment: rlimits, a wall clock, an escape-proof working directory, and no inherited environment.
//!
//! ## The binary is resolved by absolute path, once
//!
//! The sandbox clears the environment and sets `PATH` to the system directories, so a vips installed by
//! mise under `~/.local/share/mise` is not on the child's `PATH` at all. [`Toolchain::discover`] resolves
//! the binaries *before* that happens and hands absolute paths to the sandbox — which is also the right
//! posture for a tool being pointed at untrusted bytes: no `PATH` ambiguity about which decoder ran.

use crate::sandbox::{Limits, Outcome, Sandbox};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("libvips is not available: {0}")]
    Unavailable(String),

    /// vips ran and refused the input. Carries vips's own stderr, because "probe failed" sends someone
    /// to read our code when the answer is in the tool's diagnosis.
    #[error("vips could not read {path}: {detail}")]
    Rejected { path: String, detail: String },

    #[error("vips exceeded its limits reading {path}: {outcome}")]
    Bounded { path: String, outcome: String },

    #[error(transparent)]
    Sandbox(#[from] crate::sandbox::Error),
}

type Result<T> = std::result::Result<T, Error>;

/// Absolute paths to the vips binaries.
#[derive(Debug, Clone)]
pub struct Toolchain {
    vips: PathBuf,
    vipsheader: PathBuf,
}

impl Toolchain {
    /// Locates the binaries.
    ///
    /// `DAMRS_VIPS_BIN` overrides the search, so a deployment can pin an exact build without relying on
    /// whatever `PATH` happens to say — and so a container image can place it wherever it likes.
    pub fn discover() -> Result<Self> {
        if let Ok(dir) = std::env::var("DAMRS_VIPS_BIN") {
            return Self::from_dir(Path::new(&dir));
        }
        // `which` semantics, done here rather than by the shell: the child's PATH is cleared, so the
        // lookup has to happen in the parent while the real PATH is still visible.
        let vips = which("vips").ok_or_else(|| {
            Error::Unavailable(
                "no `vips` on PATH and DAMRS_VIPS_BIN is unset; it is pinned in .mise.toml, so \
                 `mise install` should provide it"
                    .to_owned(),
            )
        })?;
        let dir = vips.parent().unwrap_or(Path::new("."));
        Self::from_dir(dir)
    }

    fn from_dir(dir: &Path) -> Result<Self> {
        let vips = dir.join("vips");
        let vipsheader = dir.join("vipsheader");
        for binary in [&vips, &vipsheader] {
            if !binary.exists() {
                return Err(Error::Unavailable(format!(
                    "{} does not exist",
                    binary.display()
                )));
            }
        }
        Ok(Self {
            // Canonicalised so the path handed to a subprocess cannot be redirected by a symlink
            // swapped in between discovery and use.
            vips: vips.canonicalize().unwrap_or(vips),
            vipsheader: vipsheader.canonicalize().unwrap_or(vipsheader),
        })
    }

    pub fn vips(&self) -> &Path {
        &self.vips
    }

    pub fn vipsheader(&self) -> &Path {
        &self.vipsheader
    }
}

/// Minimal `which`, for the parent process only.
fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// What vips reports about a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VipsProbe {
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    /// `srgb`, `cmyk`, `b-w`, `rgb16` — vips's own interpretation, which is more precise than a MIME
    /// type and is what the ICC path (D11) branches on.
    pub interpretation: Option<String>,
    /// Which loader handled the file. The honest answer to "what is this really", and the value that
    /// says whether an untrusted loader was involved.
    pub loader: Option<String>,
    /// Pages, for paged formats only. `None` for a single image rather than `Some(1)`: a JPEG is not a
    /// one-page document, and storing 1 for every photograph would make documents unfilterable.
    pub page_count: Option<usize>,
    /// Whether the file carries an embedded ICC profile (D11).
    pub has_icc_profile: bool,
}

/// Probes a file with the default limits.
pub async fn probe(tools: &Toolchain, path: &Path) -> Result<VipsProbe> {
    probe_with_limits(tools, path, Limits::default()).await
}

/// Probes a file, bounded by `limits`.
///
/// There is deliberately no path that skips the sandbox. A malformed file that makes a decoder spin or
/// allocate is the reason §16 exists, and an unbounded convenience wrapper is how that protection gets
/// bypassed at 4pm on a Friday.
pub async fn probe_with_limits(
    tools: &Toolchain,
    path: &Path,
    limits: Limits,
) -> Result<VipsProbe> {
    let sandbox = Sandbox::new(limits)?;
    // `-a` prints every header field as `key: value`. The file is passed by absolute path, and the
    // sandbox's working directory is its own temp dir, so vips has nowhere to write even if asked.
    let outcome = sandbox
        .run(
            &tools.vipsheader.to_string_lossy(),
            &["-a", &path.to_string_lossy()],
        )
        .await?;

    match &outcome {
        Outcome::Ok { stdout, .. } => parse_header(&String::from_utf8_lossy(stdout)),
        Outcome::Failed { stderr, .. } => Err(Error::Rejected {
            path: path.display().to_string(),
            detail: String::from_utf8_lossy(stderr).trim().to_owned(),
        }),
        Outcome::Killed { .. } | Outcome::TimedOut { .. } => Err(Error::Bounded {
            path: path.display().to_string(),
            outcome: format!("{outcome:?}"),
        }),
    }
}

/// Parses `vipsheader -a` output.
fn parse_header(text: &str) -> Result<VipsProbe> {
    let mut fields = std::collections::BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }

    let number = |key: &str| fields.get(key).and_then(|v| v.parse::<u32>().ok());
    let (Some(width), Some(height)) = (number("width"), number("height")) else {
        return Err(Error::Rejected {
            path: fields.get("filename").cloned().unwrap_or_default(),
            detail: format!("vipsheader reported no dimensions; output was:\n{text}"),
        });
    };

    // A single image has no `n-pages` field at all, and a paged format with one page reports 1. Both
    // become `None`, because "one page" is not a meaningful distinction from "not paged" — what a
    // caller wants to know is whether there is more than one.
    let page_count = fields
        .get("n-pages")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|pages| *pages > 1);

    Ok(VipsProbe {
        width,
        height,
        bands: number("bands").unwrap_or(0),
        interpretation: fields.get("interpretation").cloned(),
        loader: fields.get("vips-loader").cloned(),
        page_count,
        // vips exposes the profile as a binary blob field; its presence is the answer D11 needs, and
        // extracting it is the delivery path's job.
        has_icc_profile: fields.contains_key("icc-profile-data"),
    })
}

/// Every loader this vips build provides, e.g. `pngload`, `dcrawload`.
///
/// Read from the running binary rather than assumed, because a loader that quietly disappears between
/// builds turns "we support RAW" into a surprise on a customer's upload.
pub async fn loaders(tools: &Toolchain) -> Result<BTreeSet<String>> {
    Ok(list_operations(tools)
        .await?
        .into_iter()
        .map(|(name, _)| name)
        .collect())
}

/// The loaders vips itself marks `untrusted`.
///
/// Surfaced so the reason this module runs in a sandbox stays checkable rather than becoming folklore.
pub async fn untrusted_loaders(tools: &Toolchain) -> Result<BTreeSet<String>> {
    Ok(list_operations(tools)
        .await?
        .into_iter()
        .filter(|(_, untrusted)| *untrusted)
        .map(|(name, _)| name)
        .collect())
}

/// `(loader name, is untrusted)` for every loader in `vips -l`.
async fn list_operations(tools: &Toolchain) -> Result<Vec<(String, bool)>> {
    let sandbox = Sandbox::new(Limits::default())?;
    let outcome = sandbox.run(&tools.vips.to_string_lossy(), &["-l"]).await?;
    let stdout = match &outcome {
        Outcome::Ok { stdout, .. } => String::from_utf8_lossy(stdout).to_string(),
        other => {
            return Err(Error::Unavailable(format!(
                "`vips -l` did not succeed: {other:?}"
            )));
        }
    };

    // Lines look like:
    //   VipsForeignLoadPdfFile (pdfload), load PDF..., priority=0, untrusted, ...
    // The nickname in parentheses is what a caller would use, and `untrusted` is a flag on the same
    // line — so both come from one parse rather than from two lists that could disagree.
    let mut found = Vec::new();
    for line in stdout.lines() {
        let Some(open) = line.find('(') else { continue };
        let Some(close) = line[open..].find(')') else {
            continue;
        };
        let nickname = &line[open + 1..open + close];
        if nickname.ends_with("load") {
            found.push((nickname.to_owned(), line.contains("untrusted")));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_with_no_page_field_reports_no_page_count() {
        let probed =
            parse_header("width: 10\nheight: 20\nbands: 3\nvips-loader: pngload\n").expect("parse");
        assert_eq!(probed.page_count, None);
        assert_eq!((probed.width, probed.height), (10, 20));
    }

    #[test]
    fn a_single_page_document_also_reports_no_page_count() {
        // `Some(1)` would be technically true and practically useless: it makes every photograph look
        // like a one-page document and "has more than one page" unanswerable.
        let probed = parse_header("width: 200\nheight: 100\nn-pages: 1\nvips-loader: pdfload\n")
            .expect("parse");
        assert_eq!(probed.page_count, None);
    }

    #[test]
    fn a_multi_page_document_reports_its_count() {
        let probed = parse_header("width: 200\nheight: 100\nn-pages: 7\nvips-loader: pdfload\n")
            .expect("parse");
        assert_eq!(probed.page_count, Some(7));
    }

    #[test]
    fn output_without_dimensions_is_an_error_carrying_what_was_seen() {
        // vipsheader can exit zero having printed something unexpected. Failing with the output
        // attached is what makes that diagnosable at all.
        let err = parse_header("something: else\n").expect_err("must fail");
        assert!(format!("{err}").contains("something: else"), "got {err}");
    }

    #[test]
    fn an_icc_profile_is_detected_from_the_header_field() {
        let with =
            parse_header("width: 1\nheight: 1\nicc-profile-data: 3144 bytes of binary data\n")
                .expect("parse");
        assert!(with.has_icc_profile);
        let without = parse_header("width: 1\nheight: 1\n").expect("parse");
        assert!(!without.has_icc_profile);
    }

    #[test]
    fn a_colon_in_a_value_does_not_break_the_parse() {
        // Filenames contain colons, and `split_once` rather than `split` is what keeps the rest of the
        // value intact.
        let probed =
            parse_header("width: 1\nheight: 1\nfilename: /tmp/a:b/c.png\n").expect("parse");
        assert_eq!((probed.width, probed.height), (1, 1));
    }
}
