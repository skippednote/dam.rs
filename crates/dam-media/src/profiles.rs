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
//! ## Why this set is in code, and where the other one lives
//!
//! These three are the profiles the *system* needs — a grid thumbnail, a lightbox preview, a web proxy. They
//! are in code because the code depends on them by name: no tenant can delete `thumb-256` and leave the grid
//! without cells.
//!
//! The formats a tenant *offers a person downloading* are a different set, and they live in the `conversions`
//! table (Q.11): named, described, ordered, permission-gated, editable. They share this module's cache
//! argument and its hash function — see [`tenant_op_hash`] — because the failure it prevents is the same one.
//!
//! ## The renderer revision applies to both
//!
//! [`Profile::revision`] handles one profile being redefined. A change to the *renderer* — a different
//! resampling filter, a sharpening pass — leaves every field of every profile identical, so no per-profile
//! revision can catch it. [`RENDERER_REVISION`] is folded into every hash for that: bumping it once
//! invalidates every cached derivative, built-in and tenant alike, which is the only safe direction. A tenant
//! row cannot be hand-bumped in the commit that changes the pipeline, so without this there would be nothing
//! to bump.

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

/// Bumped by hand when the *renderer* changes in a way no profile field describes.
///
/// A different resampling filter, a sharpening pass, a vips upgrade that changes output bytes: every field of
/// every profile stays identical, so every cached derivative would keep being served. Folded into every hash —
/// built-in and tenant — because a tenant's `conversions` row cannot be hand-bumped in the commit that changes
/// the pipeline, and a mechanism that only covers the profiles somebody remembered is the mechanism that fails.
///
/// Bumping this invalidates every derivative in every tenant. That is the intent: re-rendering is work, and
/// serving a stale rendition is invisible.
pub const RENDERER_REVISION: u32 = 1;

impl Profile {
    /// The cache key for this profile.
    ///
    /// Includes both revisions, so neither a definition change nor a renderer change can be served from cache.
    pub fn op_hash(&self) -> String {
        self.op_hash_at(RENDERER_REVISION)
    }

    /// The same key for a named renderer revision.
    ///
    /// Exists to be testable. [`RENDERER_REVISION`] is a constant, so no test can vary it — and a test that
    /// re-derives the expected string by hand proves only that two hand-derived strings differ, which is
    /// exactly what mutation testing showed: dropping the term from the real function left such a test passing.
    /// Taking the revision as an argument makes "bumping it changes the key" a property rather than a
    /// restatement, and leaves the public method a single line that cannot lose the term without failing.
    pub fn op_hash_at(&self, renderer_revision: u32) -> String {
        // Folded in by appending to the intent rather than by changing `op_hash`'s signature: `op_hash` is
        // shared with the ingest path, and the length-prefixing it already does makes appending unambiguous.
        let intent = format!("{}#r{}#p{}", self.intent, self.revision, renderer_revision);
        op_hash(&self.rendition, self.color_profile, &intent)
    }
}

/// The colour treatment every tenant conversion is rendered with.
///
/// Fixed, not configurable, and the reason is that `derive::render` does not apply either value — `op_hash`
/// takes them (§18.1) and the renderer ignores them. A tenant-settable colour profile would change the cache
/// key while the output stayed identical, which is a way to serve the same bytes under two names and call one
/// of them a CMYK conversion. When the renderer honours them, these become columns.
pub const TENANT_COLOR_PROFILE: &str = "srgb";
/// See [`TENANT_COLOR_PROFILE`].
pub const TENANT_INTENT: &str = "perceptual";

/// The cache key for a tenant-defined conversion (Q.11).
///
/// Same function as a built-in's, and that is the point: one cache, one key derivation, so a tenant conversion
/// whose recipe happens to match `web-2048` shares its rendered bytes rather than duplicating them.
///
/// There is no per-row revision term. Every field a tenant can edit is already an input here, so a
/// redefinition *is* a different key — see the `conversions` migration on why a revision column would be a
/// second mechanism for what this one already guarantees.
pub fn tenant_op_hash(rendition: &Rendition) -> String {
    tenant_op_hash_at(rendition, RENDERER_REVISION)
}

/// The same key for a named renderer revision. See [`Profile::op_hash_at`] on why this argument exists.
pub fn tenant_op_hash_at(rendition: &Rendition, renderer_revision: u32) -> String {
    let intent = format!("{TENANT_INTENT}#p{renderer_revision}");
    op_hash(rendition, TENANT_COLOR_PROFILE, &intent)
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
    fn bumping_the_renderer_revision_changes_every_hash() {
        // The property the constant exists for: a renderer change leaves every profile field identical, so
        // without this term bumping it would invalidate nothing and every cached derivative would keep being
        // served — silently, which is the failure mode that matters.
        for profile in ALL {
            assert_ne!(
                profile.op_hash_at(1),
                profile.op_hash_at(2),
                "{} ignores the renderer revision",
                profile.name
            );
        }
        assert_ne!(
            tenant_op_hash_at(&WEB_2048.rendition, 1),
            tenant_op_hash_at(&WEB_2048.rendition, 2),
            "a tenant conversion ignores the renderer revision"
        );
    }

    #[test]
    fn the_public_hash_uses_the_current_renderer_revision() {
        // The wiring, which is the half a property test cannot reach: the derivation above could be perfect
        // while the public entry point passed nothing. Mutation testing found exactly that gap.
        assert_eq!(WEB_2048.op_hash(), WEB_2048.op_hash_at(RENDERER_REVISION));
        assert_eq!(
            tenant_op_hash(&WEB_2048.rendition),
            tenant_op_hash_at(&WEB_2048.rendition, RENDERER_REVISION)
        );
    }

    #[test]
    fn a_tenant_conversion_shares_the_cache_with_an_identical_built_in() {
        // One cache, one derivation. A tenant conversion whose recipe matches `web-2048` should share its
        // bytes rather than render a second identical object — which is only true while the colour treatment
        // matches too, hence the assertion on that rather than on the recipe alone.
        assert_eq!(WEB_2048.color_profile, TENANT_COLOR_PROFILE);
        assert_eq!(WEB_2048.intent, TENANT_INTENT);

        // Not equal, because `web-2048` carries a per-profile revision a tenant row has no field for. Asserted
        // rather than glossed: the sharing is real for the *recipe*, and the revision term is what separates
        // them. If a future change drops built-in revisions, these become one key.
        assert_ne!(WEB_2048.op_hash(), tenant_op_hash(&WEB_2048.rendition));
    }

    #[test]
    fn every_recipe_field_is_in_a_tenant_conversions_key() {
        // The whole reason there is no revision column: a redefinition has to be a different key, and it is
        // only a different key if every editable field is an input.
        let base = tenant_op_hash(&WEB_2048.rendition);
        for (label, mutate) in [
            (
                "width",
                (|r: &mut Rendition| r.width = 1024) as fn(&mut Rendition),
            ),
            ("height", |r: &mut Rendition| r.height = 1024),
            ("format", |r: &mut Rendition| r.format = OutputFormat::Png),
            ("quality", |r: &mut Rendition| r.quality = 60),
            ("fit", |r: &mut Rendition| r.fit = Fit::Cover),
            ("background", |r: &mut Rendition| r.background = [0, 0, 0]),
        ] {
            let mut changed = WEB_2048.rendition;
            mutate(&mut changed);
            assert_ne!(
                tenant_op_hash(&changed),
                base,
                "{label} is not in the key, so redefining it would serve stale bytes"
            );
        }
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
