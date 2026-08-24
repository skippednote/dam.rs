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
| [`web/static/brand/damrs-mark.png`](../../web/static/brand/damrs-mark.png)     | Public mark and documentation  |
| [`web/src/lib/assets/damrs-mark.png`](../../web/src/lib/assets/damrs-mark.png) | Bundled app mark and favicon   |
| [`signal-ledger-reference.png`](signal-ledger-reference.png)                   | Approved application direction |

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
