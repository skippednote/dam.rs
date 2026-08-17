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
    /// URL of the thumbnail. Absent while a newly-uploaded asset is still being processed.
    pub thumbnail_url: Option<String>,
    /// Model confidence in the asset's automatic tags, 0..1. `None` means nothing scored it — which
    /// is not the same as zero, and the UI renders it differently.
    pub tag_confidence: Option<f32>,
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
}
