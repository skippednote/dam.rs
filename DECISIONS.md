# Decision log

Design decisions live in `ARCHITECTURE.md` §1 (D1–D17). This file records
decisions taken **during implementation**, especially ones taken autonomously so
they can be reviewed rather than discovered.

Format: date · decision · why · reversible?

---

## 2026-08-17 — settled by owner

| Decision | Value |
|---|---|
| Storage driver | S3 / S3-compatible only to start (D1) |
| Tenancy | Schema per tenant (D2) |
| Drupal | 11+ only, no Drupal 10 compat |
| Frontend | Svelte 5 + Tailwind + shadcn-svelte, bits-ui directly where no wrapper exists (D9) |
| Toolchain | mise |
| Local S3 | Garage (Deuxfleurs) |
| Testing | Docker + testcontainers, TDD (D17) |
| Repo / licence | `skippednote/damrs`, Apache-2.0 |
| `.sqlx/` offline data | Committed |
| Branch | `m0-foundation`, no pushes |
| Errors | `thiserror` per crate, `anyhow` in binaries only |
| Blocked-behaviour | Log here, take the reversible option, continue — except rights / consent / provenance / access control, which stop for review |

## 2026-08-17 — Garage cannot test the tiering state machine

Garage implements the S3 data plane (multipart, presign, SigV4, path-style) but
**not** storage classes, `RestoreObject`, object versioning, or object lock — the
four features ARCHITECTURE §6 is built on.

Two drivers behind one `BlobStore` trait: `S3Store` against Garage for wire
protocol, `FakeS3Store` with a controllable clock for the tiering state machine.
Both run the same conformance suite so the fake cannot drift. Real Glacier
semantics are covered by a nightly credential-gated CI run against AWS.

The fake is the better tool here regardless of Garage — a twelve-hour Deep Archive
restore is untestable in a unit test, and LocalStack's Glacier emulation is neither
fast nor faithful. Object lock has no local coverage at all; that is a known hole.

Reversible: yes, but the trait boundary is what makes it cheap, so keep it.

## 2026-08-17 — Rust 1.94 available, edition 2024 confirmed

Installed toolchain is rustc 1.94.0; edition 2024 needs ≥ 1.85. `.mise.toml` pins
1.94.0 rather than a range so the container and the laptop agree.

Node 24 present; `pnpm` and `sqlx-cli` are not installed and will be added via
mise / cargo during 0.1.

---

## Autonomous decisions

<!-- Appended during implementation runs. Newest last. -->

## 2026-08-17 — task 0.1: dependency pins were substantially stale

Verified all 31 pins against crates.io. Material drift from the design-time
guesses, all corrected in `Cargo.toml`:

| Crate | Design pin | Actual |
|---|---|---|
| `tantivy` | 0.22 | **0.26** |
| `rmcp` | 0.8 | **3.1** (the Rust MCP SDK went past 1.0) |
| `sqlx` | 0.8 | **0.9** |
| `winnow` | 0.7 | **1.0** |
| `opentelemetry` (+sdk, otlp) | 0.27 | **0.32** |
| `tracing-opentelemetry` | 0.28 | 0.33 |
| `testcontainers` / `-modules` | 0.23 / 0.11 | 0.28 / 0.15 |
| `governor` | 0.7 | 0.10 |
| `ndarray` | 0.16 | 0.17 |
| `fast_image_resize` | 5 | 6 |
| `libvips` | 1 | 2 |
| `infer` | 0.16 | 0.22 |
| `symphonia` | 0.5 | 0.6 |
| `tokenizers` | 0.21 | 0.23 |
| `whisper-rs` | 0.14 | 0.16 |
| `pdfium-render` | 0.8 | 0.9 |
| `ort` | 2.0.0-rc.10 | 2.0.0-rc.13 (still no stable) |

Three manifest errors the design draft would have hit on first build, all fixed:

1. **`[workspace.dev-dependencies]` is not a real Cargo table.** Dev deps are
   declared in `[workspace.dependencies]` and inherited by members via
   `dev-dependencies` + `workspace = true`.
2. **`optional = true` is invalid in workspace deps.** Optionality is a
   member-crate concept; the workspace declares the version, members opt in with
   `{ workspace = true, optional = true }`. Affected libvips, pdfium-render, ort,
   whisper-rs.
3. **`utoipa` was referenced by `dam-api` but never declared.** Added, with
   `utoipa-axum` 0.2 for the router integration.

**sqlx 0.9 split the runtime and TLS features apart.** `runtime-tokio-rustls` no
longer exists; it is now `runtime-tokio` + a TLS choice. Picked
`tls-rustls-ring` (ring backend) over `tls-rustls-aws-lc-rs` — ring is the more
widely deployed of the two and we have no FIPS requirement. Reversible: yes, it is
a feature flag.

Also installed: `rustfmt` and `clippy` components were missing from the toolchain.

Reversible: all of it. Nothing here is a design change — the design was right, the
version numbers were guesses, and guessing version numbers is what a first build
is for.
