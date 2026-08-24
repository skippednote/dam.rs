# dam.rs brand guide

dam.rs should feel like accountable media infrastructure: calm enough for careful visual work, precise
enough for rights and provenance decisions, and direct enough for operators working through thousands of
assets.

## Brand foundation

| Element         | Standard                                                    |
| --------------- | ----------------------------------------------------------- |
| Product name    | `dam.rs`                                                    |
| Code identifier | `damrs`                                                     |
| Descriptor      | Rights-aware digital asset management.                      |
| Tagline         | Find it. Trust it. Use it.                                  |
| Promise         | Every asset remains findable, explainable, and safe to use. |

Write the public name in lowercase with the dot: **dam.rs**. Use `damrs` only where punctuation is not
available, such as crate names, environment variables, service names, and database identifiers.

## Naming decision

Keep **dam.rs**. It is compact, memorable, honest about the product category, and naturally tied to the
Rust implementation. A different invented name would spend recognition without clarifying the product.
The dot is part of the public identity; `damrs` remains the dependable technical handle.

Before a public launch, complete a domain, package, and trademark clearance. That is a release check, not a
reason to dilute the current identity while the product is still being built.

## Logo

The mark combines an image crop frame with a provenance endpoint. It should read as a lowercase `d` at
small sizes without becoming a literal camera, folder, shield, database, Rust crab, or water dam.

| Asset                                                                          | Use                            |
| ------------------------------------------------------------------------------ | ------------------------------ |
| [`damrs-mark.svg`](damrs-mark.svg)                                             | Public mark and documentation  |
| [`web/src/lib/assets/damrs-mark.svg`](../../web/src/lib/assets/damrs-mark.svg) | Bundled app mark               |
| [`web/src/lib/assets/favicon.svg`](../../web/src/lib/assets/favicon.svg)       | Tab icon, square framing       |
| [`signal-ledger-reference.jpg`](signal-ledger-reference.jpg)                   | Approved application direction |

Give the mark clear space equal to the width of its vertical stem. Do not recolour it with semantic rights
colours, add effects, place it inside another shape, or pair it with `DAMRS` in uppercase.

## Colour

The product palette begins with the existing accessible OKLCH tokens in
[`web/src/routes/layout.css`](../../web/src/routes/layout.css). These are functional tokens, not decorative
swatches.

- **Ink:** near-black blue, the default image-review surround.
- **Surface:** one measured tonal step above ink.
- **Raised:** controls and hovered rows only.
- **Brand accent:** cobalt blue; used for selection, primary action, focus, and the dot in `dam.rs`.
- **Rights colours:** green, amber, red, and violet remain semantic. Never use them as brand decoration.

The dark theme is the primary brand presentation because a bright surround changes perceived image tone.
The app initially respects the system preference, then keeps the user's explicit theme choice. The complete
light theme remains required for embedded and user-selected contexts.

## Typography

Use the native UI sans stack for fast, platform-appropriate rendering. Use the native monospace stack only
for hashes, object keys, API keys, queries, and values compared character by character. Product headings are
compact and confident; metadata remains at readable operator density.

## Icons

The interface uses Phosphor's regular-weight family. Icons support labels rather than replace them in
expanded navigation. Semantic states always carry a label and never rely on colour alone.

## Voice

Write as the system already behaves:

- Say what happened, what did not happen, and what the operator can do next.
- Prefer “Rights unknown” to a reassuring but unsupported state.
- Prefer “Preview processing” to an unexplained blank tile.
- Call AI-assisted work assisted; do not make “AI-powered” the product promise.
- Avoid revolutionary, magic, seamless, and other claims that cannot be verified.

## Product principles

1. **Findable:** the library should answer where an asset is and why it matched.
2. **Explainable:** rights, provenance, storage, and automation expose their evidence.
3. **Fail-closed:** uncertainty never becomes permission.
4. **Reversible:** risky operational actions are staged, visible, and auditable.
5. **Operator-first:** dense workflows stay fast without becoming cryptic.

## The mark as shipped

`web/src/lib/assets/damrs-mark.svg` — the rail's lockup, and `favicon.svg`, the same paths in a square frame
because a favicon is rendered into a square and letterboxing a portrait mark leaves it smaller than the space
allows.

`docs/brand/damrs-mark.svg` is the same file again, and the duplication is deliberate this time: the README and
this guide are read on the forge, where a path into `web/src` reads as reaching into the application to borrow
an asset. It is 358 bytes. The one that mattered was the 161KB PNG that existed twice.

Both app copies are **traced from the 793×1069 PNG they replace**, which spent 161KB of raster on four flat
single-colour shapes and was drawn at 32×28. Every number came off that file's own pixel runs rather than
from eyeballing it: stroke 60 for the brackets and the pin, 51 for the ring, and each cap and corner position
derived from where the ink starts and stops. Verified by overlaying the rendered SVG on the original in
difference blend, which comes out black to within antialiasing.

Coordinates are kept in the source image's own space, so the mark's viewBox is its ink bounding box and any
future measurement against the original still lines up. `favicon.svg` reuses those coordinates and only moves
the frame.

The files carry no comments, deliberately. Vite inlines anything under 4KB as a data URI, so a rationale in
the asset is a rationale URL-encoded into every client's bundle — which is why it is here instead.

Before the trace, the application's tab icon was the **Svelte logo**: the scaffold shipped it and nothing had
replaced it, so the framework's mark had been the product's icon in every customer's browser.
