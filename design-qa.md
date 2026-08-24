# Design QA: Signal Ledger

Final result: passed

## Evidence

- Selected direction: [`docs/brand/signal-ledger-reference.png`](docs/brand/signal-ledger-reference.png)
  (`1487 × 1058`, RGB).
- Live implementation: `/tmp/damrs-brand-identity-assets-implementation.jpg` (`2504 × 1363`, browser
  capture).
- Combined comparison: `/tmp/damrs-brand-identity-design-comparison.jpg`. Both captures are normalized to a
  `760px` comparison height without altering either aspect ratio.
- State: authenticated local API, 183 visible assets, dark theme, first asset selected, inspector open,
  rights and storage badges visible, download choices visible.
- Full-view evidence covers the navigation rail, facet rail, search/action bar, grid, selection state,
  bulk-action bar, and detail inspector. The detail inspector is the focused region evidence because it
  includes the densest typography, state, image, form, and action hierarchy in the view.

The connected Chrome viewport was larger than the generated concept viewport. The responsive implementation
therefore uses the same four-column/product-inspector composition at a wider working size; the comparison
preserves aspect ratios rather than claiming pixel equivalence between unlike captures.

The two browser-derived images remain local QA evidence instead of repository assets because the connected
library contains private media and identifiers. The generated design direction is safe and versioned.

## Comparison history

| Pass | Severity | Finding | Resolution | Evidence |
| --- | --- | --- | --- | --- |
| 1 | P1 | The first live capture followed the host light preference, which obscured the selected dark, image-review direction. | Added an accessible persisted theme control and selected dark mode for the approved presentation. | Navigation exposes `Use light theme`; final capture uses the dark token set. |
| 1 | P1 | The initial overview had no selected asset, so the most important trust and download surface was absent. | Exercised grid selection and captured the persistent inspector with real data. | Final capture includes selection outline, rights/provenance/tier badges, preview, use context, and download profiles. |
| 1 | P2 | The document title still used the code identifier `damrs`. | Changed the user-facing title to `dam.rs`; retained `damrs` only as the technical identifier. | Browser title is `dam.rs`; the lockup and brand guide use the dotted name. |
| 2 | P2 | The concept's horizontal filter chips did not cover the product's existing nested taxonomy and counted facets. | Preserved the real facet rail, then matched the concept's hierarchy through low-contrast dividers, compact labels, and a dark media canvas. | Live facet rail shows taxonomy, orientation, colour, brand, status, and people counts without hiding product capability. |

## Final review

- **Typography:** compact system sans for navigation and metadata; monospace remains limited to identifiers,
  hashes, and the connected-key label. No visible clipping or broken wrapping in the captured state.
- **Spacing and layout:** grouped navigation, toolbar, grid, and inspector remain visually distinct without
  stacking generic card containers. The wide capture keeps the grid readable and the inspector persistent.
- **Colour:** near-black blue surfaces and cobalt selection/actions match the chosen direction. Rights
  unknown remains violet and labeled; semantic colours are not reused as decoration.
- **Images:** real library thumbnails and the selected original are used. Image wells retain their checker
  treatment for transparency and dark-image boundaries; no placeholder illustration or CSS-drawn asset is
  substituted.
- **Icons:** Phosphor regular icons are used consistently for navigation, actions, and state labels. The
  generated logo is a real transparent PNG asset, not CSS or inline SVG artwork.
- **States and interactions:** search updates the URL, a new search clears stale selection, the inspector
  opens from grid selection, the navigation collapses and expands, and theme choice persists.
- **Accessibility:** controls keep visible text or accessible names; status icons retain labels; the existing
  skip link, landmarks, grid semantics, focus behavior, and complete light/dark token sets are preserved.
- **Responsive resilience:** the rail has an explicit collapsed layout below desktop width and can also be
  collapsed manually. Existing grid overflow and inspector behavior remain intact for dense operator work.

No P0 or unresolved P1 visual or interaction findings remain in the reviewed state.
