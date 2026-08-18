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
  *Remaining in 0.10:* the Tantivy rendering and the differential test asserting both back ends return
  identical sets. That test cannot exist until the second back end does; it lands with 2.6.
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
- [ ] **2.5 Shorthand search syntax.** `bra:acme`, quoted phrases, ranges, negation. *Test first:* an
  unclosed quote is a parse error with a column, not a silent whole-string match.
- [ ] **2.6 Tantivy index per tenant.** Schema derived from `field_defs`, an LRU writer pool (§19), and
  a cold-open path. *Test first:* 1,000 tenants do not open 1,000 indexes.
- [ ] **2.7 Faceted search.** Fast fields, counts that respect the access predicate.
- [ ] **2.8 Rights model (G4).** Licences, scopes, releases, and the distribution chokepoint that D12
  requires — enforced, not recorded.
- [ ] **2.9 Search eval harness (G8).** `relevance_judgements` → nDCG/MRR over a fixture corpus, wired
  so a ranking change reports its effect instead of being argued about.
- [ ] **2.10 Bulk operations (G18).** `bulk_operations`, dry-run first, per-item outcomes, resumable.

## M3 — Delivery, sharing, restore

Scope from §13: signed transform delivery, embeds, CDN, video + HLS, share links, restore flow with
cost guards, notifications/Paths (G9), saved searches (G15).

- [ ] **3.1 Signed transform URLs.** The one chokepoint every download passes through, so rights and
  ABAC are enforced by the delivery design rather than by a caller remembering (D12).
- [ ] **3.2 Derivative delivery + cache.** `op_hash` keyed, with the profile and intent in the key.
- [ ] **3.3 Share links.** Passcode, expiry, download limits, revocation — and revocation that takes
  effect on an already-issued URL.
- [ ] **3.4 Restore flow (§6.5).** `202` with an ETA and a cost estimate, batching sibling requests, and
  the expiry sweep. ffmpeg is mise-installable, so video lands here.
- [ ] **3.5 Video and HLS.** ffmpeg in the subprocess sandbox, loudness normalisation, the 720p H.264
  master proxy §2 specifies.
- [ ] **3.6 Notifications and Paths (G9).** `paths`, `path_firings`, and delivery that is idempotent
  under retry.
- [ ] **3.7 Saved searches (G15).** Stored query IR, re-evaluated against current access rather than the
  access at save time.

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
