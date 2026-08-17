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
