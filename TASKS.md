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

- [x] **0.4 Postgres harness.** Done, behind a `testing` feature so other crates reuse
  it from dev-dependencies. 5 tests, ~4s for 6 containers.
  *Surprise:* needed `pool_for_schema()` with connect-time `search_path` — see 0.6.

- [x] **0.5 Migration runner.** Done. 11 tests; counts asserted and matching.
  *Surprise:* sqlx 0.9 requires `AssertSqlSafe` for dynamic SQL (good — it marks every
  site needing an injection audit). Index count is 207 not 206: `_sqlx_migrations`
  has its own PK index, so the assertion excludes the ledger.

- [x] **0.6 Compliance gate suite.** Done — 16 gates, all green, D12–D15 now
  regression-protected.
  *Surprise, and the most valuable of the night:* the first run failed 10/16 and
  **three of the six passes were false** — `refused()` was `.is_err()`, so "relation
  does not exist" counted as "a constraint refused it". Caused by `SET search_path`
  on a pool affecting only one connection (exactly the §5.2 hazard). Now
  `refused_by_constraint()` asserts SQLSTATE 23/P0001 and panics on class 42.
  See DECISIONS.md.

- [x] **0.7 `TenantConn`.** Done — one constructor, and it begins a transaction. 8
  integration tests: no leak after commit or rollback, drop rolls back, cross-tenant
  read impossible, 20 concurrent writes across two tenants stay separate, missing
  schema fails at `begin`.
  *Surprise:* the dam-core length test asserted a single-char slug was valid while
  citing the regex that forbids it (`{1,38}` is a minimum of one *additional* char).
  Replaced with a cross-check that feeds 16 inputs to both the Rust validator and
  Postgres's own regex. See DECISIONS.md.

- [x] **0.8 Tenant provisioning.** Done — 7 tests green first run, plus `damctl
  migrate`/`provision-tenant`/`migrate --all` driven against the live dev stack.
  D14 verified in a real database: `face_identify enabled=false requires_dpia=true`.
  *Note:* order is schema → migrations → seed → tenant row, so a partial failure
  leaves an inert schema rather than a tenant row pointing at nothing. Idempotent
  rather than transactional. Index dir deferred to M2 with Tantivy.

- [x] **0.9 Job queue.** Done — 13 tests, incl. 10 workers × 50 jobs with no
  double-claim and no loss.
  *Surprise, and the best find of the night:* a mandatory per-tenant cap of 4 made a
  worker claim 5 of a requested 10 while 200 jobs waited — the cap starved the
  **worker**, not the greedy tenant. Fairness now comes from rank ordering alone, which
  gives fairness *and* full utilisation. Also: `SKIP LOCKED` is unavailable alongside
  window functions, so correctness rests on the UPDATE's own `state='queued'` predicate
  being re-evaluated under READ COMMITTED. See DECISIONS.md.

- [ ] **0.10 ABAC predicate compiler.** ⛔ **STOPPED FOR REVIEW — see NEEDS-REVIEW.md.**
  The mechanism is settled (one predicate, query-time, three consumers); the semantics
  are not. Five decisions determine whether an unapproved, unreleased or expired asset
  can be seen or fetched — role combination, release/expiry visibility, EULA scope,
  rule-group evaluation, and whether `all_asset_groups` bypasses expiry. Recommendations
  written; awaiting sign-off.

- [x] **0.11 CI.** Done — four parallel jobs plus a nightly AWS conformance run.
  *Surprise, and a genuine one:* `cargo deny` found **three `rustls-webpki`
  vulnerabilities** compiled into every binary. `aws-config`'s default features pull
  `legacy-rustls-ring` (rustls 0.21 / hyper-rustls 0.24) alongside the modern client, so
  the vulnerable stack was linked while never being the path we use. Fixed with explicit
  feature lists; rustls 0.21 is now absent from the graph. Also fixed a `licences`
  spelling that would have failed the job, and `publish = false` so path deps stop
  reading as wildcards. `.sqlx/` deferred — nothing uses `query!` yet. See DECISIONS.md.

## M1 — Ingest and storage

- [x] **1.1 `BlobStore` trait + conformance suite.** Done — 12 shared cases, capabilities
  declared and verified, skips reported in a `Report` rather than swallowed.
  *Note:* the storage-class skip message says outright that echoing the header back
  without changing behaviour does not count — which is exactly what SeaweedFS does.
- [x] **1.2 `S3Store` on `aws-sdk-s3`.** Done — 7 conformance cases (11 shared passes,
  2 declared skips), 5 multipart cases, 6 versioning/object-lock cases.
  *Surprises, in order of how badly they'd have bitten:*
  1. The pinned SeaweedFS tag (3.80) answers `PutBucketVersioning` with `501
     NotImplemented`. Because a version-scoped delete then fails with `AccessDenied`,
     the earlier "legal hold verified live" note was a **pass for the wrong reason**.
     Fixed by pinning 4.42 (D19) — where the hold genuinely refuses and releasing it
     genuinely permits the delete.
  2. `x-amz-bypass-governance-retention` is permission-gated. An anonymous container
     can never bypass, so the harness now declares two identities and the suite proves
     the refusal is the *permission* rather than an unimplemented header.
  3. The wait strategy matched on stdout; SeaweedFS logs to **stderr**, so it waited on
     an empty stream until timeout. Bumping the tag also cut container start 25s → 4s.
  4. `.mise.toml` exported `DAMRS_S3_*`, which match no config field — the config is
     nested and denies unknown fields, so `damd` could not have started in the dev
     environment at all. Caught by dam-core's own config tests once the full gate ran.
- [x] **1.3 `FakeS3Store`.** Done — passes the shared suite with zero skips, plus 10
  timing cases no real backend can be tested on.
  *Surprise:* my expiry test advanced to *exactly* `expires_at` and called it "just
  under". The implementation was right; the boundary is exclusive, matching S3's
  `expiry-date`, and is now pinned at t+59/t+60 — an inclusive boundary would serve one
  request more than AWS and show up only as an intermittent production 403.
- [x] **1.4 Pool + placement resolution.** Done — 15 cases, pure logic, no container.
  *Note:* "cheapest wins" is wrong stated alone. Deep Archive is the cheapest place to
  keep bytes and its per-GB retrieval can undercut Glacier IR's, so a price-only ranking
  turns ordinary downloads into restore tickets. Resolution is lexicographic:
  readable-now, then price, then a stable name tiebreak. Mutation-checked — removing the
  readability filter fails 3 tests, skipping an unknown pool fails 1.
  *Surprise:* clippy caught `40 / 1000` in my own expected value — integer division, so
  per-request charges were truncating to zero out of every estimate at the database's
  1e-8 scale (an S3 GET is 4e-7). `Rate` now works at 1e-12 with an exact ×10,000
  conversion from `numeric(12,8)`. Two pools differing only in request price would
  otherwise have compared equal.
  *Decision taken:* `enabled = false` retires a pool from new placements but keeps it
  readable — blocking reads too would turn a config change into a data outage.
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

  *Prerequisite found in 1.2:* §6.3 tiers superseded versions on their own schedule
  (`GLACIER_IR` at 30 d), and `lifecycle_policies.only_superseded` expresses it — but
  `object_placements` is keyed `(object_key, pool_id)` with no `version_id`, so a
  noncurrent version has nowhere to record its class, minimum duration, or restore
  state. Either noncurrent tiering is delegated wholly to S3-native
  `NoncurrentVersionTransition` rules (in which case a restore of a superseded version
  has no row to hang off), or placements gain a version dimension. Decide before
  writing the engine; not decided here.

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
