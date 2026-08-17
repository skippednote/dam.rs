# damrs — implementation queue

Worked top to bottom. Each task is TDD: **write the failing test first**, then the
implementation, then `mise run check` must pass before moving on.

Rules for autonomous runs:

- Never skip the failing-test-first step. A task whose test was written after the
  code is not done; it is untested code with a test-shaped comment next to it.
- Commit per completed task, on the current branch. Never push.
- Blocked? Append to `DECISIONS.md`, take the **reversible** option, continue.
- **Stop and leave it for review** if a task turns out to touch rights
  enforcement, consent, provenance, or access control in a way the design does not
  already settle. A wrong guess there is a compliance problem, not a refactor.
- Update this file: `[ ]` → `[x]`, with a one-line note on anything surprising.

---

## M0 — Foundation

- [x] **0.1 Workspace skeleton.** Done. 12 members build clean; `clippy -D warnings` clean.
  *Surprise:* 17 of 31 pins had drifted, and three manifest constructs in the design
  draft were simply invalid Cargo (`[workspace.dev-dependencies]`, `optional` in
  workspace deps, undeclared `utoipa`). sqlx 0.9 split runtime/TLS features. See
  DECISIONS.md.


- [x] **0.2 `dam-core` errors + config.** Done. 24 tests; full bar green.
  *Surprise:* the failing test caught a real bug — `Secret`'s deliberately-lossy
  `Serialize` round-tripped through figment's `Serialized::defaults`, turning every
  secret default into the literal `"[REDACTED]"`. Production would have booted with
  a forgeable URL signing key and no complaint. See DECISIONS.md.
  Also landed: `Secret<T>` (redacts in Debug/Display/Serialize), domain `Error` with
  machine-readable denial reasons, `deny_unknown_fields` so typo'd keys fail startup.

- [x] **0.3 Tracing + OTel.** Done in a new `dam-telemetry` crate; 33 tests, bar green.
  *Surprise:* the child-span test proved the "tenant_id on every span" convention was
  decorative — `with_current_span` emits only the innermost span's fields, so
  everything the worker does in a child span had no tenant attribution. Needed
  `with_span_list(true)`. Also: opentelemetry 0.32 removed
  `global::shutdown_tracer_provider`, and OTLP transports are feature-gated (took
  HTTP/protobuf over gRPC to avoid tonic). See DECISIONS.md.
  All three binaries now boot through one init path; `damctl config` prints a
  resolved config with secrets redacted.

- [ ] **0.4 Postgres harness.** testcontainers helper that boots pgvector/pg17,
  runs the §5.3 bootstrap (schemas + 3 extensions), and hands back a pool.
  *Test first:* harness returns a usable pool and is torn down.

- [ ] **0.5 Migration runner.** Two independent tracks, per-schema `_sqlx_migrations`
  ledger, `search_path` set at **connect time** (not `SET LOCAL` — the sqlx
  migrator manages its own transactions).
  *Test first:* apply global then tenant to a fresh container; assert 14 global
  tables, 58 tenant tables, 206 indexes, 75 CHECKs, 5 HNSW, 2 triggers, 2 rules.

- [ ] **0.6 Compliance gate suite.** Port the eleven adversarial cases already
  proven during design into a permanent suite: consent trigger (unnamed / named /
  withdrawn), DPIA-gated flag, audit-log immutability, legal-hold override,
  perpetual+end-date, AI defaults deny, minor without guardian, dual-owner
  placement, restore-without-expiry, two-current-versions, in-flight restore
  dedupe. Plus `provenance_gaps` and the unmarked-synthetic index.
  *These are the D12–D15 guarantees. If one regresses, the build fails.*

- [ ] **0.7 `TenantConn`.** Cannot be constructed outside a transaction. Schema
  name via `quote_ident`, never interpolation.
  *Test first:* path does not leak post-commit; a bad slug is rejected;
  cross-tenant read is impossible.

- [ ] **0.8 Tenant provisioning.** `damctl provision-tenant --slug` → row, schema,
  migrations, seeded defaults (field defs, starter taxonomy, `everyone` group,
  builtin roles, feature flags with `face_identify` **off**), index dir.
  *Test first:* provision two tenants, assert isolation both ways.

- [ ] **0.9 Job queue.** `FOR UPDATE SKIP LOCKED`, leases, dedupe, backoff,
  per-tenant round-robin fairness.
  *Test first:* concurrent workers never double-claim; a dead worker's lease is
  reclaimed; dedupe holds; fairness holds under a single-tenant flood.

- [ ] **0.10 ABAC predicate compiler.** One predicate → SQL fragment + Tantivy
  filter. Query-time, never post-filter.
  *Test first:* identical asset sets from both back ends for the same grants;
  pagination counts do not leak excluded assets.

- [ ] **0.11 CI.** fmt, clippy `-D warnings`, test with containers, `cargo deny`,
  `.sqlx/` committed and verified current.

## M1 — Ingest and storage

- [ ] **1.1 `BlobStore` trait + conformance suite.** One suite, two drivers.
- [ ] **1.2 `S3Store` on `aws-sdk-s3`.** SeaweedFS harness via `GenericImage`
  (no testcontainers module exists). Path-style, SigV4, multipart, presign,
  ranged GET — plus versioning and object lock (GOVERNANCE / COMPLIANCE /
  legal hold), which is why a real server is in the loop at all.
- [ ] **1.3 `FakeS3Store`.** Controllable clock; the full tiering state machine
  no practical local server expresses — class transitions, `InvalidObjectState`,
  `RestoreObject`, `x-amz-restore`, restore expiry, minimum-duration charges.
  ARCHITECTURE §20.2.
- [ ] **1.4 Pool + placement resolution.** Cheapest available placement wins;
  unknown `pool_id` is a hard error, never a silent fallback.
- [ ] **1.5 Content addressing.** Streaming BLAKE3, key layout per §6.2, dedupe on
  re-upload proven by a test that uploads the same bytes twice.
- [ ] **1.6 Upload.** TUS resumable + presigned direct-to-S3. Magic-byte sniffing
  via `infer` — never trust client `Content-Type`.
- [ ] **1.7 Probe + derivatives.** libvips primary, `image` fallback. Subprocesses
  with rlimits, wall-clock, and an escape-proof temp dir.
- [ ] **1.8 Master proxy.** The §2 invariant. *Test first:* `enrichment_runs.used_original`
  is false for every run — the alarm that keeps cold storage viable.
- [ ] **1.9 C2PA.** Verify on ingest, preserve inbound manifest, re-sign
  derivatives. *Test first:* `provenance_gaps` is empty after deriving from an
  asset with credentials. **This is D13; a derivative pipeline that strips
  credentials is wrong, not incomplete.**
- [ ] **1.10 Lifecycle engine.** Dry-run default, `min_duration_until` respected,
  `pinned` honoured, `max_objects_per_run` halt.

## Frontend track (parallel, once 0.11 lands)

- [ ] **F.1** SvelteKit + Tailwind 4 + shadcn-svelte/bits-ui scaffold in `web/`.
- [ ] **F.2** Design tokens from the UI spec — the four-dimension state vocabulary
  (tier in form, rights in semantic colour, provenance neutral, confidence as
  bars) as components, with axe-core in CI from the first commit.
- [ ] **F.3** OpenAPI → TS client generation, wired so drift is a build error.
- [ ] **F.4** Asset grid: virtualised, keyboard-navigable, ARIA grid semantics.

---

## Not in scope for the overnight run

M2 onward. If M0 completes and M1 is underway, that is the night's target met —
this is ~110 engineer-weeks of work in total and no amount of autonomy compresses
that. Stop at a green `mise run check` and a clean commit rather than leaving a
half-built layer.
