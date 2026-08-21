//! Wire types.
//!
//! Separate from the domain types they are built from, because a wire type is a *contract* — renaming
//! a field breaks every generated client — while a domain type is free to change. Where the two
//! coincide (the state enums) the domain type is reused rather than mirrored, since a second
//! definition is the drift F.3 exists to prevent.

use dam_core::{AssetTier, ProvenanceState, RightsState};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// One asset, as the grid renders it.
///
/// Deliberately small. A 100k-row grid fetches pages of these, so every field here is multiplied by
/// the page size — and anything a cell does not draw belongs on the detail endpoint instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssetSummary {
    pub id: Uuid,
    /// The name the uploader gave it, preserved verbatim. The stored MIME is sniffed, not this.
    pub filename: String,
    /// Sniffed on ingest, never taken from the client (`assets.mime`).
    pub mime: String,
    pub bytes: i64,
    /// Display dimensions — orientation already applied, so a grid cell can lay out from these
    /// without knowing about EXIF. `None` for formats with no intrinsic size.
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// Where the original lives, derived server-side from storage class and restore state so the UI
    /// never reimplements that mapping.
    pub tier: AssetTier,
    /// Rights evaluation outcome. A *display* input: enforcement happens at the distribution
    /// chokepoint (D12), and this field is what dims a button rather than what protects an asset.
    pub rights_state: RightsState,
    pub provenance_state: ProvenanceState,
    /// URL of the thumbnail, as a signed internal-preview delivery URL.
    ///
    /// Absolute when the deployment configures `server.public_url`, root-relative otherwise — so a client on a
    /// different origin must resolve it against the API's base rather than its own. Absent when the asset has
    /// no thumbnail yet, which is the normal state between an upload finishing and the worker deriving it: a
    /// URL minted regardless would 404, and a grid cannot tell a broken link from work still in progress.
    pub thumbnail_url: Option<String>,
    /// Model confidence in the asset's automatic tags, 0..1. `None` means nothing scored it — which
    /// is not the same as zero, and the UI renders it differently.
    pub tag_confidence: Option<f32>,
    /// Whether *this caller* has favourited it, so a cell can draw a filled star (Q.5b·3).
    ///
    /// Only the two engagement facts a cell actually draws are here — see the note on this struct about every
    /// field being multiplied by the page size. The counts, the caller's own stars and the watch state are on the
    /// detail endpoint, because no grid cell shows them.
    pub is_favourite: bool,
    /// The asset's average rating, or `None` when nobody has rated it.
    ///
    /// The average rather than the caller's own rating: a grid shows what the library thinks, and `None` is not
    /// zero — "unrated" and "rated badly by everyone" must not draw the same.
    pub average_stars: Option<f64>,
    /// Whether it has paperwork attached (Q.9) — a release, a licence.
    ///
    /// A boolean rather than a count, because the question a cell answers is "is the rights picture documented",
    /// and *how many* documents there are is a detail for the panel. Scoped like everything else: paperwork the
    /// caller cannot see does not count, or the flag would disclose that something exists.
    pub has_attachment: bool,
}

/// One page of results.
///
/// `total` is separate from `items.len()` and load-bearing for accessibility: a virtualised grid must
/// report the real row count in `aria-rowcount`, or a screen reader announces "20 items" for a library
/// of a hundred thousand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssetPage {
    pub items: Vec<AssetSummary>,
    /// Total matching assets, not the number returned.
    pub total: i64,
    /// Zero-based index of the first item in `items` within the full result set.
    pub offset: i64,
    /// Whether `items` are in relevance order.
    ///
    /// False when the query contained a clause the index cannot answer — category or collection membership,
    /// which are joins — so it was answered in SQL and ordered by recency instead. Reported rather than left
    /// implicit because a grid that says "ranked by relevance" over a `created_at` ordering is lying about the
    /// one thing a reader might act on; and because the *count* means something different too, being exact
    /// rather than capped by the ranking overfetch.
    #[serde(default = "ranked_by_default")]
    pub ranked: bool,
    /// A query worth trying instead, when this one matched nothing (Q.17).
    ///
    /// Only ever set on an empty result set, and only when a value close to one the caller typed really exists
    /// in their visible library — so it is an offer to run something that will work, not a guess. Absent
    /// otherwise, including when the search simply has no matches: "no results" with no suggestion is an
    /// honest answer, and inventing one would send somebody round a second empty loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub did_you_mean: Option<String>,
}

/// Ranked unless a response says otherwise: every path but the relational one ranks, and a client reading an
/// older response should assume the behaviour that has always held.
fn ranked_by_default() -> bool {
    true
}
