//! Named delivery profiles (3.2).
//!
//! `derivatives.profile` is the name a caller asks for — `web-2048`, `thumb-256`. This is where a name
//! becomes a [`Rendition`], and the reason that indirection matters is the cache.
//!
//! ## The cache is keyed on the recipe, not on the name
//!
//! `derivatives_op_idx` is `UNIQUE (asset_id, op_hash)`, and `op_hash` covers the size, format, quality,
//! fit, background, colour profile and rendering intent (§18.1). So if `web-2048` is redefined — a
//! different quality, a different intent — every asset's existing `web-2048` derivative has a *different*
//! `op_hash` from the one the new definition produces, and the next request renders fresh.
//!
//! Looking a derivative up by **name** instead would serve the old bytes forever, with no error and no way
//! to tell. That is the failure this module exists to prevent, and it is why [`Profile::revision`] exists:
//! a definition change must move the hash even when the fields it changed are ones `op_hash` cannot see.
//!
//! ## Why the set is in code
//!
//! There is no profiles table. `derivatives.profile` is free text, so the schema does not constrain the
//! set — and a tenant-defined profile set is a real requirement that needs a table of its own, with the
//! usual questions about who may edit one and what happens to derivatives already rendered under the old
//! definition. That is worth doing deliberately rather than as a side effect of 3.2, so the built-in set is
//! here and the gap is recorded in TASKS.md.

use crate::derive::{Fit, OutputFormat, Rendition, op_hash};

/// A named profile: a rendition plus the colour treatment that `op_hash` needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    pub rendition: Rendition,
    /// The target colour profile, as `op_hash` sees it (§18.1, D11).
    pub color_profile: &'static str,
    /// The rendering intent.
    pub intent: &'static str,
    /// Bumped by hand when a definition changes in a way `op_hash` cannot observe.
    ///
    /// `op_hash` covers the fields it is given. If a future change alters the *pipeline* — a different
    /// resampling filter, a sharpening pass — the fields stay identical and every cached derivative would
    /// keep being served. Folding a revision into the hash makes such a change a cache miss, which is the
    /// only safe default: serving a stale rendition is invisible, and re-rendering is merely work.
    pub revision: u32,
    /// The `derivatives.role` this profile writes.
    ///
    /// Drives lifecycle: §6.4 never tiers `proxy`, `thumbnail` or `preview`, because the 128 KB minimum
    /// billable size makes tiering a 20 KB thumbnail cost more than leaving it in Standard.
    pub role: &'static str,
}

impl Profile {
    /// The cache key for this profile.
    ///
    /// Includes the revision, so a definition change cannot be served from cache.
    pub fn op_hash(&self) -> String {
        // The revision is folded in by appending it to the intent rather than by changing `op_hash`'s
        // signature: `op_hash` is shared with the ingest path, and the length-prefixing it already does
        // makes appending unambiguous.
        let intent = format!("{}#r{}", self.intent, self.revision);
        op_hash(&self.rendition, self.color_profile, &intent)
    }
}

/// A thumbnail for a grid cell. Square, cropped, small.
pub const THUMB_256: Profile = Profile {
    name: "thumb-256",
    rendition: Rendition {
        width: 256,
        height: 256,
        format: OutputFormat::WebP,
        quality: 80,
        // `Cover`, because a grid of ragged rectangles is what a fixed cell size exists to avoid.
        fit: Fit::Cover,
        background: [255, 255, 255],
    },
    color_profile: "srgb",
    intent: "perceptual",
    revision: 1,
    role: "thumbnail",
};

/// A lightbox preview. Fits inside the box, so nothing is cropped out of an image somebody is inspecting.
pub const PREVIEW_1024: Profile = Profile {
    name: "preview-1024",
    rendition: Rendition {
        width: 1024,
        height: 1024,
        format: OutputFormat::WebP,
        quality: 82,
        fit: Fit::Contain,
        background: [255, 255, 255],
    },
    color_profile: "srgb",
    intent: "perceptual",
    revision: 1,
    role: "preview",
};

/// The general-purpose web delivery size.
pub const WEB_2048: Profile = Profile {
    name: "web-2048",
    rendition: Rendition {
        width: 2048,
        height: 2048,
        format: OutputFormat::Jpeg,
        // 82 rather than higher: measured against §2's 0.5 MB budget for a 12 MP photograph, where 88 came
        // back at 766 KB.
        quality: 82,
        fit: Fit::Contain,
        background: [255, 255, 255],
    },
    color_profile: "srgb",
    intent: "perceptual",
    revision: 1,
    role: "proxy",
};

/// Every built-in profile.
pub const ALL: &[Profile] = &[THUMB_256, PREVIEW_1024, WEB_2048];

/// The name reserved for the untransformed original.
///
/// Not a profile: it has no rendition and is served from the content-addressed key. Named here so the
/// delivery path and the callers agree on the spelling rather than each writing the literal.
pub const ORIGINAL: &str = "original";

/// Looks a profile up by name.
///
/// `None` for an unknown name, which callers refuse. Rendering something plausible instead would let a
/// typo'd profile silently deliver a different size than the caller integrated against.
pub fn by_name(name: &str) -> Option<&'static Profile> {
    ALL.iter().find(|profile| profile.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_distinct_op_hash() {
        // Two profiles sharing a hash would make them one derivative, and whichever rendered first would be
        // served for both — a thumbnail delivered where a 2048px proxy was asked for.
        let mut hashes: Vec<String> = ALL.iter().map(Profile::op_hash).collect();
        hashes.sort();
        let before = hashes.len();
        hashes.dedup();
        assert_eq!(before, hashes.len(), "profiles must not share an op_hash");
    }

    #[test]
    fn bumping_the_revision_changes_the_op_hash() {
        // The property the whole module exists for. Without it, redefining a profile in a way `op_hash`
        // cannot see leaves every cached derivative being served forever, with no error anywhere.
        let mut bumped = WEB_2048;
        bumped.revision += 1;
        assert_ne!(WEB_2048.op_hash(), bumped.op_hash());
    }

    #[test]
    fn changing_any_visible_field_changes_the_op_hash() {
        let base = WEB_2048.op_hash();

        let mut smaller = WEB_2048;
        smaller.rendition.width = 1024;
        assert_ne!(smaller.op_hash(), base, "size must be in the key");

        let mut lossier = WEB_2048;
        lossier.rendition.quality = 60;
        assert_ne!(lossier.op_hash(), base, "quality must be in the key");

        let mut cropped = WEB_2048;
        cropped.rendition.fit = Fit::Cover;
        assert_ne!(cropped.op_hash(), base, "fit must be in the key");

        let mut cmyk = WEB_2048;
        cmyk.color_profile = "cmyk";
        assert_ne!(
            cmyk.op_hash(),
            base,
            "the colour profile must be in the key (§18.1)"
        );

        let mut relative = WEB_2048;
        relative.intent = "relative";
        assert_ne!(
            relative.op_hash(),
            base,
            "the rendering intent must be in the key"
        );
    }

    #[test]
    fn an_unknown_profile_is_not_found_rather_than_approximated() {
        assert!(by_name("web-4096").is_none());
        assert!(by_name(ORIGINAL).is_none(), "the original is not a profile");
        assert_eq!(by_name("web-2048").map(|p| p.name), Some("web-2048"));
    }

    #[test]
    fn the_roles_match_what_the_lifecycle_engine_never_tiers() {
        // §6.4: proxy, thumbnail and preview stay hot, because the 128 KB minimum billable size makes
        // tiering a small derivative cost more than leaving it in Standard. A profile that wrote some other
        // role would quietly become tierable.
        for profile in ALL {
            assert!(
                matches!(profile.role, "proxy" | "thumbnail" | "preview"),
                "{} writes role {:?}, which §6.4 would let tier",
                profile.name,
                profile.role
            );
        }
    }
}
