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


## Where we are

Updated with every slice. The detail is in the sections below; this is the part you can read in ten seconds.

| Track | State |
|---|---|
| **M0–M3c** Foundation, ingest, metadata/search/rights, delivery/sharing/restore | complete |
| **F** The UI: browse, detail, upload, filter rail, lightbox, bulk bar, design pass | complete |
| **F.11b** Share/portal UI, schema administration, metadata types | complete except restore UX |
| **Q** Acquia parity, 20 slices | Q.1–Q.14 done; Q.14b and Q.15–Q.20 open |
| **M3d** Drupal 11 connector | not started |
| **M4** Local AI: embeddings, OCR, ASR, faces, dedup, semantic search | schema exists, behaviour unwritten |
| **M5** Claude enrichment, MCP server, AI Act marking G2, budget caps G20 | **done** — two clients, BYO keys, spend caps, the enrichment job, G2 marking, the review queue, batch backfill, NL→query, the MCP server |
| **M6** Workflow/proofing, annotations, analytics | not started |
| **Pre-GA** Import G7, SCIM/BYOK/audit G10, DR G11, metering G19, quotas | not started |

**Next up, in order:** the Q.15–Q.19 search set → Q.14b collections in the app → Q.20 sundries → M4 local AI → M6 → M3d → Pre-GA.
M5 is complete. M5a and M5b are done and verified against the running stack: both
hosted clients reach their real vendor endpoints, and a full enrichment ran end to end through the worker against
a local OpenAI-compatible endpoint — values written with provenance, a disclosure row, tags suggested, 0.75¢
charged as a sub-cent remainder, `used_original` false, and a tag confirmed from the review screen with its
feedback row.

**Earlier plan, kept for the record:** M5 hosted-model enrichment (re-prioritised ahead of Q.14 and M4 — see below)
→ Q.6 comments → Q.7 activity feed → Q.8 versions → Q.9 attachments → Q.10 history → Q.11 conversions →
Q.12 intended use → Q.13 orders → **M5 hosted-model enrichment** → Q.14 portals → Q.15–Q.19 search →
Q.20 sundries → M4 local AI → M6 → M3d → Pre-GA → Entries.

**Re-prioritised 2026-08-20, at your request: hosted models before local AI.** M5 moves ahead of M4 and ahead of
the rest of the Q slices. The reasoning holds up independently — M5 needs no model files, no ONNX runtime and no
GPU, so it is the half of the AI story that can actually run on a laptop and in CI, and §8.2 has the local
embeddings as the *workhorse* with Claude for "what embeddings cannot see". Building the expensive-per-call half
first also forces the budget caps (G20) and the provenance/marking surfaces (G2) to exist early, which are the
things that are painful to retrofit.

**One thing your instruction adds that ARCHITECTURE does not settle:** §8.3 specifies Anthropic, over raw HTTP,
and builds the cost model on two Anthropic-specific features (batch at 50%, prompt caching at ~90% off a shared
prefix). ChatGPT and Kimi mean a *provider seam*. The efficient shape, and the one I am building: one
OpenAI-compatible client covers ChatGPT, Kimi/Moonshot, DeepSeek, Together and most others — they all speak
`/chat/completions` — and Anthropic keeps its own client because its wire format and its batch/caching economics
differ. So it is two clients, not one per vendor.

**Open questions parked for a human:** `NEEDS-REVIEW.md` Q.6 (may an admin read a private comment?), item 2.4 (rule-based asset groups) and task 3.x
(which AWS-native features to rely on rather than build).

**The bar every slice clears:** `cargo fmt --check`, `cargo clippy --all-targets --all-features -D warnings`,
the Rust suites, then prettier + eslint + svelte-check + vitest + playwright with axe in both themes. Each
slice is also mutation-tested, and driven against the running stack before it is called done.

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

- [x] **0.10 ABAC predicate compiler.** Pure policy layer + SQL rendering done — 24 pure cases and
  12 against real Postgres. The five semantics are the delegated decisions in DECISIONS.md, each
  asserted rather than described, because every one is a decision somebody will later assume went the
  other way.
  *Two functions, because visibility and usability are different questions:* `compile` answers "which
  assets may this caller see", `evaluate` answers "may they do this to this asset, and if not, why".
  Conflating them is what makes an expired asset vanish from search — and an asset that vanishes on
  expiry is one nobody renews.
  *The §7 leak is asserted, not assumed.* A post-filter returns the same rows as an in-query filter
  and differs only in the count, so there is a test comparing `count(*)` against the row set, plus one
  paginating the filtered set. It looks redundant; it is the whole point.
  *Mutation-checked:* rendering `(true)` instead of `(false)` for an empty predicate fails 2 tests,
  breaking the group clause fails 7, dropping the soft-delete filter fails 1.
  *Refused rather than approximated:* a granted group carrying a rule `predicate` errors out, because
  the language those are written in is the query IR (2.4). Ignoring it would grant *less* access than
  configured — fail-closed but silently, so the first anyone would know is an asset that should have
  been visible and was not.
  *Surprises:* sqlx 0.9 dropped `QueryBuilder`'s lifetime parameter and returns `SqlStr` rather than
  `String` from `into_sql`. And my parenthesisation test split on `"WHERE "` while the fragment
  contains its own inner `WHERE` — it was asserting against a half-fragment.
  *Remaining in 0.10 — now done, in 2.6:* the Tantivy rendering and the differential test asserting both
  back ends return identical sets. 15 query shapes × 4 access predicates = 60 comparisons, plus a
  non-vacuity guard. Mutation-verified three ways: dropping Tantivy's group filter, its soft-delete
  exclusion, or making its empty `Or` match everything each fails with the two sets printed side by side.
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
- [x] **1.5 Content addressing.** Done — 9 integration cases + 3 unit, including the
  two-upload dedupe test (second upload transfers nothing, one object under the prefix)
  and a known-answer vector pinning BLAKE3 so a dependency swap cannot silently change
  every key in the estate.
  *Note:* the duplicate check compares **size as well as presence**. A key implies its
  bytes under content addressing, so a wrong-sized object at that key is corruption, not a
  cache hit — and treating it as one would make the corruption permanent, because every
  later upload of the correct bytes would also skip the write. Full digest verification
  would be stronger but costs a download per duplicate upload.
  *Note:* `Digest` normalises case at construction rather than validating at use, so an
  uppercase digest cannot produce a second key for identical content.
  *Deferred to 1.6:* a streaming upload cannot know its key before it has read the bytes,
  so the large-file path needs a staging key promoted by a server-side copy. `hash_reader`
  and `StreamHasher` are the pieces; the promotion belongs with TUS.
- [x] **1.6 Upload.** TUS resumable + presigned direct-to-S3. Magic-byte sniffing
  via `infer` — never trust client `Content-Type`.
  *Done:* the HTTP surface — OPTIONS/POST/HEAD/PATCH/DELETE plus `POST /uploads/presign` — over the
  1.5 engine, 23 cases in 4 drivers.
  *Two things writing it down exposed.* The 404-for-another-tenant claim was not yet true:
  `uploads::load` filtered on `upload_id` alone and relied on `search_path`, then post-checked
  `tenant_id` and returned `Inconsistent`, which renders as a **500** — a status that varies with the
  input is the disclosure the rule exists to remove. A `tenant_id` predicate fixes it; the post-check
  stays as a tripwire. Mutation-verified: with `search_path` isolation neutralised the cross-tenant
  test still passes, and with the predicate also gone it fails with exactly that 500. And a NUL byte
  in an upload id reached Postgres, which rejects NUL in text — same 500-instead-of-404. The id rule
  is now one public `dam_store::validate_upload_id` applied at the edge *and* in `Key::staging`.
  *`grants_for` is now executor-generic*, like `uploads`: an unqualified `FROM roles` resolves through
  the request transaction's `search_path`, so a pool would have read the wrong schema and found no
  grants — fails closed, looks like a permissions bug.
  *Cost note:* a container per case put 19 Postgres instances in one suite, taking the run from 12 s
  to 231 s and then breaking it on connection timeouts. Cases are now plain functions over a borrowed
  fixture with 4 drivers (5 s). A shared pool is not available — each `#[tokio::test]` builds its own
  runtime and a pool hangs when used from another.
  *Part done:* staging-key promotion (the piece 1.5 deferred). A streamed upload lands at
  `<tenant>/staging/<upload_id>` and is promoted to its content key by a **server-side**
  copy once the digest is known, so the bytes never cross the client twice. `copy` is now a
  `BlobStore` method covered by the shared conformance suite — it was otherwise the one
  trait method the suite did not exercise, which is how the fake diverges.
  *Note:* S3 rejects `CopyObject` above 5 GiB, so promoting a §18.3 file is a multipart
  copy of ranged parts, not a rename. `copy_part_ranges` is unit-tested for contiguity at
  200 GB (40 parts, no gaps, no overlaps) and grows the part size rather than exceeding the
  10,000-part cap — a plan S3 would otherwise reject at completion, after every part copy
  had been paid for. The >5 GiB path itself only runs in the AWS nightly.
  *Note:* staging is deleted only **after** the content object exists. It is the sole copy
  of the bytes until then, so an early delete would destroy a retryable upload; an
  abandoned staging object is reaped on a timer instead — a leak that costs storage beats a
  loss that costs the upload.
  *Part done:* magic-byte sniffing (`dam-media::sniff`, 22 cases). Findings, all verified by
  probing `infer` 0.22 directly rather than assumed:
  - **`infer` does not detect ELF at all.** It carries Mach-O, PE, wasm and Java class, so a
    Linux binary fell through to `application/octet-stream` with class `Unknown` — meaning
    `is_dangerous()` was false and the upload path would have stored it happily. Explicit
    signature added.
  - An SVG **with** an XML prolog is detected as `text/xml`, so it never reached the content
    sniffer. Generic text answers are now refined — and refinement may only *specialise*: my
    first version let a catalogue XML file fall back to `text/plain`, replacing a specific
    answer with a vaguer one.
  - A shell script arrives as `text/x-shellscript` with a `Text` matcher, which a naive text
    mapping files as a document. Classed `Executable`.
  - OLE storage stays `Archive` on purpose: the same container holds legacy `.doc`/`.xls`
    **and** `.msi` installers. `Document` would point a renderer at an installer; `Executable`
    would refuse the customer's legacy Word files.
  *Part done:* the resumable-upload engine (`dam-store::resumable`, 12 integration + 3 unit).
  The constraint that shapes it: a TUS chunk may be 64 KB while S3 refuses any part under
  5 MiB except the last, so chunks accumulate into a **tail** held *in object storage* — a
  process-local buffer would make resumption sticky to one node. Mutation-checked: reversing
  the tail/chunk order fails 3 tests, removing the offset-conflict check fails 2.
  *Part done:* `dam-media::ingest::finalize` — sniff, hash, promote (9 cases). This is the
  single validation point every upload path shares, and the **presigned** path is why it has
  to exist: a presigned `PUT` cannot cap the size, cannot constrain the type, and does not
  report what arrived, so nothing checked at mint time is more than advisory. A refused upload
  has its staged bytes destroyed rather than left for the reaper — until the reaper runs, a
  refused executable stays retrievable at a key the uploader knows. Mutation-checked.
  *Note:* one of my test names over-claimed — "hashed in bounded chunks not buffered whole"
  only proves the digest is right under a small window; boundedness is structural and not
  observable from a test. Renamed to what it actually checks.
  *Part done:* `upload_sessions` (migration 0009, 10 constraint cases) plus `dam_db::uploads`
  — create/load/save/reapable/reap (8 cases, both containers).
  *Notes:*
  - The session row carries `tenant_id`. My first version derived it from
    `dam_global.tenants` via `current_schema()`, which coupled the reaper to a provisioned
    control-plane row — precisely what may be missing when cleanup matters most. Keys already
    embed the tenant (`object_placements` stores whole keys rather than reconstructing them),
    so the row carries the prefix it needs. It is **not** an access-control boundary; the
    schema is that (D2).
  - `reap` reclaims **storage first, then the row**. Marking the row first would orphan the
    parts permanently with nothing left pointing at them; the other order repeats an
    idempotent cleanup.
  - The `part_count = jsonb_array_length(parts)` constraint caught my own test's positive
    case, which set a counter of 1 against an empty list. That is the exact inconsistency the
    Rust loader refuses, so both ends now agree.
  *Still to do — and deliberately stopped:* the TUS HTTP surface. See NEEDS-REVIEW.md — it
  needs request authentication and tenant resolution, and no task in M0/M1 schedules an API
  skeleton. Presigned URL minting has the same dependency: `presign_put` exists, but *who* may
  ask for one is an authorisation decision.
- [x] **1.7 Probe + derivatives.** libvips primary, `image` fallback. Subprocesses
  with rlimits, wall-clock, and an escape-proof temp dir.
  *Part done:* the subprocess sandbox (`dam-media::sandbox`, 14 cases, no media tools
  needed — it tests the runner). Mutation-checked: interpolating the command line instead of
  using positional parameters fails 6 tests; dropping `env_clear` fails 1.
  *Measured platform findings, all of which changed the design:*
  - **`ulimit -v` does nothing on macOS.** The shell rejects it outright and a 200 MB
    allocation runs unimpeded under a 50 MB cap; on Linux the same cap makes `dd` fail with
    "out of memory". So `capabilities()` declares the memory limit Linux-only and
    `unenforced()` names it — a runner that accepted it on both would create a protection
    that exists only in production.
  - **`ulimit -f` block size differs by shell**: 512 bytes on busybox, **1024 on bash**
    (which is `/bin/sh` on macOS), and `POSIXLY_CORRECT` changes nothing. So the same number
    means twice the bytes on one platform. The ulimit is the coarse bound; `oversized()` is
    the authoritative post-run check.
  - `ulimit -t` works everywhere tested — it killed a spin at exactly 1s.
  *Note:* rlimits without `unsafe` means letting the shell apply them:
  `sh -c 'ulimit …; exec "$0" "$@"' prog args…`. Positional parameters make injection
  structurally impossible rather than a quoting exercise.
  *Note:* `TimedOut` originally discarded the partial output. A hung tool's last lines are
  the whole diagnosis, so it now carries them — which needed the readers to accumulate into
  shared buffers rather than returning at the end, since a sink written on completion is empty
  exactly when it matters.
  *Also fixed:* a full-workspace run flaked once on container startup under load. Both
  harnesses now set a 120s startup timeout — a suite that fails one run in ten teaches people
  to re-run instead of to read the failure.
  *Part done:* the probe (`dam-media::probe`, 12 cases + 3 unit). Mutation-checked: swapping
  the axes for the wrong orientation range fails 2 tests, trusting an out-of-range orientation
  fails 1, removing the pixel-budget guard fails 1.
  *Decision taken — dimensions are reported twice.* A phone stores a portrait photo as
  4000x3000 with `orientation = 6`; reporting those numbers makes every grid cell and thumbnail
  sideways, and reporting only the rotated size loses what the file contains. So
  `stored_width`/`stored_height` and `display_width`/`display_height` are both named
  explicitly, and nothing is called `width` — a bare `width` is the field somebody uses without
  deciding which one they meant.
  *Notes:*
  - Dimensions come from the header alone (`ImageReader::into_dimensions`). A 65535x65535 PNG
    header is a few hundred bytes and ~50 GB decoded, so the probe answers it without
    allocating; `perceptual_hash`, which must decode, enforces the budget itself because "hash
    everything on ingest" is exactly what a worker will do.
  - An orientation outside 1-8 is dropped rather than applied. Cameras have written 0 and 9,
    and an unknown transform would corrupt the derivative.
  - `None` and `Some(1)` are kept distinct: no EXIF at all versus a file that says "upright".
  - Building the bomb fixture took two attempts: a 13-byte GIF header looks ideal but `image`'s
    GIF decoder wants more than the logical screen descriptor and fails with EOF. Patching a
    real PNG's IHDR (and recomputing its CRC) means only the claimed size differs from something
    the encoder produced, so the test cannot pass because of an unrelated malformation.
  *Part done:* derivative rendering (`dam-media::derive`, 12 cases + 5 unit) — Lanczos3 resize,
  contain/cover fit, matting, and `op_hash`.
  *Surprise — my own halo test was vacuous.* It used a `size / 4` inset, which puts the square's
  edge exactly on a block boundary when scaling 64→16, so no output pixel straddled the edge and
  there was no halo to detect. It passed with premultiplication *disabled*. Fixed by offsetting
  the inset and scaling to 15: the mutation now produces `Rgba([77, 77, 77, 77])` — a grey
  fringe — and fails. This is the fourth "passes for the wrong reason" defect this project has
  turned up, and the first one I introduced in a test of a property I had specifically set out to
  prove.
  *Notes:*
  - `op_hash` length-prefixes the profile and intent. Concatenated, `("srgbper", "ceptual")` and
    `("srgb", "perceptual")` would collide, and a collision here serves the wrong colour from
    cache forever.
  - Orientation is applied once and the output carries **no** EXIF, so a viewer cannot rotate it
    a second time. All eight values are handled, including the four mirrors.
  - Nothing is upscaled: the source is the ceiling. Mutation-checked (removing the clamp fails
    2 tests).
  *Part done:* the libvips path (`dam-media::vips`, 9 integration + 6 unit). vips 8.18.5 is now
  installed via mise's conda backend — **not** Homebrew, which cannot link freetype on this machine
  because a Codex runtime holds that path. `pdfload` gives page counts, which I had previously said
  needed pdfium.
  *The find that shaped the design:* **libvips marks 14 of its own loaders `untrusted`** —
  `dcrawload`, `magickload`, `pdfload`, `svgload`, `openslideload`. The formats a DAM most needs are
  the ones its maintainers flag as risky on hostile input, so libvips runs as a CLI inside the
  sandbox rather than as an in-process binding: a malformed RAW then kills a bounded subprocess
  instead of corrupting `damd`'s address space. There is a test asserting those loaders are *still*
  marked untrusted, so the rationale stays checkable rather than becoming folklore.
  *Surprise — my own sandbox broke tool discovery.* It clears the environment and sets `PATH` to the
  system directories, so a mise-installed vips is not on the child's `PATH` at all. `Toolchain` now
  resolves absolute paths in the parent before the environment is stripped, which is also the right
  posture for a tool pointed at untrusted bytes: no `PATH` ambiguity about which decoder ran.
  *And it broke CI, which I fixed before pushing anything:* CI ran a bare `cargo test`, so these
  tests would have passed locally and failed there. CI now installs the same pinned tools through
  `jdx/mise-action` — an apt libvips would be a different build with a different loader set, which is
  precisely what the capability test refuses to guess at.
  *Surprise:* my integration loop asserted a 1-page PDF reports `Some(1)` while the unit test next to
  it asserted `None` — the two contradicted each other directly. `None` is right: a JPEG is not a
  one-page document, and storing 1 for every photograph makes documents unfilterable.
  *Part done:* vips-backed rendering (`vips::render`, 9 cases), which makes libvips the *primary* path
  and lands **D11** — the ICC handling §18.1 calls "non-negotiable for any brand or print library".
  `image` has no colour management at all, so this is the half that makes D11 real.
  *The colour tests assert on pixels, not metadata.* An embedded profile proves a profile is embedded,
  not that the transform ran; swapping the tag without converting looks correct in every metadata check
  and is wrong on screen. Measured: P3 red `230 49 35` → sRGB `251 0 5`, and unchanged when no output
  profile is set (D11's "masters keep their profile"). My first attempt compared profile *bytes* and
  would have passed regardless — the two profiles share an identical ICC header.
  *Surprise — `vipsthumbnail` upscales by default.* A 64x48 source asked for 2048x2048 came back
  2048x1536, while the pure-Rust path caps at the source. The `>` size modifier fixes it. Two renderers
  disagreeing on every small asset is a bug nobody finds until they compare two derivatives of one file.
  *Surprise — rendering intents do nothing between matrix profiles.* All four intents gave identical
  pixels for P3→sRGB, which is correct ICC behaviour: both are matrix/TRC with a shared D65 white point.
  Intents diverge for LUT profiles, and CMYK is one — relative `232 31 42` against perceptual
  `232 0 0` — which is exactly D11's print case. Both facts are now tests, because the next person will
  reasonably expect P3 to behave like CMYK.
  *Done:* audio and video probing (`dam-media::avprobe`, 7 integration + 6 unit) via ffprobe in the
  sandbox — ffmpeg's demuxers face the same hostile input as vips's loaders and have their own CVE
  history. Fixtures are generated by ffmpeg itself (`sine`, `testsrc`), so every expected value is
  derivable from the command that produced it.
  *Surprise — ffprobe calls a PNG a video.* A still image is reported as a stream of
  `codec_type: "video"` with `format_name: png_pipe` and **no duration**. Treating "has a video stream"
  as "is a video" would route every photograph into the video pipeline, so `is_timed` is derived from
  the duration instead. Mutation-checked.
  *Note:* `r_frame_rate` is a rational, and NTSC is `30000/1001`. Reading it as a plain number yields
  nothing for the most common broadcast rate there is. Durations round rather than truncate — `1.5s`
  is 1500 ms, and truncating loses half a second on every clip.
  *Note:* ffprobe is inconsistent about types by design — `channels` is a number, `sample_rate` a
  string. Every numeric field is read leniently.
  **1.7 is now complete for probing and rendering.** Video *derivatives* (HLS, loudness) remain M3.5.
- [x] **1.8 Master proxy.** Done — 15 media cases + 4 database cases, plus migration 0010.
  *The invariant is structural, not documentary.* An enrichment stage takes an
  `EnrichmentSource`, whose only alarm-free constructor **refuses any key outside the `p/`
  namespace**. A doc comment saying "read the proxy" is a convention, and conventions decay one
  commit at a time — invisibly, in this case, because nothing breaks until the next model
  upgrade turns into a restore storm. Reading the original stays possible (C2PA verification at
  ingest legitimately attests to the master's own bytes) but sets `used_original` and demands a
  reason.
  *Note:* "not the original" would have been the wrong check. A thumbnail is hot and cheap too,
  but it is 400px, and re-embedding against it would silently degrade every vector in the
  library. The test asserts thumbnails, derivatives, manifests and staging keys are all refused.
  *Surprise — my chosen quality contradicted §2's own footprint number.* I picked q88; measured
  against a photo-like 12-megapixel master it costs **766 KB for the proxy alone**, against §2's
  ~0.5 MB budget for the *entire* hot set. Dropped to q82 (561 KB) and put the measured table
  into ARCHITECTURE §2, so the number is grounded rather than asserted.
  *Surprise:* my first size test compared the proxy against a synthetic PNG gradient, which
  compresses to 187 KB for 12 megapixels — so the "master" was smaller than its own proxy and
  the test failed for a reason unrelated to the code. Fixtures that reason about *size* need
  photo-like data.
  *Also landed:* migration 0010 makes the alarm triageable — `original_read_reason` with a CHECK
  that it is present whenever `used_original`, an `enrichment_original_reads` view that joins the
  asset (filename and size, because "which files" and "what would a restore cost" are the first
  two questions), and a partial index so the alert can be polled cheaply forever.
- [x] **1.9 C2PA.** Verify on ingest, preserve inbound manifest, re-sign
  derivatives. *Test first:* `provenance_gaps` is empty after deriving from an
  asset with credentials. **This is D13; a derivative pipeline that strips
  credentials is wrong, not incomplete.**
  *Done:* `dam_media::provenance` (verify / preserve / sign, 12 cases) and `dam_db::provenance`
  (record + the `provenance_gaps` query, 8 cases). c2pa 0.90.15.
  *The state mapping was the thing to get right.* c2pa-rs reports `Valid` for a signature that
  verifies and `Trusted` for one that also chains to a known root. Our `valid` means **trusted**;
  `Valid` maps to `untrusted`. Collapsing them would display "credentials verified" for a manifest
  anyone can mint, and collapsing `absent` into `invalid` would bury every real tamper signal under
  every ordinary photograph.
  *Four spec requirements found by testing, each of which produced a manifest that verified as
  **invalid** — indistinguishable from a tampered file:* the chain must open with `c2pa.created` or
  `c2pa.opened`; `c2pa.created` must carry a `digitalSourceType`; `c2pa.opened` must reference its
  ingredient by a hashed URI that does not exist until the manifest is assembled; and `c2pa.opened`
  requires a `parentOf` ingredient. The first action is therefore **not the caller's to build** —
  `Provenance::{Created(Origin), DerivedFrom(Parent)}` drives `BuilderIntent`, which makes all four
  unrepresentable. Also: the claim generator moved to `claim_generator_info` in claim v2, so reading
  the flat field reported `None` for everything we sign.
  *D15/G2 comes free:* `digitalSourceType` **is** the Article 50 machine-readable mark, so
  `Origin::AlgorithmicMedia` writes it today and M5 is left with a database concern rather than a
  manifest-format question.
  *Two features deliberately off:* `openssl` (replaced by `rust_native_crypto`, removing a system
  dependency from every build) and `fetch_remote_manifests` — the latter makes the reader dereference
  a URL found **inside an uploaded file**, which on an ingest path is an SSRF primitive handed to
  anyone who can upload.
  *Costs:* MSRV 1.85 → 1.88 (cargo silently resolves to c2pa 0.58 otherwise), which turned on
  clippy's let-chain lint and required one pre-existing fix in `dam-telemetry`. And c2pa pulls **245**
  crates, not the 83 estimated during feasibility — recorded because the estimate was wrong, not
  because the conclusion changed.
- [x] **1.10 Lifecycle engine.** Done — 22 cases, pure logic. Dry-run default, `pinned`
  honoured unconditionally, `min_duration_until` respected with an inclusive boundary,
  `max_objects_per_run` halting *and reporting how many were left*. Mutation-checked: flipping
  the dry-run default fails 2 tests; removing the pinned check, the tier-exempt check, the
  min-duration boundary, or the halt's remaining-count each fail 1.
  *The engine plans; it never moves anything.* That separation is the deliverable — a plan can be
  read, diffed and approved before terabytes of a customer's masters go somewhere unreadable for
  48 hours.
  *Two defaults chosen to fail safely:*
  - `LifecyclePolicy::new` sets `dry_run: true`, and there is no `Default` impl and no builder
    ending in execution, so turning it off shows up in a diff.
  - Every candidate appears in the plan exactly once, as a transition or as a skip **with a
    reason**. An object that is neither moved nor explained is indistinguishable from one the
    engine forgot — there is a test asserting the two counts add up.
  *Note:* a policy can never move an object toward a warmer tier. Cold→hot is a restore, not a
  transition, so a configuration typo naming `STANDARD` as the target for archived objects would
  otherwise produce an enormous surprise bill. Coldness is ranked by retrieval characteristics
  rather than by name, so a new class slots in by what it costs.
  *Note:* the `only_superseded` prerequisite I flagged in 1.2 is resolved the honest way rather
  than guessed. `object_placements` still has no version dimension, so the engine **halts with
  `Unsupported` and names the gap** instead of matching zero objects — a policy that looks
  configured while silently doing nothing is the worst of the available outcomes, because a quiet
  success is never investigated. Adding a version dimension remains a decision for review.

## Frontend track (parallel, once 0.11 lands)

  *Prerequisite found in 1.2:* §6.3 tiers superseded versions on their own schedule
  (`GLACIER_IR` at 30 d), and `lifecycle_policies.only_superseded` expresses it — but
  `object_placements` is keyed `(object_key, pool_id)` with no `version_id`, so a
  noncurrent version has nowhere to record its class, minimum duration, or restore
  state. Either noncurrent tiering is delegated wholly to S3-native
  `NoncurrentVersionTransition` rules (in which case a restore of a superseded version
  has no row to hang off), or placements gain a version dimension. Decide before
  writing the engine; not decided here.

- [x] **F.1** SvelteKit + Tailwind 4 + bits-ui scaffold in `web/`. Done — 6 axe/keyboard cases,
  2 component cases, and a `mise run check:web` gate wired into CI.
  *Versions the `sv` CLI picked (not guessed):* SvelteKit 2.63, Svelte 5.56, Tailwind 4.3,
  Vitest 4.1.8 (browser mode via Playwright), Playwright 1.60, TypeScript 6.0.3, Vite 8,
  bits-ui 2.18, `@tanstack/svelte-virtual` 3.13 ready for F.4.
  *Surprise, found by the gate on its very first run:* **the SvelteKit scaffold ships with no
  `<title>`.** axe reported `doc-has-title` (serious, WCAG 2.4.2) — every page would have been
  announced to a screen reader by its URL. The layout now derives a title from page data with a
  fallback.
  *Note:* the scaffold had no `<main>` and no skip link either, so four of the six a11y cases were
  red before any implementation — which is the point of writing them first. The skip link is
  positioned off-screen rather than `display: none`: hiding it until focus is the common
  implementation and it removes the element from the accessibility tree, so the affordance ends up
  existing only for sighted keyboard users, who need it least.
  *Note:* `:focus-visible` styling is global rather than per-component. Tailwind's preflight
  removes the browser outline, and a new component cannot forget a rule it never had to write —
  nobody tests with a keyboard by accident.
  *Note:* `check:web` is deliberately **not** folded into `mise run check`. The Rust gate runs on
  every edit and adding a browser install plus a Vite build to it would make the inner loop
  minutes long. CI runs both jobs; `mise run check:all` is there for a pre-push sweep.
  *Deferred to F.2:* shadcn-svelte component adoption. Its `init` writes the token layer, which is
  exactly what F.2 defines from the UI spec — running it now would mean writing those values twice.
- [x] **F.2** Design tokens + the four-dimension state vocabulary. Done — 48 unit/component cases
  and 8 a11y cases; axe has been in CI since F.1's commit.
  *The tests pin the channel assignment, not the pixels.* Giving tier a colour of its own is not a
  restyle — it takes the channel rights depends on, and then "archived" amber and "expiring" amber
  are the same badge. So there are direct assertions that every tier shares one neutral token, that
  each rights state has its own, and that provenance never borrows a rights colour.
  *State names come from the schema, not from me:* `rights_state` is
  `allowed|expiring|denied|unknown` and `provenance_state` is `none|valid|invalid|untrusted`, both
  asserted against the CHECK constraints. A state the backend can produce but the UI cannot render
  shows as *no indicator at all*, which reads as "no restriction".
  *The sharpest case:* `unknown` must never be styled like `allowed`. `rights_state` defaults to
  `unknown`, and the schema's own AI-gate comment says unevaluated rights are not permission — so
  `unknown` gets its own hue (not a paler green) and reports `blocksDistribution: true`.
  *A `/style` route renders every variant of all four dimensions,* which means one axe scan checks
  the contrast of every token pair on every CI run. Mutation-checked: lightening a single foreground
  token to 82% makes it fail with `color-contrast (serious)`. A tint plus a hue is exactly the
  combination that looks fine and measures 3.9:1.
  *Surprise:* five component tests failed because they rendered in a `for` loop —
  `vitest-browser-svelte` mounts into a shared container, so repeated renders leave several copies in
  the DOM and every locator matches more than one element. It surfaces as a 15-second timeout rather
  than a duplicate-match error. `it.each` fixes it and names the failing state.
  *Note:* `null` confidence renders no meter at all rather than an empty bar. A null score usually
  means a human applied the tag; an empty bar would claim the model was certain it was wrong.
- [x] **F.3** OpenAPI → TS client generation, wired so drift is a build error. Done — the document
  is emitted by `damctl openapi --write`, checked in as `openapi.json`, and consumed by
  `openapi-typescript`. `mise run openapi` regenerates both.
  *Three layers, each verified by deliberately breaking the chain — adding a `Revoked` variant to the
  Rust enum:*
  1. **Stale `openapi.json`** → the Rust suite fails with the regeneration command in the message.
  2. **`openapi.json` current but the client stale** → the web contract test fails. This is the case
     TypeScript *cannot* see: a stale generated union type-checks perfectly against out-of-date
     constants.
  3. **Both regenerated** → `svelte-check` fails naming `vocabulary.ts` and `RightsBadge.svelte`,
     because the `Record<RightsState, …>` tables must be exhaustive. A new state cannot reach the UI
     without a label, an icon and a colour token.
  *Note:* F.2's hand-written unions are gone — `RightsState` and `ProvenanceState` now come from the
  generated client, so the chain is database CHECK → `dam-core` enum → `openapi.json` →
  `schema.d.ts` → the badge tables, with a test at every hop.
  *Line drawn deliberately:* the new `dam-core::rights` module defines the *vocabulary* and nothing
  else — no `blocks_distribution()`. That is enforcement, it belongs at the distribution chokepoint
  (D12), and the predicate that decides it is 0.10, which is stopped pending review. A convenience
  method there would quietly become the definition, in the one layer that has no idea who is asking.
  *Note:* the generated file is excluded from Prettier. Formatting machine output would make the CI
  drift diff depend on two tools agreeing about formatting forever.
  *Watch item:* `openapi-typescript` 7.13 declares a peer of `typescript@^5.x` and the scaffold
  installed TypeScript 6.0.3. Generation and `svelte-check` both work; the warning is worth
  remembering if a future release starts to care.
  *Bonus:* the Rust doc comments travel through as JSDoc, so the invariants ("only `Present` is
  readable", "unevaluated is not permission") are visible in the editor on the frontend.
- [x] **F.4** Asset grid: virtualised, keyboard-navigable, ARIA grid semantics. Done — 19 component
  cases plus 3 end-to-end cases through a real browser, and an `/assets` route so CI exercises it.
  *The three requirements pull against each other,* which is the whole difficulty: virtualisation
  removes rows from the DOM and assistive technology reads the DOM. The reconciliation is that the
  container holds the truth about the collection (`aria-rowcount`, `aria-colcount`) while each
  rendered row carries its absolute `aria-rowindex`. The bug that prevents: a grid reporting its
  *rendered* rows announces a hundred-thousand-asset library as twenty items, **with no visual
  symptom at all**.
  *Two wire types landed first so the grid is typed off the generated client:* `AssetTier` (derived
  server-side from storage class + restore state) and `AssetSummary`/`AssetPage`.
  *The tier derivation carries the trap the schema warns about twice:* an **expired** restore of an
  archived object is archived again, not restored. Conflating them leaves the download button enabled
  until the day someone presses it. Nine cases, including a stale `restore_state` on an object since
  transitioned back to Standard.
  *Surprises:*
  1. Chrome re-serialises a large px length in exponential form — `height: 3e+06px`. My first
     assertion read `style.height` and reported a bug that does not exist; the layout is correct at
     3,000,000px. Now measured with `getBoundingClientRect`, plus a probe test pinning the platform
     behaviour so a future clamp would be caught.
  2. Svelte 5 flushes to the DOM asynchronously, so every synchronous `getAttribute` after a
     keypress read the *pre-keypress* state. Six tests failed for a reason unrelated to the
     component; `await tick()` throughout.
  3. `svelte-ignore` treats **every following token as another rule name**, so an inline explanation
     became thirty invented rule names and eslint rejected each one. The directive needs its own
     comment.
  *Improvement eslint prompted:* selection uses `SvelteSet` rather than cloning a plain `Set`.
  Cloning is O(n) per click, and a shift-range selection over 40,000 assets does it on every
  keystroke.
  *Note:* arrowing past an edge holds position rather than wrapping. In a grid a wrap moves the eye
  the full width or height of the viewport, which disorients rather than helps.

---

## M2 — Metadata, search, rights

Scope from ARCHITECTURE §13: metadata schema engine, taxonomies, collections, Tantivy, faceted +
shorthand search, the rights model (G4), and the eval harness (G8).

- [x] **2.1 Field definitions and validation.** `field_defs` → a validator that accepts or refuses a
  metadata payload. Every `kind` in the CHECK, `multivalued`, `required`, `read_only`. *Test first:* a
  `taxonomy_ref` field refuses a term from the wrong taxonomy.
  *Done:* `dam_core::fields` (28 pure cases) + `dam_db::fields` (5 cases, one container). The named test
  is mutation-verified: dropping the taxonomy comparison fails it.
  *Four choices worth more than the type checking.* An **unknown key is refused, never ignored** —
  ignoring returns 200 and silently discards data the user believes they saved, and they find out months
  later. **Every rejection is collected**, because a twenty-field import that reports one problem per
  attempt is twenty round trips. **`required` applies on create, not patch** — enforcing it on a patch
  makes every single-field edit a read-modify-write with a lost-update race in it. And **`ai_writable`
  restricts enrichment, not the field**: a person writes anywhere, an enrichment run only where the
  tenant said so, or one pass overwrites a caption a person wrote.
  *Refusals rather than coercions,* each because coercion hides a client bug: a bare scalar on a
  multivalued field (`"red,blue"` would become one wrong value), `"true"` for a bool, a timestamp for a
  `date` (which would acquire a timezone and move the day), a datetime with no offset (ambiguous by up
  to 26 hours — an embargo lifting on the wrong day).
  *Security:* a `url` field allowlists http/https. A `javascript:` or `data:` value in a field a UI
  renders as a link is stored XSS, and a denylist is one scheme away from wrong. Patterns are **anchored**
  (an unanchored `[A-Z]{3}` accepts `"oops ABC oops"`) and capped at 512 bytes, since a field definition
  is tenant-controlled input reaching a regex compiler on every write.
  *Lengths count characters, not bytes* — a 5-char limit rejecting `café` is a bug a European customer
  finds on their first import. Taxonomy refs resolve in **one** query for the whole payload, not one per
  term.
- [x] **2.2 Taxonomies.** `ltree` paths, move/merge/deprecate, and the rule that a deprecated term stays
  resolvable so old assets keep their meaning.
  *Done:* migration 0012 (`deprecated_at`, `superseded_by`) + `dam_db::taxonomy`, 16 cases in one
  container plus 2 unit. Mutation-verified twice: making `deprecate` delete fails the resolvability
  test, and narrowing the move's `path <@` to `path =` fails the subtree test.
  *Every operation is destructive in its obvious form,* and what it destroys is the meaning of assets
  tagged years ago. `asset_tags` cascades on term deletion, so deleting a "duplicate" term silently
  untags every asset that used it. Reparenting without moving descendants leaves them on a path whose
  prefix no longer exists, so `path <@ 'outdoor'` quietly returns nothing. Hard-deleting after a merge
  breaks every id held outside this database.
  *So:* a merge retags to the survivor and retires the source with `superseded_by`, resolution walks the
  chain (bounded at 32 hops, so a hand-written cycle cannot hang a request), and a move is **one**
  UPDATE over `path <@ subtree` — 10,000 terms in one statement with no half-moved window.
  *Refused, each for a stated reason:* a cross-taxonomy merge (it changes what an asset means, not which
  term carries it), a deprecated survivor, a cycle, a move under one's own descendant (the subtree
  detaches from the tree entirely), and deprecating a parent with live children.
  *Two things found while writing it.* An asset tagged with **both** terms would make the merge's UPDATE
  violate `asset_tags`' primary key and abandon the whole merge — over an asset that already has the
  meaning; the duplicate is dropped first. And `taxonomy_terms_slug_idx` is `UNIQUE (taxonomy_id, slug)`,
  not `(parent_id, slug)`, so **a vocabulary cannot have the same leaf label twice** — no "Other" under
  two parents. That makes path collision unreachable; the guard stays anyway. Logged as Taxonomy 3 and
  worth a product decision before a real vocabulary import.
- [x] **2.3 Collections.** Membership, ordering, `pin_hot` interaction with the lifecycle engine.
  *Done:* `dam_db::collections`, 15 cases in one container. Both key properties mutation-verified.
  *Order has to be stable.* `position` defaults to 0 with no uniqueness, so not managing it leaves every
  row at 0 and the order is whatever the planner returns — a customer's presentation reshuffling between
  page loads looks like a bug in *their* work. Positions are kept **dense** (`0..n-1`) and reads order by
  `(position, asset_id)` so even a corrupt tie is deterministic. Dense rather than sparse-with-gaps
  because a sparse scheme needs the rebalance anyway and meanwhile "is this collection well-ordered" is
  unstateable. Renumbering is one window-function UPDATE, so there is no half-renumbered window.
  *`pin_hot` is a **union**, not a flag.* An asset in several collections stays pinned while any pinned
  one holds it. Computing it per collection and letting the last writer win is the bug — and its symptom
  is a master tiered to Glacier while a live portal page still links it, appearing hours later as a broken
  image. `pins()` is one query for the whole batch, because the caller is the lifecycle worker walking
  thousands of placements.
  *Judgement calls:* a move past the end is **clamped**, not refused (a drag-and-drop reporting "47 of 30"
  is an off-by-one, and refusing loses the user's action); re-adding an asset leaves its position alone,
  so a retry cannot reorder somebody's curation; and a **soft-deleted asset is not pinned** by membership
  — the pin keeps things reachable for people, and nobody is reaching a deleted asset. Legal hold is
  separate and still blocks tiering *and* purge.
- [x] **2.4 Query IR.** One parsed representation shared by SQL and Tantivy, with the access predicate
  as an injected term rather than a post-filter (§7, §12).
  *Done:* `dam_core::query` (the IR + validation) and `dam_db::query_sql` (the SQL consumer), 14 cases +
  5 unit. Tantivy is the second consumer and lands with 2.6, together with 0.10's deferred differential
  test.
  *The access filter cannot be forgotten, structurally.* `Planned`'s only constructor takes an
  `AccessPredicate`, so **no value of the renderer's input type lacks one** — a stronger guarantee than a
  test, because it survives whoever adds a third back end. The user's query and the access predicate are
  kept as separate trees so an access term can never end up inside a `Not`, and a test asserts the filter
  precedes any `NOT (` in the rendered SQL. Mutation-verified: short-circuiting `Query::All` to `true` —
  the obvious optimisation — fails immediately.
  *The jsonb trap, measured rather than assumed.* jsonb's "an array contains a primitive" rule applies
  only at the **top level**: `'{"c":["red","blue"]}' @> '{"c":"blue"}'` is **false**, while
  `@> '{"c":["blue"]}'` is true. So the obvious single-`@>` equality silently misses every multivalued
  field — most tag-like fields — with no error anywhere. Both forms are emitted; both use the GIN index.
  *`!=` renders as `NOT (@>)`, not `<>`*: `<>` compares the whole array, so "not red" would match an
  asset tagged red *and* blue.
  *Other things that would be quietly wrong:* an empty `Or` must render `false` (rendering nothing drops
  the filter and returns the tenant's whole library); ranges cast before comparing (`'9' > '10'` is true
  as text); `LIKE` metacharacters are escaped **backslash-first** with an explicit `ESCAPE` (unescaped,
  `contains("50%")` becomes a prefix match on "50"); `Missing` covers absent, `null` **and** `[]`; and
  taxonomy queries match `confirmed` tags only, so an unreviewed AI suggestion cannot affect results.
  *Depth and node bounds* are checked before rendering — both consumers recurse, so a few kilobytes of
  nested boolean is a stack overflow, not a slow query. `depth()` itself is iterative, or the check would
  overflow while measuring.
  *Left for review:* rule-based asset groups stay refused. The IR exists now, but wiring it creates a
  recursion ARCHITECTURE does not settle — a group's membership defined by a query whose access predicate
  is defined by group membership. See NEEDS-REVIEW.md; it contradicts Decision 4, so I did not take it
  unilaterally.
- [x] **2.5 Shorthand search syntax.** `bra:acme`, quoted phrases, ranges, negation. *Test first:* an
  unclosed quote is a parse error with a column, not a silent whole-string match.
  *Done:* `dam_core::shorthand`, 34 pure cases. Parses to the **same** `Query` the API accepts, so there
  is no second query language and no second place for validation to differ.
  *The named test, mutation-verified:* letting an unclosed quote fall through makes `"beach holiday`
  a search for the literal text — which returns nothing and explains nothing. The error reports the
  **opening** quote's column, because the missing character is at the end but the one to look at is the
  quote you opened. Columns are 1-based and counted in **characters**; switching to bytes fails a test
  with `café` in it, since a caret under byte 7 points at the wrong character.
  *Case-sensitive operators:* `OR` is an operator, `or` is a word. A user typing `cats or dogs` means the
  word, and a case-insensitive keyword would make it unsearchable short of quoting. `AND` binds tighter
  than `OR`; getting that backwards silently answers a different question.
  *Two exceptions stop the shorthand becoming a trap:* anything containing `://` is text (pasting a URL
  into a search box is common), and a key that is not `field_defs`-key-shaped is text, so `9:30` and
  `Ratio:16` search rather than fail. An unknown key that *does* look like a field is an error naming it.
  *Also:* a leading `-` negates but an internal one is a hyphen (`sold-out` is a word); a quoted value
  suppresses every operator meaning including `:`; `field:*` is presence and `field:-` absence; ranges are
  `a..b`, `>`, `>=`, `<`, `<=` with the exclusivity the symbol implies; values are typed from the field
  kind, so `year:2026` is an integer and `brand:2026` is text; and input length and paren depth are
  bounded before parsing, since a search string arrives in a URL and the parser recurses.
- [x] **2.6 Tantivy index per tenant.** Schema derived from `field_defs`, an LRU writer pool (§19), and
  a cold-open path. *Test first:* 1,000 tenants do not open 1,000 indexes.
  *Done:* `dam_search::{schema, pool, document, query}` — 8 pool cases, 3 differential cases (60
  back-end comparisons), 3 schema unit. **This also closes 0.10's remainder.**
  *The named test passes at 32 open for 1,000 tenants,* and the half that makes it mean something is the
  second assertion: an evicted tenant is reopened and its committed document is still there, and is *that*
  tenant's document. Eviction closes an index; it does not delete one.
  *A bug the first version of that suite hid.* `concurrent_first_requests_...` passed with a
  get-then-insert that had a real race in it, because `#[tokio::test]` is single-threaded and the open was
  synchronous — so sixteen tasks never overlapped. Under `flavor = "multi_thread"` it reports **16 cold
  opens instead of 1**. Fixed with `try_get_with`, and the open moved to `spawn_blocking` — a synchronous
  index open on an async worker stalls every request sharing that thread, precisely when the tenant is
  already slowest.
  *Metadata is one JSON field, not one per definition.* A Tantivy schema is fixed at creation and
  `field_defs` is not, so per-definition fields would make **every field addition a full reindex** — hours
  for a large library. A test asserts the schema is unchanged by adding definitions.
  *Tantivy ranks; Postgres authorises.* Group membership is indexed and changes in Postgres immediately, so
  between the two the index is **permissive**. The rendered predicate narrows candidates; it is not the
  authority. Logged as Search 1, and 2.7's facet counts inherit the same staleness.
  *Clauses the index cannot answer are refused, not dropped* — taxonomy, collections, substring. Dropping a
  filter clause returns *more* than asked, and the extra rows look like ordinary results.
- [x] **2.7 Faceted search.** Fast fields, counts that respect the access predicate.
  *Done:* `dam_db::facets`, 13 cases in one container. `field_defs.facetable` now reaches `FieldDef`.
  *Counts are computed in SQL, not the index, and that discharges Search 1's caveat.* A result list can be
  re-filtered on hydration, so a stale index only costs an extra id. **A facet count cannot be
  re-filtered — it *is* the disclosure.** `brand: Acme (5)` shown to someone who may see three tells them
  two exist that they cannot, which is exactly §7's "pagination counts alone disclose". A facet rail is a
  pagination count with better presentation. If profiling later says this is too slow, the fix is fresh
  group membership in the index, not counts from a stale one.
  *A value with no visible assets produces **no bucket**, not a zero one.* A zero bucket discloses that the
  value exists, and for a `client` or `campaign` facet that existence is usually the sensitive part —
  the count is beside the point. Buckets come from the filtered set, never from enumerating a field's
  values.
  *Two `DISTINCT`s that each fix a visible bug,* both mutation-verified: without the first, an array
  repeating a value counts it twice; without the second, an asset tagged with two leaves under one ancestor
  makes the rollup read `outdoor (3)` over a library of two.
  *Governance:* only `facetable` fields, and geo is refused even when marked — a coordinate has no discrete
  values, so it would produce one bucket per asset. Truncation is **reported**, because a rail that
  silently truncates makes "no other brands" and "ninety other brands" look identical.
- [x] **2.x Test-gate throughput, and the TLS defect it exposed.** The Rust gate had grown past ten
  minutes — measured: a Postgres container takes **6.2 s** to become usable, a SeaweedFS one **14.2 s**, and
  all twelve migrations together **0.5 s**. With 19 Postgres-backed and 8 S3-backed suites (75
  `Harness::start()` call sites) startup *was* the run, and starting twenty at once made each ~5× slower
  than alone (`uploads_repo`: 6.2 s isolated, 37.3 s in the full run).
  *Postgres is now shared* via `dam_db::testing::SHARED_URL_ENV` (`DAM_TEST_PG_URL`) — a **database** per
  harness, so isolation is unchanged and the cost is milliseconds.
  *SeaweedFS is deliberately **not**.* Sharing it was built, measured, and reverted: a full run creates ~75
  buckets, each a SeaweedFS *collection*, and past some point the instance stops accepting writes to new
  ones — the same container that had served fifty suites failed **eleven of the next twelve**.
  `-volume.max=200` changed nothing, so the volume count is not the limit. A gate that fails one run in
  five is worse than one that is slow.
  *The TLS defect, fixed at the source.* `aws_sdk_s3`'s default client enables the platform root store, and
  `aws-smithy-http-client` loads it **once per process** into a `LazyLock`; one failed macOS keychain read
  then trips `debug_assert!(valid > 0)` for **every later client construction in that binary** — which is
  why nine cases failed together, one of them never opening a connection. Plain-`http://` endpoints now get
  a connector with no TLS at all. `https://` keeps the platform store, since an empty one rejects
  everything and a private CA is only findable there. Every self-hosted deployment was paying for a root
  store it could never consult.
  *Also fixed:* **a created bucket is not a writable one** — SeaweedFS allocates volumes lazily and the
  first PUT can 500 past all five client retries, so the harness now probes with a write until it succeeds.
  This is the same layered-readiness pattern as the bucket-create retry, one level up, and it removed
  flakes in the per-container mode too.
  *And a naming trap:* `DAMRS_TEST_PG_URL` broke **every** config load — `Config::load` claims that whole
  prefix and refuses unknown keys. The strictness is right; the name was wrong.

- [x] **2.8 Rights model (G4).** Licences, scopes, releases, and the distribution chokepoint that D12
  requires — enforced, not recorded.
  *Done:* `dam_core::rights_eval` (30 pure cases) + `dam_db::rights` (14 cases, one container). The
  chokepoint that *enforces* it is 3.1; this is the calculation and its cache.
  *Four properties, all mutation-verified.* **Intersection, not union** — attaching a permissive licence
  must not launder another's restrictions; under a union it does, and the test catches it. **Unknown
  denies** — no licence is `unknown`, not a soft yes, because the cost of guessing wrong is a rights claim.
  **Exclusions beat inclusions** — "worldwide except China" has `WORLD` in the inclusion list, so checking
  inclusions first grants China. And **`Expiring` is a verdict that still permits distribution** — a
  warning that blocks is a denial with extra steps, and people route around it.
  *A bug in my own first version:* I took the **shortest** renewal-notice window across licences. A licence
  needing 90 days to renew then reads as merely allowed until renewing it is no longer possible. It is the
  longest now.
  *Other things that would be quietly wrong:* a licence with **no scope grants nothing** (distinct from a
  scope with empty lists, which grants everywhere — conflating them turns a half-configured licence into
  blanket permission); scopes *within* a licence are alternatives while licences are conjunctive; a cap of
  `Some(0)` is "none permitted", not "uncapped"; a release that lapsed on the clock denies even while its
  `status` column still says valid, because that column is worker-maintained and trusting it distributes on
  the strength of a job that has not run; withdrawn consent denies regardless of term; and the AI gates are
  answered **independently of the distribution verdict**, since a territorial restriction says nothing about
  internal cataloguing.
  *The cache never serves a stale `allowed`* — the expiry check is in the query, and `expires_at` is the
  earliest instant a verdict could change on its own. A miss recomputes rather than denying, or the first
  download of the day fails for every asset and people learn to retry instead of read the error.
- [x] **2.9 Search eval harness (G8).** `relevance_judgements` → nDCG/MRR over a fixture corpus, wired
  so a ranking change reports its effect instead of being argued about.
  *Done:* `dam_core::eval` (16 pure cases), `dam_db::judgements` (10 cases), `dam_search::query::search`
  — the ordered accessor (4 cases), `dam_search::eval_run` (3 cases), and `damctl eval`. Verified by
  running it: a two-asset corpus scored `mean nDCG 0.8155 / MRR 0.7500`, `--min-ndcg 0.95` exited
  non-zero, and a corpus query naming a removed field was reported rather than dropped.
  *The metric refuses to flatter itself.* Four separate places where the obvious implementation reports a
  perfect score over nothing: **0/0 nDCG is `None`, never 1.0** (an unjudged corpus would otherwise report
  perfect relevance); **`for_query` returns `None` rather than an empty set** for an unjudged query,
  because an empty `Judgements` is *scoreable*; a query whose every judged asset was **soft-deleted becomes
  unjudged**, so deleting labelled assets cannot improve the numbers; and `--min-ndcg` **fails on `None`**
  rather than passing, because "nothing to measure" as a pass is how a gate stops gating.
  *Unjudged results score zero, they are not skipped* — skipping them means a ranking that returns junk
  scores the same as one that returns nothing, and the ideal DCG uses **every** judgement rather than only
  the returned ones, or a ranking is compared against its own output.
  *A refused query is reported, never dropped.* `Run::is_trustworthy` is false while any query failed to
  parse or plan, and `damctl eval` exits non-zero: a corpus that silently skips its broken queries scores
  *better* the more of it breaks, which is worse than having no harness.
  *`eval.rs` lives in `dam-core`, not `dam-search`* — `dam-search` already depends on `dam-db`, so loading
  judgements from a module inside `dam-search` is a dependency cycle. The compiler found it.
  *Two things 2.9 needed that did not exist:* Tantivy had no **ordered** accessor at all (the differential
  suite compares sets and says so), and there was no way to **build an index from Postgres** — so
  `dam_search::reindex` and `damctl reindex` landed here too, with 3 cases: tombstones are indexed with the
  flag set so an undelete is a flag flip rather than a reindex; the walk is cursored on `assets.id` because
  a LIMIT/OFFSET walk over a table taking inserts both skips and repeats rows; and the index is replaced in
  one commit, so three reindexes leave one document rather than three.
- [x] **2.10 Bulk operations (G18).** `bulk_operations`, dry-run first, per-item outcomes, resumable.
  *Done:* `dam_db::bulk` — 15 cases over one container, all eight load-bearing properties
  mutation-verified.
  *A real bug in my own first version, found by a mutation that survived.* `next_batch` cursored on
  `resume_after`, a high-water mark of the greatest asset id recorded. A worker that fans a batch out
  concurrently records in *completion* order, not id order — so recording the highest id first stepped the
  cursor past every lower pending item. They could never be served again, `done + failed = target` would
  never hold, and the operation could not legitimately finish. Selection is on **item state** now, which
  cannot skip a row it has not seen an outcome for; `resume_after` stays as the progress marker an operator
  reads. Migration 0013 puts `asset_id` in the pending partial index so the batch order comes from the
  index. The mutation that exposed this (dropping the cursor) had *passed*, which is what sent me looking.
  *The state is derived from the counters, never chosen.* `finish` computes `failed`/`partial`/`completed`
  in SQL, so a caller cannot report an operation with 9,000 failures as green — the thing somebody
  discovers a month later.
  *A dry run writes nothing at all,* not even a `bulk_operations` row: an abandoned row would sit in the
  actor's history, which is exactly where somebody looks to find what they actually ran.
  *Other things that would be quietly wrong:* a repeated id from a multi-page selection is **deduplicated
  and the target count corrected**, or the primary key aborts the whole operation and `done + failed =
  target` never holds; `record_outcome` counts **only an actual transition**, so a retried worker cannot
  inflate the counters past the target; `skipped` counts as neither done nor failed and carries its reason,
  because a silent skip is indistinguishable from a bug; an operation over **zero targets is refused**
  rather than recorded as instantly complete; the error sample is bounded at 20 while the full list stays
  queryable row by row; and cancelling **rolls nothing back** and says so, because a bulk tag over 31,000
  assets cannot be undone by a cancellation without a second bulk operation.

## A — The HTTP API surface

Unnumbered in the original plan and discovered to be missing when the UI needed something to talk to.
The backend was complete through M3 and `damd` had no server at all: only the TUS and delivery
routers existed, and neither was mounted anywhere.

- [x] **A.1 The server, and one place that composes it.** `dam_api::app::router` merges every feature
  router and applies the layers that must wrap *everything* — a request timeout, a JSON body limit,
  `X-Content-Type-Options: nosniff`, CORS, tracing. `damd` binds, serves, and drains on SIGTERM as well
  as SIGINT, because a container runtime sends SIGTERM and a server that only handled Ctrl-C would be
  killed rather than drained on every deploy.
  *Composition in one file is the point:* "is this endpoint authenticated" is answerable by reading it,
  and a route added without a timeout becomes visible rather than inheriting nothing.
  *`/health` says `ok` and nothing else.* A health endpoint reporting version, tenant counts or database
  state is an unauthenticated disclosure endpoint, and it is the first thing anybody scans.
  *`damd` refuses to start when the delivery tenant is ambiguous,* because that path still resolves its
  tenant from configuration rather than from the signed token. Serving several from one process would
  mint URLs against the wrong tenant's objects, which would look like a caching bug. Hit for real on a dev
  database that had grown a second tenant — so `server.delivery_tenant` names it, and the refusal (which
  now lists the slugs it found and the variable to set) is only for unset-and-ambiguous.
  *CORS is permissive outside production and configured inside it.* Defensible because the credential is
  a bearer token in a header rather than a cookie: a cross-origin request without the header is
  anonymous. Written down rather than left as an unexamined `Any`.
- [x] **A.2 One authorization path for every handler.** `dam_api::caller::authorize` authenticates the
  bearer token, loads grants across the D2 boundary, compiles the predicate through the *same*
  `policy::compile` every other consumer uses, and refuses a caller whose scope matches nothing.
  *Three unit cases on the header alone,* which sounds trivial and is not: the scheme is
  case-insensitive per RFC 9110 so `bearer` must work, and a key pasted from a terminal carries a
  trailing newline whose hash is not the key's hash — producing a 401 that survives every attempt to
  check the credential.
  *A machine key grants nothing.* No identity means no membership means no roles. Fail-closed, and the
  safe direction for a shape the role model does not yet describe.
- [x] **A.3 `GET /assets`, `GET /assets/{id}`, `PATCH /assets/{id}/metadata`.** `dam_db::assets` does
  the reading; 14 database cases and 16 HTTP cases.
  *§7's leak is the property the whole read layer is shaped around:* the total is counted by a window
  function **in the same statement as the rows**, under the caller's own predicate. A post-filter
  returns exactly the right rows and leaks through `total` — a caller learns their library has seven
  assets somebody has hidden from them. Mutation-verified: counting without the predicate fails two
  cases.
  *A real bug found while writing it:* a plain `LEFT JOIN` to `object_placements` returns a replicated
  asset once per placement — the primary key is `(object_key, pool_id)` — so a two-pool asset appeared
  twice in the grid *and* twice in the window count. A `LEFT JOIN LATERAL … LIMIT 1` makes one row per
  asset structural, and it picks the **warmest** present copy: ordering by class alone would let a Deep
  Archive replica of a hot object report `archive` and disable a download that would have worked.
  *The ORDER BY tie-breaks on `assets.id`,* because `created_at` is not unique and an offset walk over a
  non-total order skips and repeats rows between pages — a virtualised grid scrolling back would show
  different assets, which reads as data corruption. The test needed **600 rows sharing one timestamp**:
  at twenty rows Postgres returns a stable physical order and the case passed with no tie-break at all.
  *A metadata PATCH validates as a patch* (`Mode::Patch`), so editing one caption does not demand every
  required field — and clearing a required field is still refused, with the stable code a UI maps to a
  message. The read, the validation and the write are one transaction: two would let a concurrent edit
  land between them, and the loser's merge would silently revert the winner rather than conflict.
  *404 for a forbidden asset, on read and on write alike.* A 403 confirms the asset exists.
- [x] **A.4 `GET /search`, `GET /search/facets`.** Shorthand → IR → Tantivy, hydrated through Postgres.
  *Tantivy ranks, Postgres authorises,* and both halves run: the predicate is rendered into the index
  query *and* applied again when the ids are hydrated. The index carries group membership so it can
  narrow, but it is eventually consistent and therefore *permissive* while stale — and a permissive
  stale index used as the gate on a governed library is a leak.
  *The ids are overfetched 4×,* because hydration drops rows: without it a page would be short whenever
  anything was filtered, and a grid reads a short page as the end of the results.
  *Facets are counted under the same query the results were,* or a rail says "240 outdoor" beside three
  visible assets. One `plan` helper serves both handlers for exactly that reason.
  *A clause the index cannot answer is 501, not 400.* The query is valid and this back end cannot answer
  it; a 400 sends somebody looking for a typo that is not there. And it is refused rather than dropped,
  because a dropped filter returns *more* than was asked for.
- [x] **A.5 `damctl issue-key`, `damctl reindex`, `damctl eval`.** The three commands that make a
  deployment usable: a credential to talk to the API with, an index built from Postgres, and the eval
  harness run against it. The key's plaintext is printed once and never stored — only a hash — so it
  cannot be recovered from a database backup. Its prefix goes to the log; the secret goes to stdout and
  nowhere else.
- [x] **A.6 `GET /fields`.** Added because the UI needed it and nothing else would do. The metadata editor
  has to know each field's **shape** — `multivalued` decides whether a value is a string or an array — and
  an earlier version inferred the field list from the facet keys, which carry no shape. It sent
  `"blue, red"` to a field that takes an array and the server refused it with a message about delimiters
  the user could do nothing with.
  *Found by editing a multivalued field in a real browser,* not by any test that existed at the time. Four
  e2e cases now cover it: the array is sent as an array, a read-only field is not offered, a required field
  is announced as required rather than only marked with an asterisk, and the comma hint is bound to the
  field with `aria-describedby`.
  *`Read`, not `Manage`:* a schema is not secret and every reader needs it to render a form or a rail.
- [x] **A.7 Thumbnails, and the rights decision they needed.** Answered: *"we should see thumbnails."*
  Implemented as a `Purpose` **signed into the delivery claim** rather than as a bypass — `Distribution`
  or `InternalPreview` — with the token format version bumped 2 → 3.
  *`InternalPreview` skips the rights verdict and nothing else.* Still signed, still verified at the
  chokepoint, still access-checked, still the only path to the bytes. D12's "one code path" holds; the
  chokepoint now knows what it is being asked for.
  *Three restrictions, checked at the mint and again at delivery:* a known built-in profile only (so never
  the original, never a typo, never a future tenant-defined render), an identity required, a share link
  refused — a share is distribution by definition.
  *A v2 token is refused rather than defaulted.* Defaulting a missing purpose is wrong in a different
  direction each way: `Distribution` breaks every preview URL, `InternalPreview` lets a token minted
  before the field existed skip the rights check.
  *Downloads are unchanged,* and one case asserts both on the same asset in the same run: the preview is
  served, the download is 403.
  *All seven mutations of the restrictions now fail a test.* Two survived the first pass, and both were
  tests passing for the wrong reason — the share-link case invented a share id that did not exist, so
  `shares::is_live` refused the token before the restriction was consulted; and the role check was
  **unfalsifiable**, since every built-in profile is proxy-class. The share is live now and the branch is
  gone rather than left untested.

## P — The ingest pipeline

0.9's remainder, and the reason nothing had a thumbnail. `dam_db::jobs` had the queue — leases,
round-robin fairness, dedupe keys, attempt counting — and nothing consumed it, so every stage that is "a
job" was unreachable: an upload landed in staging and stopped there.

- [x] **P.1 `dam-pipeline`, and why it is a library.** Finalisation, derivation and the queue consumer.
  Both stages need a real object store *and* a real database, and an integration test cannot reach a
  binary's private modules — so it is a crate, which also lets `damctl` run a stage by hand and makes a
  stuck asset recoverable without a queue.
  *Every stage is idempotent, because the queue leases rather than deletes.* At-least-once is the design —
  a SIGKILLed worker must not lose its work — so a stage that is not safe to run twice turns that into
  duplicate assets. Finalisation keys on the session's `asset_id`, derivation on `(asset_id, op_hash)`.
  *A permanent failure skips its remaining attempts.* A malformed file will not parse on the fifth try, and
  this mattered immediately: the first real upload failed on a missing storage pool and landed in `dead` in
  one step with an accurate message rather than after twenty minutes of backoff.
- [x] **P.2 Finalisation: a staged upload becomes an asset.** Built on `dam_media::ingest::finalize`, which
  already validated and promoted — heading the object, refusing an empty or oversized one from the HEAD
  alone, sniffing from a ranged prefix, hashing in **bounded windows** so a 200 GB master never
  materialises in memory. Deliberately not reimplemented: a second hashing path is a second answer to
  "what is this object's digest".
  *Deduplication falls out of content addressing rather than being a feature.* Two uploads of one file are
  two assets sharing one object — asserted, because they have different filenames, metadata and rights.
  *A real bug found by running it:* promoting an object and recording an asset cannot be one transaction,
  so a failure between them left the bytes promoted, staging gone, and no asset row — and the retry failed
  **permanently** with "object not found" on an upload whose bytes were safely stored the whole time.
  Migration 0014 adds `upload_sessions.content_hash`, written the moment the promotion succeeds; a re-run
  reads it, skips the promotion, and records the asset. Covered by a case that winds a session back to
  exactly that state.
- [x] **P.3 Derivation: thumbnail, preview, master proxy.** Pure-Rust renderer first, libvips for the
  formats §18.2 puts behind it — inside `dam_media::sandbox`, because libvips marks 14 of its own loaders
  untrusted. The original is read **once** for all three profiles, which is §18.3's budget.
  *A design error the tests caught:* a text file came back as a *transient* failure, so the queue would
  have retried a `.txt` five times and dead-lettered it. A format no renderer can read is now **reported,
  not failed** — a DAM stores documents, and the grid draws a placeholder. A *missing tool* is the case
  that does fail the job, because that is a deployment mistake rather than a fact about the file.
- [x] **P.4 The worker.** `dam-worker` claims, dispatches, completes or fails, and drains on SIGTERM —
  dropping claimed jobs would leave them locked until the lease lapsed, a two-minute stall per deploy.
  Finalisation queues derivation, derivation queues indexing, in that order so an asset reaching search
  already has a thumbnail to draw. An unknown job kind is **permanent**: it means version skew, and
  retrying will not teach this binary a job it does not know.
  *Polling, not `LISTEN`/`NOTIFY`,* which would need a dedicated connection per worker *and* a fallback
  poll anyway — a notification delivered while nobody listened is lost, and a job that becomes runnable
  through `run_after` generates none at all.
  *Indexing is incremental:* the asset's own document is deleted and re-added, rather than rebuilding the
  tenant's index, which would make ingest cost time proportional to the library.
- [x] **P.5 The gaps running it exposed.** Four, each fixed at its source:
  1. **Provisioning created no storage pool,** so a tenant `provision-tenant` had just made could not
     ingest at all. Now created with the tenant row, from the deployment's own configuration, with
     `credentials_ref` a *reference* — a credential in that column is a credential in every backup.
  2. **A delivery URL was a bare token.** `server.public_url` makes them absolute; without it they are
     root-relative and the client resolves them against the API base it already holds. A browser resolving
     `/d/<token>` against a Vite port gets a 404 from the wrong server.
  3. **The delivery route read tenant tables on the global pool** — `relation "derivatives" does not
     exist`, invisible until a real derivative existed. It gets a pool pinned to the delivery tenant, and
     the *second* `DeliveryState` that caused it is gone: there is one now, shared with the asset endpoints
     that mint preview tokens, which also removes the two-keyrings failure waiting to happen.
  4. **`RUST_LOG` named the project, not the crates,** so every binary logged nothing while looking
     configured. The worker ran a full job chain silently.

*Verified end to end in a browser:* a PNG uploaded through the UI → `finalise_upload` → `derive` →
`index`, all three jobs `succeeded`, three derivative rows, and a 15 KB WebP thumbnail fetched through the
signed chokepoint and rendered in the grid.

*Still not wired:* re-enrichment, lifecycle transitions, notification firing and the restore poller are
all jobs this worker will dispatch, and none has a handler yet.

- [x] **A.x A §12 divergence, found by running the thing.** `bra:acme` returned **22** results through
  Tantivy and **11** through SQL on the same corpus. Not a leak — both sides were inside the access
  predicate — but it is precisely what §12 forbids: the same query returning different rows depending on
  which back end served it.
  *Cause:* the metadata JSON field was indexed with the **default** tokeniser and `json_term` lowercased
  its literal to match. So a stored `"Acme Corp"` indexed as `acme` + `corp`, and `brand:acme` matched it —
  while SQL renders equality as jsonb containment, which compares the whole value, case-sensitively.
  *Fix:* the JSON field takes the **raw** tokeniser and the literal is passed verbatim. Free text is
  unaffected: it searches the separate `text` blob, which stays analysed. One field for matching values
  exactly, one for searching prose.
  *Why the differential suite missed it:* every fixture value in it was a single lowercase word. It now
  carries a multi-word, mixed-case one, plus four cases — one word of a two-word value, the whole value,
  the wrong case, and a two-word value inside a multivalued field. Both halves of the fix are
  mutation-verified: restoring either the tokeniser or the lowercasing fails the suite.

## F — The UI

Continues the frontend track. F.1–F.4 built the scaffold, the token vocabulary, the generated client and
the grid against sample data; these wire it to the API and make it a thing a person can drive.

- [x] **F.5 The application shell and the connection screen.** Nav with `aria-current="page"`, and a
  `/settings` page that stores an API key and *proves* it works before letting the user leave.
  *The check is two requests, deliberately:* `/health` first, then an authenticated `/assets`. One request
  cannot tell "wrong port" from "wrong key", and somebody re-issues a credential that was fine. A third
  case is called out separately — a key that authenticates and grants nothing is a machine key with no
  membership, and saying "no permission" rather than "bad key" is the difference between fixing it and
  re-issuing it.
  *A key that fails is not stored.* A stored key that does not work produces a 401 on every screen and
  reads as a broken app rather than an unconfigured one.
  *The exposure is written down rather than hidden:* the key lives in `localStorage`, which script on this
  origin can read. A cookie would move the risk (CSRF) rather than remove it; the real fix is a session
  endpoint, which is a backend change. Until then only the key's *prefix* is ever displayed — the same
  thing `api_keys.key_prefix` stores and an audit log shows.
- [x] **F.6 The asset browser.** Search box, facet rail, grid, detail panel, upload, all against the live
  API. 29 e2e cases through a real browser, every route axe-scanned at WCAG 2.1 AA.
  *One query string, and the URL owns it.* The box, the rail and the address bar hold the same value, which
  is what makes a search shareable and the back button work — and it removes the class of bug where a
  rail's selection and a box's text disagree about what is being shown.
  *The rail writes shorthand, with quoting, and that is tested:* `brand:Acme Corp` parses as a brand filter
  plus the free text "Corp" — the wrong assets, and it looks like a search bug rather than a quoting one.
  13 unit cases on the composer alone, including a hand-typed `year:>2020` the rail must **not** rewrite
  when a facet is clicked.
  *`truncated` is rendered.* A rail that silently cuts off makes "no other brands" and "ninety other
  brands" look identical.
  *Two bugs the browser found that no component test would have:*
  1. **"Saved" flashed and vanished.** The editor diffed against its `values` prop, so a successful save
     updated the parent, the parent re-rendered, and the seeding effect cleared the confirmation. It diffs
     against its own copy of the document now.
  2. **A single click did not open the panel.** The grid's selection lived in a `SvelteSet` inside the
     component with nothing outside able to read it. It reports `onselect` now — lifting the selection into
     the parent instead would clone a set on every click, which a shift-range over 40,000 assets makes
     O(n).
- [x] **F.7 The metadata editor.** The server is the validator; this puts its answer next to the field it
  names, with `aria-invalid` and `aria-describedby`. An error in a banner at the top of a twenty-field form
  is one a screen-reader user meets after leaving the field that caused it.
  *A patch, not a document.* Only changed fields are sent, an emptied box is an explicit `null` (the
  server's "clear" instruction), and the panel re-seeds from the *server's* normalised document — a date it
  reformatted has to be what the panel shows, or the next read looks like an unexplained change.
- [x] **F.8 The upload queue.** TUS by hand, ~130 lines, no `tus-js-client`.
  *The offset always comes from the server.* After any failure the next `PATCH` starts at whatever `HEAD`
  reports, never at what the client last sent — trusting the client's counter either re-sends bytes the
  server has (slow) or skips bytes it does not (corrupt, and only visible when someone opens the file). A
  409 re-reads rather than failing, which is the whole point of a resumable protocol.
  *One upload at a time,* because eight-MiB chunks times six parallel uploads is six times the memory and
  the multipart state for no gain on one connection — and six bars at 40% tell a user nothing about when
  they can leave.
  *A failure keeps its place in the queue* and resumes, rather than being dropped.
  *`btoa` is Latin-1 only,* so `Upload-Metadata` goes through `TextEncoder` — otherwise a filename with an
  accent or an em dash throws, and in a DAM those are the normal case.
  *Verified by uploading through the browser:* 120,000 bytes, create → PATCH → `HEAD` reporting
  120000/120000, and the `upload_sessions` row to match.
- [x] **F.9 The lightbox.** `onactivate` was wired and unused; now Enter on a focused cell opens the asset
  full-screen, and `preview-1024` exists to show in it because P.3 renders it.
  *A real `<dialog showModal()>`, not a div.* The focus trap, the inert background, `Escape`, the top layer
  and `::backdrop` all come from the platform — and the trap is the one hand-rolled modals reliably get
  wrong, because a keyboard user tabs straight out of a div "modal" and operates a UI they cannot see. The
  one thing `<dialog>` does not do reliably under a framework is restore focus when the element is
  *destroyed* rather than closed, so that is explicit.
  *`preview-1024`, not the thumbnail and not the original.* The thumbnail is a 256px square crop, so
  enlarging it is a blurry crop of the wrong aspect; the original may be a 200 MB TIFF the browser cannot
  decode and fetching it to glance at spends the customer's egress. The preview is `Contain`-fitted, which
  is what it was defined for.
  *Arrow keys step without closing,* because reviewing a shoot is forty frames in a row and a lightbox that
  must be reopened per frame is one nobody uses. Bounded by the loaded page rather than the whole result
  set: an arrow key that sometimes pauses for a fetch feels broken, so the controls are hidden at the edges
  and the affordance matches what it can do.
  *No preview says why* — cold storage or not yet rendered — because an empty frame reads as a broken image.
  *`preview_url` is on the detail endpoint only.* A list of sixty preview URLs would mint sixty tokens for
  images no grid draws.
- [x] **F.10 A real design pass, and one argument that decides it.** The shell was default-Tailwind
  utilitarian. It is now dark by default, with a complete light palette beside it.
  *Dark is an argument about images, not taste.* Every serious image tool is dark — Lightroom, Capture One,
  Bridge, Resolve — because a bright surround biases colour and tone judgement. A librarian deciding whether
  a proof matches a brand colour does that against whatever chrome we put around the image, so a light-grey
  UI makes the app an instrument that lies. Light stays fully supported because the Drupal picker (§11.2) is
  a guest inside Drupal's own admin theme and cannot impose a dark surround.
  *Every token pair is defined in both palettes,* and none only inside a media query — a colour defined only
  there never applies in the un-stamped "system" state, and the page renders one theme's text on the other
  theme's ground. Each hue also has values tuned per ground: a foreground clearing 4.5:1 on a dark tint is
  not the one that clears it on a light tint, which is how the first version measured 3.9:1 while looking
  fine.
  *The axe contrast scan now forces **both** themes,* plus both `data-theme` stamps against the opposite OS
  setting. It previously scanned whichever scheme Chromium defaulted to, so the dark palette was never
  contrast-checked at all — which is precisely how a broken dark theme reaches a release.
  *New tokens the old set was missing:* `--color-raised` for a control on a surface, `--color-line` for
  hairlines (chrome borders were borrowing the *state* neutral, which F.2 reserved for the vocabulary), and
  `--color-accent-fg` because `text-white` on a light accent is the classic 3:1 failure.
  *An image well, as a checkerboard.* It disambiguates a transparent PNG from one with a white background —
  the thing the convention exists for — and it solves a defect the first dark screenshot exposed: a dark
  photograph on a dark cell has no visible edge, so the grid looked like one empty box beside eleven
  placeholders while the image was loading perfectly.
  *Reduced motion is honoured globally* rather than per component, because a component-level
  `transition-colors` cannot be switched off by a user who asked for less motion, and a grid of forty
  thousand cells has enough hover states for that to matter.
- [x] **F.11a The bulk-operations bar, and the executor it needed.** 2.10 built the bookkeeping and nothing
  drove it — a created operation sat `queued` forever. The full slice landed together.
  *`dam_pipeline::bulk_exec`* drives an operation to its terminal state: batch, apply, record, repeat, then
  derive the state from the counters. Two kinds are executable — `metadata_set` and `delete` — and the rest
  of the schema's vocabulary is refused **by name**, permanently: an operation that "completed" while doing
  nothing puts a success in the history for work that never happened. Legal hold blocks a bulk delete per
  item with its reason (`Skipped("legal hold blocks deletion")`), the guards live in the UPDATE's own WHERE
  so a hold cannot land between check and change, and an invalid metadata patch fails **before any item** —
  it is the same patch for every asset, so it would fail all 40,000 identically. Changed assets — and only
  changed ones — are re-queued for indexing, or a bulk delete leaves ghosts in every search result. Lease
  renews per batch; `LeaseLost` surfaces as "stop, another worker owns this". Seven mutations, all caught.
  *The API (`/bulk/preview`, `/bulk`, `/bulk/{id}`)* filters the client-assembled id list through the
  caller's **Manage** predicate in every request — a caller scoped to one group must not bulk-delete another
  group's assets by guessing ids. Out-of-scope ids fall out silently and are reported as a *count*, never a
  list (§7 applied to writes). Preview and creation share the filter, so the dialog's number is the
  operation's number. A selection with nothing manageable is 422, not an instantly-completed no-op. Four
  endpoint mutations, all caught — including status requiring Manage rather than Read.
  *The bar* appears with a selection, previews before every confirm, polls the worker's progress, and renders
  `partial` as exactly that — the named failures, no green tick. Changing the selection abandons an
  unconfirmed dialog, because the previewed numbers are for another set.
  *A reactivity bug the e2e caught:* the abandon-on-selection-change effect also read `flow`, so setting
  `flow` to `confirm` re-triggered it and the dialog closed itself in the same tick. `untrack` is
  load-bearing there, and the comment says so.
  *Verified against the real stack:* two assets selected in the browser, previewed, confirmed, executed by
  the real worker — 25 → 23 live, operation `completed 2/2`, index jobs run.
  *`_on` variants* landed across `dam_db::bulk` so the executor and the API run on tenant-scoped
  connections; the pool versions are now thin wrappers, and the existing suite passed unchanged.
- [x] **F.11b·1 Share links, end to end.** The API grew a share surface (`POST/GET /shares`,
  `DELETE /shares/{id}` under Manage, with the selection filtered through the caller's own scope) and an
  unauthenticated portal (`POST /share/{token}`, `POST /share/{token}/download`) where the token is the
  credential: 256 random bits shown once, a BLAKE3 digest stored, passcodes argon2id and carried in the
  POST body. The portal's preview and download both go through `issue_for_share` — a full Distribution
  rights check — and the download limit is consumed only after rights pass, atomically. 10 API cases in
  one driver; 4 portal mutations all caught. The UI: a SharePanel in the detail panel (link shown once,
  like an API key), a `/shares` management table with revoke, and a public portal page with no app chrome
  (passcode form on 401, the server's refusal text verbatim, download gated on `download_allowed`).
  8 e2e cases including axe on both portal states; one real bug caught (`opacity-60` on dead table rows
  pushed muted text below AA contrast — dimmed by ground instead). Validated against the live stack:
  create → licensed preview → download (count 0→1) → revoke → the portal says "revoked" immediately.
- [x] **F.11b·2a Schema administration.** `field_defs` was readable and not editable; now it is both, with
  the refusals that keep stored data and the definition describing it in step. `dam_db::fields` grew
  `define`/`amend`/`remove`/`reorder` (+ `_on` variants) and a `SchemaRefusal` per fixable mistake: a key is
  checked against the intersection of what JSONB, the search shorthand and the generated SQL can all spell;
  reserved names are refused; a kind cannot change once **any** asset carries a value — including a
  soft-deleted one, since a restore would resurrect it under a kind it was never validated against; a
  removal keeps the values, so re-defining the key adopts them back. `POST/PATCH/DELETE /schema/fields` and
  `PUT /schema/fields/order` need Manage where `GET /fields` needs only Read, with 409 for "the world
  refuses this" and 422 for "the request is wrong". 16 db cases + 8 API cases; 16 + 10 mutations all caught,
  and mutation testing found three blind spots and one live bug (the kind lock ignored soft-deleted assets).
  The UI is a `/schema` page: usage count per row, four behaviour toggles, keyboard reordering, and a
  removal confirmation that says how many assets use the field and that the values come back. 7 e2e cases,
  axe-clean in both states; the run caught a real bug — the shared API client read `message` where the
  server sends `reason`, so *every* reason-carrying refusal in the app was reaching users as "Request
  failed (409)". Driven against the live stack end to end.
- [ ] **F.11b·2b Restore UX.** The remaining surface.

**The gap the UI runs into, stated plainly:** `dam-worker` has no queue consumer, so nothing generates a
derivative and nothing finalises an upload into an asset. An upload lands in staging and stops there — the
browser says so rather than leaving the user to wonder. That is 0.9's remainder and the first thing M4
needs, since every M4 deliverable is a job.

## H — A mutation-testing sweep

Asked for as "fix all the bugs". The technique that has actually found defects in this build is mutation
testing — roughly fifteen tests in earlier milestones passed for the wrong reason — so the sweep applied it to
the modules that had never had it, prioritising the ones where being wrong is a security problem.

*The harness itself had a bug worth recording:* the first version checked for compile errors before test
failures, and `cargo` prints `error: test failed` for a genuine failure — so it reported every **caught**
mutation as "did not compile". Four real catches read as noise. The order matters, and shell escaping of SQL
fragments silently produced "text not found" for half the mutations until the harness moved to Python.

- [x] **H.1 One live security bug: a suspended tenant's API keys still worked.** `authenticate` joined
  `dam_global.tenants` and checked nothing about it, so the join proved the row existed and nothing more.
  Suspension for non-payment or abuse did not cut off API access. Now `status = 'active'` only, with each other
  status refused for its own reason. Found because a `LEFT JOIN` mutation broke no test — nothing asserted the
  tenant side at all.
- [x] **H.2 A function that promised exclusivity it could not give.** `paths::claim_queued` carried
  `FOR UPDATE SKIP LOCKED` on a pool — its own transaction, locks released on return — so it claimed nothing
  between calls. Renamed `due_for_delivery`, clause removed, contract stated, and a case pins that two readers
  both see the same firing. No consumer exists yet, so this was a trap set for whoever writes the notification
  worker rather than a live fault.
- [x] **H.3 An access predicate nobody decided.** Saved-search ownership used `IS NOT DISTINCT FROM`, so a
  caller with no identity owned every ownerless search and an identified caller owned none of them. Narrowed to
  plain equality — the fail-closed direction.
- [x] **H.4 Four untested security properties, now pinned.** An expired share must stop already-issued delivery
  URLs; `consume_download`'s duplicated expiry condition needs its own test; a restore at exactly the approval
  threshold does not need approval; re-indexing must leave one document.
- [x] **H.5 A comment that overclaimed.** Finalisation's "deleted last, so a crash cannot orphan an object" is
  defensive rather than load-bearing — the digest column is what makes that crash recoverable. Corrected rather
  than left, because a comment claiming an enforced property is a defect in its own right.

*Clean under mutation:* `access.rs` (all four §7 leak mutations caught), `query_sql.rs`'s jsonb equality forms,
`auth.rs` after H.1, `shares.rs` after H.4, the restore cost guard, and the whole new pipeline — twelve
mutations, one survivor, now closed.

## M3 — Delivery, sharing, restore

Scope from §13: signed transform delivery, embeds, CDN, video + HLS, share links, restore flow with
cost guards, notifications/Paths (G9), saved searches (G15).

- [x] **3.1 Signed transform URLs.** The one chokepoint every download passes through, so rights and
  ABAC are enforced by the delivery design rather than by a caller remembering (D12).
  *Done:* `dam_core::signed_url` (17 pure cases) + `dam_api::delivery` (12 cases, one container).
  *A signed URL is permission to **attempt**, not to receive.* The signature proves we issued this exact
  request unaltered; rights are evaluated **at delivery**. That ordering is the whole design — a URL issued
  Monday under a valid licence stops working Tuesday when it lapses, and 3.3's revocation-on-an-issued-URL
  is the same mechanism. Mutation-verified: removing the delivery-time rights check serves bytes (302 where
  403 belongs).
  *The injectivity test took three attempts to be non-vacuous.* Both earlier versions passed while the
  canonical form used `|` separators instead of length prefixes. The real collision needs the delimiter to
  move a boundary between two **populated** fields — `transform="web|2048", channel="x"` and
  `transform="web", channel="2048|x"` both render `web|2048|x|`. Under the mutation the two tokens are
  **byte-identical**: one signature covering two claims, so anyone who can influence the transform forges
  the channel — and the channel selects which licence terms apply.
  *Other properties:* the signature is checked **before** the expiry, so a forged expired token reports a
  bad signature rather than telling a forger their attempt was otherwise accepted; every signature failure
  collapses to one flat **404**, which also avoids confirming the asset exists; the transform resolves
  against `derivatives` rather than being trusted as a path (a signed path parameter would be traversal we
  had *signed*); `Expiring` still delivers; the redirect is `private, no-store` with a 30-second presign,
  since it hands out a credential the rights check can no longer supervise; and the original's key is
  **derived** from `content_hash`, so a URL cannot name an object the asset's own hash does not account for.
  *A bug the tests exposed in the tests:* the handler called `Utc::now()` while the fixture used a fixed
  clock, so a token minted one second before the fixture's `now()` was still in the *future* in real time —
  the expiry case asserted 404 and was handed a 302. The clock is injected now, which is what makes every
  time-dependent property here actually testable.
- [x] **3.2 Derivative delivery + cache.** `op_hash` keyed, with the profile and intent in the key.
  *Done:* `dam_media::profiles` (5 unit) + `dam_db::derivatives` (12 cases) + delivery rewired, and **3.1's
  cache lookup fixed** — it shipped resolving `WHERE profile = $2`, by *name*. `op_hash` covers size,
  format, quality, fit, background, colour profile and rendering intent (§18.1), so a redefined profile has
  a different hash; a name lookup keeps serving bytes rendered under the old definition **forever**, with no
  error and a customer seeing yesterday's quality setting indefinitely. Delivery now resolves
  name → profile → `op_hash` → row, and a test asserts a superseded recipe stops being served.
  *`Profile::revision` exists for what `op_hash` cannot see.* If a change alters the **pipeline** rather
  than the fields — a different resampling filter, a sharpening pass — every field stays identical and every
  cached derivative keeps being served. A revision bump makes it a miss, which is the only safe default:
  serving a stale rendition is invisible, re-rendering is merely work.
  *The schema caught a design hole.* `derivatives_proxy_idx` is `UNIQUE (asset_id) WHERE role = 'proxy'` —
  **one** master proxy per asset, because D5 makes it the search-and-AI substrate rather than one rendition
  among many. So a redefined proxy cannot coexist with its predecessor the way a thumbnail can. `record`
  refuses and names `replace_proxy`, which swaps the row and **returns the superseded object key** so the
  caller reclaims it after committing — the row and the object live in different systems, and deleting first
  risks a placement pointing at nothing. An upsert would have looked tidier and orphaned an object on every
  proxy redefinition.
  *Other choices:* two workers rendering the same recipe produce byte-identical output, so the loser's row
  is redundant and `ON CONFLICT DO NOTHING` keeps the first `object_key` rather than orphaning it;
  `last_served_at` is written at most hourly, because per-delivery writes turn the hottest read path into a
  row of WAL per download (the same argument `auth::LAST_USED_RESOLUTION` makes); and `superseded` **refuses
  an empty profile set** — `<> ALL('{}')` is true for every row, so it would propose evicting the entire
  cache, which is a configuration failure rather than a plan.
  *Not done here:* render-on-demand. A cache miss is an honest `404` until the render path is wired; what it
  must never do is fall back to a name match. Tenant-defined profiles need a table of their own — see the
  note in `dam_media::profiles`.
- [ ] **3.x AWS-native features to rely on instead of building.** *Raised 2026-08-18; needs a decision on
  items 1 and 2 because they change architecture.* Every item is AWS-only while D1 says S3-compatible, so
  each belongs behind `dam_store::Capabilities` with a fallback — the pattern already exists, and the
  conformance suite already refuses an operation a driver claims but does not implement.
  1. **S3 Event Notifications for presigned-upload finalisation.** *This closes a real gap in 1.6.* The
     presigned path records a session and hands out a URL; **nothing detects that the client finished.** A
     client that uploads and then crashes before calling back leaves the object in staging until the reaper
     deletes it — a silently lost upload. An event → queue → worker finalises regardless of the client.
     MinIO supports bucket notifications; SeaweedFS partially.
  2. **Intelligent-Tiering for originals.** §19 lists access-pattern prediction as an unknown sitting on the
     lifecycle engine. Intelligent-Tiering does access-based movement with no retrieval fee between the
     frequent and infrequent tiers. Our engine's value is the **policy** — never tier the master proxy (D5),
     honour pins and legal hold, produce a reviewable plan — not guessing access. Hybrid: originals in
     Intelligent-Tiering, policy stays ours.
  3. **S3 Batch Operations for 3.4's bulk restore.** Manifest-driven, with retries, throttling and a
     completion report. Better than a job loop. Does **not** help 2.10, which is database-side.
  4. **S3 Inventory instead of LIST for reconciliation** — a daily manifest rather than paginated LIST.
  5. **SSE-KMS for BYOK (G10).** Per-tenant key; building anything here would be worse.
  6. **A lifecycle rule on `*/staging/`** as a safety net beneath the reaper. Prefix-based, so it maps onto
     `Key::is_tier_exempt`'s existing scheme.
  7. **CloudFront in front of derivatives — not replacing the chokepoint.** CloudFront signed URLs cannot
     consult live database state, and D12 requires rights evaluated at delivery. It fronts the presigned URL
     we redirect to.
  *Rejected:* S3 Object Lambda / Lambda@Edge image resizing. It would replace the derivative pipeline and
  lose libvips (RAW/PSD), ICC control (D11) and C2PA preservation (D13) — differentiators, not overhead.

- [x] **3.3 Share links.** Passcode, expiry, download limits, revocation — and revocation that takes
  effect on an already-issued URL.
  *Done:* `dam_db::shares` (14 cases) + 3 delivery cases for the end-to-end property.
  *The named requirement is real, not aspirational.* Resolving the share token per request makes revoking the
  *share page* immediate — but a share mints delivery URLs valid for their own TTL, so without more, revoking
  would leave every outstanding download URL working for up to a day. The share's id is therefore **inside
  the signature** and delivery re-checks it, the same shape as D12's rights check. Mutation-verified: bypass
  the re-check and a revoked share's URL still returns 302. A spent download limit stops issued URLs by the
  same mechanism — otherwise the limit bounds how often the share *page* is opened, not how often the asset
  leaves.
  *Two secrets, two hashes, opposite reasons — and the asymmetry is the point.* The token is 256 CSPRNG bits:
  no dictionary, so BLAKE3, and argon2 would add ~100 ms to every share view for nothing. The passcode is
  human-chosen: `spring2026` **is** in a dictionary, so argon2id and salted, since BLAKE3 would make an
  offline attack on a leaked digest instant. Salting is tested too — two shares with the same passcode must
  not share a digest, or cracking one cracks them all.
  *The download-limit race is closed in one statement.* Read-compare-increment lets two concurrent requests
  both take the last slot; under that mutation eight concurrent downloads against `max_downloads = 1` grant
  **two**, and the asset leaves twice from a link that said once.
  *Refusals are distinguished* — revoked / expired / exhausted / passcode-required / passcode-wrong — which is
  a deliberate disclosure: the token is 256 random bits so nobody can enumerate one, and the recipient is the
  person who needs to know whether to ask for a new link or re-read the email. Revocation is checked *first*,
  being the most absolute reason.
  *Token format bumped to v2*, since the payload gained a field. Outstanding v1 tokens stop verifying, which
  is correct — they were issued for at most 24 h, and the alternative is supporting two layouts so a URL from
  yesterday can bypass a check added today.
- [x] **3.4 Restore flow (§6.5).** `202` with an ETA and a cost estimate, batching sibling requests, and
  the expiry sweep. *(Video moved to 3.5, which is where it belongs.)*
  *Done:* `dam_core::restore` (15 cases + 3 unit) + `dam_db::restores` (13 cases).
  *Everything here is deliberately wrong in the safe direction,* because Expedited against Bulk is roughly
  **10× on price and 100× on latency**. Cost estimates **round up** — a restore costing a cent more than the
  figure somebody approved is a conversation nobody wants. ETAs quote the **slow end** of each documented
  window, since a promise made from an average is broken half the time. And the **per-object term survives**:
  a collection restore is hundreds of objects and S3 bills per object too, so dropping it makes a 400-file
  restore look free.
  *Expedited on Deep Archive is refused, not downgraded* — mutation-verified. Substituting Standard answers a
  request for five minutes with twelve hours and no explanation, and the user waits for something that was
  never going to happen on that timescale.
  *Three findings.* **Duplicate requests coalesce** — a second `RestoreObject` on an ongoing restore is
  billed, and without `ON CONFLICT DO NOTHING` the partial unique index throws so the second caller gets an
  error instead of the answer they wanted (which is the same answer). **Availability recomputes the expiry**:
  the plan's figure came from an *estimated* ETA, so keeping it makes a Bulk restore that lands six hours
  early expire six hours early, and the difference is a second restore billed again. **Spend counts
  failures**, because a failed retrieval is often still billed and a budget ignoring them lets a retry loop
  spend without limit — while queued requests do not count, since nothing reached S3 and counting them would
  make a queue block itself.
  *`Budget::default` asks for approval above ~$50 with no hard cap.* A default of "no budget" makes the
  guardrail opt-in and the failure mode of an opt-in cost control is a surprise invoice; a default of zero
  makes everything need approval before anyone configured anything, which teaches people to disable it.
  *Needs a table:* per-tenant budgets. The threshold lives in code today and `spent_this_month` is computed
  per tenant schema, but the limits themselves are not yet configurable.
- [x] **3.5 Video and HLS.** ffmpeg in the subprocess sandbox, loudness normalisation, the 720p H.264
  master proxy §2 specifies.
  *Done:* `dam_media::video` — 12 integration cases against real ffmpeg plus 7 unit, whole suite 1.2 s.
  Fixtures are **generated** by `testsrc`/`sine`: a binary video fixture is one nobody can review, and a
  two-second 320×240 clip proves the same properties as a two-minute 4K one.
  *A real bug the tests found.* I mapped ffmpeg's `-inf` (silence) to a sentinel and fed it back to
  `loudnorm`, which asks the filter to lift silence to −16 LUFS. The resulting gain emits samples the AAC
  encoder rejects — `Input contains (near) NaN/+-Inf`, then `Conversion failed!`. So "handle silence
  gracefully" was **producing a corrupt audio stream**. Silence is now detected and normalisation skipped,
  and that is not a preference a caller can override. The track survives: a silent track is still a track.
  *Limits are derived from duration, and both directions matter.* The probe default's 120 s wall clock kills
  any real transcode; a budget sized for a three-hour film lets a **hung** ffmpeg on a ten-second clip hold a
  worker for hours. Budget is 4× the media's own duration, floored at 60 s and capped at 6 h. There is
  deliberately **no CPU cap** — `ulimit -t` bounds exactly what a transcode is supposed to spend, so the wall
  clock is the bound that separates slow from stuck.
  *Loudness needs two passes.* Single-pass `loudnorm` is **dynamic**: it adapts as it goes, pumping quiet
  passages and leaving the volume moving *inside* each clip — the opposite of what normalising is for. The
  test measures the **output** to confirm the offsets were applied.
  *Other things that would be quietly wrong:* `min(ih,720)` never upscales (a plain `scale=-2:720` blows a
  240p clip up to look worse and cost more); `-2` keeps the width even, which 4:2:0 requires as a hard error;
  `-an` for a video with no sound, since a silent AAC track is bytes kept hot forever and makes "has audio"
  answer yes; the measurement is read from **stderr**, because ffmpeg writes filter output there and a
  stdout reader concludes every file is silent; `+faststart` is verified by **byte offset** (`moov` before
  `mdat`), which a parse-check cannot see and which is the commonest reason a valid MP4 "does not play"; and
  HLS segments are read from the **playlist**, not a directory glob, since a glob sorts lexically and stops
  working past 99,999 segments.
- [x] **3.6 Notifications and Paths (G9).** `paths`, `path_firings`, and delivery that is idempotent
  under retry.
  *Done:* `dam_db::paths` — 13 cases + 8 unit.
  *The digest key **is** the deduplication,* since `path_firings_dedupe_idx` is `UNIQUE (path_id, digest_key)`.
  A daily "expiring in 30 days" sweep sees the same asset thirty times: keyed on the **deadline** it warns
  once, keyed on when the sweep ran it warns thirty times — after which the recipient filters the path to
  trash and misses the real one. The inverse matters too: a **renewed** licence gets a fresh warning, or
  renewing once silences an asset forever.
  *A test that passed for a timing reason.* The thirty-sweeps case cannot catch a sweep-time-keyed mutation —
  thirty iterations finish inside one wall-clock second, so `Utc::now()` deduplicates by accident. Replaced
  with a case demonstrating the realistic **call-site** mistake (reaching for `Subject::Recurring { bucket: now }`,
  which is the natural thing to write) and contrasting five notifications against one.
  *A test that was simply wrong.* I asserted four sweeps six hours apart share a daily bucket. They do not:
  buckets truncate **from the epoch**, so a one-day window has midnight boundaries and noon + 18 h is the next
  day. The code was right. Epoch truncation is what lets two workers agree on a bucket without coordinating,
  so the boundaries are not negotiable.
  *A throttled firing leaves **no** ledger row* — a suppressed row would claim the digest key and turn a rate
  limit into permanent silence. Throttling is per **asset**, not per path: a bulk import touches many assets,
  and a global throttle would notify about the first and silently drop the rest.
  *On "idempotent under retry", stated honestly:* there is no local-only way to get it. A worker that sends and
  then crashes before recording `sent` leaves a queued row — retry may duplicate, not retrying may lose.
  Insert-then-send is at-least-once; send-then-insert is at-most-once. For a notification, at-least-once is the
  right side to fail on, so the firing is recorded first and the digest key is handed to the provider as **its**
  idempotency key, which is where the duplicate actually collapses. The module docs say so rather than claiming
  the ledger alone suffices.
- [x] **3.7 Saved searches (G15).** Stored query IR, re-evaluated against current access rather than the
  access at save time.
  *Done:* `dam_db::saved_searches` — 14 cases. **M3's backend is complete.**
  *The named property decides whether sharing is safe at all.* Store the results, or store the query with its
  access filter baked in, and a search saved by an administrator becomes a permanent leak wearing the shape of a
  bookmark. Only the *user's* query is stored; the predicate is compiled fresh for whoever opens it, and
  `Planned::new` is the single join point — there is no variant taking a stored predicate. Asserted on the
  **stored bytes**, not inferred from behaviour: no group id, no `asset_group_members`, no `deleted_at`.
  *Sharing shares the question, never the answer* — a separate test opens a shared search as a scoped viewer and
  confirms the saver's hidden results stay hidden.
  *Two fallbacks that would each turn a broken bookmark into "every asset",* both mutation-verified: an
  unreadable stored shape is **refused**, not read as `Query::All`; and a clause naming a since-deleted field is
  **refused**, not dropped — dropping widens the result set, the same argument `dam_core::query` makes.
  *The stored form is a wire format,* so it is hand-written rather than derived: a derive would rename itself out
  from under existing rows the first time somebody reordered the enum. Literal types are **tagged**, because
  `2026` could be an int, a decimal or a year in a text field, and guessing on load compares the wrong column
  type — silently wrong results rather than an error. 17 query shapes round-trip exactly.
  *`result_count` is a badge, not the viewer's count.* It is stored per search rather than per viewer, so it is
  at best somebody else's number, and presenting it as *the* count would leak how many assets exist beyond a
  viewer's scope — §7's disclosure in a sidebar.

## State at the end of the overnight run

Everything actionable is done. **370 Rust tests + 70 frontend unit/component tests + 11 browser
a11y/e2e tests**, all green, with `cargo fmt --check`, `clippy -D warnings`, `cargo deny` (advisories,
bans, licences, sources), `svelte-check`, eslint and Prettier clean. 12 migrations, 17 commits on
`m0-foundation`, nothing pushed.

**Done:** M0 entirely except 0.10. M1's storage and media track entirely: `BlobStore` + two drivers +
shared conformance suite, multipart, versioning, object lock, pool/placement resolution, content
addressing, staging promotion, sniffing, upload finalisation, the resumable-upload engine,
`upload_sessions` + reaper, the subprocess sandbox, probe, derivatives, the master proxy and its §2
alarm, and the lifecycle engine. Frontend track F.1–F.4 entirely.

**Blocked on a decision — see NEEDS-REVIEW.md, in the order I would want them answered:**

| # | Task | What it needs |
|---|---|---|
| 1 | **0.10 ABAC predicate compiler** | Five access-control decisions, each with a recommendation. This one blocks the most: the API layer sits on top of it. |
| 2 | **1.6 TUS HTTP surface** | Request authentication and tenant resolution. No M0/M1 task schedules an API skeleton, so building it means inventing an auth model inside a handler. Everything *below* the HTTP layer is finished and tested. |
| 3 | **1.9 C2PA** | Whose signature it is, where the production certificate comes from, and — the question ARCHITECTURE does not address — what ingest does when an inbound manifest fails validation. |

**Blocked on a system library**, not a decision: 1.7's remainder — RAW/PSD/INDD needs libvips
(`brew install vips`; **not** in mise's registry, so outside the stated "CLIs via mise" preference) and
page counts need pdfium or LibreOffice. Video is M3 and out of scope, though note ffmpeg *is*
mise-installable when that milestone arrives.

**One housekeeping item:** commits are unsigned. 1Password's SSH agent returned "failed to fill whole
buffer" all night, which is a locked vault rather than a misconfiguration — every prior commit in the
repository is unsigned too. Re-sign with
`git rebase --exec 'git commit --amend -S --no-edit' -i e7e30db~1` once it is unlocked.

---

## Not in scope for the overnight run

M2 onward. If M0 completes and M1 is underway, that is the night's target met —
this is ~110 engineer-weeks of work in total and no amount of autonomy compresses
that. Stop at a green `mise run check` and a clean commit rather than leaving a
half-built layer.

## Q — Acquia DAM parity

Surveyed against a live Acquia DAM tenant on 2026-08-19; the full inventory, the gap against damrs and the
reasoning behind this order are in `ACQUIA-PARITY.md`. Read that first — the short version is that Acquia's
catalogue turns out to be mostly a concretisation of M4–M6 and Pre-GA plus about a dozen features
ARCHITECTURE never named, and that the product is *six applications* rather than one.

Each item is one full-stack slice: schema, API, UI, tests, mutation-tested, driven against the real stack.

- [x] **Q.1a Metadata types: the model, and the writes that respect it.** `metadata_types` +
  `metadata_type_fields` + `assets.metadata_type_id` (migration 0015). `field_defs` stays the tenant's field
  *vocabulary* — one key, one kind, one rule — because that is what the F.11b·2a refusals protect; a type is a
  *selection* over it, which is also why `dam_core::fields::validate` needed no change at all. Resolution has
  no dead end: the asset's type, else the tenant default, else the whole vocabulary — so the migration is a
  no-op for every existing tenant and a resolution bug can never *hide* stored metadata. Ingest assigns a type
  from the sniffed mime's media class; the single-asset patch validates against the asset's own form; a bulk
  item whose type excludes the field is a named per-item failure, not a silent write, so the two write paths
  agree about what the schema means. 12 db cases + one pure class test; 13 mutations all caught. Mutation
  testing found two tests passing for the wrong reason — an SVG "verified" by a fallback, and a refusal that
  only worked because the field was undefined rather than out-of-type.
- [x] **Q.1b Metadata type administration.** `GET/POST /schema/types`, `PATCH/DELETE /schema/types/{id}` and
  `GET/PUT /assets/{id}/metadata-type` — Manage to edit, Read to list, and a missing type is a 404 when the
  *path* named it but a 422 when the *body* did, because in the second case nothing is missing: the request is
  wrong. `field_keys` replaces the list wholesale, so a delta against a stale copy cannot silently drop what
  the client had not seen. The `/schema` page grows a named "Asset types" landmark under the field list — the
  order the two depend on each other — with per-type field selection, ordering, media classes, an exclusive
  fallback and a removal that says how many assets re-form and that nothing is deleted. The detail panel gains
  a Form picker, and the metadata editor is now filtered to the asset's own field list: it had been offering
  the whole tenant vocabulary, which with types means offering fields the API refuses. 8 API cases, 7
  mutations all caught; 5 e2e cases, axe-clean in both states. Driven against the live stack end to end.
- [x] **Q.2a Categories: the tree, and how assets get filed in it.** Not a new hierarchy — `taxonomies.kind`
  already admitted `'category'`, `taxonomy_terms` already carried an ltree path, `asset_tags` was already the
  asset↔term join, and `query_sql::push_term` already filtered by a term *including descendants*. What was
  missing was reading the tree as a tree and putting an asset in it. `dam_db::categories` does both, plus the
  rollup counts and the uncategorised worklist. Counts run through the caller's `Planned`, never
  `taxonomy_terms.asset_count`: that column is a denormalised global, and §7 says counts disclose — showing it
  would tell a scoped caller how much of the library they cannot see. A rollup counts *distinct* assets, so an
  asset filed under two leaves of a branch does not make "Exterior (7)" appear over a library of five. A human
  placement is written `confirmed`/`human` because filing is a decision, not a hypothesis. Migration 0016 drops
  the taxonomy-wide unique slug index: a category tree needs "Yellow" under both Exterior and Interior, and
  `(taxonomy_id, path)` already enforces the rule that matters. That also makes `move_term`'s `PathTaken`
  guard reachable for the first time — a previous iteration kept it deliberately for this day, and it is now
  tested. 11 db cases; 13 mutations all caught, three of which found genuine blind spots.
- [x] **Q.2b The category API, and two access-control bugs it uncovered.** `GET/POST /categories`,
  `GET /categories/{id}`, `POST /categories/{id}/nodes`, `GET /categories/{id}/uncategorised`, and
  `GET/PUT/DELETE /assets/{id}/categories[/{category_id}]`. Reading is Read (nobody navigates a library without
  the tree); anything that changes where an asset lives is Manage. Counts and the worklist run through the
  caller's own predicate, so two people legitimately see different numbers on one branch.
  **Two real bugs, both found by building the first group-scoped caller any API test has had:**
  `check_groups_are_renderable` queried `asset_groups` — a *tenant* table — on the **global** pool, so every
  group-scoped caller got a 500 on every request; it hid because the dam-db unit tests passed a tenant pool and
  no API test was group-scoped. And the deliberate refusal of a rule-based group (decision 4: refuse rather
  than approximate) surfaced as a bare 500, indistinguishable from a crash — it is now 501 with a body naming
  the group, since it describes the deployment's limitation rather than the tenant's data. 8 API cases; 8
  mutations all caught, one of which proved nothing checked that `authorize` still *called* the renderability
  check.
- [x] **Q.2c·1 `in:` — categories in the query language.** `in:exterior.yellow` filters by category, always
  including descendants (which is what clicking a branch means, and why the paths are ltree). It lives in the
  query string rather than in a `category=` parameter *because* the filter rail's own rule is that it edits one
  string — so "copy this search" copies all of it, and a rail with state beside the text box is the split that
  rule exists to prevent. `in` is reserved, so a tenant field of that name cannot shadow the browse tree;
  values are case-folded like every other selector; unknown, empty and retired categories are refused by name
  rather than silently becoming free text. `search_schema` now loads the tenant's live category paths
  lower-cased, excluding vocabularies and retired terms. 4 parser cases plus one end-to-end case proving the
  whole chain — database paths → parser schema → resolution → SQL that returns descendants — because each link
  is individually plausible and the failure mode is a join between them. 9 mutations all caught; two found real
  blind spots (mixed-case ltree paths, and a vocabulary term becoming filterable as a category).
- [x] **Q.2c·2 Categories in the UI, and the four bugs it took to get there.** A browse tree in the rail
  (navigating, not ticking: selecting a branch replaces the previous one, because `in:a in:b` means "filed in
  both" and returns nothing while looking broken), disclosure derived from the selection so a shared link
  arrives expanded, empty branches kept visible so the tree's shape does not change with the reader's scope, and
  a `CategoryPanel` on the detail side that files and unfiles from a picker over what exists.
  **Four real bugs, each found a different way.** Driving the real thing found two: `/search` refused every
  relational clause with a 501 saying it was "routed through SQL" — aspirationally, so the whole feature was
  unreachable from the UI; and once routing existed, a composed query like `in:exterior harbour` 500'd because
  `page_matching` had copied `page`'s `FROM assets` while the SQL text renderer references `asset_metadata`.
  Opening the page found the third: a category tree was *also* emitted as a facet, rendering twice, the second
  time under a heading that was the taxonomy's UUID. And looking at it found the fourth: the selected label used
  `text-accent-fg` — the foreground *for* the accent surface — so it rendered exactly the background colour and
  vanished, which axe did not flag. 5 new API cases, 9 e2e cases, 4 unit cases; 6 routing mutations all caught.
- [ ] **Q.2c·3 The uncategorised worklist surfaced** in the UI, with the other admin worklists (Q.20).
- [x] **Q.3a Upload profiles: the model, and the ingest that honours them.** A profile answers three questions
  asked at three different times by three different pieces — the uploader needs the form and whether to insist
  on required fields, finalise needs the defaults and the metadata type, and enrichment needs to know whether
  machine tagging was permitted *long after the session row is reaped*. Only a row can serve all three, which is
  why it is one. Migration 0017; `assets.upload_profile_id` already existed, reserved by 0001 with no table and
  no constraint, so this added the reference rather than the column. Defaults are **metadata**, validated by the
  tenant's own validator as `Writer::Human`/`Mode::Patch` — so a profile cannot write a read-only field or a
  value of the wrong kind — and validated *twice*, at save and at apply, because a definition can change in
  between and a default that has quietly become invalid must fail visibly rather than be dropped from every
  upload. A default fills only absent keys: overwriting what somebody typed would discard their work. Ingest
  takes the profile's metadata type over the mime's class (a profile is a statement, a class is a guess), applies
  the defaults as real validated metadata, and records the profile on the asset. 9 db cases + the ingest case;
  11 mutations all caught.
- [x] **Q.3b·1 The upload-profile API, and naming a profile at upload time.** `GET/POST /upload-profiles`,
  `PATCH/DELETE /upload-profiles/{id}`. **Listing is Read** — deliberately, because the uploader has to render
  the picker and honour the required-field rule before it can upload anything, so a client that could not list
  profiles could not obey them; editing is Manage. Invalid defaults are 422 with the field named, on amend as
  well as create, so a form can put the error where the value was typed and a profile cannot be *edited* into a
  state that breaks every intake from that source. Both intakes — TUS and presigned — accept a `profile` key in
  `Upload-Metadata` and record the resolved id on the session, because finalise runs from a queue long after the
  request and the profile has to be recoverable from the row. An unknown key resolves to nothing and finalise
  falls back: the bytes are the point, and a mistyped profile is recoverable afterwards while a refused upload is
  not. 7 API cases, 2 tus cases, 2 unit cases; 9 mutations all caught, two of which found untested behaviour —
  the documented `ai_tags_enabled` default, and re-validation on amend.
- [x] **Q.3b·2 The profile UI, and a guard for the bug it exposed.** An "Upload profiles" section on `/schema`
  (third of three named landmarks: fields → types → profiles, the order they depend on each other), offering
  only fields a profile *may* write — a read-only field would produce a refusal the person could not have
  predicted. Empty defaults are cleared rather than sent as `""`, which the validator would accept and which
  would silently blank every asset from that intake. The uploader gains a profile picker, preselecting the
  tenant's fallback because that is what the server would apply anyway, and applies `require_complete` — the
  rule exists *only* here, since the server deliberately will not refuse a finished upload over it.
  **The guard matters more than the UI:** the profile router was never merged into `app::router`, so seven
  passing API cases and a correct OpenAPI entry coexisted with a 404 from the running server — each endpoint's
  suite builds its own router, so a module can be fully tested and unmounted. `openapi.rs` now asserts every
  documented path is served by the app router. Mutation-testing *the guard* found its first version useless: it
  probed with OPTIONS, which the CORS layer answers for any path, so it passed with three routers removed. 8 e2e
  cases; 3 mount mutations caught.
- [x] **Q.4 Auto-import mappings: the file's own metadata, into the tenant's fields.** `dam_media::embedded`
      extracts EXIF and XMP as flat named text — a fixed list of names, because a tenant's mapping refers to them
      and so they are configuration surface. XMP is *scanned*, not XML-parsed: pointing a parser at
      attacker-controlled bytes buys entity expansion for no gain. Migration `0018` holds the mappings; three
      rules earn their keep — priority decides between sources, `overwrite` defaults to off, and an imported value
      goes through the tenant's own validator so a rejection is reported rather than dropped. Coercion lives in
      the mapping layer, which knows the target's *declared* kind, so `exif.iso` can fill an int field; the
      extractor still refuses to guess. `GET/POST/PATCH/DELETE /auto-import-mappings` plus `/sources`, all Manage,
      and an "Auto-import from the file" panel on `/schema`. Ingest reads the header once and hands it to both the
      probe and the extractor, then imports *before* the profile's defaults — a blanket default applied first
      would have counted as a held value and silently defeated every per-asset import.

      Three things this turned up. **Six of the twelve EXIF names live in the Exif sub-directory** and none were
      tested: a tag number in the wrong directory is a different tag, so `exif.iso`, `exif.aperture`,
      `exif.shutter`, `exif.lens`, `exif.focal_length` and `exif.taken_at` were all unverified. The fixture now
      writes both directories and lives in `dam_media::testing` so ingest's suite cannot drift from it.
      **`exif.taken_at` was unmappable**: EXIF spells a timestamp `2026:03:14 09:26:53`, which is a date in no
      interchange format, so every date field refused it. Transcribed to ISO now — and given a zone only when the
      camera recorded one, because appending `Z` to a local reading would move a photograph by up to a day and
      store the guess as fact. **The UI failed both reads together**, so one bad mapping list emptied the source
      picker and left a `required` select that could never be satisfied.
- [x] **Q.4 Auto-import mappings.** Embedded metadata (XMP/EXIF) → field definitions, on ingest.
- [x] **Q.5a Ratings, favourites and watches: the model, and the access rules that are the substance.** Migration
      `0019`, three tables rather than one — a rating aggregates across people, a favourite is a private list, a
      watch is a standing request, and one table would carry a null column per unused role. `dam_db::engagement`
      puts every read *and write* through the caller's `Planned`: an endpoint that accepts a rating for any id is
      an existence oracle, and one that accepts a favourite for any id lets somebody assemble a private list of
      assets they cannot see. "Hidden" and "absent" are the same refusal. A private list is filtered too, because
      access can be withdrawn after the row is made. Aggregates are disclosed, identities never are, and watches
      have no public count — see `DECISIONS.md`. Clearing is deleting, so no average counts an absence.

      Fourteen mutations, and the first pass found a test passing for the wrong reason: the "a refused write must
      not write" assertion sat *after* the removal attempts, which deleted exactly what the write attempts had
      written — so the visibility check could be removed from `visible()` entirely and the count was still zero.
      Two more were coin-flips: a two-element ordering assertion that a reversed implementation passed half the
      time, and nothing at all covering whether re-favouriting reshuffled the caller's own list.

- [x] **Q.5b·1 The engagement endpoints.** `PUT/DELETE /assets/{id}/rating`, `/favourite`, `/watch`, plus
      `GET /favourites` and `/watches`. `Read`, not Manage: whoever may look at an asset may have an opinion about
      it, and requiring Manage would mean only administrators could favourite anything. Every response is the
      asset's engagement *afterwards* rather than a 204 — the average moved because of this request, so returning
      it means the number on screen came from the write instead of a read that raced it. A hidden asset and an
      absent one are both 404, because two statuses rebuild the existence oracle the db layer collapses. The range
      check runs before any database access, which is why an out-of-range rating still refuses correctly on a
      tenant whose schema is behind — as the live check demonstrated.

      Six of eight mutations caught. The two that survive are the identity check, and the reason is worth
      recording: `caller::authorize` already refuses a key with no identity for *every* endpoint, so the
      handler-level check is a fail-closed unwrap behind a guarantee that lives upstream. Mutation testing found a
      second real gap — both private lists held the same single id, so the watch route could have been wired to
      the favourites table and the suite would have passed.

- [x] **Q.5b·2 The engagement selectors.** `stars:>=4` for the asset's average and `is:favourite` / `is:watched` /
      `is:rated` for the caller's own state. Two new IR variants, and the shape of them is the decision:
      `Query::Mine` carries **no identity**, because a saved search stores the query IR — so an identity in the
      tree would make a search shared with a colleague return the author's favourites, the leak wearing the shape
      of a bookmark. Who is asking travels beside the access predicate instead, in `Planned::viewed_by`, and a
      renderer meeting a personal clause without one *fails* rather than returning an empty page, because
      "nothing matched" and "nobody said who you are" look identical on a screen.

      Both clause kinds are relational and route through SQL: a rating is an aggregate over a table, and a
      personal state has a different answer for every reader, so neither can be an index field. The rating
      renderer uses a correlated subquery rather than a join, because a join would multiply each asset by its
      ratings and every count downstream would be wrong; and `!= 4` excludes the unrated explicitly, or the
      complement of a bucket would come out smaller than the library minus the bucket. `stars:*` and `stars:-`
      mean "rated by anyone" and "unrated" — the two buckets a rail needs beside the stars.

      Fifteen mutations caught, and two more could not be written at all because `is_relational` has no wildcard
      arm — the exhaustive match makes that class of mistake a compile error. One found a real gap: the operator
      refusal was asserted only in dam-db's renderer suite, so dam-core's own validation could be deleted and
      only the other crate noticed.

- [x] **Q.5b·3 Engagement on the asset payload, and favourites first.** The summary carries only what a cell
      draws — `is_favourite` and the library's `average_stars` — because every field on a summary is multiplied by
      the page size; the counts, the caller's own stars and the watch state are on the detail, which draws them.
      One engagement read per page rather than per row, on all *three* page paths: browse, relational search and
      ranked search. The third is a separate code path because the ranking is the order, so it walks its window a
      row at a time — and folding engagement into the other two proved nothing about it. Exercised by building a
      real index in the test, which is what it took to reach that path at all.

      `order=favourites` puts the caller's favourites first and leaves the tail in the default order, so the sort
      changes which assets are at the top and nothing else. The identity lives *in* the `Order::FavouritesFirst`
      variant: this is the one order that is not a property of the library, so asking for it without saying whose
      favourites is a question with no answer, and the type makes it unaskable. It is bound, not formatted into
      the SQL — a uuid happening to be injection-safe is luck, not a design.

      Ten mutations, nine caught. The survivor is documented rather than papered over: `page_engagement` applies
      the caller's predicate a second time, and no current call site can observe it because the ids always come
      from a read that already filtered them.
- [x] **Q.5c·1 The engagement panel.** Stars, a favourite toggle and a watch toggle in the detail panel. The
      rating is a **radio group**, because five stars are five values of one thing — as buttons they are five
      unrelated controls to a screen reader, with nothing saying which is chosen. The drawn stars show the
      *average* and the checked radio the caller's own rating, stated separately in words, because a widget
      showing one number could only be lying about the other. Clearing is its own control and appears only when
      there is something to clear: a sixth star meaning "none" is exactly the conflation the model avoids. The
      counts are people, never a list of them, and the panel says outright that nobody is told how many are
      watching.

      Nine mutations, all caught after two rounds. The first pass found the partial star fill untested — the claim
      that 3.4 and 3.5 differ lived only in a comment — and nothing at all switching between assets, which is the
      one thing the derived-override design exists to handle.

      Two real problems surfaced. A stateful mock was needed because a fixed reply made the favourite click report
      "watching", so the watch toggle correctly sent DELETE and the test read as a component bug. And adding a
      required field to `AssetDetail` broke **ten existing browse and share cases**: their mocks predate it, and
      reading a field off an undefined object took the *whole* detail panel down — metadata editor, categories and
      sharing with it. The mocks are updated and the panel now hides itself rather than crashing, because absent
      and "nobody has rated this" are different facts.

- [x] **Q.5c·2 The grid star and the two private lists.** A star on each cell with `tabindex="-1"`, so the grid
      keeps exactly one tab stop — the WAI-ARIA grid pattern allows a widget in a cell and reaches it through the
      container's key handling, which is what `f` on the focused cell is for. Without that key the star would be
      mouse-only, the same asymmetry the uploader's drop target once had; and because focus stays on the cell, the
      change is announced or it would be silent. `ctrl+f` is left to the browser. `/favourites` and `/watches` are
      real routes with nav links, sharing one component because they differ only in copy and endpoint.

      **The `ListPage` shape from Q.5b·1 was wrong and is corrected here.** It returned ids, reasoning that a grid
      already knows how to render a set of assets — but no endpoint fetches assets *by id set*, so a client holding
      fifty ids had fifty requests to make. It now returns the same `AssetPage` browse and search return, hydrated
      in the order the caller added each one, which is the order no other endpoint can produce and the reason these
      routes exist rather than `?q=is:favourite`.

      Four real problems surfaced. A `$effect` reading a prop **re-ran when the object was replaced even though the
      id was identical**, so patching the open asset after a grid toggle threw away the server's answer and the
      sentence describing it — fixed by comparing the id rather than merely reading it. An e2e assertion read the
      request recorder on the line after a keypress, **the same race this suite already had once**; the fix is the
      same, await the visible end state first. A `span.font-medium` selector matched the rights badge as well as
      the filename, so an order assertion was comparing against `'✓ Cleared'`. And the star's `aria-label` carries
      its filename, which collided with an existing loose `getByLabel('Campaign')` — the locator was always
      imprecise and the label made it ambiguous.
- [ ] **Cleanup: `Caller::identity_id` is `Option` but `authorize` guarantees `Some`.** Three call sites re-check
  it (`assets.rs`, `tus.rs`, `engagement.rs`), each with a different refusal, and one of them is unreachable code
  that looks load-bearing. Narrowing the type touches every handler, so it is its own change.
- [x] **Q.5c The engagement UI.** Stars in the detail panel, a favourite toggle on the card, a watch toggle, and
      the two private lists as places you can go. Shipped in `0339f41` (the panel) and `42fac0c` (the grid star and
      `/favourites`, `/watches`); this box was left unticked by mistake and the commits are the record.
- [x] **Q.6a Comments: the model, and two gates in the right order.** Migration `0020`, `dam_db::comments`. Every
      read passes the caller's *asset* predicate first and the comment's own visibility second, because the other
      order — find what is addressed to me, then check the assets — discloses the existence of assets through the
      comments hanging off them. Being addressed is therefore not a grant: a recipient who loses access to the
      asset stops seeing the comment, while the routing row stays exactly where it was. A private comment with no
      recipients is refused, because a note only its author can read is one that failed silently. One level of
      threading. Statuses (`open`, `resolved`, `approved`, `changes_requested`) are movable by any reader and
      record who moved them — `approved` is somebody else's verdict, so a status only its author could move could
      never mean approval — and nothing is gated on them yet, deliberately.

      **The strict visibility rule is in `NEEDS-REVIEW.md`, not decided.** Whether a tenant admin may read a
      private comment is a promise about the product, both answers are defensible, and the permissive direction
      cannot be reversed for anything already written. Q.6b/Q.6c build on the strict rule until told otherwise.

      Fifteen of sixteen mutations caught; the survivor is a redundant join documented as such. Two were real
      gaps: the thread ordering was unobservable because the reply happened to be the next comment created, and
      nothing covered a reply pointing at a comment on a different asset.

- [x] **Q.6b The comment API, and the first identity endpoint.** `GET/POST /assets/{asset_id}/comments`,
      `PATCH/DELETE /comments/{comment_id}`, and `GET /people`. `Read` is the bar and an identity is required: a
      comment is somebody's words, and an anonymous one could never be edited, deleted or attributed.

      Names are resolved server-side, in one lookup per request. A thread rendering `author_id` as a uuid is
      unreadable, and making the client resolve them would be a request per distinct person on the page. A comment
      whose author has since been deleted reads "Someone no longer here" rather than blank — offboarding is the
      ordinary case, and an empty name reads as a rendering fault.

      `PATCH` takes the words *or* the status, never both: they carry different rights, so a request naming both
      would half-apply for a caller who holds one and not the other. An unknown visibility or status is refused by
      name rather than defaulted — silently widening a comment somebody meant to keep private is the worst
      available outcome.

      `/people` is the first endpoint that reads the control plane's identity tables. Scoped by the *credential*,
      with no parameter to point at another tenant with, and it carries the email deliberately: two colleagues can
      share a display name, and a picker that cannot tell them apart misroutes a private comment.

      Eleven of twelve mutations caught. The survivor is the identity unwrap, which `caller::authorize` already
      guarantees — the same shape as the engagement handlers, and the reason the `Caller::identity_id` cleanup is
      on this list. One mutation found a real gap: nothing covered a comment outliving its author.
- [x] **Q.6c The comment thread, and three latent UI bugs.** A thread in the detail panel: compose, reply, edit
      your own, delete your own, move a status. The compose box states the *consequence* — "Everyone who can see
      this asset" / "Only you and the people you choose" — because somebody who infers a switch wrongly cannot take
      the words back. A private comment with nobody named cannot be sent, said before writing rather than refused
      after. Edit and Delete appear only on your own comments, since an affordance that exists to be refused
      teaches people to distrust every control beside it; the status control appears on all of them, because
      `approved` is somebody else's verdict. No reply control on a reply. Added `GET /me`, without which the panel
      could not know whose comments to offer to edit.

      Seventeen mutations caught, and the three that survived first were all real:

      1. **`checked={expr}` on a radio sets `defaultChecked`, not the property** — so the control was not
         controlled, and flipping the default in source changed nothing on screen.
      2. **`bind:group` over booleans ignores the initial value entirely.** Replacing the attribute form with it
         fixed nothing; the radios are string-valued states now, which is what radios are.
      3. **The reset effect's first run overwrote the declared defaults**, so the `$state` initialiser was dead
         code and the compose box had two defaults that could disagree. Guarded, as the engagement panel already
         was.

      Driving the real thing found a fourth: on a tenant with one member the private option was a dead end — the
      picker empty, the requirement unsatisfiable, and nothing saying so. It now names the situation and the way
      out.
- [x] **Q.7 The activity feed and the dashboard, and a partition nobody was rolling.** `events` has existed since
      migration 0001 and **nothing had ever written to it** — which hid a live landmine: it is partitioned by month
      with one January 2026 partition and a comment promising a `damctl` roll-forward command that was never
      written. The first event write would have failed, and so would every one after. Migration `0021` adds a
      default partition, and a case proves the dependency by detaching it and watching the write fail.

      `dam_db::events` records and reads; `finalise`, the comment POST and the share POST now write. Each write is
      in the same transaction as the thing it describes, so a feed cannot show an upload that rolled back. Two
      things are deliberately *not* in an event's context: a share's **token**, because a feed is read by everyone
      who can see the asset and a token is a bearer credential; and a comment's **words**, because a private comment
      would otherwise reach a public screen through its own feed entry.

      `GET /dashboard` returns counts, feed and saved searches in one request — the page cannot render without all
      three, and three endpoints would let the numbers disagree with the list beneath them. Every count is the
      caller's own. The landing page is no longer the scaffold.

      Two bugs found on the way. **`saved_searches::visible_to` takes a pool**, and until the dashboard nothing
      outside its own tests had ever called it — the same shape that once gave every group-scoped caller a 500 from
      `check_groups_are_renderable`, because a tenant table resolved against `dam_global`. Added `visible_to_on`.
      And **comment threads were ordered by uuid**: `coalesce(parent_id, id)` groups them correctly and orders them
      arbitrarily, which looked right for exactly as long as the uuids happened to agree with the timestamps. They
      now sort by the thread root's own creation time.

      Thirteen mutations caught, three after closing fixture gaps (the page cap was untested with fewer than a
      page of rows; an *empty* metadata document was untested because every fixture had no row at all). One survives
      and is documented: the feed's id tie-break has no observable effect until an offset exists. axe caught an
      invalid `<dl>` containing an `<a>` directly.
- [x] **Q.8 Versions, and a filter nothing was applying.** `version_group_id`, `version_no`, `is_current` and
      `replaces_id` have been on `assets` since migration 0001, nothing ever wrote a second version — and so
      **nothing ever filtered `is_current`**. Every asset is current until a version exists, so every listing looked
      correct and would have shown each asset once per version the moment one appeared. `CURRENT_ONLY` is now
      applied to the browse page, the relational search page, the facet counts and the dashboard's asset count.

      The rule: **listings show current versions; a named asset is whatever was named.** Reading, previewing or
      downloading an old version by id works, or keeping versions would be pointless. `dam_db::versions` adds,
      lists and promotes; `POST /assets/{id}/versions` joins an asset the caller *already uploaded* through the
      ordinary route, because a multipart endpoint here would be a second ingest path and two ingest paths diverge.
      Reading a history is Read, superseding is Manage. A stale supersede is **409** — the request is well formed
      and the world moved on, so reload-and-retry is the honest instruction. Promoting an earlier version keeps its
      number: a promotion, not a copy, because renumbering would claim an upload that never happened.

      Twenty-six mutations caught across the three layers. Two survivors were removed rather than papered over: an
      explicit `deleted_at` clause duplicating what the access predicate already decides, and a test locator that
      matched "current" inside the words "Make current".

      **Open:** engagement and comments attach to the version row they were made on. Whether a rating of March's
      cut should roll up to the group is a real question and a later one.
- [x] **Q.9 Attached documents, and one clause for two rules.** A release, a licence, a contract: files *about* an
      asset. Migration `0022` makes an attachment an ordinary `assets` row marked as belonging to another, so it gets
      the whole ingest path for free — the alternative was a second place to sniff, probe and place objects, which is
      a second path to diverge from the first.

      The cost is one rule: a row with `attached_to` set is not part of the library. That is the *same* requirement
      `is_current` has, so both live in one fragment — `LIBRARY_ROWS`, applied at the four places that describe the
      library. One rule to miss instead of two, which matters because missing either is invisible: no asset is
      superseded or attached until somebody makes it so.

      Attaching is Manage (it asserts something about an asset's rights), reading is Read and deliberately no
      narrower than the asset — paperwork exists to answer "may we use this", and a rights question somebody cannot
      check is one they will answer by guessing. Detaching is not deleting. `has_attachment` reaches the grid,
      scoped, so paperwork the caller cannot see does not set the flag. The three state-of-the-world refusals are
      409: already attached elsewhere, a superseded version, paperwork about paperwork.

      Twenty-two mutations caught. Two survivors were resolved by removing redundancy rather than documenting it: a
      duplicate `documents = []` that the loading gate already covered, and one earlier in Q.8. And one of my own
      tests was not testing what it claimed — the delay meant to open a window for a flash of the *previous* asset's
      paperwork was on a branch the request never reached, so the window was zero wide and the case passed
      vacuously.

      **Open:** whether paperwork should be readable by a narrower audience than the asset it belongs to. A release
      form carries a signature and an address, so it is arguably more sensitive than the photograph.
- [x] **Q.10 History tab.** `events::for_asset` plus `GET /assets/{id}/history` and a disclosure in the detail
      panel. Three decisions worth keeping:

      **It reads the whole version group, not one row.** "The history of this asset" means the story of the thing, and
      somebody looking at March's cut needs to see that April's replaced it — the single entry that explains all the
      others. A per-row history would make each version's story a fragment missing exactly that. Attached paperwork is
      *not* folded in, for the opposite reason: a release form has its own story, and mixing "somebody signed this"
      with "somebody re-cropped that" under one heading serves neither.

      **It filters on the access predicate, not the caller's query.** Whatever somebody searched for to arrive at an
      asset does not narrow what has happened to it. An asset outside their scope is 404 rather than an empty list,
      because "nothing has happened to this" is a different and untrue statement — and the gap between the two
      answers is an existence oracle.

      **One renderer, shared with the dashboard.** A history line and a feed line are the same sentence about
      different scopes, so the API returns the same shape and the phrasing moved into `$lib/activity`. A second copy
      would be a second place to add a verb when a new event kind appears, and the forgotten one would silently fall
      through to the plain form. A mutation to the shared module now fails *both* suites, which is what makes the
      sharing real rather than nominal.

      Eleven mutations caught, six in Rust and five in the panel. Driving it against the live server found what the
      mocked suite could not: the failure message and "nothing recorded yet" rendered together, which reads as "the
      read failed, and also there is no history" — a claim a failed read is in no position to make. The dev database
      was also a migration behind, which is how `LIBRARY_ROWS` referencing `attached_to` turned every listing into a
      500 the moment the API was restarted: a reminder that `mise run migrate` belongs in the same breath as a
      migration, not the next session.

      **Not done here:** the alternate preview upload that shared this Acquia slice. It is an ingest concern rather
      than a history one — a second rendition for an asset whose own bytes preview badly — and folding it in would
      have made this slice two features.

- [x] **Q.11 Asset conversions, and the download the DAM never had.** Four slices in one feature: the
      `conversions` table (Q.11a), its API and the offer for one asset (Q.11b), `POST /assets/{id}/download` plus
      the worker that renders a format on demand (Q.11c), and the panel that offers them (Q.11d).

      **The DAM had no authenticated download at all.** Only the share portal minted delivery URLs; a signed-in
      person could read "the original is available" with nothing to press. That is the larger half of this slice.

      **The cache key is the recipe, so there is no revision column.** Redefining a conversion *is* a different
      `op_hash`, and the next request renders fresh — a revision an editor bumps would be a second mechanism for
      what the first already guarantees. What `op_hash` cannot see is the renderer, which is global: one
      `RENDERER_REVISION` folded into every hash, built-in and tenant alike, because a database row cannot be
      hand-bumped in the commit that changes the pipeline. A consequence worth knowing: two names for one recipe
      share one rendered object.

      **A format not yet rendered is 202 with the render queued**, deduplicated on `(asset, conversion)` so twenty
      people choosing the same thing is one job. Not a dead URL, and not a synchronous render inside a request.

      **Two gates, asset first**, and a conversion's permission narrows only — `Caller::permissions` now carries
      the fine-grained `roles.permissions` strings that `Action` deliberately does not model. A format the caller
      may not use is absent from the offer and *named* on a direct request, which is the one place this departs
      from the hidden/absent rule: a format is tenant configuration, and somebody refused one is better served by
      knowing which permission to ask for.

      Thirty-eight mutations caught across the four slices. Three of my own tests were vacuous and mutation
      testing found each: an ordering case with one row in the table, a permission case that never ran against the
      unit implementing it, and a renderer-revision case that compared two hand-derived strings — which is why
      `op_hash_at` now takes the revision as an argument rather than reading a constant.

      **What only the live run found**, and neither the API tests nor the mocked browser suite could:
      delivery resolved a transform against the built-in profile set alone, so the download endpoint returned a
      perfectly good signed URL that then 404'd; and `request()` in the web client left `Content-Type` to each
      call site, so the one endpoint where I forgot it answered 415 while every mocked test passed. The header now
      lives in the helper once, the ~15 duplicated declarations are gone, and the download suite's mock enforces
      the header the way axum does — a mock more permissive than the server certifies bugs.

      **Open:** video conversions, which need a parameterised ffmpeg recipe (`transcode_proxy` is one fixed proxy,
      not a format somebody chooses); the CHECK is `media_class = 'image'` so the database cannot hold a promise
      nothing keeps. Also an administration screen for the format list — the API is complete and the UI for it is
      not, so a tenant configures formats through SQL or the API today.
- [x] **Q.12 Intended-use capture, and the cap that was decoration.** The question asked before a download, the
      record that makes the answer auditable, and — as a consequence — `license_scopes.max_downloads` finally
      refusing.

      **`rights_usage` has been summed against `max_downloads` since migration 0005 and nothing ever wrote a
      download row.** The cap permitted an unlimited number, exactly as that migration's own comment warned about
      `max_impressions`: "Without it, `max_impressions` is decoration." Writing the ledger closes it. Flagged in
      NEEDS-REVIEW.md because it changes outcomes for a tenant who already set a cap, and because history is not
      backfilled — every cap starts counting from now, since attributing past downloads to a licence scope would
      mean guessing, and a guess in a rights ledger is worse than a gap.

      **A default is not a declaration.** `channel` and `territory` are optional and their *presence* is what
      marks the record as declared — not a flag a client sets, which a client could assert without anybody having
      answered. Half an answer counts as none: a person who named a channel and left the territory has still been
      asked, but the record must not claim more than they said. Migration 0024 adds the column and a CHECK that
      only a download may claim one, because a connector report has no person at a dialog.

      **The download is recorded before the URL is minted.** An unrecorded download makes a cap under-count and
      permits more than the licence allows; a recorded one that then fails to mint over-counts and permits fewer.
      A licence breach is worse than an inconvenience.

      **Attribution comes from the evaluation, not a second derivation.** `Evaluation::consuming_scope` is the
      covering scope with the most headroom, counting uncapped as unlimited — the same rule
      `downloads_remaining` reports by, so recording a download makes the reported figure go down by one.

      **The vocabulary is derived, not configured**: the channels and territories the tenant's own licences
      reference, exclusions included, because "worldwide except China" makes `CN` worth declaring and the honest
      answer to declaring it is a refusal with a reason. Twenty mutations caught. One of my own tests was
      order-dependent — the exhausted-scope case only tried one scope ordering, so a mutation that attributed
      before checking the cap survived it; it now asserts both orderings.

      **Open:** a *declared* vocabulary of its own, for a tenant that wants options no licence mentions. That is a
      table and a screen, and the derived list covers the case where declaring changes an answer.
      Also open: the panel's own behaviour was verified by its twelve Playwright cases against a production build
      rather than by a live click this time — the dev detail panel would not open to synthetic clicks, and the
      substance (vocabulary, declared and defaulted records, attribution, the ledger read) was verified against
      the live server directly.
- [x] **Q.13a–c Orders: the request, the decision, and the two lists.** Fulfilment is Q.13d below.

      **The design delegates nothing**, which is the decision worth keeping. An order could grant the requester a
      download right on approval: a fourth kind of grant, with its own scope and lifetime, that ARCHITECTURE does
      not settle. Instead approval is a *decision*, and fulfilment creates a share link — the machinery that
      already answers who may take what, including rights re-evaluated at every delivery and revocation that
      stops URLs already issued. Written up in NEEDS-REVIEW.md, because the delegating reading is what most
      systems pick.

      Asking is Read, deliberately: the feature exists for somebody who may see assets and not take them, so
      requiring Download would restrict it to the people who do not need it. Deciding is Manage.

      Three rules make it an audit trail rather than a form. You cannot order what you cannot see — and a
      partly-visible request narrows rather than refusing, without saying which asset was invisible, because that
      would be the enumeration the filter prevents. An approver cannot approve what they cannot see, and is told
      how many are out of reach; rejection has no such requirement, or an order could reach a state nobody can
      close. And a requester cannot cancel after a decision, because an approval is somebody else's recorded act.
      Self-approval is recorded rather than prevented — prohibiting it would be inventing a tenant's policy.

      `expired` is deliberately not a state: an expiry is a timestamp passing, and a stored one would need a
      sweeper to stay true. The order carries the intended use (Q.12) and the format (Q.11), so the pickup's
      downloads will land in the ledger as declared and an approver agrees to a 2048px JPEG rather than a master.

      Twenty-five mutations caught across the model, the API and the two screens.

- [x] **Q.13d Order fulfilment: the pickup.** Approval now makes the pickup in the same request, and the share
      portal renders a *set* — the case it has said it could not show since 3.4.

      **The pickup is a share of kind `order`** (migration 0026 widens the vocabulary), pointing at the order
      rather than at a manufactured collection. An order is already a named list of assets with an owner, an
      expiry and a reason, so a synthetic collection per order would be a shadow object with nothing to add.

      **Per-item previews and per-item refusals.** An order of forty where two are unlicensed is a pickup of
      thirty-eight; collapsing that into one refusal would deny a recipient what they were entitled to because of
      somebody else's paperwork. The refused item is still listed by name, because the order is a record of what
      was asked for.

      **A pickup download lands in the ledger as a declared use** (Q.12), attributed to the requester: they named
      the channel and an approver agreed to it, which is a stronger record than most downloads carry. The
      recipient has no identity, so recording them is not an option and recording nobody would lose the only
      accountable party. Rights are evaluated before the ledger write and before the cap is spent, so a refusal
      costs the recipient neither.

      **The link is shown once and re-issuable.** A share token is stored as a digest, so the response that mints
      it is the only readable copy — which the first version of this slice missed entirely: the pickup was created
      and the token discarded, so an order could be fulfilled that nobody could ever collect. `POST
      /orders/{id}/fulfil` now re-issues, revoking the previous share so an order never has two live links.

      **The metadata export stops at the tenant's edge.** `GET /orders/{id}/metadata.csv` is for the requester or
      an approver — somebody signed in exporting metadata they can already read. Putting it in the *pickup* would
      send descriptive metadata to an outsider, and `field_defs` has no notion of which fields an outsider may
      see. Recorded in NEEDS-REVIEW.md rather than defaulted.

      Twenty-six mutations caught. Two guards were masking each other — the API and the db layer both refused a
      re-issue of a non-ready order, so removing either changed nothing observable; the redundant one is gone and
      the refusal now comes from the layer that owns the invariant. Mutation testing also found a leftover `if
      false` from an earlier run that had silently disabled the export's audience check; the tree was swept for
      other residue and is clean.

      **Not done:** a zip. The pickup is a list with a rights-checked download per item, which needs no worker, no
      archive storage and no second expiry. A single-file archive is a convenience, and `bulk_operations` already
      reserves `download_zip` for it.
- [x] **Q.14 Portals.** Standard, Brand, Video and Channel, branded, over the share-portal foundation.

      **A portal is a share with a name.** `share_links.kind` gained `'portal'`, and every question about access
      — the passcode, the expiry, the download cap, revocation, and the rights re-evaluated per delivery — is
      answered by the share machinery that already existed. The portal row carries what it *looks like*: a slug,
      a title, an intro, one of the four kinds, a logo asset, an accent. Two addresses, one page: the public slug
      and the share token both land in one `render`, so a check cannot hold on one route and not the other.

      **Presentation never changes permission**, with one exception that is stated rather than hidden: a Video
      portal narrows the set to `video/*`, because a video portal showing stills is a video portal in name only.
      Standard, Brand and Channel show whatever the collection holds. Nothing in the four kinds grants anything.

      **The slug resolves only when public and live.** The narrow lookup, so a private portal is not even a 403
      by name; a retired one is nothing at all. Retiring revokes the link in the same transaction — the portal
      alone would leave a live token rendering a retired page, and the link alone would leave a page nobody can
      reach and nothing saying why. Presentation can be edited; the *source* cannot, because a portal that
      swapped its set would show a different library to everyone holding the old URL.

      **The set is refused rather than guessed.** The schema anticipates three sources and this build shows one —
      a collection, where a person put each asset there on purpose. A saved search or a media class is a live
      query, so the portal would publish every future asset that happens to match: nobody decides, a rule does.
      Both are named in the request shape and answered 422 with the reason, so asking gets the decision back
      rather than "missing field `collection_id`". Written up in NEEDS-REVIEW.md with the three ways to make them
      safe, because what becomes visible to the public internet is not a default I wanted to pick.

      **Search inside a portal is `ILIKE` over the collection, not Tantivy.** The set is small and bounded, and
      routing an anonymous visitor's query through the index would put it against documents carrying group ids
      they have no predicate for. The count and the rows share one `member_query`, so a portal cannot say two
      hundred and show twelve for a reason other than the cap.

      **The accent is data, so the contrast is derived.** White on a tenant's `#ff6600` is 3.1:1 and the browser
      suite failed on it. `web/src/lib/portal-colour.ts` picks the ink by contrast and then shifts the background
      away from it in 6% steps until the pair clears 4.5:1 — the brand colour still reads as the brand, and the
      button is legible. Swept over every hue and the whole lightness range.

      Eleven mutations caught in the model and the API, seven more in the colour maths. Three of the eleven
      survived the first sweep and were real gaps: nothing tested that a *retired* portal refuses an edit, that
      the token's own liveness check holds when the link is un-revoked by hand, or that `LIBRARY_ROWS` keeps
      superseded versions and attached paperwork off the page.

      **Two bugs the browser found that no test could.** The portal minted a preview URL for the one asset its
      licence allowed, and the URL 404'd: `issue_for_share` checked rights and never checked that the rendition
      existed, so a derivative rendered under an older definition of the profile produced a signed URL pointing
      at bytes the current recipe has no row for. Every test asserting "a URL came back" passed. The check now
      happens at the mint, after rights and before signing — which also makes the "no preview has been rendered
      yet" sentence the share portal and this one both carry reachable for the first time.

      That check then failed the download suite, which is how the second one surfaced: a tenant conversion may
      be named `web-2048`, delivery resolves a name against the *built-ins* first, and the format was therefore
      queued and rendered under its own recipe's hash and served under the built-in's. A download that reports
      ready and hands back a URL nobody can fetch. A key that shadows a built-in rendition is now refused where
      an administrator can still pick another one, and the fixture that carried the collision was renamed.

      **Not done:** the tenant-facing screen for *making* one — see Q.14b, which is the slice that makes a
      collection something a person can create and curate. Until then a portal is created through the API.
- [ ] **Q.14b Collections in the application.** Q.14 exposed the gap: `dam_db::collections` has been done since
  2.3 — membership, dense ordering, `pin_hot` — and there is no way to make or fill one outside a test. A portal
  publishes a collection, so a portal cannot be created by the person who would want one. Needs a small API
  (list, create, rename, add and remove members, reorder), the bulk-bar action that puts a selection into one,
  and the portal administration screen on top of it.
- [ ] **Q.15 The built-in facets:** asset status, orientation, average rating, has-attachment. Orientation is
  free — it is a function of dimensions already stored.
- [ ] **Q.16 Search-within, substring, advanced search, multiple-asset search.**
- [ ] **Q.17 Predictive search and did-you-mean.**
- [ ] **Q.18 Export search results to CSV.**
- [ ] **Q.19 Refine-search configuration**, including dependent metadata fields.
- [ ] **Q.20 Site branding, webhook delivery, the admin worklists, tag vocabulary administration.** The
  worklists are the cheapest real value on this list: they are queries over data damrs already holds.

Absorbed by the existing roadmap rather than duplicated here: the AI set (tags, faces, document text,
transcripts, semantic search, duplicate detection) is M4; conversational access is M5's MCP server; workflow
and Insights are M6; FTP and import are Pre-GA G7; SAML/SSO and user administration are Pre-GA G10; the
storage and usage reports are G19. Entries (the PIM) is a new application and lands after them.

Not building: Hootsuite, Mobile, Templates, Video Creator, Syndicate, Digimarc, Google Analytics linkage —
third-party or separate products, reached through the API and webhooks, which are on the list.


## M5 — Hosted-model enrichment

Re-prioritised ahead of M4 and the rest of the Q slices on 2026-08-20, at your request: "claude/chatgpt/kimi/etc
and other over local ai". Three decisions you made when I asked, and they shape every slice below: **both
clients in the first slice** (Anthropic and OpenAI-compatible, per-tenant provider choice from the start),
**built against a fake with a key added later**, and **per-tenant BYO keys from the start** rather than one
platform key.

- [x] **M5a·1 Sealed credentials.** A tenant's API key has to be readable by the worker and unreadable in a dump,
      which is encryption at rest with a key the database does not hold. `dam_core::sealed`: ChaCha20-Poly1305,
      a per-purpose subkey from BLAKE3's `new_derive_key`, and the associated data bound to
      `tenant:provider:credential_id` so a ciphertext moved between rows or tenants fails to open rather than
      decrypting into the wrong context. A keyring, not a key: the first entry seals, every entry opens, which is
      what makes rotation a deploy rather than a migration.

      *Surprise:* my tamper test was flaky. Flipping the last base64 character can produce a non-canonical
      encoding, so the failure arrived as `Malformed` rather than `Refused` — the right refusal for the wrong
      reason. Tampering at the byte level and re-encoding fixed it; twelve consecutive passes.

      Two "caught" mutations were real survivors on a clean re-run: the version prefix was not actually checked,
      and the derivation was not domain-separated. Both are now.

- [x] **M5a·2 The credential table.** `ai_credentials` (migration 0027) stores the sealed key, a `…1234` hint for
      an admin to recognise it by, the provider, the base URL and the model. `dam_db::ai_credentials` never sees
      plaintext — it takes and returns sealed strings — so there is no path from a query to a key. One default
      per tenant per provider, enforced by a partial unique index rather than by application code.
      `sealed_under_other_keys` exists for the rotation case: it answers "what still needs re-sealing" without
      opening anything.

- [x] **M5a·3 The two clients.** `dam_ai::model` is the seam — `Ask`/`Part`/`Effort` in, `Completion`/`Usage`
      out, `ModelError` classified into what a queue can act on. `dam_ai::anthropic` speaks
      `POST /v1/messages`; `dam_ai::openai_compatible` speaks `/chat/completions` and therefore speaks ChatGPT,
      Kimi, DeepSeek, Together, Groq and every local server imitating them. `dam_ai::http` is the reqwest
      transport, kept to the three decisions that need making (status is not an error, `Retry-After` is lifted
      out, always a timeout). `dam_ai::testing` is the recorded transport, behind a feature so it cannot reach a
      production build.

      **The request is the part that can be wrong**, so that is what the suite reads. A live call would prove the
      provider answered; it would not prove the cache breakpoint sits at the end of the stable prefix, or that
      `output_config.format` is used rather than the deprecated `output_format`, or that `budget_tokens` — a 400
      on this model family — is absent, or that an image goes to Anthropic as `source` and to the other family as
      an `image_url` data URI. Seventeen tests, most of them reading JSON that was never sent.

      **A refusal is a 200.** Both providers say no in the body of a successful response, and the check happens
      before the content is read — otherwise a refusal returns an empty completion that looks like success, and
      the queue retries it to the attempt limit before dead-lettering an asset that was never going to work.

      **The two vendors disagree about what `prompt_tokens` means** — Anthropic's `input_tokens` excludes the
      cache read, OpenAI's includes it — so `Usage` normalises, or a cost estimate overstates one provider by the
      size of the cache hit, which for a shared taxonomy prefix is most of the prompt.

      *Surprise:* the `Transport` trait as first written returned `(status, body)`, which silently made
      `Retry-After` unreachable — a 429's wait is a header, and the client was left inventing one. It returns an
      `Answer` now, with that one header lifted out and no general header map, because nothing else reads one.

      **Nothing here has spoken to a real provider.** Fixtures are transcribed from the vendors' documented
      examples, so this suite cannot notice a vendor changing a field. That is the honest limit of a recorded
      transport and the reason M5a·4 carries a smoke test.

- [x] **M5a·4 Provider selection, the admin surface, and the spend cap.** `dam_ai::credential::open` turns a
      stored row into a client, refusing three things separately because the fixes differ: a provider this build
      has no client for (an older binary against a newer row), a key the keyring cannot open (a rotation that
      dropped a retired key), and an OpenAI-compatible credential with no endpoint (never usable, and refused on
      the way in rather than at enrichment time). `GET/POST/PUT/PATCH /ai/credentials` is the admin surface, and
      `POST /ai/credentials/{id}/verify` asks the provider one short question — the only real call in the
      codebase, and the only thing that can tell a pinned request shape from a working integration.

      **A key goes in and never comes out.** One route accepts plaintext; none returns it. The suite reads the
      raw response body looking for the key it just sent, because an admin surface that could show a key turns
      every session into an exfiltration path. Rotation is a replacement, and `needs_resealing` is computed from
      the sealing key id without opening anything.

      **Budget caps (G20), which the schema has had since global 0002 and nothing had ever read.** `tenant_quotas`
      and `tenant_spend` now have a reader: check before a call, charge after it, because a call's cost is not
      known until it returns. The cap can be overshot by the calls in flight when the limit is crossed — bounded
      by concurrency rather than by library size, which is the trade a reservation ledger would buy back at the
      price of a compensating write on every failure.

      *Surprise, and it would have made the cap decoration:* `used_value` is a bigint of whole units and one
      enrichment call costs a fraction of a cent, so charging in cents rounds every small-model call to nothing.
      Migration global 0003 adds `spend_remainder_micro`, charges arrive in millionths, and the sub-unit part is
      carried into the next charge. There is a test that a hundred charges of a third of a cent are thirty-three
      cents.

      *Second surprise, found by the integration test and not the unit test:* `warn_at_fraction` is a `real`, so
      a configured 0.8 is stored as 0.800000011920929, and rounding the warning line *up* put it at 81 cents of a
      100-cent cap — the warning fired late, and on a small enough limit never at all. It rounds down now, which
      can fire a unit early; that is the direction a warning exists for.

      Prices are configuration (`ai.prices`), merged over a built-in table, and an unpriced model is charged at
      the most expensive rate in it — an understated estimate lets a cap be blown through silently, where an
      overstated one stops work and somebody notices.

      The screen is `/settings/ai`: the cap in dollars, the credential list with its hint and its rotation
      worklist, and a verify button that distinguishes "the key is wrong" from "the model declined", because the
      second means the key is fine and somebody told only "failed" would re-issue it.
- [x] **M5b The enrichment job, end to end.** One asset, one model call, and the seven things that have to be
      true around it — each of them a way for a paid pipeline to go wrong quietly. `dam_ai::enrich` builds the
      ask, `dam_db::enrichment` writes the answer, `dam_pipeline::enrich` is the stage, and `/ai/*` plus
      `/review` are how a person configures it and checks it.

      **Off by default, and that is a migration not a comment.** `enrichment_settings` (0028) is a singleton row
      whose `is_enabled` starts false. §8.3 puts a naive run over a million assets at $23k, so a feature that
      billed per asset and started switched on would produce an invoice before a decision. Turning it on with no
      credential is refused where the person who can fix it is looking, rather than becoming a queue of runs that
      all say "no credential".

      **A suggestion is not a tag.** Every LLM tag lands `suggested` whatever confidence the model claimed —
      self-reported numbers are not calibrated, and `taxonomy_terms.ai_threshold` exists for the probe paths
      where they are measured. So `/review` exists, and *No* is a first-class button there: `tag_feedback` is the
      training set, and 0003's own note says losing the rejections loses the signal that matters most.

      **A model never overwrites a person.** A value with no provenance was typed by somebody, so a re-run leaves
      it and the run says `kept_human`. The converse matters more: a person's edit *removes* the marking, because
      a disclosure that claims a human sentence is machine output teaches people to ignore the marking. The
      metadata route now clears provenance for every key it touches.

      **The vocabulary gap is kept.** A word the model reached for that the tenant has no term for is the most
      useful thing in a paid answer, so it lands in the run's `stages` rather than being dropped.

      **G2 marking is a row, not a flag.** One `ai_disclosures` row per written field, `metadata_only` because
      the picture is untouched and only its description is machine-written — 0006's grading exists precisely so
      an authentic photograph with an AI-written caption is not labelled "AI generated". The prompt is stored as
      a digest; a tenant's guidance is its own business.

      *Surprise:* `PATCH /assets/{id}/tags/{term}` returned 200 with an empty body, and the web client — which
      tolerates a 204 and parses everything else — threw on it. The browser suite found it; the endpoint answers
      204 now. A 200 with no body is a shape every client has to special-case.

      Twenty-eight mutations caught across the prompt, the writes, the queue and the stage.

- [x] **M5b·4 The disclosure on the asset itself.** A "Written by AI" panel on the detail view, listing each
      machine-written field with the model that produced it, the confidence it claimed, and whether anybody has
      checked it. Read-gated, because a marking only administrators can see is not a disclosure — that is the
      whole obligation. Graded, too: it says the *words* are a model's and nothing about the picture, which is
      the distinction 0006's `disclosure_kind` exists to draw.

      Closed by default and fetched only when opened, like the history panel. An asset a model never touched says
      so rather than hiding the panel — "no AI here" is an answer somebody may specifically be looking for — and
      a failed read says only that, because a panel whose request failed knows nothing about what is on the asset.

      Six browser cases including both themes; verified live against the dev tenant, where the description a
      batched model wrote shows up under the asset with its model name and "not checked yet".
- [x] **M5c Batch backfill.** §8.3: "all library backfill runs here, never synchronously", and its cost table is
      why — the same million assets are ~$23k synchronously and ~$6–8k batched with a cached prefix. `dam_ai::batch`
      speaks the three calls (submit, poll, results), `dam_pipeline::backfill` is the two stages, and the chain is
      driven by the queue: one batch at a time, the next slice starting only when the last lands.

      **The run rows are the state, and there is no batch table.** `llm_batch_id` says which batch a run belongs
      to, `llm_custom_id` says what its answer will be called, `state = 'running'` says it is open. A worker that
      dies mid-batch loses nothing. The custom_id is the run id and is persisted *before* submission, because
      results come back unordered and a mapping held in memory would not survive a restart.

      **Every terminal state is handled, including the one that never arrives.** Errored is a failure; expired
      and cancelled are not — those requests never ran and were never billed, so their assets go back on the work
      list. A request missing from the results is closed as failed rather than left running, because an open run
      hides its asset from the work list for good.

      **Anthropic only, and it says so.** The OpenAI-compatible family batches through a file upload and a
      different polling shape; a tenant on one of those keeps the synchronous path and is told that, rather than
      quietly paying twice.

      *Surprise, and it only showed up live:* the collector re-queued itself under its own dedupe key. The dedupe
      index covers `queued` and `running` jobs — and a handler re-queueing itself *is* running, so the insert
      conflicted with the job doing the enqueueing, `enqueue` returned that job's own id, and the chain ended the
      moment it completed. One poll, "still working", and a batch nobody ever came back for. Fixed by leaving the
      key off the self-requeue, written up on `JobSpec::dedupe_key` because any future chained stage could fall
      into it, and covered by two tests that drive the chain through the queue rather than calling the stages.

      Twenty mutations caught, including that one re-expressed as a mutation. Verified live: submit → "still
      working" → re-queue → apply, with the batch charged at 0.375¢ against the 0.75¢ the synchronous path
      charged for the same shape.
- [x] **M5d·1 Natural language to a query.** §8.3 calls it "NL search → structured query IR"; it produces
      **shorthand** instead, and that is the design rather than a shortcut. `dam_core::shorthand` is already the
      one validated entry point for a query, so a model emitting shorthand goes through the same parser a person
      does — §12's argument, applied. It is also *visible and editable*: the answer lands in the search box, so a
      wrong query is correctable rather than mysterious, and the results come from the ordinary search path
      instead of a second retrieval route into a governed library. And it cannot widen anything: the parsed query
      is composed with the caller's predicate like any other.

      **Its own switch** (`enrichment_settings.natural_language_search`, migration 0029, off by default). Two
      features, two costs, two decisions: describing the library bills per asset, answering questions bills per
      question — and adding a key so the library can be described should not silently make every reader's search
      box a paid endpoint. Gated on Read, because searching is what a reader does; the spend cap is the control,
      and the endpoint answers **429** when it is reached, which is the first genuine 429 in the API.

      **A query that does not parse is reported, not returned as usable.** The parser's own message and column
      travel back, the question stays in the box, and the screen says to press Search to look for the words —
      which costs nothing and is what the box would have done anyway.

      The date goes in the *question*, not the instructions: a date in the cached prefix invalidates it every
      midnight, and the vocabulary is what the prefix is for. Sixteen mutations caught; three survivors on the
      first sweep were real gaps — an untested clamp, an untested wrong-shaped answer, and a vocabulary
      assertion that only checked a heading.

      Verified live: the prompt carried the tenant's fields and categories, the answer parsed, and running the
      returned shorthand through `/search` found the asset.

- [x] **M5d·2 The MCP server.** Five tools at `POST /mcp`, over rmcp's streamable-HTTP transport, and the whole
      design is what the crate does *not* contain: `search_assets` **is** `dam_api::search::run`,
      `get_download_url` **is** `dam_api::downloads::issue`. Both were split out of their route handlers for
      this. §8.5 says "over the same ABAC layer", and the strongest form of that is calling the same functions —
      a second implementation would be a second place where the predicate is composed, rights are evaluated and
      the ledger is written, and the drift would be invisible until an agent saw something it should not.

      **Authorisation is per call, from the HTTP request.** rmcp injects the request parts into each tool call,
      so every call re-reads the bearer token and re-authorises. A key revoked mid-session stops working on the
      next call rather than at the end of the session — there is a test that revokes one and calls again.

      **Absence is the refusal.** An asset out of scope is "no such asset, or not one this key may see", the
      same sentence a nonexistent id gets, on both paths that can produce it. The gap between "you may not see
      it" and "it does not exist" is an existence oracle, and an agent is exactly the caller that would map it.

      **Refusals are tool errors, not protocol errors.** A refusal, a missing asset, a query that does not parse:
      all come back as `isError` with a sentence, because a protocol error tells an agent's *client* that the
      server is broken — a different and usually false claim. A tool name that is not in `tools/list` is the one
      genuine protocol error.

      Off by default (`server.mcp_enabled`), like every other switch here that opens something: it grants nothing
      a key does not already grant, but it is a second protocol surface with its own framing and rebinding
      checks. `dam_db::rights` gained `evaluate_on`/`inputs_for_on` along the way, because a server that serves
      whichever tenant the key belongs to has no one pinned pool to hand over.

      Fifteen mutations caught, seventeen tests. Verified live against the running stack: initialize, tools/list,
      all five tools, and both refusals — an unauthenticated call and a bad key get the same sentence and leak
      nothing about the library.

## A mutation sweep killed mid-run used to leave the source mutated

Recorded because it cost real time twice and looks exactly like a code failure.

`hunt.py` restored the file in a `finally`, which does not run when the process is killed — and a sweep that
exceeds a command timeout *is* killed. The residue then reads as something else entirely: the next sweep reports
`SKIP … text not found` (the original is gone), and a plain `cargo test` run passes or fails over code nobody
wrote. One instance sat in `dam-api/src/orders.rs` as an `if false` that had silently disabled an audience check,
and a test caught it only because that check had a test.

The harness now snapshots each file before editing, handles `SIGTERM`/`SIGINT`, restores anything a previous run
left behind on the way in, and takes `--restore` to clean up by hand. Two habits go with it: `git diff` before
believing a test result during a sweep, and `rg "if false|if true|== \"never\""` over `src/` after one.

## Observed flakes

Failures seen once that were not reproducible, recorded so a later session does not mistake one for a
regression — and does not claim a fix nobody made.

- **`web/e2e/schema.e2e.ts:372`**, seen once in a full run: passed 96 consecutive repeats and three clean full
  runs afterwards. Not reproduced, not claimed fixed.

- **`dam-store::s3_conformance::a_plain_http_endpoint_gets_a_client_with_no_tls_at_all`**, seen 2026-08-20 under
  full-workspace parallel load: `TrustStore configured to enable native roots but no valid root certificates
  parsed!` from `aws-smithy-http-client`'s rustls provider. Passes in isolation, twice in a row, immediately
  after. The message comes from reading the *OS* trust store, which this test does not need — the endpoint under
  test is plain HTTP — so the likely cause is the macOS keychain being briefly unreadable while a dozen test
  binaries and containers start at once.

  **Reproduced a second time** later the same day, again only under a full-workspace run and again passing in
  isolation (twelve consecutive runs). Frequent enough to plan for, then. The test's own comment already
  documents the mechanism: `aws-smithy-http-client` loads the platform root store **once per process** into a
  `LazyLock`, and if that single load comes back empty — which a concurrent macOS keychain read can cause — every
  later client construction in the process trips a `debug_assert!`. The production code already avoids that path
  for plain HTTP; what trips it is another test in the same binary building a TLS client.

  Not a damrs defect, and no fix taken yet because each candidate has a cost worth choosing deliberately:
  (a) run `dam-store`'s suite with `--test-threads=1`, which slows the gate's slowest crate; (b) give the TLS
  client an explicit webpki trust store instead of the platform one, which changes production behaviour for
  customers with private CAs and is not a change to make in order to quiet a flake; (c) pin a newer
  `aws-smithy-http-client` if it stops asserting on an empty store.

- **Docker vanishing mid-run.** Twice on 2026-08-20 OrbStack stopped during a full suite, which surfaces as
  `postgres did not accept connections` or `Socket not found: /var/run/docker.sock`. After an OrbStack restart
  the `/var/run/docker.sock` symlink does not come back, so testcontainers needs
  `DOCKER_HOST=unix://$HOME/.orbstack/run/docker.sock`. Neither is a code failure; both look like one in a log.
