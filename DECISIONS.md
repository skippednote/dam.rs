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

## 2026-08-17 — D18: local S3 landscape checked; adding SeaweedFS, keeping Garage

Asked whether a different local S3 closes Garage's four gaps. Short answer: the
two obvious candidates no longer exist as open source.

- **MinIO community edition archived 25 April 2026.** Maintenance mode from
  December 2025, admin console already removed from CE, AGPL, no community
  binaries, engineering moved to the paid AIStor product. Not a dependency to take
  on in August 2026.
- **LocalStack archived its OSS repo March 2026**, consolidated behind one
  authenticated image. Glacier restore was Pro-only anyway.

That makes Garage a good call rather than a compromise — it is one of the few
genuinely maintained OSS S3 servers left.

**Decision: add SeaweedFS as a second CI backend, keep Garage as the dev stack.**

SeaweedFS (Apache 2.0, actively maintained) has real object lock — GOVERNANCE and
COMPLIANCE retention modes plus legal holds — and versioning. That closes the one
hole `FakeS3Store` genuinely cannot: object lock's whole point is that *the server*
refuses the delete, so a fake that refuses proves nothing about a real server. It
also gives a second independent implementation for the conformance suite, catching
places where the driver is coded to Garage's quirks rather than to S3.

**Rejected: moto.** Its `restore_object` is real, but the fake is strictly better
for restore, because restore is a *timing* problem — the temporary copy expiring
mid-download, minimum-duration blocking a re-tier — and a controllable clock is an
in-process concern no emulator provides.

**Rejected: Ceph RGW.** Closest to AWS parity, and needs 3 nodes at 4+ GB each.
Not a test dependency.

**Noted, not adopted: RustFS**, a Rust MinIO replacement that appeared after the
wind-down. Philosophically appealing for this project; months old.

The alternative worth revisiting: consolidate entirely on SeaweedFS — one backend,
a superset of Garage's surface, Apache 2.0 rather than AGPL. Rejected for now only
because Garage is a single small binary that starts faster, and every suite boots
one. If SeaweedFS turns out fast enough to be both, drop Garage.

Reversible: yes. All three sit behind the `BlobStore` trait, which is what makes
swapping test infra a harness change rather than a code change.

## 2026-08-17 — task 0.2: TDD caught a real bug where two safe decisions collided

The test `production_environment_rejects_the_dev_placeholder_signing_key` failed
against the first implementation, and the cause was worth the whole exercise.

`Secret<T>` serialises as `"[REDACTED]"` on purpose — config gets `Debug`-logged
at startup, so a secret that can render itself is a leak waiting to happen.
Separately, the config loader seeded its defaults with
`Figment::from(Serialized::defaults(Config::default()))`, which is the documented
figment pattern.

Together they were a bug: seeding **serialises** the default config, so every
`Secret` default came back as the literal string `[REDACTED]`. The default database
URL was unusable, and the production signing-key check compared `"[REDACTED]"`
against the real placeholder and passed. A production deployment would have booted
with a forgeable URL signing key and no complaint.

**Fix:** drop the `Serialized::defaults` layer entirely. Every config struct already
carries `#[serde(default)]`, so serde fills missing keys from `Default` during
extraction with no serialise round-trip. Locked in by
`secret_defaults_are_real_values_not_redaction_placeholders`.

Worth noting for later: **any lossy `Serialize` is dangerous in a round-trip.** If
another type gains redacting serialisation, check it never passes through a
provider-seeded default.

Two smaller decisions taken along the way:

1. **Config has one code path, not a test-only one.** The first draft took an
   explicit `&[(&str, &str)]` env slice so tests could avoid `std::env::set_var`
   (`unsafe` in edition 2024, and unsound under `cargo test`'s threading).
   That meant tests exercised a path production never took. Replaced with
   `figment::Jail`, which isolates env and cwd under a mutex — real code path,
   parallel-safe tests. Reversible: yes, but do not reintroduce the fork.
2. **Panic lints are relaxed in test code only.** The workspace denies
   `clippy::unwrap_used` and warns on `expect_used`; in a test, panicking *is* the
   assertion. Integration tests get a file-level `allow` preamble (they are separate
   crates and do not inherit `lib.rs` attributes); inline `mod tests` gets
   `#![cfg_attr(test, allow(...))]` in each `lib.rs`. `result_large_err` is also
   allowed in tests — it fires on `figment::Jail` closures whose `Err` type we do
   not control. Production code keeps the deny. Reversible: yes.

Also added, unprompted but cheap: `deny_unknown_fields` on every config struct, so
a typo'd key (`prot = 9999`) fails startup instead of being silently ignored —
which is how an operator ends up certain they changed a setting when they did not.
