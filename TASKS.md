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
| **F.11b** Share/portal UI, schema administration, metadata types | complete, restore UX included |
| **Q** Commercial-DAM parity, 20 slices | **complete** — Q.1–Q.20, including Q.14b collections and Q.20a–d |
| **Go-live Tier 1** Deployment image, backups, metrics, rate limiting, virus scan, C2PA | **done** — all six, each verified against the running stack |
| **Archival** Tiering engine, restores, the storage screen | **done** — the sweep and poll jobs, the plan/quote/request/approve API, delivery's 202, bulk archive and restore, the restore panel |
| **M3d** Drupal 11 connector | **done** — M3d·1–·4, and M3d·5's six submodules, verified against a live Drupal 11.4 |
| **M4** Local AI: embeddings, OCR, ASR, faces, dedup, colour | dedup and colour **done** (M4a); the rest needs model files — see M4 below |
| **M5** Claude enrichment, MCP server, AI Act marking G2, budget caps G20 | **done** — two clients, BYO keys, spend caps, the enrichment job, G2 marking, the review queue, batch backfill, NL→query, the MCP server |
| **M6** Workflow/proofing, annotations, analytics | **done** — annotations (M6a), proofing (M6b), analytics (M6c) |
| **Pre-GA** Import G7, SCIM/BYOK/audit G10, DR G11, metering G19, quotas | G19 **done**; G7 **done** (crosswalk, dry run, filesystem source, transfer); G10 **done** (audit chain, user administration, SCIM, BYOK) |

**Next up, in order:** G7, G10 and M3d·5 are complete. What remains is decisions rather than build work: G22c (the public URL space), G10·3b (per-tenant keys), the AWS-native items 1 and 2, and M4b's model-distribution question.
M4b (local models) is parked on a distribution decision — see the M4 section.

**`NEEDS-REVIEW.md` is empty.** Every parked question was answered on 2026-08-21 with the recommendation each
note carried; `DECISIONS.md` records what was chosen. Two of them needed code: a portal may now be backed by a
live query because publication became a per-asset act (Q.14 above), and a namespace wildcard in a permission
string is expanded, which fixed a seeded `admin` role that conferred nothing unless its holder also carried the
tenant-admin flag. C2PA (task 1.9) is unblocked and still unbuilt.
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
- [x] **F.11b·2b Restore UX.** Done with the archival slice: the detail panel quotes all three tiers with
      their ETAs before the button, shows the unavailable one refused with its reason rather than hidden, and
      becomes a status once a restore is running. `/storage` is the administrator's half — every rule with its
      dry-run state and a Plan button, skips grouped by reason with the pins spelled out.

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
### Pre-GA

- [x] **G19 Quota enforcement.** `tenant_quotas` and `tenant_spend` have been in the schema since global 0002.
  G20 (AI spend) already read them; every other key had a cap nobody enforced — and M6c is what made them
  enforceable, because it is the first thing that ever wrote `tenant_usage_daily`.

  **A level is set, not charged, and confusing the two is not a subtle failure.** `charge` accumulates, which is
  right for a flow: cents spent, bytes served, restores requested. A *level* is a measurement of what exists —
  bytes stored, assets held, seats occupied — and the metering pass remeasures it daily. Feeding one through
  `charge` would add the whole library to the counter every pass, so a tenant holding a steady terabyte would
  trip a two-terabyte cap on the second day without having stored anything more. So `observe` sets rather than
  adds, it refuses a flow key by name rather than trusting the call site, and a level goes *down* when the
  library does — which a counter could never do, and which matters because a tenant who tidied up has to be
  allowed to work again.

  **Refused before a byte moves.** The gate is at TUS session creation, not at finalise: the worker runs from a
  queue, so refusing there arrives after the client uploaded the whole file and waited on a job that was always
  going to say no. **507, not 413** — an integration can act on the difference (413 means send a smaller file;
  507 means nothing you send will work until somebody raises the cap) and collapsed into one status a client
  retries with progressively smaller files forever.

  **Soft is the default and must not read as an outage.** A hard cap on ingest loses a customer's work, which is
  why the schema makes enforcement per-quota. A row over a soft cap says the work continues.

  **The stamp can lag a level by one measurement, and that is what the number means.** `check` is a read on the
  request path, so it deliberately does not write. A cap lowered below a tenant's current level refuses the very
  next upload while `exceeded_at` stays null until the next pass — observed live. For a level that is not a gap:
  nothing else measures, so "when did this start" can only mean "when did a measurement first show it".

  Reading a cap needs `Manage` — somebody who can upload should not learn how close the library is to a limit
  they cannot change — and there is no way to *set* one through the API at all. A tenant raising its own limit is
  not a feature; that is `damctl quota`. The screen exists because a 507 carries no body: the explanation has to
  be available before the wall, not at it.

  5 new db cases (14 total), 1 API container, 3 upload-gate cases, 10 browser cases. Verified live: the worker's
  boot pass measured 183 assets, a cap of 100 refused the next upload with 507, softening it returned 201,
  raising it returned 201, and forcing a metering pass stamped `exceeded_at`.

- [x] **G7·1 The crosswalk, the dry-run report and the phase machine.** `import_jobs` and `import_records` have
  been in the schema since tenant 0008 with nothing reading them. §G7 is blunt about which part matters:
  "underestimating metadata cleanup is the single most common cause of failed DAM migrations." Moving bytes is a
  loop and a `PUT`; the mapping is what fails.

  **The dry run uses the real validator.** `crosswalk::apply` produces a payload and nothing more; whether it is
  *acceptable* is `fields::validate`'s answer, and the dry run asks it. A dry run with its own idea of validity
  would certify something different from what the transfer does — a signed-off report followed by a failed run,
  which is worse than no report.

  **An empty source cell is not a finding.** A CSV header lists every column and most rows leave most blank.
  Reporting each would bury the twelve that matter under forty thousand that do not, and a report nobody reads
  certifies nothing. But a value that carried and went nowhere is *always* a finding — unless the crosswalk
  says it was decided against, which is why `ignored` exists as a separate list from "not mapped yet".

  That distinction produced the one design change my own test caught: `total_losses()` was listing deliberately
  ignored columns among its losses, which defeats the point of being able to decide against a field. The column
  an operator scans has to be able to shrink to nothing, or they stop scanning it.

  **Nothing is guessed.** An unparseable date is dropped and named rather than parsed hopefully — `03/04/2026`
  is two different dates depending on locale, and a plausible wrong date is worse than a missing one because
  nobody notices it for two years. A mapping miss is the caller's decision in all three directions, because
  keep, drop and fail are each right somewhere: an open keyword vocabulary wants keep, a closed list wants drop,
  and anything a rights decision rests on wants the asset not to arrive at all.

  **The phase machine advances one step, with one loop.** A jump from `discover` to `transfer` would move a
  library under a crosswalk nobody reviewed. Writing that rule as "forward only" lost something the design
  needs, and the test caught it: `verify → transfer` has to be legal, because "phased/incremental transfer
  rather than single cutover" means the transfer/verify pair runs once per batch, many times, before anything
  completes. `failed` is deliberately not terminal — a run that failed on a bad mapping is fixed by changing the
  mapping, which is what 0008 means by "editable between phases".

  **Records are never deleted, not even by a rollback.** 0008 retains `source_id` permanently because "two
  years later, 'which source asset did this come from' is a question that gets asked", and a second attempt needs
  to know what the first one did. A rollback takes only what the job created *and nothing has touched since*, so
  an escape hatch cannot become a second incident — and a legal hold still refuses, which the command says out
  loud rather than swallowing.

  **Records arrive as JSON lines on stdin**, not through a reader per vendor. That is §G7's architecture read
  the other way round: the mapping is the hard part and it is source-agnostic, so anything that can emit JSON
  lines is a source — `jq` over an incumbent DAM's response, a spreadsheet converted, a script walking a file share. It
  also avoided a dependency decision that is not mine to make: a correct CSV reader wants the `csv` crate rather
  than a hand-rolled quoting parser.

  11 pure crosswalk cases, 8 db cases. Verified live against the dev tenant's real twelve fields: a first-pass
  crosswalk reported `Photographer` arriving nowhere across every record and two unparseable dates; correcting
  it reported "every source column that carried a value arrived somewhere" with one warning left — and that
  warning is a genuine finding the tool surfaced, because the source data has **mixed date formats** and no
  single crosswalk can handle both. Which is exactly the metadata cleanup §G7 says migrations underestimate.

  Two things writing a real crosswalk file caught. `Transform` was externally tagged, so a transform with no
  parameters spelled `"copy"` while one with parameters spelled `{"split":{"on":";"}}` — and `{"copy":{}}` failed
  with "invalid type: map, expected unit". It is internally tagged on a `type` field now, one uniform shape. And
  the id column was appearing in every report as a column that arrives nowhere; it is consumed as the source
  identifier, so it is not a loss.

- [x] **A.6 A multi-tenant load pass, and the two things it found.** Five tenants, 481 synthetic assets
  uploaded through the real TUS path across four of them in 107s with no failures, all processed by the worker
  to three derivatives each. Images generated with vips rather than reusing the dev tenant's real photographs,
  and each one distinct — content addressing would have deduplicated identical files into a single asset and
  the load would have been imaginary.

  **Metering never saw a tenant provisioned against a running worker, which means it never billed one.** The
  chain was started only in the boot path: nothing enqueues one at provision, and `enqueue_usage_rollup`'s own
  doc-comment claimed it was "started at provision" — describing an intent no caller implemented. Provisioning
  three tenants against a running stack and uploading 360 assets between them produced no `usage_rollup` job,
  no `tenant_usage_daily` rows, and `damctl usage` reporting nothing for any of them. Since that table is what
  an operator bills from, they were unbilled, and would have stayed so until somebody happened to restart a
  worker.

  Fixed as a sweep on a cadence rather than an enqueue at provision, and the reason is that the fix should not
  be forgettable: an API endpoint or a migration script that creates a tenant tomorrow gets metering without
  knowing to ask. It also repairs a chain that broke, which the metering module already worried about — "a gap
  in a billing series is indistinguishable from a worker that was down". One query finds active tenants with no
  `queued`-or-`running` rollup, which is the same condition the dedupe index is partial on, so a tenant
  mid-chain with a future `run_after` reads as covered and normally the query returns nothing. Verified live:
  three chains started within seconds of the restart, and every tenant now reports its own asset count and
  stored bytes.

  **Delivery serves exactly one tenant per process, and nothing was queued to change that.** `globex`'s
  thumbnail URL 404s while `acme`'s 302s, because the delivery path resolves its tenant from
  `server.delivery_tenant` rather than from the signed claim — the claim carries asset, transform, channel,
  territory, identity, share link, expiry and key id, and no tenant. DECISIONS.md records the choice and says
  it "becomes unnecessary once 3.x puts the tenant in the claim", and the 404 is the deliberate behaviour: the
  design refuses rather than inferring and minting URLs against another tenant's objects. But no open item
  carried it, so a multi-tenant deployment needing delivery for more than one tenant currently needs one
  `damd` per tenant, and the work to fix it was not on any list. It is now — see G22 below.

  **What held.** Every isolation surface, checked rather than assumed: cross-tenant detail reads all 404 in
  both directions; search returns only a tenant's own filenames and zero hits for another's corpus; every one
  of 480 object keys sits under its own tenant prefix; each tenant's audit chain is its own and verifies, with
  40 concurrent governance writes across four tenants leaving four intact chains and no forks; quota
  enforcement refuses an over-cap tenant with 507 while an uncapped one uploads normally in the same second.
  A two-asset gap between `acme`'s database count and its API total turned out to be exactly right — one
  superseded version and one model release, both excluded by `LIBRARY_ROWS`, which says so.

  *Residue in the dev database:* four tenants (`globex`, `initech`, `umbrella`, `harbour`) each holding 120
  synthetic `plate-*` assets, ten legal holds each, and a soft `asset_count` quota on `globex` left from the
  enforcement check. Say the word and it goes.

- [x] **The browser suite's flakiness had one cause, and it was countable.**

  Playwright's `click()` waits for an element to be *actionable* — attached, visible, stable,
  unobscured. It cannot wait for a Svelte handler to be attached, because the DOM does not expose that.
  So a click landing between first paint and hydration hits a button that looks entirely ready and does
  nothing: no request, no panel, and a failure that surfaces later as `element(s) not found` on whatever
  the click should have produced, thirty seconds from its cause, in an assertion that is not wrong.

  **The distribution proved it.** No settling pattern existed anywhere in the suite, and 85 places
  navigate and then immediately click or fill — 30 of them in `browse.e2e.ts`, which is why that file
  appeared in every failure list while passing 52 of 52 in isolation three times running. Failures
  tracked unsettled navigations rather than any particular test, which is why five observations named
  five different tests.

  Fixed at the fixture rather than the call sites: `e2e/fixtures.ts` overrides `page.goto` to settle, so
  the guarantee belongs to navigation. Eighty-five `waitForLoadState` lines would have fixed the
  eighty-five that exist and none written next month.

  **One test opts out, and it found itself.** `branding.e2e.ts` asserts that a customer's header never
  flashes the vendor's name *while branding is still loading* — visible only before the load finishes.
  Settling unconditionally made it watch the settled page and fail, correctly. An explicit `waitUntil`
  now means the test knows which moment it wants and the fixture stands aside.

  **Measured, and not cured.** Before: `--workers=4` failed two tests reliably, and three default runs
  lost 4, 3 and 1 of 410 with no test failing twice. After: three `--workers=4` runs clean at 410, and
  one failure across three default runs. A large reduction rather than zero, so the CI retries stay —
  covering a rare residue now instead of masking a systematic race.

- [x] **G22b The signed-token path resolves its tenant from the claim.**

  `/d/{token}` reads the tenant out of the verified claim, resolves the slug through
  `provision::slug_of`, and opens one `TenantConn` for the request. `server.delivery_tenant` no longer
  decides which library a token is answered from.

  **One transaction covers the whole request, and that turned out to be right rather than a compromise.**
  The rule everywhere else in this codebase is to release the connection before store I/O — `scrub` and
  `tiering::one_policy` both do. Delivery does not need to: every step is a local read and `presign_get`
  signs with HMAC without calling S3. Checked rather than assumed, because holding a transaction across a
  network round trip is exactly the mistake that rule exists to prevent.

  **The circular dependency, and how it resolves.** `connectors` is a tenant table and a connector's own
  secret verifies its token, so the tenant has to be known before the signature can be checked — and the
  tenant is inside the signature. `signed_url::tenant_id_of` reads it unverified to choose the schema,
  mirroring the argument `key_id_of` already makes for the key id, and the verified claim is asserted
  against that value afterwards so nothing downstream trusts the unverified read.

  **The protection is now structural, which a test proved by not failing.** Disabling the
  claim-versus-scope comparison left the cross-tenant case passing: the library is chosen *from* the
  token, so there is no path on which one could be answered out of the wrong library. The comparison
  stays — it is cheap and it pins that the two reads cannot drift — but the test was rewritten to
  exercise the real mechanism, against a second tenant that genuinely exists.

  **And that new case found a 500.** A valid token for a tenant whose schema lacks the asset reaches the
  rights evaluation, which returns `NotFound`, which delivery mapped to `Internal`. It is a refusal now.
  Unreachable before, because a cross-tenant token was stopped by a comparison — so this is a fault G22b
  created and the same change's test caught.

  `ConnectorAuth` lost its configured slug, which is dead once the slug comes per request and was the
  thing limiting a process to one tenant's connectors.

- [ ] **G22c Give the public visitor URLs a tenant, and then delete `server.delivery_tenant`.** The half
  G22b did not reach, and the reason it exists at all — which the old entry did not capture.

  `/p/{key}` names a portal and `/s/{token}` names a share. `portals` and `share_links` are **tenant
  tables**, and neither URL carries a tenant, so the process has to already know which library to look
  in. That is why `delivery_tenant` survives: not for the token path, which now resolves itself, but for
  the eighteen `.pool()` reads on the visitor surface.

  **The two URLs do not face the same problem, which halves the decision.** *Measured 2026-08-28; the
  eighteen `.pool()` reads are still exactly eighteen, in `shares.rs`, `portals.rs` and `downloads.rs`.*

  `share_links.token` is 256 bits from the OS CSPRNG. `portals.key` is a human-chosen slug —
  `^[a-z0-9][a-z0-9-]{1,62}$`, commented in the migration as "the URL name, when public". That difference is
  not cosmetic:

  - **`/s/{token}` has no collision problem to solve.** Two tenants minting the same 256-bit token is not
    something that happens, so a global token→tenant lookup in `dam_global` costs nothing in collisions and
    changes no published URL. The trilemma below simply does not apply to shares.
  - **`/p/{key}` is the whole of the problem.** Two customers will both want `spring-2026`, and a slug is
    chosen precisely so it can be typed and remembered. Every cost in the three options — collision, URL
    migration, length — lands here and only here.

  So whatever is picked can be picked for portals alone, and shares can be resolved globally today. Worth
  knowing before treating this as one uniform change to "the visitor surface".

  **What a global registry actually costs, for either.** A row in `dam_global` written when a share or portal
  is created, which is a cross-schema write on a path that currently touches one schema. D2's "no joins
  across tenants" is about queries rather than about a lookup table, and the control plane already holds
  `tenants` and `storage_pools` — but it is a second place a share exists, and a share deleted in a tenant
  schema without its global row removed becomes a token that resolves to a tenant and then finds nothing.
  That is a reconciliation job, not a constraint, because the two live in different schemas.

  **This is a decision about the public URL space rather than a refactor**, which is why it is its own
  item. Three shapes, and they are not equivalent: a globally unique key registry in `dam_global` (one
  lookup, but portal keys stop being per-tenant names and two customers can collide); a tenant in the
  path or a subdomain (no collision, but every published portal URL changes); or keys that carry an
  encoded tenant (no migration, longer URLs, and a format to version). Pick one before writing code —
  the current single-tenant behaviour is correct and documented, so there is no pressure to pick fast.

- [x] **G7·2 Source connectors and transfer.** `damctl import transfer` streams a folder of files into the
  library through the ordinary upload path. `dam_pipeline::source` holds the `Source` trait and the filesystem
  implementation; `dam_pipeline::transfer` holds one record's worth of work; the loop, the batch ceiling and
  the phase gate are in `damctl`.

  **Everything the plan said held.** The filesystem was the right first connector — the slice was driven end
  to end against real files, which is where four of the five findings below came from. Transfer has no ingest
  of its own: it opens a session, streams the bytes, and calls `finalise`. Records still do not carry their
  source payload, so the transfer re-reads the JSON lines and `source_id` is the idempotency key.

  **The prerequisite refactor found a bug before the third copy arrived, which is what it was for.** The two
  metadata writes had already drifted: the bulk executor's, documented as merging "exactly as the single-asset
  PATCH endpoint does", omitted `enrichment::forget_provenance`. A bulk edit left a model's marking on a field
  a person had overwritten, so every AI disclosure built on it named a model as the author of somebody's
  sentence. Now `dam_db::metadata::merge` and three callers.

  **Five things only running it could have found.**

  1. **`finalise` does not queue the follow-on work — its caller does.** Its one production caller is the
     worker's finalise handler, which calls `enqueue_derive` afterwards, and that single enqueue is what
     chains derive → index → similarity → enrichment. The first real run produced five assets with a
     placement each and nothing queued: no proxy, no thumbnail, nothing in the index. The library looked full
     and searched empty — precisely the drift the "no second ingest" rule exists to prevent, arriving through
     the gap between `finalise` and the thing that calls it.
  2. **A migration must not queue its renders in the interactive band.** `enqueue_derive` uses priority 40 on
     the premise that somebody is watching the grid for a thumbnail. True for an upload; false for the four
     hundred thousandth asset of a transfer, which would sit in front of every real upload on that tenant for
     as long as it ran. Split into `enqueue_derive_at`; transfers use the default band.
  3. **A store outage is not a bad record.** Every failure used to mark the record `failed`. The object store
     was unreachable during a run and all seven records were branded failed, permanently, for a connection
     refused — on a real migration, four hundred thousand records to reset by hand over a one-minute outage,
     and a report blaming the export. `Error::is_transient` already existed to make this distinction; now a
     transient error stops the run and leaves the record `pending`, and only `Permanent` is written against it.
  4. **`damd` and `dam-worker` could not be given a config file at all.** Both read the path from
     `DAMRS_CONFIG`, which sits under the prefix the environment provider scans, so figment offered a key
     named `config`, strict extraction rejected it as unknown, and setting the variable that names the config
     file was the one thing guaranteed to stop the process starting. Only `damctl` was unaffected, because it
     takes a flag. Fixed in `Config::load`. This is a go-live bug that nothing in the repo would have caught:
     every test constructs config in-process.

  5. **The sniffer panicked on binary content, on any upload path.** Not a migration bug at all — the SVG
     search truncated the lowercased head at byte 1024, and that head comes through `from_utf8_lossy`, whose
     replacement character is three bytes, so on binary input the slice often landed mid-character. Any
     upload of a file that is not text could take the worker down with a panic where a sniff verdict belonged
     — the exact failure `dam-pipeline` denies `expect_used` to prevent. It surfaced here only because every
     other fixture in the repo is an image or text; the migration suite was the first thing to push nine
     megabytes of arbitrary bytes through `sniff`.

  **Verified by migrating.** Five files off disk became five assets with the type sniffed from the bytes
  rather than taken from the record, dimensions probed, content hashes computed, and the crosswalked metadata
  landed. A record naming a missing file and one whose path was `../../../../etc/passwd` each failed alone and
  the run continued. Re-running skipped what had arrived. `--limit 2` stopped at the ceiling. Then a real
  worker drained the queue: 15 derivatives — thumbnail, preview and proxy for each of the five — and five
  index jobs, all succeeded. The suite in `crates/dam-pipeline/tests/transfer.rs` pins the chain, the
  idempotency skip, the traversal refusal and the per-record isolation.

  **Still open, deliberately.** Vendor connectors and a CSV reader; the JSON-lines input routes around both.
  The batch ceiling stops a run but nothing yet advances the job to `verify` or `complete` — an operator moves
  it, which is the right default while the QA gate between batches is a human.

- [x] **G10 SCIM, BYOK, audit export.** Three unrelated things behind one heading; the audit chain is first
  because it is the one nothing else depends on and the one an RFP treats as pass/fail.

  All six children landed — `G10·1` and `1b` the chain and the legal hold, `2a·0`, `2a` and `2b` user
  administration and SCIM 2.0, `3` the customer-managed key. The parent box stayed unticked after the last
  of them, while the summary table two hundred lines above already read "G10 is complete". Two statements
  about the same work disagreeing is how a ledger stops being worth reading, so this is the box catching up
  with the table rather than any new work.

- [x] **G10·1 The tamper-evident record, and the legal hold worth recording.**

  **Two absences that only make sense together.** `audit_log` has carried `prev_hash`, `hash`, its four
  indexes and two rules refusing UPDATE and DELETE since migration 0007, with the hash formula written in a
  comment beside the column — and nothing has ever written a row. `assets.legal_hold` has been *read* since
  migration 0001 — the rights gate refuses to deliver a held asset, the tiering scan refuses to move one, the
  purge view excludes one, the detail panel draws a badge for it — and nothing has ever written that either.
  Shipping one alone would have been half a feature: a chain with nothing worth chaining, or a hold nobody can
  prove was placed.

  **The formula in the schema says to concatenate, and concatenation is wrong.** `action || target_kind` makes
  `("a", "bc")` and `("ab", "c")` the same bytes, so one row's digest can cover a different row's content.
  Length prefixes, as in `signed_url`, which documents the identical break for the identical reason. An
  optional field needs more than an empty one: `None` and `Some("")` both render as zero bytes, so a marker
  byte inside the field separates them.

  **The payload is canonicalised rather than serialised**, and the reason is a Cargo feature. `serde_json`'s
  `preserve_order` switches `Map` from sorted to insertion order, features are additive across a workspace,
  and any crate three levels down that turns it on would break every hash written before that day — presenting
  as historical tamper evidence. Sorting the keys here costs an allocation and removes the dependency.

  **Timestamps are fixed-width or they are an intermittent false alarm.** `chrono`'s own `Serialize` uses
  `AutoSi`, which drops the fraction entirely when the microseconds are zero. Hashing that rendering would
  fail verification for exactly the entries that land on a trailing zero — and intermittent tamper evidence is
  worse than none, because it teaches the reader the alarm is noise. One `canonical_time` is used by both the
  digest and the exported view, which is the only way those two stay in step. Found by writing an independent
  verifier against a real extract and watching it disagree.

  **`seq` and `at` are supplied, not defaulted.** Both columns have defaults and this path uses neither,
  because the hash covers both and only Rust computes the hash — and the repair (insert, read back, update the
  hash) is exactly the UPDATE the table refuses. `clock_timestamp()` rather than `now()`, because a
  transaction that waited on the chain lock started *earlier* than the one that overtook it, so `now()` would
  write timestamps running backwards against the sequence. The consequence is that **a gap in `seq` is not
  evidence of tampering**: `nextval` is deliberately non-transactional, so every rolled-back governance action
  leaves a hole. Verification chains on `prev_hash` for that reason and never on contiguity — a verifier that
  counted numbers would report routine failures as deleted evidence.

  **A chain without a lock forks, silently, and is unrepairable when found.** Two transactions reading the
  same tail both insert claiming the same predecessor; nothing fails at the time, and the damage surfaces
  months later in front of an auditor with no way left to tell which branch was the real history. So `record`
  takes an advisory transaction lock keyed on the table's own OID — unique across the cluster, so no two
  tenants can collide, where a hashed schema name would collide sometimes and couple two customers in a way
  nobody would ever diagnose.

  **And `record` opens that transaction itself.** `pg_advisory_xact_lock` outside a transaction is taken and
  released by the statement that calls it: a lock in the shape of a no-op, undetectable by the code depending
  on it, and the same silent failure `tenant_conn` exists to prevent for `SET LOCAL`. `Connection::begin`
  gives a `BEGIN` when there is none and a `SAVEPOINT` when there is, and an advisory transaction lock taken
  inside a savepoint is held to the top-level commit either way.

  **What is hashed is what is stored.** `jsonb` normalises on the way in — `-0.0` reads back as `0.0` — so
  hashing the submitted value makes a row unverifiable from the instant it was written, which is tamper
  evidence for an entry nobody touched. The statement that draws the sequence casts the payload through
  `jsonb` and hands it back. I first wrote this up claiming `jsonb` rewrites `1e2` as `100`; it does not,
  because `serde_json` has already turned it into `100.0`. Probing the real database for a case that actually
  differs is what produced the negative zero.

  **The rules are the fence; the chain is the alarm.** UPDATE and DELETE are refused in the database, so the
  attack available to an application-level compromise is an *append* — a plausible extra entry, needing no DDL
  rights, and the one that must not work. The cases that alter and remove rows disable the rules first,
  deliberately: that is what an attacker with DDL does, and the chain is what remains. Reported as two
  distinct findings, because "this row was edited" points at a record and "a row is missing between these two"
  points at a gap.

  **A broken chain is a 200.** A 500 would be indistinguishable from the database being down, and "we cannot
  tell you" and "the record has been altered" are different sentences of which only one is an emergency.
  `damctl audit verify` exits non-zero instead, because that is the one report that has to fail a cron job.

  **The export is a POST, because it writes.** It appends the entry saying a copy was taken; behind GET that
  entry would be written by every link preview, browser prefetch and uptime probe — noise, and a false trail
  of people who never asked for the data. The extract carries the anchor its first entry links back to, so a
  window into the middle of a chain can be checked rather than trusted, and it cannot contain the record of
  its own creation.

  **`damctl audit` exists as well as the API route**, and the reason is the threat model: the chain detects an
  alteration made by whoever holds enough rights to make one, which includes whoever holds the application's
  credentials. A verification that only ever runs *through* the application is one an attacker in that
  position controls.

  **The reason for a hold is not a column.** There is no `legal_hold_reason` and this does not add one: the
  question is always who, when and why, and a column answers the least useful third while overwriting itself
  on every change. It is required in both directions, and releasing needs it more — "somebody lifted the
  litigation hold" with no sentence attached is the row that makes an auditor distrust the rest of the log.
  Re-asserting a hold records nothing and says so, because a log where most entries are re-assertions is a log
  nobody reads to the end.

  `Action::Manage` and no new permission string, following `ai.rs`: the built-in administrator role's
  permissions are wildcards that nothing expands, so a new string would be a gate no existing role could pass.
  The audit log is not asset-scoped and so is not filtered by the caller's predicate, which is the reason it
  takes the strongest gate the model has.

  **Verified by an independent implementation.** `verify_chain.py` rebuilds the digest from the exported JSON
  with no damrs code, working only from the published field order and framing, and agrees with both the server
  and `damctl` — including on the recomputed hash of a row tampered with in the live database, which all three
  then reported at the same sequence number. The dev tenant's chain was restored to its original payload and
  the UPDATE rule re-enabled afterwards.

  30 unit cases (15 pure over the canonical form, 15 over the chain), 12 API cases, 14 browser cases across
  two screens. Driving the real browser against the real server is what caught the accent-button contrast
  failure and three colour tokens I had invented.

  *Found here, fixed in G10·1b below:* the bulk action bar offers **Delete** for a selection under legal hold.

- [x] **G10·1b The confirmation that promised a number the operation would not deliver.** Started as the
  cosmetic item G10·1 left open — the bulk bar offering **Delete** over held assets — and the interesting part
  was one layer down.

  **The data was never at risk.** `apply_delete` is `AND NOT legal_hold` and reports the reason per row, so a
  held asset was always skipped rather than deleted. Checking that first was worth more than the button.

  **`bulk::preview` was the actual defect, and it contradicted its own documentation.** `dam_api::bulk`'s
  module note says "`POST /bulk/preview` filters the ids exactly as `POST /bulk` will, so the number in the
  confirmation dialog is the number that will be touched… the drift is a dialog that says 40 and an operation
  that does 38." Scope was the only filter either side applied. So a selection holding four frozen assets
  previewed as 42 and finished as 38 done, 4 skipped — precisely the drift, in the code that claims not to
  have it. The evidence was already in the browser suite, which asserts `partial: 1 applied, 1 failed` with
  "legal hold blocks deletion": the dialog's promise being broken one screen later, tested and passing.

  A preview now reports `blocked` and `blocked_reason` — what the *operation* will refuse among targets that
  already passed scope. Kept separate from `out_of_scope` because they are different facts a caller can act on
  differently: an out-of-scope id belongs to somebody else, a blocked one is theirs and frozen. `target_count`
  stays the attempt count so it still matches the job's own and `done + failed = target` holds; the dialog does
  the subtraction and says both numbers. Per kind rather than as a shared predicate, because the refusal is the
  delete executor's and putting it in front of operations whose executors do not make it would be inventing a
  rule.

  **An operation whose every target is refused is now a 422**, by the same argument the empty-selection case
  already makes: recorded instead, it sits in the history as `partial` with nothing done, which reads as
  something that half worked. A *mixed* selection still runs, because deleting the deletable half is what was
  asked for.

  **And `legal_hold` moved from `Detail` to `Summary`**, which is what let the grid draw a `Held` badge — the
  fact belongs where the selection is assembled, not in its result. It was on `Detail` *and* would have been on
  `Summary`: two sources for one truth, so the `Detail` field went.

  1 db case, 2 API cases, 2 browser cases.

- [x] **G10·2a·0 Disabling a person did nothing.** Found while scoping the removal half of user
  administration. `auth::authenticate` joined `tenants` and checked its status — a fix with its own write-up —
  and never joined `identities` at all. So `identities.status`, there since 0001, and
  `identities.deprovisioned_at`, there since 0002, had no effect on anything: a disabled person's keys kept
  working, in every tenant they belonged to.

  Removing a `tenant_members` row did lock them out of the asset surface, because `authorize` then finds no
  roles and a predicate matching nothing is a refusal — but that is authorisation covering for authentication,
  and it does not cover an endpoint that authenticates without authorising.

  A `LEFT JOIN`, because `api_keys.identity_id` is nullable and an inner join would have refused every machine
  key in the fleet. Allowlisted on `status = 'active'` rather than denylisting `disabled`, so a status added
  later refuses by default instead of authenticating by omission. Reversible, like a tenant suspension:
  re-enabling somebody does not require reissuing their key. `deprovisioned_at` refuses on its own, even while
  the status still reads active, because the two columns mean different things.

  2 cases. This is what makes the deprovisioning in G10·2a a removal rather than a flag.

- [x] **G10·2a User administration.** Found while scoping SCIM, and it changed the order of the work:
  **there was no way to add a person to a tenant.** `tenant_members` is read by `caller`, `auth`, `browse` and
  `comments`, and was written in exactly one place — connector registration, inserting a service account. No
  endpoint invited a colleague, granted a role, or removed somebody who had left.

  So SCIM could not come first. Built first it would be the *only* way to provision a person, leaving a
  customer without an IdP unable to add a second user — and it drives these same operations, so building it
  first means building them twice. `Action::RoleGranted`, `RoleRevoked`, `IdentityProvisioned` and
  `IdentityDeprovisioned` were already in the audit vocabulary for this.

  **The membership and its audit entry are one transaction, and the connector path's reason they cannot be is
  wrong.** That code says "the identity, membership and key live in the control plane, so they cannot be in the
  tenant transaction" — true of two *databases*. `dam_global` and `t_acme` are two schemas in one, so the
  transaction `TenantConn` opens reaches both. Which matters because the alternative has no good ordering:
  audit first and a failed write leaves a permanent record, in an append-only log, of a grant that never
  happened; effect first and a failed write leaves a grant with nothing saying who made it. Neither is
  correctable. One transaction removes the choice, and a case asserts the rollback leaves neither.

  **An identity is global, which is a disclosure problem.** `identities` has no tenant column and is unique on
  the lowercased address, so adding somebody has to find-or-create — and the finding must not be visible. A
  409 saying "that person already exists" would answer "does this company use damrs" about an address somebody
  merely typed. The conflict is about *this tenant's* membership; whether the identity pre-existed goes in the
  audit payload, which only this tenant's administrators read.

  **An unknown role name is named, because the alternative is silent.** `role_names` has no foreign key and
  `auth` ignores a name it cannot resolve — right there, a trap here: `editors` for a role called `editor`
  produces somebody who can see nothing with nothing saying why.

  **The last administrator cannot be removed *or* demoted.** Demotion reaches the same state, so guarding only
  removal would be a rule with a documented workaround.

  I first wrote that check under a `FOR UPDATE` on the membership being changed and a comment claiming it
  stopped two administrators stepping down at once. It does not: the rule is a check on a *set*, and locking
  one row leaves two demotions each counting two and each seeing somebody remaining — a tenant with no
  administrator, recoverable only by an operator with database access. Caught reviewing my own code before the
  suite ran. Locking every administrator's row instead trades the race for a deadlock, because a demotion and
  a removal each hold one administrator's row while waiting for the other's — so membership changes serialise
  per tenant on an advisory lock taken before anything is read, in the two-argument keyspace so it can never
  collide with the audit chain's one-argument lock. Asserted by holding that lock and checking a change
  blocks, rather than by racing two tasks and hoping the window opens.

  **Removal is the half that matters**, for the reason `0002_enterprise.sql` gives about SCIM: an account
  marked gone that keeps its keys is a flag. So it revokes the keys first — everything after that can fail and
  leave an account that cannot get in, where the other order leaves one that can — and returns the count,
  which is the difference between removed and marked-removed. The identity itself is only disabled if this was
  their last tenant, because `deprovisioned_at` is global and somebody working with two customers of one
  deployment must not lose their other account. Re-adding them re-enables the identity, or the new key would
  not work.

  **Two findings that only came from reading real data.** `GET /roles` offered the `connector:<uuid>` roles
  every registered site creates, sorted into the middle of the list where they look like they belong — an
  invitation to grant a person a role that exists to describe a machine. And the People list contained the
  connected sites themselves, with "Change roles" and "Remove" buttons: changing them would hand a website an
  editor role or take away the one that makes it work, and removing one would revoke its key while the Sites
  screen went on listing it as connected. Both excluded — the second by joining `connectors` on `api_key_id`
  rather than by matching the `@connectors.invalid` address, because the join is the fact and the address is a
  convention that could change silently.

  **And a copy bug in the same class:** a tenant administrator gets every asset group without holding a role,
  so "No roles — can sign in and see nothing" was printed beside four administrators who could see everything.

  17 db cases, 11 API cases, 11 browser cases. `MemberAddBody` and the rest carry that prefix because
  `AddBody`, `AddedView` and `RemovedView` already existed elsewhere in the document, and utoipa accepts a
  duplicate schema name silently with the last one winning — which showed up as the collections screen's own
  type changing shape.

- [x] **G10·2b SCIM 2.0.** `scim_clients` had existed since migration 0002 with nothing reading it, alongside
  `identities.scim_external_id`, `scim_managed` and `deprovisioned_at` in the same state.

  **Provisioning creates an account nobody can sign into, and that is the honest answer.** There is no login
  flow here — a person authenticates with an API key, which is why `members::add` mints one. SCIM must not: the
  identity provider is the thing that signs its people in, and putting a key in a SCIM response would hand the
  provider a long-lived bearer token for a person, into its own logs, for an account it does not authenticate
  with, with nobody to give it to. So provisioning creates the identity, the membership and the roles, and no
  credential. Until SSO exists a provisioned person has access and no way to exercise it, and an administrator
  issuing them a key from the People screen is the interim answer. The *deprovisioning* half — the half 0002
  says a security questionnaire asks about — is complete and unaffected. `members::add` was split into
  `attach` plus the key issuance so both paths share the one that matters.

  **The schema had the SCIM state on the wrong table, and it took two migrations to see it.** 0002 put
  `scim_external_id` and `scim_managed` on `identities` — one row per person across the deployment — and indexed
  the id uniquely across the whole table. `0005_scim_client_scope.sql` fixed the obvious half: customers'
  providers number users independently and Okta's default `externalId` is an opaque per-org id, so the second
  tenant to provision a colliding id got a constraint violation in a sync they do not control.

  Reviewing my own code before running the suite found what was underneath. The columns were still
  single-valued on a shared row, so **two tenants' providers provisioning the same person overwrote each
  other's link** — the second silently took ownership and the first tenant's sync then failed its own ownership
  check, its provisioning broken because a different customer provisioned the same consultant. And
  **`scim_managed` made somebody uneditable everywhere**: provisioned by one customer's provider, they could no
  longer have their roles changed by an administrator in another tenant, where no provider manages them at all.

  `0006_scim_link_is_per_tenant.sql` moves the three columns to `tenant_members`, keyed exactly right.
  `status` and `deprovisioned_at` stay on `identities`, which is not an inconsistency: whether an account works
  is global, and who provisions it is not. Two migrations rather than an edited one, because 0005 was already
  applied to a database and rewriting an applied migration is a checksum failure and a reset somebody has to
  do by hand.

  **Two more refusals rather than silent drops.** A role the tenant does not define is a named 400 listing what
  it *does* define — the same trap the human path documents, since `auth` ignores an unresolvable role name and
  the provisioned person would simply see nothing. And a `userName` change is refused with what to do instead:
  it is the email, `identities` is unique on it globally, and a provider told its rename applied never sends it
  again.

  **Both deprovisioning paths, because there are two.** Okta sends `DELETE`, Entra sends
  `PATCH active: false`. Implementing one leaves the other silently unable to offboard anybody. Both revoke
  credentials; `DELETE` also drops the membership, which is the difference, and the audit payload says which
  happened.

  **Entra sends the string `"False"`.** Not the boolean. A strict parse rejects it, the sync fails, and the
  symptom is an employee who has left and still has access — the exact failure SCIM is bought to prevent. Read
  from either shape, case-insensitively, and `op` matched the same way because the specification says
  lowercase and Entra capitalises.

  **The envelope is the integration.** `status` is a *string* in an error; `Resources` is capitalised in a
  `ListResponse`; `startIndex` is 1-based, and treating it as an offset drops the first user of every sync;
  `application/scim+json` on the way out. A provider that cannot parse a response fails silently from our side.

  **An unsupported filter or op is refused by name rather than ignored.** A filter we drop is a provider
  receiving the whole directory and concluding every user already matches; a PATCH we accept and drop is a
  provider that will never send it again. Only `userName eq` and `externalId eq` are supported, which is what
  providers actually send — the specification's filter grammar is large and implementing it would be a parser
  nothing exercises.

  **A provider may only touch what it provisioned**, or one tenant's misconfigured provider disables somebody
  an administrator added and the audit trail shows the provider's token doing it. Attributed to `system` with
  the provider named, never to a person: an audit row naming somebody who was asleep is worse than one naming a
  machine.

  **`last_sync_at` and `last_sync_status`** — two more columns 0002 declared and nothing filled — are written
  on reads as well as writes, because the most common provider request is a `GET` and a healthy integration
  that only ever lists would otherwise look dead.

  **A provisioning token cannot mint another.** Registration takes the ordinary `Manage` gate; a credential
  that could issue its own successor is one that cannot be revoked. And SCIM is a *second* authenticator in a
  codebase that deliberately has one — justified because a client holds no ABAC predicate and has no
  membership, so there is nothing for `caller::authorize` to compile, and giving it one would make the
  provisioning system a user of the library it provisions.

  8 db cases, 16 API sub-cases, 6 browser cases. Driven live against the dev tenant as a provider would:
  ServiceProviderConfig, provision, `userName eq` filter, the Entra `"False"` PATCH, reactivate, `DELETE`, and
  the whole lifecycle in the hash chain — 17 entries, verified intact afterwards.

  *Not built:* Groups. `scim_clients.scopes` advertises it and a `Users`-only token is refused for it by name,
  so a provider learns rather than guesses. Mapping an IdP group onto a tenant role is a real feature and its
  own slice; claiming support and silently ignoring group pushes would be the version of this that wastes
  somebody's afternoon.

- [x] **G10·3 BYOK.** `DAMRS_STORAGE__SSE_KMS_KEY_ID` encrypts every object under a customer-managed KMS key.
  One key for the deployment, which is what this claimed and delivered; per-tenant keys are **G10·3b**.
  §19's "building anything here would be worse" was about an encryption layer of our own, not the wiring.

  **The risk is not that it does not work; it is that one write path forgets.** Seven calls create an object —
  `put`, the small promote copy, the large promote's multipart create, the self-copy that performs a
  storage-class transition, the resumable multipart create, the one real uploads go through in `multipart.rs`,
  and the presigned PUT. A write that misses the key does not fail. It lands under the bucket's default,
  indistinguishable from success until somebody audits the bucket.

  So the applicator is one trait over the three builder types, applied at all seven — and the load-bearing test
  reads the source and asserts that *every* call which creates an object carries it, with the count asserted
  so a refactor that removes a path, or breaks the test's own parsing, fails rather than passes vacuously.
  Deliberately a test about the code, like `the_embedded_migration_counts_match_the_files_on_disk`: the failure
  it prevents is a future path added without the line, and no behavioural test of today's paths can catch that.
  Verified by deleting one application and watching it name the file and line.

  Its first version failed on correct code: the scan stopped at the first line ending in `;`, and one of the
  chains carries a comment reading "…carries metadata across;". Comments are skipped now.

  **The transition copy needed it re-stated.** `MetadataDirective::Copy` carries metadata across a
  storage-class transition and does not carry the encryption choice, so a tiering pass without it would
  rewrite objects under the bucket default — silently converting an encrypted library to an unencrypted one,
  one lifecycle run at a time.

  **Setting it is not enforcing it, and the deployment guide says so as a requirement.** A presigned PUT is
  executed by the browser, which can decline to send the headers that were signed; a client that omits them
  receives a 200. Only a bucket policy denying `s3:PutObject` without the expected key id closes that, so
  `docker/DEPLOY.md` carries the policy and states it as required rather than advisable.

  **A finding from running it.** Pointed at the dev SeaweedFS, the unencrypted write succeeded and the
  encrypted one failed with `InternalError` and no mention of encryption — which is what a deployment gets for
  setting BYOK against a gateway with no KMS. Not refused, because Ceph RGW with Vault and MinIO with KES do
  implement it; surfaced instead as a startup advisory. `Config::advisories` returns them as data rather than
  logging from `dam-core`, which has no `tracing` dependency and should not gain one for a warning — and an
  advisory that is a `String` can be asserted.

  **Per-tenant keys are not this, and are blocked on something else.** `storage_pools` carries `tenant_id` and
  everything a pool needs, but `damd` builds one store from `storage.*` and never from a pool row, so there is
  nowhere to hang a per-tenant key until per-pool store resolution exists. A deployment CMK is worth having on
  its own; the per-tenant version should be scoped with pool resolution rather than faked alongside it.

  3 store cases, 2 config cases. Real KMS is not testable locally — neither SeaweedFS nor MinIO implements it
  — so end-to-end confirmation belongs to `tests/aws_conformance.rs` and the nightly AWS workflow.

### M3d — the Drupal connector

Five slices. The damrs side first, because the Drupal module is a client of it and a module written against an
API that does not exist yet is a module written twice.

- [x] **M3d·1 Connector registry.** `connectors` has existed since migration 0004 with nothing writing to it,
  and `rights_usage.connector_id` has pointed at it the whole time.

  **Registration composes the ordinary machinery rather than adding a second one.** A connector needs to
  authenticate and to be scoped to asset groups, and both already exist — so registering a site creates an
  identity, a membership, a role carrying the groups, and an API key, and nothing new. That is what makes
  §11.1's claim true: "a misconfigured Drupal view cannot surface an unapproved asset, because the ABAC
  predicate already excluded it" only holds if the connector goes *through* the predicate. The test that
  matters drives the returned key against the real asset listing rather than asserting what the role row says.

  **The service account is deliberately not a person.** It needs an email because `identities.email` is unique
  and not null, so it gets one at `.invalid` — the reserved TLD (RFC 2606) that can never resolve and can never
  receive a password reset. A synthetic address in a real domain eventually belongs to somebody.

  **Two secrets, two lifetimes, both shown once.** The API key is how the remote calls damrs; the signing secret
  is how it signs render URLs itself so a page render never blocks on an API call (§11.3) — which makes it a
  forgery capability for whatever that site may render. So it is sealed with the deployment's keyring exactly as
  a model credential is, and the sealed form carries its own key id, so no column was added. Reading a connector
  back returns neither, not even the ciphertext.

  **Rotation asks which situation it is.** A scheduled rotation keeps the old secret verifying for a week,
  because the DAM-side rotation and the site-side config change are separate deploys and a rotation with no
  window is an outage. A leak does not, because that week would be a week of forgery. The endpoint takes the
  answer rather than picking one, and the window is enforced by comparing `secret_rotated_at` rather than by a
  job that clears the column — a cleanup job that fails leaves a superseded secret valid forever and nothing
  says so. Revoking clears both secrets and is terminal.

  A connector scoped to no groups is a **403**, not an empty library. That is `caller::authorize`'s existing
  rule — a predicate matching nothing is a refusal — and it is the better answer here: an empty picker reads as
  "the DAM has no assets" and sends a site operator looking in the wrong place. I wrote the test expecting a
  200 with zero rows, which was the second time this session I asserted against a deliberate codebase rule
  rather than reading it first.

  9 db cases, 10 API cases. No screen yet: a registry whose URLs are not honoured is a screen for a feature
  that does not work end to end, so the screen lands with M3d·2.

- [x] **M3d·2 Connector-signed delivery.** The property the whole integration rests on: the remote signs render
  URLs itself, so a damrs outage degrades to stale-but-working pages rather than white screens.

  **One change to `signed_url`, and it is the interesting one.** `Keyring::find` returned the *first* secret
  under a key id, because the ordinary case gives each key its own id and rotation means a new id. A connector
  cannot work that way: it signs its own URLs and *it* decides when to switch, so during the grace window the
  same id is in use with two different secrets and damrs cannot tell which from the token. So `find` returns
  every secret under an id and `verify` tries all of them — written as a fold rather than a short-circuiting
  `any`, so the number of HMACs computed does not leak how far through a rotation a site is.

  Plus `key_id_of`, which reads the claimed key id without verifying. Selecting a key from an unverified id is
  unavoidable — verification needs a key before it can decide anything — and safe, because naming the wrong key
  produces a signature that does not match. What it must not do is let the *choice* of key confer anything.

  **Which is the whole security argument, and it is four bounds.** A connector that holds the secret can sign
  anything, so `bound_by_connector` refuses: a `Purpose::InternalPreview` claim (it skips the rights check —
  this is the bound that matters most, because without it a site signs one and every licence check on every
  page is gone); a share-link claim (a share's authority belongs to the share); `original` unless
  `allow_original`; and any asset outside the connector's groups, resolved through `grants_for` and the ordinary
  predicate rather than by reading the groups off the connector row, which would be a second place a
  connector's scope is decided. Plus one substitution rather than a refusal: a cold original with
  `allow_restore` off becomes the master proxy, because §11.1 says a page render must never wake Glacier and
  refusing would blank an image for a reason the site cannot act on.

  Pausing, revoking, or revoking the connector's *API key* all stop URLs already signed — the same property as
  a revoked share, and the API-key one matters because otherwise revoking a credential would leave render URLs
  working for as long as the site kept signing them. An `error` state still renders: a failed webhook is not a
  reason to blank somebody's home page.

  With no connector auth configured, a connector token is refused rather than falling back to the server
  keyring. A fallback would verify against a key the site never had — which fails, until somebody "fixes" it by
  trying both.

  **Verified against the running server with an independent implementation of the signing format** — a Python
  script reimplementing the length-prefixed canonical form and the HMAC, with no damrs code in the signing
  path, which is exactly what a PHP module will do. A site-signed proxy URL is served (302); `original`,
  `purpose=InternalPreview` and a wrong secret are all refused (404); pausing stops it and resuming restores
  it; a rotation with grace leaves both secrets working; a leak rotation kills the superseded one at once.

  That exercise also surfaced why the dev library's own proxies do not deliver: they were rendered under an
  older `web-2048` definition, so their stored `op_hash` no longer matches the profile. Delivery refuses them,
  which is 3.2's design working — a name lookup would serve yesterday's quality setting forever.

  6 delivery cases, 6 new `signed_url` cases, 14 browser cases, and the screen covering M3d·1 and M3d·2
  together.

- [x] **M3d·3 Browse and oEmbed.**

  **The decision this slice turned on: how a browser-side picker authenticates.** §11.1 asks for a
  CORS-enabled browse endpoint "for the embedded asset picker", which implies a browser holding a credential —
  and it cannot be the connector's API key. That key is long-lived, grants every read the site has, and putting
  it in JavaScript hands it to every editor, every browser extension and every page the picker is embedded in.

  The answer needed no new mechanism: **the site signs a short-lived token itself**, in PHP, with the same
  secret it signs render URLs with. No endpoint to mint one, no round trip in the path of opening a dialog, and
  rotation and the grace window work on it for free because it is verified with the same keyring. Ten minutes
  maximum, enforced at *verification* — the site chooses the expiry, so a ceiling only whoever verifies can
  hold is the only kind that means anything. It carries no scope of its own, deliberately: a token that could
  widen what the picker sees would let a site mint itself reach it was never granted.

  **`GET /browse`** answers with results and the facet rail in one call — two would let the rail's counts
  disagree with the grid beside them. Both credentials resolve through `caller::authorize_as`, which is new:
  `authorize` was split so the grant loading, predicate compilation and both of its guards are one code path
  for a bearer key and a signed token alike. A connector-shaped scope resolver would have been a second place
  access is decided.

  CORS is the connector's own `site_url`, never a wildcard, and **only on the token path** — a cross-origin
  request carrying `Authorization` would be a site putting its key in a browser, and answering it would endorse
  that. A mismatched origin gets `null` rather than a refusal: the browser blocks the read, which is what CORS
  is for, and a 403 would tell a page it guessed wrong.

  **One change with a reason beyond this slice.** An empty query now goes to SQL rather than the index. There is
  nothing to rank, so the index would answer with whatever it happens to contain — a document not yet written,
  a reindex in progress — and a listing that claims to be the library would quietly be missing rows. The picker
  opens on exactly this, and so does anything else that lists before filtering.

  **`GET /oembed`** is authenticated, which the spec does not contemplate — an unauthenticated endpoint that
  turns an asset id into a filename, a size and a preview URL is an enumeration API for the whole library. The
  deviation costs nothing: CKEditor's fetch happens in Drupal's server-side code, which holds the key.

  Its statuses are the spec's, not this codebase's usual mapping, because a consumer implements against them:
  404 for a URL the provider does not recognise *and* for an asset the caller cannot see (one answer, or the 404
  confirms existence), 400 for a URL belonging to another provider, 501 for a format it will not emit. Only an
  image is a `photo`; a video would need an embeddable player this does not have, so everything else is a
  `link`. And an asset whose rendition has not been rendered yet is a `link` too, not a 500 — a fresh upload is
  an ordinary state, and a consumer that pasted a real URL is better served by a card than by a server error it
  can do nothing about. `cache_age` sits below the signed URL's own lifetime, or a caching consumer serves a
  broken image for most of a day.

  9 token cases, 9 browse cases, 8 oEmbed cases.

  **And a bug every one of those tests missed, that one `curl` found.** `tower_http`'s `CorsLayer`
  *overwrites* `Access-Control-Allow-Origin`, so while `/browse` and `/oembed` were mounted under the global
  layer, the per-connector header they set was replaced — by `*` in development, and by whatever
  `server.allowed_origins` lists in production. The endpoint tests drove the routers in isolation, where no
  global layer exists, so they passed while the deployed answer was a wildcard.

  Both are now mounted *outside* that layer, and the regression test asserts it structurally: `/assets` still
  gets the deployment-wide policy and `/browse` gets no header from it at all. Their origin policy is per
  *credential* rather than per deployment — a browse token lives in a browser, so the only origin that should
  read with it is that connector's own — and a deployment-wide list cannot express that.

  Verified live with the independent Python signer: the connector's own origin comes back echoed, another origin
  comes back `null`, and the rest of the API still answers `*`. oEmbed too — a photo with a real signed URL and
  correct dimensions, `maxwidth` picking 256/1024/2048, and each refusal at its spec status (401 no credential,
  400 another provider's URL, 501 for XML, 404 for an unknown asset).

- [x] **M3d·4 The usage index.** `connector_asset_refs` has existed since migration 0004 with nothing writing
  to it. Three things depend on it, and each one turned on a different decision.

  **The pin has to expire, and that is the whole design.** 0004 already says of `usage_sample` that it is
  "populated by the connector, so it is advisory rather than authoritative" — fine for a report, dangerous for
  a signal that keeps objects out of cold storage. A site that goes quiet (decommissioned, broken module, a
  token nobody renewed) is *indistinguishable* from a site that stopped using the asset. Pin forever and one
  abandoned integration holds a library in Standard indefinitely; never pin and a live page causes a restore
  storm the first time somebody thaws the original. So a reference pins only while it is fresh — refreshed
  inside thirty days, in use, on an active connector — and the test drives that lapse through the *planner's
  own query* rather than a helper.

  **Which is where the pin lives: in `tiering::candidates`, beside the three pin sources already there.** Not
  applied by the caller. A fourth place deciding whether something is pinned would let a dry-run plan read from
  SQL disagree with what actually moves, and the reason string is ordered second — above a pinned collection,
  below a legal hold — on that query's own negotiability argument: an operator can go and unpin a collection
  and cannot unpublish somebody else's website.

  **A full sync is one request.** Reporting what is used only grows the index; something has to say what went
  away, or a deleted node pins its asset hot forever and every takedown report over-counts. Split into
  report-then-sweep, a site that crashed between them would leave what it had just re-reported looking
  abandoned. So `full_sync` orphans the absent rows in the same transaction — orphans rather than deletes,
  because an operator asking why something stopped being pinned needs to see that it was once used.

  **Only a site's own credential may report its own usage.** Not `Manage`. An administrator does not know which
  pages render which media, and this write feeds the pin — so a caller who can forge it can hold a library in
  Standard. Narrowing to the one credential with first-hand knowledge is both the honest rule and the tighter
  one. A paused or revoked site cannot report at all.

  **Two kinds of stale mean different things**, and both are derived rather than stored so they cannot disagree
  with the timestamps under them. Version drift is a job to run; a missed refresh is a site to go and look at.
  The `state` column's CHECK still permits `'stale'` and nothing ever writes it: the column records what
  somebody *asserted*, and staleness is computed.

  **The impact report counts the live and lists the dead.** The counts are what pulling the asset would break;
  the list is everything, including a reference a site stopped reporting — because showing only counts hides
  "one site went quiet three weeks ago", which is exactly what makes a number untrustworthy. And `pages` is
  labelled as the site's own number rather than folded into a total: damrs cannot see somebody else's website.
  The panel lives on the asset, because that is where a takedown decision is made.

  7 db cases, 9 API cases, 7 browser cases.

  Two fixture traps worth recording. `lifecycle_policies` refuses a transition without both a target class and
  a target pool — the schema refusing a policy that could never execute, which is right and which my first
  fixture ignored. And `provenance_state: 'unknown'` is not a value: that vocabulary is
  none/valid/invalid/untrusted, and `unknown` belongs to *rights*. An invented one makes the detail panel throw
  from a metadata lookup, which presents as the panel simply never opening — it cost a debugging round here and
  was already lurking, harmlessly, in the proofing suite's fixture.

- [x] **M3d·5 The Drupal module.** `integrations/drupal/`, Drupal 11+ only, six submodules (§11.2).
  **All six submodules done and verified.**

  **The deferral's premise was wrong, and checking cost nothing.** It read "nothing in this repository can
  run PHP or a Drupal install". True of the repository, false of the machine: DDEV was already installed, so
  a throwaway Drupal 11.4.5 took four commands. The whole reason for the deferral evaporated on inspection,
  which is worth remembering the next time something is parked on an assumed impossibility — the deferral
  had been restated across several sessions without anyone testing it.

  **Done and verified: `damrs`.** API client, settings form, service-account auth, health check, and the
  delivery-URL signer. Enabled on a real Drupal 11.4.5; the settings form builds all nine fields through the
  real container and both services resolve.

  **The signer is the part that mattered, and it is now pinned across two languages.** §11.3 requires
  transform URLs signed *in PHP* with no API call in the render path, so a second implementation of the
  delivery-token canonical form exists and the two have to agree byte for byte forever.
  `cargo run -p dam-core --example signing_vectors` emits the vectors; the PHPUnit suite compares against
  them offline. Mutation-tested: character length instead of byte length, omitting an absent optional
  instead of writing a zero-length field, and signing a UUID's text instead of its raw bytes each fail the
  suite. A second example, `verify_token`, closes the other direction — a URL minted by the live Drupal
  service from real config and the real clock verifies in Rust with every field correct, and both a wrong
  secret and a one-character tamper are refused.

  **What running it caught immediately.** Drupal's routing file must be `MODULE.routing.yml`; it was written
  as `MODULE.routes.yml`, so the module enabled cleanly, reported no error, and had no settings page at all.
  A module shipped against the spec rather than against an install would have shipped that.

  **Done and verified: `damrs_media`.** The `damrs_asset` MediaSource plugin, holding an asset id in a plain
  string field and nothing else. Enabled on the live site, a media type created on it, and metadata mapped
  into real fields.

  **The hazard was not in the plugin, it was in how Drupal calls it.** `Media::preSave()` assigns whatever
  `getMetadata()` returns straight into the mapped field — so a source that returned NULL because damrs was
  unreachable would blank the cached title, alt text and dimensions of every item re-saved during an outage.
  Stale metadata is the correct degraded state; empty metadata is silent data loss. The plugin therefore
  falls back to the value already in the mapped field, and the kernel suite fails without that fallback.

  **A test that was wrong before it was right, which is worth recording.** The first version of that check
  created a new entity with values already set and concluded the fallback worked. It did not: Drupal only
  re-reads metadata when a mapped field is *empty* or the source field *changed*, so nothing had called
  `getMetadata` at all and the values survived because nothing touched them. Removing the fallback changed
  nothing, which is how the bad test was caught. The real case is an existing entity whose asset id changes
  during an outage.

  **And a bug found by reading damrs rather than Drupal.** The media source was written against an invented
  transform string — `w=320,h=320,fit=inside,fmt=webp` — on the assumption that a transform describes an
  image. It does not: `delivery::op_hash_for` resolves a transform against the built-in profiles and then the
  tenant's conversions, and anything else is `NotDeliverable`. Every thumbnail this module fetched would have
  been refused, and nothing in Drupal would have said why. The valid names are `original`, `thumb-256`,
  `preview-1024` and `web-2048`, plus a tenant's conversion keys. They are now a `Transforms` class pinned to
  a fixture generated by `cargo run -p dam-media --example transform_names`, so a rename upstream fails the
  connector's tests rather than its users' pages — the same arrangement as the signing vectors, and for the
  same reason.

  **Two defects only a live Drupal produced.** The module shipped no config schema for the source's
  `source_configuration`, so `media.type.*` failed Drupal's schema check and a site with strict checking
  could not create the media type at all — the kernel run surfaced it. And the module failed
  `phpcs --standard=Drupal,DrupalPractice` on 129 counts, mostly the 80-column limit, which §11.2's
  "contrib-shaped composer package" does not permit. Both fixed; CI now runs the standards and the kernel
  tests, so neither can come back.

  **Done and verified: `damrs_image_style`.** The piece that actually puts a picture on a page. A field
  formatter renders the source field as an `<img>` whose URL is signed locally, so painting a page still
  makes no request to damrs. Verified by rendering a real media entity and feeding the resulting token back
  through `verify_token`: it decodes with the mapped transform and every field correct.

  **It is a mapping, not a translation, and that follows from damrs rather than from Drupal.** The obvious
  design reads an image style's effects and emits an equivalent transform. It cannot work, because a
  transform is a name and an unrecognised one is refused. So a site says which of the transforms damrs
  *does* render each image style corresponds to — a decision about intent, not arithmetic — and a site
  wanting a size damrs does not offer adds a conversion there and maps to its key. One place decides what
  renditions exist, which is what keeps the derivative cache and the rights model coherent.

  **The cache lifetime is the URL lifetime.** A render array cached longer than the signed URL's TTL becomes,
  after that TTL, a cached page whose every image damrs refuses — with nothing in the logs connecting the
  two. The formatter caps its own `max-age` at the configured TTL rather than leaving an operator to keep two
  unrelated numbers in the right order by hand. Mutation-verified: making the render permanent fails the
  suite.

  **Done and verified: `damrs_sync`.** A signed endpoint, a queue, and a worker that applies events to the
  media items referencing an asset. Driven end to end with deliveries signed by the real Rust signer: a valid
  one is accepted 202, and no signature, a wrong secret, a tampered body and a stale timestamp are each
  refused 401.

  **The interaction that made two correct modules destroy data together.** `damrs_media` falls back to the
  value already in a mapped field when damrs cannot answer, so an outage cannot blank cached metadata.
  Refreshing works by *clearing* those fields so Drupal's own "field is empty, read it" branch runs — which
  removes the very value the fallback would have returned. A refresh event arriving during an outage
  therefore erased the metadata it was meant to update. Both modules' suites were green, and the live check
  showed a title going to NULL.

  The fix required a distinction the client could not express: `asset()` returns NULL both for "damrs did not
  answer" and for "damrs says there is no such asset", and those are opposites for anything deciding whether
  to retry. Treating every failure as retryable makes a deleted asset an item that never drains; treating
  every failure as final makes a one-minute outage erase what it could not refresh. So `ApiResult` carries
  the status, nothing is cleared until damrs has produced the asset, an unreachable damrs suspends the queue
  run, and a deletion never asks at all.

  **The webhook verifier is pinned to the forgeries, not the happy path.** This endpoint is reachable without
  a session, so a verifier wrong in the accepting direction is an endpoint anybody can post content changes
  to — a different severity from the delivery tokens, where a mistake only stops images rendering. The
  vectors therefore carry the forgeries a plausible implementation accepts, including a correct digest over
  the body without the timestamp, which passes every happy-path test and makes every signature a permanent
  replay token. Mutation-tested four ways; three were caught behaviourally and the fourth — `===` instead of
  `hash_equals` — provably cannot be, since only the timing differs. That one is guarded by a structural
  assertion that says outright what it is.

  **Done and verified: `damrs_editor` — and half of it turned out to be core's already.** Inserting a damrs
  asset as a *media entity* needs nothing from this connector: `damrs_media` makes an ordinary media type, so
  CKEditor 5's Media Library button and the `media_embed` filter handle it. Checked rather than assumed — a
  `<drupal-media>` tag pointing at a damrs item renders our signed URL today. Writing a CKEditor plugin for
  that would have been re-implementing something that already worked.

  What core cannot do is resolve a *pasted URL*, because its OEmbed source uses the public provider registry
  and sends no credential, while damrs's oEmbed is authenticated on purpose. So the filter makes that call
  server-side with the connector's key — the arrangement damrs's own oEmbed module documents as its
  expectation. A `photo` becomes an image; anything else becomes a thumbnail link rather than a player this
  has no code for.

  **The cache-lifetime trap, in its third form.** damrs reports a `cache_age` deliberately shorter than the
  signed URL inside the response. A filtered body is cached separately from the formatter's render array, so
  the same mistake was available again here; with several embeds in one body the shortest age wins, because
  one expired URL is enough to break the page.

  **A test harness bug worth recording, because it made a green suite meaningless.** The HTTP client was
  being replaced in `setUp()`, which is too late whenever anything has already caused `damrs.client` to be
  constructed — the client keeps the real Guzzle it was handed, the queued mock is never consumed, and the
  call fails a genuine DNS lookup and returns NULL as though damrs had refused. Four tests failed for that
  reason and none of them was about what it appeared to be. Replacing the service in `register()`, at
  container-build time, is the fix, and all three kernel suites now do it.

  **Done and verified: `damrs_search_api`.** A Search API backend that proxies to damrs rather than indexing
  into Drupal. That is the design and not a shortcut: Search API's usual shape is index-then-query, and a
  local copy would make every rights decision be taken against it — an asset whose licence lapsed would keep
  appearing in results until the next reindex, which is exactly what the connector exists to prevent. So
  `indexItems()` stores nothing and *says so*, because returning the ids would tell Search API's tracker
  these items are searchable here and the lie surfaces later as results that never appear.

  `/browse` rather than `/search`, because it answers with the results and the facet rail from one call,
  counted over the same query — one round trip, and a facet cannot claim forty while the grid beside it shows
  three.

  Three refusals are pinned by tests: paging past damrs's depth cap raises rather than returning an empty
  page that reads as "no more results"; an unreachable damrs empties the results with a warning rather than
  throwing, since this backend can sit behind a block on every page; and clearing the Drupal index must not
  contact damrs at all, because a site clearing its search index has not asked to empty somebody's asset
  library.

- [ ] **G10·3b Per-tenant keys, and a table that has never been read.** *Found 2026-08-28 while checking
  whether the AWS-native list's SSE-KMS item was stale. It was half stale, and the other half is larger than
  the list suggested.*

  `dam_global.encryption_keys` has existed since migration `0002_enterprise.sql` — `tenant_id`, `purpose`
  with a CHECK of `blob`, `c2pa_signing`, `field`, `backup`, a `provider` CHECK across five KMS vendors,
  `key_ref` commented "ARN or URI; never key material", `customer_managed`, a `state` lifecycle including
  `rotating` and `revoked`, plus a unique partial index on the active key per tenant and purpose. Nothing
  reads or writes it. Searched across Rust, SQL, TypeScript, Svelte and Markdown: the only three hits are the
  `CREATE TABLE` and its two indexes.

  The same shape as `audit_log`'s hash columns and `assets.legal_hold` before G10·1 — a schema that describes
  a capability the code does not have. Recording it here rather than fixing it silently, because the fix is a
  decision rather than a refactor.

  **What exists today, for each of the four purposes the table anticipates.** All four are process-wide
  configuration or absent:

  | Purpose | Today |
  |---|---|
  | `blob` | `storage.sse_kms_key_id`, one key for the deployment (G10·3) |
  | `c2pa_signing` | `security.signing_cert_pem` / `signing_key_pem`, one identity for the deployment |
  | `field` | not implemented |
  | `backup` | relies on the bucket's and the managed database's own encryption |

  So "BYOK" is true of a single-tenant deployment and of a dedicated one, and not of a shared one — which is
  worth being exact about, because it is an RFP pass/fail question and the honest answer differs by
  deployment shape. G10·3 never claimed otherwise; the AWS-native list did, by listing per-tenant keys as
  though the whole item were outstanding.

  **The decision, because it is not a refactor.** `build_store` returns one `S3Store` for the process and
  G10·3's applicator puts the key on it once, deliberately, so that a write path added later cannot miss it.
  A per-tenant key breaks that: the key is no longer a property of the process. Three shapes, and they are
  not equivalent.

  1. **A store per tenant, cached.** Smallest change to the write paths — they keep taking a store — and it
     keeps G10·3's guarantee that the key is applied in one place. Costs a client per active tenant, and the
     cache needs invalidating when a key rotates or is revoked, which is the part that will be got wrong.
  2. **Resolve the key at each write.** No cache and no lifetime question, and it is the only shape where a
     revocation takes effect on the next write rather than on the next cache eviction. But it reopens exactly
     what G10·3 closed: seven call sites that create an object, each of which must not forget, and the
     source-reading test that guards them would need to guard a threaded parameter instead of one applicator.
  3. **A pool per tenant in `storage_pools`.** The table already carries `tenant_id`, `bucket`, `endpoint` and
     `credentials_ref` per pool, so a `kms_key_ref` beside them would follow the grain of the schema and need
     no new resolution path — the pool is already looked up per placement. Least new machinery; but it ties a
     key to a pool rather than to a tenant, and `encryption_keys` was designed for the other three purposes
     too, which have no pool.

  Shape 3 is the smallest honest step for `blob` alone and does not answer `c2pa_signing`, `field` or
  `backup`. Whether those are wanted at all is the prior question — a per-tenant signing identity in
  particular has consequences well beyond storage, since it is what a consumer verifies a C2PA claim against.

  **Not started, and no pressure to start.** A single-key deployment is correctly implemented, tested and
  documented, and the gap is a capability the schema promises rather than a defect in what runs.

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
  5. **SSE-KMS for BYOK (G10).** *The wiring landed under G10·3; only the per-tenant half is open —* see
     **G10·3b** just above, which is where that now lives rather than in this list.
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

## Q — Commercial-DAM parity

Surveyed on 2026-08-19 against a live tenant of the commercial DAM this work is benchmarked against. The
survey itself is not in the repository: it was taken against a named customer's tenant, so publishing it
would disclose someone else's account rather than anything about this system. The short version is that the
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
- [x] **Q.2c·3 The uncategorised worklist surfaced** in the UI, with the other admin worklists — see Q.20a.
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
- [x] **Cleanup: `Caller::identity_id` is a `Uuid`, because `authorize` was always going to make it one.**

  There is exactly one place a `Caller` is constructed and it had already refused a key with no identity
  several lines earlier, so the `Option` described a state the system could not produce. What an
  impossible state buys you is handlers that guess at it: a `let Some(..) else` returning an empty list, a
  different one returning a 403 with a sentence about machine keys, an `ok_or` in four modules, and a
  `person()` helper in `engagement.rs` whose own doc comment said it was unreachable and pointed at this
  item. Each was a different answer to a question nobody asks.

  Also six `if caller.identity_id.is_some() { User } else { ApiKey }` ternaries deciding what to write in
  an audit row's `actor_kind` — all of which had been resolving to `User` since the day `authorize` was
  written, while reading like a real branch.

  Roughly sixty sites across twenty-five handlers, which is why it was its own change. The compiler found
  every one; the interesting part was the site it found that should *not* have changed. `tus.rs` binds an
  identity from `auth::authenticate` rather than from a `Caller`, and there a machine key genuinely has
  none — authentication happens before authorisation. Removing that `ok_or` compiled fine and would have
  been wrong, and it is now commented with the distinction rather than left looking like the others.
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

      **Not done here:** the alternate preview upload that shared this parity slice. It is an ingest concern rather
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

      **All three sources ship now.** A collection was safe because putting an asset in one is itself the
      decision to publish it; a saved search and a media class are live queries, and a portal backed by one would
      publish every future asset that happened to match. The answer is `assets.published_at`: publication is a
      per-asset act, done by a person through a bulk operation — so the actor, the selection and the per-item
      outcome land where every other bulk act's do — and a live-query portal shows only assets carrying it. The
      query narrows a set somebody admitted rather than defining one. A `publish`/`unpublish` bulk kind, a chip
      on the grid cell, and two controls in the bulk bar named for what they do to the world rather than for the
      column they write.

      Re-publishing an already-published asset is a skip, not a restamp: `published_at` answers "since when has
      this been public", which a restamp erases. Unpublishing something that was never published is a success —
      "not on a public page" is what the caller asked for, and failing there makes an unpublish over a grid
      selection look broken for doing what it was told. Ten mutations caught; one survivor was a test that
      published *everything*, so the gate it was checking had nothing left to exclude.

      **Done in Q.14b:** the tenant-facing screen for *making* one.
- [x] **Q.14b Collections in the application.** Q.14 exposed the gap: `dam_db::collections` had been done since
  2.3 — membership, dense ordering, `pin_hot` — and there was no way to make or fill one outside a test. A portal
  publishes a collection, so a portal could not be created by the person who would want one.

      **Why it was unreachable, again.** All five existing functions took `&sqlx::PgPool`. A handler holds a
      `TenantConn`, whose transaction is the thing carrying the `search_path` that makes `collections` mean
      `t_acme.collections` — so a pool signature could not be called from one at all. That is the fifth module
      this session where dead code and a `&PgPool` signature turned out to be the same fact. Converted to
      `&mut PgConnection`, internal transactions removed, and `all`/`by_key`/`create`/`rename`/`delete` added.

      **`rename` does not move the key.** A portal references a collection by key, so renaming one would break
      or silently repoint every portal built on it — and the label is what anybody actually wanted to change.
      The screen says so twice: on the create form and again in the edit panel.

      **`delete` refuses while a portal publishes.** With the count and the fix in the sentence, because a
      public page whose collection vanished serves nothing with no explanation. Two bugs found writing it: the
      guard read `deleted_at` when portals retire via `retired_at`, and it ended in `unwrap_or(0)` — which is a
      guard that permits the delete it exists to refuse, *and* leaves the caller's transaction aborted so the
      swallowed error resurfaces on a later statement as "current transaction is aborted".

      **The predicate applies in both directions.** `add` filters the ids through the caller's scope, so a
      collection cannot put an unseeable asset onto a public page; `items` filters the same way, so it cannot
      be used to learn such an asset exists. The second half is the easy one to miss — the leak arrives when a
      narrowly scoped curator opens a collection somebody wider curated. Out-of-scope ids are *counted*, never
      named. The real positions are kept, so a gap is the honest signal that the set holds more than this.

      **Members carry a thumbnail**, minted through `assets::thumbnail_url` rather than a second signing path,
      because curation is visual: reordering photographs by filename is a different and much worse job.

      **The bulk-bar action is not a bulk operation.** Publish and archive go through preview/confirm and an
      audited `bulk_operations` row; adding to a collection is arranging a working set — reversible, expected
      dozens of times an hour, and recording it would bury the rows that matter.

      **What the browser caught:** removing a member filtered the list locally, but a removal renumbers on the
      server — so the screen showed stale positions and then used the resulting hole to claim the collection
      held assets outside the caller's scope. Now every mutation takes the server's list back. Nine API cases,
      eight new db cases, eight browser cases including axe, and one more on the bulk bar.
- [x] **Q.15 The built-in facets:** asset status, orientation, average rating, has-attachment.

      **None of these can be a field definition**, which is the whole reason they needed building. `facetable`
      is a flag on a *metadata field*, and a status is a column with a CHECK behind it, an orientation is
      derived from two more columns, a rating is an aggregate over another table, and an attachment is a row
      pointing back. A rail that reads only field definitions cannot offer any of them, so `facets::Builtin`
      enumerates the four and the endpoint appends them to whatever the tenant marked.

      **A facet key is a query selector.** The rail writes the string it reads, so a bucket that cannot be
      turned into a filter is a checkbox that does nothing when clicked. That decided the spellings: the rating
      facet's key is `stars`, not `rating`, because `stars:4` is what the parser accepts, and the attachment
      facet emits one bucket named `attachment` so the rail composes `has:attachment`. Three new reserved
      selectors — `status:`, `orientation:`, `has:` — reserved for the same reason as `in:` and `is:`: a tenant
      field of that name would shadow something that is not theirs to redefine, and the rail's own links would
      stop working for reasons nobody could see.

      **The bucket expression and the filter clause have to agree**, and the test asserts that directly: it
      counts, then filters, and compares. `stars:4` rounds the average, so the facet rounds it too — a bucket
      labelled 4 that returned the 3.5s and the 4.4s while the filter returned only the 4s is a rail that lies
      quietly. Both sides also apply `LIBRARY_ROWS`: a superseded version and an attached release form are
      rows the caller may see, and neither is a library row.

      **Absent rather than zero**, everywhere. An unrated asset is not a zero-star bucket, an audio file is in
      no orientation bucket, and there is no "No attachments (1,204)" row — the complement is the rest of the
      grid, and a rail row nobody clicks is a row that costs a query.

      All three clauses are refused by the index rather than dropped, like every other relational clause: each
      *could* be an index field, and each would then have to be kept in step with the column it duplicates —
      a status change or a re-probe would have to reindex the asset, and until it did the index would answer
      with yesterday's shape.

      Seventeen mutations caught. One survivor was a test that could not see its own property: the release
      form it created was outside the caller's group, so the access filter hid it and `LIBRARY_ROWS` had
      nothing to do. Driven live against the dev stack — clicking Portrait in the rail wrote
      `orientation:portrait`, narrowed the grid to one asset, and re-narrowed the rail's own counts with it.
- [x] **Q.16 Search-within, substring, advanced search, multiple-asset search.**

      **`Contains` and `StartsWith` had been in the IR since 2.4 with nothing able to produce them.** The SQL
      renderer knew how to answer a substring and no query could ask for one. `*text*` and `text*` are that
      syntax, through the same operator parser a field already used, so a wildcard means the same thing
      everywhere rather than being a second dialect for one clause. Text only: over a date or a number a
      wildcard has no meaning that is not an accident of formatting, so it is refused with the kind named.

      **A leading star alone is refused, not widened.** `*text` asks for a suffix, and the refusal says to write
      `*text*` instead. Widening it would return more than was asked for — the wrong direction for a filter to
      be wrong in. (`filename:*.pdf` is the case somebody will type; `*.pdf*` answers it, and a media class
      answers it better. A real `EndsWith` is a variant in five places for a question `mime:` already holds.)

      **`filename:` is its own selector**, because free text is *ranked* text: the index tokenises a filename,
      so `DSC_0043` is findable through the box and `0043` is not. Somebody holding a list of names off a
      delivery note has the substring, and a substring over a column is a SQL query. Reserved like `in:` and
      `is:` — a tenant field of that name would shadow the one thing every asset has.

      **The advanced form writes the query string; it is not a second way to search.** Conditions, a pasted
      list of filenames, and "search within results" all compose shorthand and hand it back to the same box, so
      "copy this search" copies all of it and what the user sees is what the server got. A form posting its own
      structured payload would be a second query language with its own bugs and a box beside it that lies.
      Operators are named for what they ask ("starts with") rather than for the syntax they produce, and each
      kind is offered only the ones it can answer — the form does not suggest a query the server refuses.

      "Search within results" is an `AND` onto what the box holds, and nothing more. A separate result set to
      narrow would be a second thing to keep in step with the first, and the URL would stop describing the
      page. The existing query is parenthesised when it carries a top-level `OR`, or narrowing `(a OR b)` with
      `c` would widen it rather than narrow it.

      Twenty-three mutations caught, and the sweep itself needed fixing first. Four survivors were tests that
      could not tell two operators apart: the fixture had no filename that *contained* a prefix without
      starting with it, so a prefix rendered as a substring passed, and none that started with an exact name, so
      an equality rendered as a prefix passed too. Adding those two names then broke a third assertion — and
      because the baseline was red, the next sweep reported every mutation as caught. **A mutation sweep over a
      failing suite says nothing at all**, so the harness now checks each target is green before it mutates
      anything, and the whole set was re-run. Two real gaps only that re-run could see: nothing tested a negated
      filename, and nothing tested that a `filename:` query is *answered* — the index refuses it by name and
      the handler surfaces the refusal, so routing it wrong is a failed request rather than a slow one.

      Driven live against the dev stack, which is where the last fix came from: every pasted filename was
      coming back as `filename:"sample-003.jpg"`, correct and unreadable, because the quoting rule treated an
      interior hyphen as structure.
- [x] **Q.17 Predictive search and did-you-mean.**

      **A suggestion is a disclosure, and a sharper one than a facet count.** A count needs a reader to infer
      something from a number; a suggestion *names* the value, so offering "Northwind" to somebody who may see
      none of Northwind's assets hands them the fact directly. Every source — field values, confirmed terms,
      filenames — is counted over the caller's own access-filtered library through the same `push_where` every
      other read uses, and narrowed by the query already in the box, so a type-ahead two clauses into a search
      offers what is left rather than what the library holds.

      **A suggestion carries the query fragment it composes into**, for the same reason a facet key is a
      selector: the client's job is to put a string in a box, and a suggestion it has to assemble is a second
      place where the query language is spoken and can be got wrong. The place it would be got wrong is
      quoting, where the symptom is a query that changes when it is clicked.

      **No history and no popularity.** Suggesting what other people searched for is a cross-caller disclosure
      with no access filter available — the search that produced it belonged to somebody with different grants
      — and ranking by frequency across a tenant leaks which clients are busy. What is offered is what this
      caller can already see, ordered by how much of it there is.

      **Did-you-mean is offered, never applied.** A refusal that names what was meant is actionable; a refusal
      silently *corrected* is a filter nobody asked for, and the first wrong guess leaves somebody with results
      they cannot explain. Two places it appears: a parse refusal carries the closest known name — fields,
      aliases and the reserved selectors, plus the closed vocabularies suggesting from their own values — and an
      empty page carries a whole query worth trying instead. The distance cap is scaled by length, because one
      wrong letter in `id` is a different word and one wrong letter in `photographer` is a typo; suggesting
      `year` for `photographer` reads as a system that does not know its own field names.

      The value suggestion is deliberately narrow: exactly one clause comparing a field to a literal, which is
      the typo people make and the only shape where a candidate can be *checked* to exist in the caller's own
      library before being offered. A two-clause query has two candidates and no way to know which was mistyped.
      One thing tried and reverted: offering the correctly-cased value when only the case differs. It looked
      like a kindness until the corrected query was run and came back empty too, because an equality on a text
      field is answered by the index, where a long value is a row of tokens rather than one term. A suggestion
      that leads to a second empty page is worse than none.

      **A Q.16 bug this slice found.** `caption:*harbour*` answered **501**. The parser produced the clause, the
      index refused substring matching by name — an `ILIKE` and a Tantivy automaton disagree at the margins, and
      §12 forbids an approximate answer that differs between back ends — and nothing routed it to the database
      that can answer it exactly. Q.16's wildcards worked for `filename:` and for nothing else, because
      `filename:` was already relational. Field substrings and prefixes now route to SQL like every other clause
      the index cannot answer, and say they are unranked.

      Twenty-four mutations caught. Two survivors are documented rather than tested, because both are
      unobservable by construction: the `total == 0` check before the value lookup is a *cost* guard whose
      behaviour the "value already exists" check below it already implies, and the empty-result lookup on the
      SQL path was dead code — the lone-field-equality shape a value suggestion needs is never the shape that
      routes to SQL — so it was removed. A third was a redundant whitespace guard in `trailingWord`, deleted:
      splitting on whitespace already leaves an empty final token. Driven live end to end, which is where the
      last fix came from: an empty page was announcing "0 assets · ranked by relevance, capped at the first
      1,000", a sentence about the ordering of nothing, sitting between the count and the one thing worth
      clicking.
- [x] **Q.18 Export search results to CSV.**

      `GET /search/export.csv`, taking the same parameters as `/search`. One CSV vocabulary shared with the
      order export — field *keys* as columns, the fixed five first, a multivalued field flattened to `a; b`
      rather than JSON — because two exports written separately drift, and the person who notices is the one
      whose re-import fails against a file that opened fine in a spreadsheet.

      **Answered in SQL, always, even for a query the index would rank.** An export is a set rather than a
      ranking: it is a file somebody re-imports, audits or hands to a client. Both of the ranked path's failure
      modes are silent omission — the index is eventually consistent, so a just-edited asset may be missing, and
      its total is capped by the overfetch depth, so a large set cannot even be measured there. The cost is
      stated rather than hidden: a free-text export matches substrings where the grid matched tokens, and every
      structured query is identical in both.

      **Two silent truncations, both found by a mutation that should have died and didn't.** The first version
      asked the index for the count, so the cap never fired — an empty index reports zero and every set looked
      small. The second asked for ten thousand rows in one call, and a page is capped at five hundred: a file of
      exactly 500 rows, which opens perfectly and is wrong in the one way an export must never be. The export
      now pages to the cap, and past it refuses with the count, because "too many" without a number is not
      something anybody can act on.

      The test that hid both was the giveaway: it asserted the constant and that a small set worked. Crossing
      the boundary needed ten thousand rows, which one `generate_series` insert produces in a second — the
      reason not to write it was imagined.

      In the app: an Export CSV button beside Advanced, a `fetch` and a blob rather than a link because the
      endpoint is authenticated and an `<a href>` carries no header, and the server's sentence shown verbatim
      when a set is too large.
- [x] **Q.19a Refine-search configuration.** Dependent metadata fields are Q.19b below.

      Until this, the rail was every facetable field ordered by `display_order`, then every vocabulary by
      label, then the four built-ins — a reasonable default and not something a tenant could change. A library
      with thirty facetable fields has a rail nobody scrolls to the bottom of, and the two filters that matter
      are wherever the schema happened to put them.

      **An entry is a kind and a name**, not a field: `field:brand`, `taxonomy:<uuid>`, `builtin:stars`. That is
      what lets the four built-ins be arranged and switched off like anything else — "we do not use ratings" is
      expressible without asking us — and what stops a vocabulary called `brand` colliding with a field of the
      same name. The shape is a CHECK constraint rather than a handler's validation, because a row naming none
      of the three would be a rail entry nothing can render, failing as an absence.

      **Absent means default.** A tenant that has never configured anything has no rows and gets the order the
      schema implies. Seeding the defaults at provision time was the alternative and it is worse: a field
      defined next month would be missing from a table that looked complete, and the rail would quietly stop
      offering it. For the same reason, an entry the configuration does not mention appears *after* everything
      configured rather than vanishing — a filter that disappears because somebody arranged the rail last year
      is a filter nobody knows to ask for.

      **Disabling is not un-facetable.** `field_defs.facetable` is a resource decision — faceting free text
      produces a bucket per distinct value — and it governs whether the count may be computed at all. This is
      presentation: a field can be facetable and hidden, which is how `stars:4` stays typeable in the box while
      the star rail comes off the screen. The order is applied *before* counting, so a hidden facet is not three
      queries nobody reads.

      Move-up and move-down rather than drag, because the keyboard equivalent of drag *is* those two buttons —
      building them first means one interaction that works everywhere. Disabled entries stay on screen under a
      divider, since you cannot re-enable what you cannot see.

      Nine mutations caught. One survivor was a test with no vocabulary in it, so "a taxonomy is an entry too"
      had nothing to prove. Driven live: configuring `colours, orientation, brand, status` reordered the real
      rail, moving a row in the browser moved it again, and the screen was reading field *keys* where the tenant
      had written labels — the catalogue, not the definitions, is what a person-facing list needs.

- [x] **Q.19b Dependent metadata fields.** A field whose relevance depends on another field's value: shown when
      the parent matches, and required only when shown. (Shipped in `0461f0f`; the box was never ticked.)
- [x] **Q.20 Parity, the last four slices.** Worklists (Q.20a), tag vocabulary (Q.20b), webhook
  delivery (Q.20c), site branding (Q.20d).
- [x] **Q.20a The admin worklists.** Ten lists, each one SQL over data damrs already holds: no table, no queue,
  no state to fall out of date, so an asset leaves a list the moment somebody fixes the thing. `Read`, not
  `Manage` — the person who files an uncategorised asset is whoever can edit it, and gating the *finding* behind
  a permission the *fixing* does not need is how a backlog becomes one person's job.

      **Not in the query IR, deliberately.** Six of the ten could be search clauses and it would be the wrong
      place: "a required field of this asset's *resolved* metadata type is absent" is a three-way join with a
      fallback chain, and putting it in the IR would put it in saved searches, asset-group predicates and the
      MCP surface too. Administration stays administration.

      **Every count runs through the caller's predicate**, so two readers legitimately see different numbers —
      §7's disclosure rule arriving as a usability bug, because a to-do list that counts work its reader cannot
      see sends them to an asset that 404s. Ten scalar subqueries in one statement, so the numbers describe one
      instant: "12 uncategorised" beside "12 missing metadata" invites the reading that they are the same twelve.

      **What running it against the real library found, and no test would have.** The grid badged three assets
      "Expiring" while the worklist named after expiry reported zero. `RightsState::Expiring` comes from licence
      *term* dates with a per-licence notice window (60 days by default, longer where a contract says so);
      `assets.expires_at` is a retention date somebody set on the file. Two different questions, and the one I
      had built ignored the contracts. Fixed by reading `assets.rights_state` — the same column the badge
      renders, so the two cannot disagree — and adding `rights-denied` beside it. Both expiry lists now say
      which column they read.

      **And `no-licence` is not urgent.** It read 180 of 182 on the dev library and outlined the whole page in
      red. Every asset arrives unlicensed, so a badge that fires on every row from day one is background rather
      than a signal; urgent marks a *change* — a contract running out, a use that became forbidden — not an
      absence that was always there.

      Also caught by the tests: an archived asset was still appearing as filing work, which is two answers to
      "has this been dealt with" — every list is now scoped to `status = 'active'` in one place, so an eleventh
      cannot forget it. 12 db cases, 6 API cases, 10 browser cases including axe. Verified against the running
      stack: every count cross-checked by hand against SQL, and the three "Licence coverage ending" assets each
      carry the matching badge in the grid the list opens into.
- [x] **Q.20b Tag vocabulary administration, and the fifth guard rail with no road.** A vocabulary is the label
  set zero-shot tagging scores against, and §8.2's claim that "a closed vocabulary is what keeps AI tags
  governable" was not true: `taxonomies.ai_taggable` had existed since migration 0001 and **nothing read it**,
  so the enrichment query offered a model every non-deprecated term in the tenant — including the terms of
  *category trees*, which are filing structure rather than a label set. Inviting an LLM to file assets into
  somebody's browse hierarchy is a much larger claim than inviting it to suggest a tag, and nobody chose it.

      **The sixth `&PgPool` module, with a twist.** `dam_db::taxonomy`'s move/merge/deprecate have been written
      since 2.2 and unreachable since 2.2 — but here the pool was *deliberate*, documented as "a caller cannot
      accidentally run one of these outside a transaction". The intent was right and the mechanism made the
      module uncallable: a handler reaches tenant tables through a `TenantConn`, whose transaction carries the
      `search_path`, and a pool has none. `TenantConn` **is** a `Transaction`, so the guarantee survives the
      conversion and becomes structural rather than defensive — and a caller can now merge three terms in one
      transaction instead of three, closing the windows in which the vocabulary had two live terms for one
      concept.

      **Migration 0034 backfills, and the first version of the backfill was wrong.** Honouring a `false`-default
      column without a backfill would hand every existing tenant an empty vocabulary and silently stop AI
      tagging that works. So it sets the flag `true` — and setting it on *every* taxonomy, which looked like the
      conservative choice, opened the dev tenant's category tree to the model. Caught by running it: the
      backfill is now scoped to `kind = 'vocabulary'`, and the query requires the kind as well as the flag, so a
      tree can never be offered even if somebody sets the column by hand. That last case is now a test.

      **What the surface deliberately lacks.** No delete, at either level. `asset_tags` cascades, so deleting a
      term untags every asset that carried it — years of work, gone quietly, noticed when a search returns
      empty. Retire keeps the assets and keeps the id resolving; merge moves the assets and leaves a pointer.
      No slug field on the edit form either: it is what a model answers with and what an import resolves.

      **Smaller decisions.** Opening a vocabulary to a model is its own endpoint, not a field on an update body,
      so it cannot be changed while editing a label. Synonyms are trimmed, emptied and de-duplicated
      case-insensitively before they cost prompt bytes on every call — `dam_ai::enrich` already matches without
      regard to case. The threshold is clamped and *read back*, because a screen showing the 1.5 somebody typed
      would show a setting that is not in force. A term id in a URL is checked against the vocabulary id beside
      it, or the path segment would be decoration and a guessed id would confirm a term exists.

      10 db cases, 9 API cases, 9 browser cases including axe. Verified against the running stack through a
      real browser: created, terms added with `cloudy,  grey ,, Cloudy` arriving as `cloudy, grey`, opened and
      closed with the count in the sentence, merged with the survivor named, a 1.5 threshold reported back as 1,
      and retired — with two copy fixes that only showed up on real data ("its 0 assets keep it", and "every one
      of these 0 terms is in the prompt" for a vocabulary whose terms were all retired).

- [x] **Q.20c Webhook delivery, and an ordering guarantee the schema could not keep.** Migration 0004 has
  carried the whole design since the start — subscriptions, a transactional outbox, per-asset ordering, retry,
  dead-lettering, auto-disable — and nothing had ever written a row to it. `dam-connect/src/lib.rs` was a
  one-line doc comment.

      **The bug in 0004's own promise.** It guaranteed "delivery is sequential per (subscription, asset)" and
      gave `created_at timestamptz DEFAULT now()` to order by. Those are incompatible: `now()` is the
      *transaction* timestamp, identical for every statement in it, so two events enqueued together tie and
      the tie-break falls to `gen_random_uuid()` — random order, on the table whose entire purpose is order.
      Not a corner case, because an outbox row is written in the transaction that made the change, so "publish
      this version and expire the old one" is exactly one transaction with two events for one asset — 0004's
      own example of what must not be reordered. Migration 0035 adds a sequence and re-cuts the ordering index
      onto it. What a sequence does *not* promise is written into the migration rather than left to be found:
      it is allocated at INSERT, not at COMMIT, so ordering is exact within a transaction and best-effort
      across concurrent ones.

      **A signature a customer can check.** The signed string is `timestamp.body`, because a signature over
      the body alone is valid forever — and replaying the `asset.published` that preceded an `asset.expired`
      un-withdraws an asset. `verify()` ships in the crate rather than living in the test, so the scheme has
      one implementation. The delimiter is safe only because a decimal integer contains no `.`, which is a
      property of the field rather than the format, so it is asserted along with the collision it would allow.

      **The subscription URL is an SSRF vector**, and closing it exposed a product defect on the first real
      use: the guard refused `http://127.0.0.1:9099/hook`, which is the shape of every receiver a developer
      writes — so building a webhook integration locally was impossible without hand-written SQL, a workaround
      no customer has. Development now permits `http` and *loopback*, and nothing else. Private and link-local
      stay refused everywhere including development, because a development box is often a cloud VM and
      `169.254.169.254` is the metadata service.

      **Producers, so this is not a sixth guard rail with no road.** Six event kinds in one place, emitted from
      the real code paths — the bulk executor and the single-asset endpoints both, because a consumer cannot
      tell which route an edit took and an event that fired for one and not the other would be a cache that
      goes stale depending on how many assets somebody selected. A no-op emits nothing. Payloads carry ids and
      never bytes (§11's reference-not-copy premise), and a metadata event carries the *keys* that changed
      rather than the values.

      **The dispatcher never holds a connection through somebody else's timeout**: three short transactions
      with the HTTP between them, because a handful of slow endpoints holding pooled connections is how an
      integration becomes an outage. Concurrency needs no semaphore — `claim` already refuses two deliveries
      for one asset, so a batch is concurrent exactly where it is safe.

      Also caught: `unwrap_or_default()` on the worker's HTTP client builder would have silently fallen back to
      a client that follows redirects, undoing the one security property that builder exists to set. And
      `cargo fmt` collapsed a line-continued string literal while keeping its padding, putting "post to one
      even in              development" in a user-facing refusal — found by a test asserting the sentence.

      17 db cases, 11 over a real socket, 8 API cases, 2 producer cases. Verified end to end against the
      running stack with an independent Node receiver: two publications and a metadata edit delivered, every
      signature VERIFIED using only the documented scheme, a deliberate 503 retried to success with the same
      delivery id, and the metadata payload carrying `["title"]` while the value appeared nowhere.

- [x] **Q.20d Site branding, and the vendor's name in every customer's library.** Two things were wrong. The
  application called itself "damrs" in the nav of every tenant's library — a vendor's name where a customer's
  belongs. And `portals.accent` defaulted to our own `#2563eb` literal, so a tenant with six press kits set the
  same colour six times and the seventh silently reverted, with nothing on screen saying why.

      **A singleton in the tenant schema, not `tenants.settings`.** That jsonb column exists and nothing has
      ever read it, and it was tempting. Branding is tenant *data* — a logo is an asset in the tenant's own
      schema, and 0002 forbids cross-schema foreign keys, so half of it could not live there. And a jsonb blob
      has no CHECK constraint, which for a colour interpolated into CSS is the difference between a validated
      value and an injection point. Migration 0036, same shape as `enrichment_settings`.

      **The name falls back rather than defaulting.** An empty `site_name` resolves to the tenant's display
      name, which they gave us at provisioning. Resolved in the API because the column cannot see
      `dam_global.tenants` and a copy here would go stale; the response says *which* it was, because a form
      that pre-filled the fallback would make the default look chosen — after which clearing it is
      indistinguishable from never setting it.

      **The accent is not applied to the application's own colour system**, and that is deliberate rather than
      unfinished. `layout.css` tunes `--color-accent` and `--color-accent-fg` separately for light and dark,
      with the comment "a light-on-light-blue button is the classic 3:1 failure" — substituting an arbitrary
      tenant hex would break those pairs on surfaces that carry text, and axe would catch some of it but not
      all. So the shell gets a decorative mark, which carries no text and cannot fail a contrast check, and the
      accent's real job stays the portal, which is external-facing and already renders it in full. The screen
      says so rather than looking unfinished.

      **What driving it found.** The header fell back to "damrs" while branding loaded, so every page load of
      every customer's library flashed the vendor's name — the exact thing this removes, just briefly
      ("immediately: damrs | settled: Acme Picture Library"). It renders nothing until known now, with the mark
      holding the home link open and clickable. And two of my own e2e assertions were wrong about the
      implementation rather than the reverse: the header mark shows the colour *in force* rather than
      live-previewing a draft, which is the right way round, and the `pattern` attribute means a malformed
      colour never reaches the server at all — so the server's colour refusal is provoked through the API, and
      the form's refusal path is the logo scope check.

      1 unit case, 6 API cases, 1 portal-inheritance case, 10 browser cases including axe. Verified live: the
      fallback resolved to "Acme Corp", `#000;} body{display:none` refused by name, and a save updating the
      header without a reload.

Absorbed by the existing roadmap rather than duplicated here: the AI set (tags, faces, document text,
transcripts, semantic search, duplicate detection) is M4; conversational access is M5's MCP server; workflow
and Insights are M6; FTP and import are Pre-GA G7; SAML/SSO and user administration are Pre-GA G10; the
storage and usage reports are G19. Entries (the PIM) is a new application and lands after them.

Not building: Hootsuite, Mobile, Templates, Video Creator, Syndicate, Digimarc, Google Analytics linkage —
third-party or separate products, reached through the API and webhooks, which are on the list.


## Go-live — the things standing between the code and anybody using it

Asked for after the go-live gap analysis. Ordered cheapest-first among items that actually block use.

- [x] **L.1 A deployment image.** One image carrying `damd`, `dam-worker`, `damctl` and the media toolchain the
      pipeline shells out to, plus `docker/DEPLOY.md` with the deploy order and an honest list of what an image
      is not. Not distroless, because `dam-media` *executes* vips and ffmpeg rather than linking them.

      Building it found a total failure no test could have: `vipsthumbnail`'s ICC flag is `--output-profile`
      from vips 8.18 and `--export-profile` before it, and the code hard-coded the newer name — so on any
      Debian base **every** vips-rendered derivative failed. Invisible for PNG and JPEG, which the pure-Rust
      path decodes; total for HEIC, which only vips can. Discovery now asks the binary its version.

      Also found: the presigned-URL host is signed, so the endpoint the server signs with must be the one the
      browser connects to, and there is no way to configure an internal connect endpoint separately. And three
      places in the repo described committed `.sqlx/` offline query metadata for `query!` macros that do not
      exist anywhere in the tree.

- [x] **L.2 Backups and restore drills (§17, G11).** `dr_state` had existed since the first enterprise
      migration — including a column whose own comment says it "is set only by an actual restore drill" — with
      nothing writing a single field of it and nothing taking a backup. The table was an argument, not a
      mechanism.

      `dam-backup` does the per-tenant half: `pg_dump --format=custom` of one schema uploaded outside every
      tenant prefix (so a lifecycle policy cannot tier a backup into Glacier), and a replay into a scratch
      schema that counts what arrived against the count recorded in the object key. The live schema is renamed
      aside and back rather than restored over, because a drill that can damage what it verifies is one nobody
      runs on production. `damctl dr-report` exits non-zero while any tenant is unverified, so it works as a
      check and not only as something to read.

      A backup deliberately does **not** move `last_verified_restore_at`; only a drill does. There is a case
      asserting exactly that, and one asserting a drill *fails* when the restore comes back with the wrong
      count — `pg_restore` exiting zero proves the file parsed, not that the data arrived.

      Verified against the real 185-asset library and then again from inside the container image. That second
      run found a bug the laptop could not: Debian's `pg_dump` is a symlink to `pg_wrapper`, a multi-call
      program that dispatches on `argv[0]`, so canonicalising the path — copied from the vips toolchain, where
      it defends against a swapped symlink — left it unable to tell which tool was wanted. Every backup taken
      in the image failed with `os error 2` while the conda-installed binaries on the laptop, which are not
      wrappers, worked perfectly.

      Not built, and not going to be: WAL archiving, PITR, S3 versioning, cross-region replication. Those are
      infrastructure and §17's five-minute RPO comes from them. The Tantivy snapshot half of §17's table is
      also unbuilt, so index recovery today is a full rebuild from Postgres — the slow path the snapshot was
      meant to avoid.

- [x] **L.3 Metrics and a readiness probe.** `/ready` checks Postgres and the object store — both, always, so
      a wide outage does not hide behind the first failure — and names which one broke. It deliberately skips
      the search index: a tenant's index opens lazily and is rebuildable, so failing readiness over it would
      pull a replica for something that stops no upload or download.

      `/metrics` is Prometheus text and **fail-closed**: no `server.metrics_token`, no endpoint, and a 404
      rather than a 401 so a scan cannot tell "off" from "protected". The argument is the one already written
      against `/health` — it is the first thing anybody scans, and route templates plus per-route counts are a
      map of the API and a usage profile.

      The registry is hand-written in `dam-telemetry` rather than pulling the `metrics` crates: four series,
      about two hundred visible lines, no global recorder to install. The design problem is cardinality, so
      the route label is axum's `MatchedPath` template and never the URI — a label per asset id is a million
      series and a monitoring outage — with an unmatched request collapsing to one bucket so nobody can create
      series by requesting random paths. Status is a class, not a code.

      `damrs_jobs` by kind and state is refreshed on scrape rather than on a timer, and a failure reading it
      does not fail the scrape: an endpoint that 500s because one gauge is unavailable goes dark exactly when
      the database is the thing going wrong. `state="dead"` is the series worth alerting on — a worker failing
      every derivative looks identical from outside to one with nothing to do.

      Lock poisoning is recovered from rather than propagated. The worst case is a counter off by one; making
      the observability layer able to panic the process is backwards.

- [x] **L.4 Rate limiting.** `governor` had been a declared dependency of `dam-api` — commented "per-tenant
      rate limiting" — and never called.

      Applied to the **public** routes only, keyed by client address. The authenticated API is deliberately not
      address-keyed: a company sits behind one or two egress addresses, so that would be a limit on the
      customer as a whole, with one bulk upload starving everybody else's thumbnails. Authenticated traffic is
      bounded by a revocable credential and by the per-tenant quotas instead.

      Burst separate from sustained rate, because a grid loads sixty thumbnails at once and a limiter tuned
      only on the rate throttles the first screen every user ever sees. Off by default: a limiter with a
      guessed number either does nothing or throttles a legitimate page load, and neither is discovered until
      it is in front of users.

      `X-Forwarded-For` is trusted only as far as `trusted_proxy_hops` says, counting from the **right** —
      those entries are what proxies appended and a client cannot forge them. Taking the leftmost entry, the
      usual mistake, lets anybody claim a fresh bucket per request or exhaust somebody else's.

      One correction worth recording: the middleware first took `ConnectInfo` by value while its own comment
      claimed a missing address was allowed. A required extractor *rejects*, so a forgotten
      `into_make_service_with_connect_info` would have been a 500 on every public route rather than a lenient
      one. It reads the extension directly now, and a case asserts both halves — engaged with a peer, allowing
      without one.
- [x] **L.5 A virus scan on ingest.** M1 listed it; nothing existed. `dam_media::antivirus` talks `clamd`'s
      `INSTREAM` over a socket — four lines of framing, so no dependency — rather than shelling out to
      `clamscan`, which reloads the whole signature database per invocation and would need the bytes written to
      a shared filesystem before we have decided to accept them.

      Scanned **before promotion**, so infected bytes never reach a content-addressed key and never become an
      asset. Quarantining an asset row afterwards was the alternative, and it leaves the object inside the
      library's own namespace while making "is this safe" a question about a column.

      An unreachable scanner refuses the upload **transiently** — fail closed, and recoverable: the upload
      waits in staging and finalises when `clamd` returns. A configurable fail-open is the setting that is
      still switched on a year later. An unparseable reply is permanent and deliberately not read as clean,
      because a parser that fell through to `Clean` would turn a future protocol change into a silent bypass
      of the only thing between an upload and the library.

      **Files past `max_scan_bytes` (default 100 MB, matching `clamd`'s own ceiling) are accepted unscanned**,
      with a warning naming the size. A DAM for video masters cannot refuse every large file; the honest thing
      is to say so rather than imply coverage. Written down in `docker/DEPLOY.md` in those words.

      Eight unit cases including a fake `clamd` that asserts the wire framing from the server's side — the
      zero-length terminator is the mistake that presents as "the scanner is down". Then verified against a
      real ClamAV: EICAR refused permanently with its signature and no asset created, a clean PNG through to
      three derivatives, a dead port producing a re-queued job and no asset, and that same upload finalising
      itself once the scanner came back.

      Not built: re-scanning existing assets when signatures update. A scan happens once, at ingest.
- [x] **L.6 C2PA verification on ingest (task 1.9, G1).** Not unbuilt after all — `dam_media::provenance` is
      493 lines of verify, sign and state mapping, `dam_db::provenance` records and reports it, and the schema
      has carried `provenance_manifests`, `provenance_actions` and a `provenance_gaps` view from the start.
      **Nothing called any of it.** The fourth instance of this pattern in a week, after the archival executor,
      the backups, and the rate limiter.

      Two things blocked the wiring, and both were the shape of a guard rail with no road behind it:

      1. **`SigningIdentity` had only an `ephemeral` constructor**, which refuses outside `Development` with a
         message telling the operator to "configure a real signing certificate" — and there was no way to
         configure one. `from_pem` now exists, taking a chain, a key, a stated algorithm and an optional
         timestamp authority. The algorithm is stated rather than sniffed because a mismatch produces a
         manifest that verifies nowhere, and a TSA is worth configuring because without one every signature
         stops verifying the day the certificate expires — for an archive, the difference between provenance
         and a decoration with a shelf life.
      2. **`record_inbound`, `record_signed` and `insert` required `E: PgExecutor + Copy`** — a pool — so they
         were unreachable from a `TenantConn`'s `&mut PgConnection`. Exactly why `dam_db::restores` was dead
         code too. They take a connection now.

      Verification runs on the original after the asset row is committed, over the whole object rather than the
      header window: a manifest lives in a JUMBF box whose position depends on the format, and reading a prefix
      would report `absent` for a credential sitting past it — indistinguishable from "we did not look". The
      manifest is stored as its own tier-exempt object, because §2 archives masters and a credential that lived
      only inside the original's bytes would become unverifiable the moment it went cold.

      Verified end to end with a genuinely signed JPEG (there is an example binary that produces one):
      `provenance_state=untrusted`, `had_inbound_manifest=true`, an `inbound` row naming the signer, the claim
      generator and the spec version, a `c2pa.created` action row, and a 13,099-byte manifest object in the
      bucket. `untrusted` rather than `valid` is the point — the signature verifies and chains to nobody — and
      all 187 pre-existing assets stayed `none` rather than being flooded with rows.

      Placement was wrong on the first attempt: recorded before the transaction that inserts the asset
      committed, so a separate connection could not see the row its foreign key referenced. It surfaced as an
      FK violation on the first credentialed upload, and only because this path logs rather than fails.

- [x] **L.6b Re-signing derivatives — G1 closed.** Every rendered derivative now carries a manifest chained to
      its original as a `parentOf` ingredient, signed between render and store so the bytes that reach the
      bucket are the signed ones.

      Embedded rather than detached, unlike the inbound manifest, and the asymmetry is deliberate: a derivative
      is the thing that *leaves* — downloaded, embedded in a page, fetched by a connector — so a credential
      held only in our database would be provenance nobody downstream can check. An original goes the other
      way because it tiers to Deep Archive and its credential has to outlive that.

      `DerivedFrom`, never `Created`. `Created` would claim the file came into existence here, discarding what
      the original said about a camera or a generative model — and for AI-generated originals that would
      silently drop the Article 50 marking (D15) the derivative is obliged to carry.

      Actions carry their parameters: `c2pa.resized` with the dimensions and `c2pa.converted` with the format.
      "Resized" without a size records that something happened without recording what, which is not
      provenance.

      Signing failures are logged and do not fail the render. A missing identity is the ordinary case and a
      certificate problem is an operator's to fix; neither is a reason for a library to have no thumbnails.

      Three requirements found by making it work against a real certificate, each now producing a distinct
      error rather than a mystery: the key must be **PKCS#8** (`openssl ecparam` writes SEC1), the certificate
      must **not be self-signed** (C2PA refuses one, so a real leaf-and-CA chain is needed), and the algorithm
      is **stated rather than sniffed** because a mismatch produces manifests that verify nowhere.

      Verified in the bytes rather than in a row, which is the only verification that means anything here.
      `examples/verify_file` on a downloaded `web-2048` reports `state untrusted`, `signer damrs signer`,
      `generator damrs/0.1.0`, **`ingredients 1`**, and `c2pa.opened, c2pa.resized, c2pa.converted`. The
      ingredient count is the number that matters: it means the derivative names its parent rather than making
      a fresh claim about a file that appeared from nowhere. Relationally, four manifests for one asset — one
      `inbound` and three `damrs_signed`, each with `parent_manifest_id` set.


## Archival — tiering, restores, and the storage screen (§6.4, §6.5)

Raised as "do we have archival in place?". The answer was: every hard part, and none of the wiring. The
planner, the restore arithmetic, the S3 calls (conformance-tested against real Glacier nightly), the
bookkeeping and the whole schema existed; nothing called any of it. So every asset stayed in the class it was
uploaded to, permanently, `restore_requests` was a table with no writer, and `restore_requests_poll_idx` had
been indexing for a poll query that did not exist.

- [x] **A.1 The executor.** `tier_sweep` plans every enabled policy and executes what is not a dry run;
      `restore_poll` issues what is queued, checks what is in flight, and expires what has lapsed. Separate
      kinds because they fail differently — a sweep can skip a day, while a restore is a person waiting.
      Both re-queue themselves under M5c's pattern and deliberately without a dedupe key, which the note on
      `requeue_backfill_collect` explains at length.

      Three findings. `object_placements.pinned` has never been written by anything, so trusting it would have
      archived assets under legal hold — the scan derives pinning from the facts that mean it and ORs the
      column on top. `dam_db::restores` took a `&PgPool`, which is *why* it was dead code: its tables are
      tenant tables needing a tenant `search_path`, so from the worker every function in it was unreachable.
      And `storage_pools` had one retrieval price, so the tier chooser §6.5 turns on would have quoted the same
      number three times.

- [x] **A.2 The API and the two places the rest of the system had to learn about cold bytes.** Plan, run,
      quote, request, read, approve. Quoting had to become a read that records nothing, because §6.5 wants the
      estimate *before* the confirmation and the only thing producing a plan was the POST that also creates the
      request.

      Delivery answers `202` with the class, the ETA and where to ask, rather than redirecting to a GET that
      S3 refuses with an XML document damrs never sees. The download mint reports `archived` rather than
      `ready`, above the ledger write, because nothing was distributed and a cold asset that consumed a
      download per click would exhaust a capped licence without delivering a byte.

      `archive`/`unarchive` joined the bulk vocabulary — `assets.status` has accepted `'archived'` since 0001
      and `status:archived` has been a live selector since Q.15, with nothing anywhere able to set it — and
      `restore` finally gave `restores::in_batch` a caller.

      **The bug worth remembering:** the archival check read the original's placement for *every* delivery, so
      the first thing on screen after archiving one asset was a badge saying Archived beside a blank square.
      The comment above the function said that could not happen. Same shape as the search-thumbnail bug: no
      test archived anything, so nothing caught it, and driving the real thing found both.

- [x] **A.3 The screens.** The detail panel quotes all three tiers before the button and becomes a status once
      a restore is running; `/storage` is the administrator's half, with every rule's dry-run state and a plan
      whose skips are grouped by reason and whose pins are named. Ten e2e cases, two of them axe.

- [x] **A.4 Proven against real AWS, which found three defects nothing local could.** The nightly job that
      was supposed to cover Glacier semantics had never run: it invoked `--features aws-conformance`, a feature
      that did not exist, and exited early every night because the credential secret was unset. Green forever,
      having executed nothing — and both skip messages in the shared conformance suite pointed at it as the
      thing that covered them. The feature and the `#[ignore]`d target now exist; against a real bucket the
      suite reports **20 passed, 0 skipped**, and the workflow's skip is a warning that names what did not run.

      Then the full stack against the same bucket, which found:

      1. **A deployment against real AWS was inexpressible.** `storage.endpoint` defaulted to the dev
         SeaweedFS, `S3Store::aws` is chosen only when it is `None`, and neither an environment variable nor a
         TOML file can put an `Option` back to `None`. Every reachable configuration produced *some* endpoint.
         The default is now `None` — production shape, failing in the honest direction — and an empty string
         reads as "no endpoint" for the case where something upstream already set one.
      2. **Pinned rows consumed the run cap.** A tenant with 136 pinned placements and
         `max_objects_per_run = 1` planned nothing, run after run: the scan fetched cap-plus-one rows and both
         were pinned. Two attempts to order the unmovable rows last each missed a case — collection pins,
         then cold-to-warm — because each was an incomplete reimplementation of the planner in SQL. The cap
         bounds what *moves*; a separate, much wider window bounds what is *read*. Conflating the two is what
         produced a policy that silently did nothing forever.
      3. **Credential refresh timed out at five seconds.** A worker up for twenty minutes failed every
         `CopyObject` with `ConnectorError { TimedOutError(5s) }` while the same copy took 0.58 s from the CLI
         and a restarted worker succeeded at once. The five was `DEFAULT_LOAD_TIMEOUT` in the SDK's identity
         cache — an SSO exchange or an IMDS call behind a refresh does not always fit in it. Now thirty
         seconds, stated rather than defaulted. This one would have hit any long-lived deployment an hour
         after deploy and blamed the network.

      One number worth keeping: our `expires_at` for a restored copy is seven days from availability, while
      AWS reported `expiry-date` a day later — it rounds to a day boundary. Ours is the conservative side, so
      delivery stops before the bytes do, which is the direction to be wrong in.

- [x] **A.5 What A.4 did not prove, and the second way the nightly lied.** Asked directly whether archival is
      "tested thoroughly on AWS", the answer was no, and the reasons were specific enough to fix.

      **The nightly has never run.** Not "ran and skipped" — `gh run list --workflow=nightly-aws.yml` returns
      an empty list, and `gh secret list` returns nothing, so there are no credentials for it either. A.4 fixed
      the *first* way this workflow lied (a feature flag that did not exist). It kept lying a second way: the
      credential check exited **0** with a `::warning::`, so the job would have reported success having run
      nothing, and a warning in a nightly nobody opens is indistinguishable from coverage. Missing credentials
      now **fail** the job, with an error naming the three secrets to configure and what is uncovered until
      they are. Forks never reach it — the job's `if` already excludes them, which is the case the warning was
      protecting. Also given `timeout-minutes: 40`, because the new case waits.

      **Nothing asserted a restore completing.** The shared suite stops at the ticket by design — Standard is
      three to five hours — so completion was covered only by `FakeS3Store`'s controllable clock, which is a
      fake agreeing with our own state machine. Whether *AWS* reports what we expect at the moment the copy
      appears had been observed once by hand and never asserted; the `expiry-date` note above is the trace of
      that observation.

      `a_glacier_restore_completes_and_serves_the_original_bytes` closes it, and the tier is what makes it
      possible: **Expedited against Glacier is one to five minutes**, where Standard is hours and Deep Archive
      has no Expedited tier at all. So the one restore this project can watch finish is a Glacier Expedited
      one. It polls to `Available` within a fifteen-minute budget, asserts the class is still `GLACIER` on
      *every* poll rather than once at the end — a class that changed would be a permanent move reported as a
      restore, available forever and then a 403 the day the copy expired — requires the expiry §6.5 makes a
      database constraint, and asserts the bytes come back unchanged. Cleanup is a `Drop` that blocks rather
      than spawning, because a task spawned from `Drop` is never awaited and the runtime shuts down with the
      test: cleanup that runs "usually" is cleanup that bills a 90-day Glacier minimum.

      Deep Archive completion stays unprovable in a test by construction — twelve hours minimum — and is now
      said so explicitly rather than implied by proximity to the cases that do run.

      **One gap I overstated when asked.** I said the min-duration billing traps were unverified. The
      arithmetic is unit-tested in `storage.rs` and the enforcement in `lifecycle.rs` and the fake; what no
      test anywhere can confirm is that AWS *bills* the way the table says. That is a documentation fact, not
      a coverage gap, and the distinction matters because the first framing invites building something to
      close a gap that is not there.

      **Closed on 2026-08-24: the suite ran against real S3.** `ap-south-1`, against a bucket created for
      the run and deleted after it — **20 cases passed, 0 skipped**, and
      `a_glacier_restore_completes_and_serves_the_original_bytes` completed a real `RestoreObject` in
      **76.7 seconds**, returning the original bytes. Zero skips is the assertion that matters as much as
      the passes: a skip here would mean the store reported itself incapable against a backend that is
      demonstrably capable, which is a capability-detection bug rather than a fact about AWS.

      So the archival claim is now backed by the storage a reader would actually use, rather than by
      SeaweedFS's wire protocol and a fake's controllable clock. `mise run check:aws` reproduces it.

      **What remains is the nightly, and it is credentials rather than code.** A scheduled run needs static
      IAM keys — an SSO session cannot be handed to CI — plus `DAMRS_TEST_BUCKET`, and creating an IAM user
      is not something to do in passing. Until those exist the workflow fails loudly by design, which on a
      public repository means a visible red run every night: either configure the three secrets or drop the
      `schedule:` trigger and keep `workflow_dispatch`.

**Not built, and deliberately.** A cross-pool move: S3 transitions are a self-copy, so a policy naming a
different target pool asks for a copy between buckets and halts as unsupported rather than tiering in place —
"moved, but not where you said" is worse than "did nothing, and said so". Eviction and replication likewise:
representable in the schema, planned, not performed. And `only_superseded`, which `object_placements` has no
version dimension to express.


## M4 — Local AI

### M4a — the model-free half (done)

- [x] **Perceptual hashing, near-duplicate detection and dominant colour.** `asset_phashes`, `asset_colors`
  and `duplicate_candidates` had been in migration 0003 since the start with nothing writing to them, and
  `image_hasher` had been a declared dependency used only by a test. All of this is pure computation over the
  master proxy — no ONNX runtime, no model files, no GPU — which is why it is the part of M4 that could be
  built and verified on a laptop.

      Two hashes, because they fail differently: the gradient hash survives brightness and contrast changes, the
      DCT hash survives a rescale. `distance` takes the closer of the two, excluding a collapsed one.

      **What running it over 162 real assets found.** 84 pairs, of which 33 were byte-identical — 0003 says
      exact duplicates are caught at ingest and this table is for *near* ones, so nearly half the queue was
      work nobody needed. And a 932-byte test pattern was paired with an MP4 at distance 0, because both had
      `dhash = 0` and the blind minimum let the useless hash decide. Queue is now 33, every survivor plausible.

      **My first fix for the second was wrong.** Gating on the hash's population count cannot work for a DCT
      hash: it is a median comparison, so about half the bits are set for a photograph and a blank page alike.
      Two unrelated flat colours measured 11 apart — inside the 12-bit threshold. The gate is on the *image*
      now: below a luma standard deviation of 6 no hash is stored at all, which excludes the asset from both
      directions with no column and no filter. Colours are still stored, which is what tells a grey square from
      a blue one.

      **And a test had been hiding it.** The rescale case passed only because `min()` was picking `dhash = 0`;
      it never exercised the DCT hash. Two subsequent rounds of "the hash is broken" were my own fixture
      regenerating a fixed pixel-frequency pattern at two sizes — two different pictures, not one rescaled,
      the same mistake twice. Truly resized, the DCT hash is fine.

      One limitation is recorded rather than papered over: a smooth ramp keeps its hash and is unstable across
      a rescale (22 bits), because all its energy sits in two DCT coefficients. Two rescaled gradients will not
      be found as duplicates. That is the algorithm, not a constant to tune.

      16 media cases, 12 db cases, 7 API cases, 8 browser cases. Also fixed a nav that had quietly started
      clipping: six sections added over one session took it to 1461px, so its last item was outside the
      viewport at 1024 and 1280 — the two commonest laptop widths.

### M4b — the model-dependent half (blocked on a decision)

- [ ] **Embeddings (SigLIP), OCR, transcription, face detect/identify, saliency.** Every one needs an ONNX
  model file: hundreds of megabytes to a couple of gigabytes, plus the ONNX Runtime shared library. That is a
  decision about *distribution* rather than a coding task, and it should be made deliberately:

      - **Where do model files come from?** Baked into the deployment image (large, versioned, no runtime
        download), fetched on first use (small image, a startup dependency on Hugging Face), or mounted
        (operator's problem, awkward for a hosted product).
      - **What does CI do?** The suites here run on real Postgres and real files by design. A model-dependent
        stage either downloads a gigabyte per run, ships fixtures of pre-computed vectors, or is skipped —
        and a skipped stage is one nobody notices breaking.
      - **What about the GPU?** §8.1 says ~$0 marginal cost, which is true on CPU for embeddings and not for
        Whisper on a large library.

      Worth noting what is *already* in place for it: `asset_image_embeddings` and `asset_text_embeddings`
      exist with HNSW indexes, `ai_models` and `taxonomy_terms.ai_threshold` exist, the zero-shot vocabulary
      query exists and is now governed (Q.20b), and `duplicate_candidates.cosine` is the column the embedding
      half would fill — which is also why `relation` currently only claims `near_identical` or `variant`.

## M6 — Workflow, proofing, annotations, analytics

Little of this was designed in ARCHITECTURE beyond the milestone row, so the design is being made here.

- [x] **M6a Annotations.** A comment with an anchor: a rectangle on the picture, a moment in a track, or both.
  The same row as a comment (migration 0037 adds five columns to `asset_comments`) rather than a table of its
  own, because a thread mixes them — "the logo is wrong" pinned to a corner and "approved" about the whole
  thing belong in one conversation, and a join per comment read would be the cost of separating them.

      **Coordinates are fractions, never pixels.** One asset renders as a thumbnail, a preview, a proxy and an
      original, so a mark stored in pixels lands correctly on exactly one of them. The refusal names the likely
      cause — "were these pixels rather than fractions?" — because that is the mistake an integrator makes.

      **What the tests found.** The letterbox guard failed the moment it was written: the lightbox uses
      `max-w-full max-h-full object-contain`, under which the element shrinks to the image, so there are no
      bands and every coordinate assertion would have passed either way. A second test forces a fixed-size box
      — and with letterboxing finally real, the drag came back clamped to the left edge, because the
      `ResizeObserver` watched the container rather than the image and the measurement was stale.

      **A gap, stated rather than hidden:** drawing needs a pointer. Existing regions are buttons so reading
      and focusing work from the keyboard, but a drag is a pointer gesture. `role="application"` would have
      silenced the linter while telling a screen reader to stop intercepting keys — worse for the users it
      appears to help. A keyboard path for *creating* an annotation is still missing, and is a design question
      (a control that annotates a named region without a drag) rather than an attribute.

      9 db cases, 9 browser cases including axe.

- [x] **M6b Proofing rounds.** `asset_comments.status` already carried `approved` and `changes_requested`, and
  0020's own comment said nothing enforced them — deliberately, because a status that gated publishing would be
  a rights decision. What was missing was the *round*: a named review with a set of assets, a list of reviewers,
  a due date, and a verdict per reviewer rather than per comment.

  Migration 0038 adds three tables — `proof_rounds`, `proof_round_assets` (the snapshot) and
  `proof_round_reviewers`. Four things are worth recording about the shape:

  **The outcome is derived, never stored.** Only `closed_at` and `cancelled_at` are columns;
  `proofing::decide_outcome` works out `open` / `approved` / `changes_requested` / `cancelled` from the verdicts
  every time it is read, and `changes_requested` wins over any number of approvals. A stored status column is a
  second source of truth that can disagree with the verdicts underneath it, which is the failure mode people
  expect from review tools — three approvals and one request for changes showing as approved.

  **The asset set is snapshotted and cannot be widened.** A reviewer who approved eleven pictures did not
  approve a twelfth added afterwards. A second pass is a new round with `supersedes` set, which is also what
  makes "approved" distinguishable from "approved eventually".

  **Giving a verdict needs only `Read`.** A reviewer is somebody asked to look at pictures; requiring `Manage`
  to answer would mean only administrators could ever be asked to review anything. The round's own reviewer
  list is the authorisation, and the assets still have to be visible. Opening and cancelling need `Manage`,
  because both are the requester's act.

  **A partly visible round is not visible at all.** `read` refuses a round whose assets the caller cannot *all*
  see, with the same 404 it gives for a round that does not exist — distinguishing them would confirm the round
  exists. That whole-round rule is what makes `/proofing/{id}/assets` safe to draw entire: a review screen
  showing two of eleven pictures would be asking somebody to approve a set they never saw.

  Six endpoints, 12 db cases, 12 API cases and 13 browser cases. It gates nothing: approving a round publishes
  nothing and an unapproved asset is not blocked, because whether an asset may be published is a rights
  question and answering it here would put a collaboration table in the delivery path.

  Two things the browser suite caught that the Rust suites could not. The grid's live region announced a
  one-asset library as **"1 assets"** — a string only a screen-reader user ever hears, which is exactly why it
  had survived. And in the e2e harness itself, `'/assets'.endsWith('/assets')` is true, so the round-assets
  mock swallowed the grid's own listing; a bad `/search/facets` shape then left the page stuck on "Searching…"
  with no error anywhere, which is worth remembering as a symptom.

- [x] **M6c Analytics rollups and Insights exports.** `events` was already partitioned by month and
  `dam_global.tenant_usage_daily` existed since migration 0001. What was missing was the rollup job, the read
  surface — and, it turned out, a writer for the number the dashboard had been showing all along.

  **Two surfaces with opposite scoping rules, and that is the whole design.** `dam_db::insights` is the
  customer's view and every query in it runs through the caller's predicate, because §7 says a count is a
  disclosure. `dam_db::metering` is the operator's view and runs through nothing, because a bill narrowed to
  what one reader can see is not a bill. Nothing tenant-facing reads the second; serving an Insights screen
  from `tenant_usage_daily` would hand a scoped curator the library-wide totals in one field.

  The consequence of scoping is stated on the screen rather than left to be found: two people see different
  charts, there is no library-wide total anywhere, and the contributors list says out loud that it is *not* a
  performance measure — Ada's upload count is of the ones you can see, so it changes with the reader.

  **The seventh instance of the recurring pattern, and the most consequential.** `Kind::Downloaded` existed in
  `dam_db::events` and nothing wrote it. 0005's own comment said `rights_usage` was populated from "download
  events (0001 events)"; the ledger half was written and the events half never was. So
  `downloads_this_week` on the dashboard had been structurally zero since the day it shipped — verified on the
  dev library: eleven real downloads in `rights_usage`, zero download events, dashboard reporting 0. The
  download endpoint now writes both, and the number moved to 1 the moment I took one.

  Two tables, deliberately, because neither answers the other's question: `rights_usage` is licence
  consumption attributed to a scope and enforced against `max_downloads` (so its write may fail the request);
  `events` is what happened and who did it (so its write is logged and never fatal). Insights reads the ledger
  for downloads, because a share-link download is a real download and `events.actor_id` is an identity — a
  share token cannot go in it.

  **A level is not a flow.** `downloads`, `restores` and the token counters are things that happened between
  one midnight and the next, and the rows carry timestamps. `asset_count` and `bytes_by_pool` are how much is
  *stored*, which `object_placements` only knows as of now. So `metering::measure` refuses a day older than
  yesterday rather than recording today's storage against last March, which would draw a flat cost curve out
  of one number repeated. The cost is real and accepted: a fresh deployment meters from the day the worker
  first runs, and an operator wanting an older day's AI spend reads `enrichment_runs` directly.

  The rollup is a job chain like the tier sweep, started at worker boot rather than at provision — a tenant
  created before this existed has to start being metered too, and a hole in a billing series is
  indistinguishable from a worker that was down. Verified live: two tenants × two days, including an empty
  tenant that correctly meters as rows of zeroes rather than as no rows.

  **What driving the real page caught that the mocked tests could not.** Three things, and none of them were
  visible in a fixture:

  - `never_downloaded` came back **20 of 180** on a 183-asset library. The screen presented twenty rows as the
    answer, which reads as "we have twenty unused assets" — the difference between a tidy-up and a storage
    problem. A most-downloaded top-20 explains its own cap; a list of things nobody uses does not, so the
    endpoint now returns the total and the screen says "showing the 20 oldest of 180".
  - Contributors read **"ada@example.com ada@example.com"** on every row, because `display_name` falls back to
    the email when nobody set one — which is most people in a fresh tenant.
  - The stacked chart was **unreadable**: one day with 160 uploads made every other series a hairline, so the
    downloads that actually varied (2, 3, 6, 1) were invisible. Any library that has ever done a bulk import
    would look like that forever. Replaced with one labelled row per kind, each scaled to its own peak, with
    the peak stated — which is what keeps five different scales honest.

  Six reports export as CSV through the same functions the screen calls, because a second query written for
  the export is how a file comes to disagree with the page that offered it. 15 db cases, 12 API cases, 3
  pipeline cases, 13 browser cases.

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
