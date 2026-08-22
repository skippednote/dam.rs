//! The master proxy — the §2 invariant, in code.
//!
//! §2 is the load-bearing idea of the whole system: only the original master tiers to cold storage,
//! and the proxy is what makes that safe. It is "a deliberately generous derivative (2048px JPEG /
//! 720p H.264 / extracted text) good enough to serve every future preview *and* to re-run every
//! future AI model. When the tagging model is upgraded, we re-embed the entire library off proxies
//! and issue **zero restores**."
//!
//! ## Why this module contains a type and not just a function
//!
//! The failure mode is silent. Nothing breaks on the day some stage starts reading originals — the
//! bill arrives at the next model upgrade, as a restore storm across the entire archive.
//! `enrichment_runs.used_original` is the alarm, and the schema comment says as much: "worth an
//! alert, not just a column."
//!
//! But an alarm depends on somebody having set it, and a doc comment saying "read the proxy" is a
//! convention — conventions decay one commit at a time, and this one decays invisibly. So the rule
//! is a type: an enrichment stage takes an [`EnrichmentSource`], and the only constructor that does
//! not raise the alarm is the one that refuses anything but a proxy key. Reading the original
//! remains *possible* — C2PA verification at ingest legitimately needs the bytes it is attesting to
//! — but it is not *convenient*, and it records itself with a reason.

use crate::{derive, probe};
use bytes::Bytes;
use dam_store::Key;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A caller tried to build an enrichment source from something other than the proxy.
    #[error(
        "{key} is not a master proxy; enrichment reads the proxy so a cold library stays re-processable (§2)"
    )]
    NotAProxy { key: String },

    #[error("reading the original requires a reason, so the alarm can be triaged")]
    ReasonRequired,

    #[error("refusing an empty proxy: it would read as \"this asset has no content\" forever")]
    Empty,

    #[error(transparent)]
    Derive(#[from] derive::Error),

    #[error(transparent)]
    Probe(#[from] probe::Error),
}

type Result<T> = std::result::Result<T, Error>;

/// Long edge of an image proxy, in pixels.
///
/// §2's number. It is generous on purpose: at 512px an image embedding loses the fine detail a
/// future model would want, and the entire promise is that the library can be re-embedded from
/// proxies alone. Storage is the cheap side of that trade — the hot footprint scales with asset
/// *count*, not asset size.
pub const PROXY_LONG_EDGE: u32 = 2048;

/// JPEG quality for an image proxy.
///
/// Measured against a photo-like 4000x3000 master (3.1 MB as JPEG), which is the upper end of what
/// a photo library commonly holds:
///
/// | quality | proxy size |
/// |---|---|
/// | 75 | 424 KB |
/// | **82** | **561 KB** |
/// | 88 | 766 KB |
/// | 92 | 999 KB |
///
/// §2 budgets "roughly 0.5 MB per asset" for the *whole* hot set — proxy, embeddings, extracted
/// text and thumbnails together. 88 costs 766 KB for the proxy alone, so it does not fit; 82 costs
/// 561 KB at the 12-megapixel end and less for the many assets that are smaller (nothing is
/// upscaled), which averages inside the budget.
///
/// Quality below 82 would also be defensible for the *model* half of the proxy's job — embeddings
/// downsample to 224–448px, far below where JPEG artifacts matter — but not for the *preview* half,
/// where a designer zooms in. 82 is the point that serves both.
pub const PROXY_QUALITY: u8 = 82;

/// Guarded at compile time so a future "optimisation" has to argue with the build rather than with
/// a test somebody can mark `#[ignore]`. These two numbers are the difference between re-embedding
/// a cold library for free and thawing it.
const _: () = assert!(
    PROXY_LONG_EDGE >= 2048 && PROXY_QUALITY >= 80,
    "the proxy must stay good enough to serve every future preview and re-run every future model"
);

/// A built proxy, ready to store at [`Key::proxy`].
#[derive(Debug, Clone)]
pub struct BuiltProxy {
    pub bytes: Bytes,
    /// Extension for the proxy key — `jpg` for images, `txt` for extracted text.
    pub extension: &'static str,
}

/// Bytes an enrichment stage is allowed to read.
///
/// Construct it with [`EnrichmentSource::from_proxy`]. The alternative constructor exists, sets
/// [`EnrichmentSource::used_original`], and demands a reason.
#[derive(Debug, Clone)]
pub struct EnrichmentSource {
    key: Key,
    bytes: Bytes,
    used_original: bool,
    original_read_reason: Option<String>,
}

impl EnrichmentSource {
    /// The normal path. Refuses any key that is not in the proxy namespace.
    ///
    /// "Not the original" would be the wrong check: a thumbnail is hot and cheap too, but it is
    /// 400px, and re-running an embedding model against it would quietly degrade every vector in
    /// the library.
    pub fn from_proxy(key: &Key, bytes: Bytes) -> Result<Self> {
        if !is_proxy(key) {
            return Err(Error::NotAProxy {
                key: key.as_str().to_owned(),
            });
        }
        Ok(Self {
            key: key.clone(),
            bytes,
            used_original: false,
            original_read_reason: None,
        })
    }

    /// The escape hatch: read the original, and say why.
    ///
    /// Legitimate at ingest — C2PA verification attests to the master's bytes, and the master is
    /// still hot at that point. Illegitimate in any stage that runs again later, because that is
    /// what turns a model upgrade into a library thaw.
    pub fn from_original_with_reason(key: &Key, bytes: Bytes, reason: &str) -> Result<Self> {
        if reason.trim().is_empty() {
            // A flag with no reason cannot be triaged, so the alarm would be noise and get muted.
            return Err(Error::ReasonRequired);
        }
        Ok(Self {
            key: key.clone(),
            bytes,
            used_original: true,
            original_read_reason: Some(reason.trim().to_owned()),
        })
    }

    pub fn key(&self) -> &Key {
        &self.key
    }

    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// What goes into `enrichment_runs.used_original`.
    pub fn used_original(&self) -> bool {
        self.used_original
    }

    /// What goes into `enrichment_runs.original_read_reason`.
    pub fn original_read_reason(&self) -> Option<&str> {
        self.original_read_reason.as_deref()
    }
}

/// Whether a key is in the master-proxy namespace.
fn is_proxy(key: &Key) -> bool {
    key.as_str()
        .split_once('/')
        .is_some_and(|(_, rest)| rest.starts_with("p/"))
}

/// Builds the image proxy: a 2048px JPEG, upright.
///
/// Upright matters more here than in any other derivative. The proxy is what every preview and every
/// future model sees, so a sideways proxy makes the whole library sideways downstream — and fixing
/// it later means reading originals, which is the restore storm §2 exists to avoid.
pub fn build_image(bytes: &[u8]) -> Result<BuiltProxy> {
    let rendered = derive::render(
        bytes,
        &derive::Rendition {
            width: PROXY_LONG_EDGE,
            height: PROXY_LONG_EDGE,
            format: derive::OutputFormat::Jpeg,
            quality: PROXY_QUALITY,
            // Contain, never cover: a proxy must not crop. Cropping would discard image content
            // that a future model — or a human looking at a preview — would have wanted, and the
            // original may be in Deep Archive by then.
            fit: derive::Fit::Contain,
            background: [255, 255, 255],
        },
    )?;
    Ok(BuiltProxy {
        bytes: Bytes::from(rendered),
        extension: "jpg",
    })
}

/// Builds the text proxy: the extracted text, verbatim.
///
/// Stored rather than re-derived because re-extracting means reading the original, and by then the
/// original may be an hours-long restore away.
pub fn build_text(text: &str) -> Result<BuiltProxy> {
    if text.trim().is_empty() {
        return Err(Error::Empty);
    }
    Ok(BuiltProxy {
        bytes: Bytes::from(text.to_owned()),
        extension: "txt",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    const HASH: &str = "9f2a1b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8";

    #[test]
    fn the_proxy_namespace_check_matches_the_key_layout() {
        let proxy = Key::proxy(Uuid::nil(), HASH, "jpg").expect("key");
        assert!(is_proxy(&proxy));
        assert!(!is_proxy(&Key::original(Uuid::nil(), HASH).expect("key")));
        // A key with no tenant segment cannot be a proxy — and must not panic.
        assert!(!is_proxy(&Key::new("p").expect("key")));
    }
}
