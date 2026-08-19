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

## 2026-08-17 — task 0.3: telemetry, and a span convention that was decorative until tested

**`dam-telemetry` is a new crate (13th member), not part of `dam-core`.**
`tracing-subscriber` and `opentelemetry-otlp` are concrete infrastructure, and
`dam-core` is deliberately free of that. All three binaries depend on it so
initialisation cannot drift between them — which was the actual motivation:
duplicating subscriber setup three ways guarantees they diverge. Reversible: yes.

**The test caught a real defect in the field convention.**
`ids_propagate_into_a_child_span` failed against the first implementation. The JSON
layer was configured `with_current_span(true).with_span_list(false)`, which I chose
to keep log lines quieter. `with_current_span` emits only the **innermost** span's
fields — so an event inside `derive_thumbnail` nested under `request` carried no
`tenant_id` at all. The convention "tenant_id on every span" was decorative:
top-level requests had it, and everything the worker actually does happens in child
spans.

Fixed by enabling `with_span_list(true)` in both the `subscriber()` and `init()`
paths. Cost is a more verbose line; the alternative is traces that cannot be tied
to a tenant, which is worthless when a customer reports a problem. Reversible: no —
do not turn this off to reduce log volume without replacing it with something that
propagates ancestor fields.

**OTLP over HTTP/protobuf rather than gRPC.** `opentelemetry-otlp` gates transports
behind features; `with_tonic()` needs `grpc-tonic`, which pulls a whole gRPC stack.
Chose `http-proto` + `reqwest-client`: reuses the reqwest dependency already present
for the Anthropic client, and every collector accepts OTLP/HTTP on 4318. Reversible:
yes, a feature swap.

**opentelemetry 0.32 API drift.** `global::shutdown_tracer_provider()` no longer
exists — the holder must own the provider and call `provider.shutdown()`. `Guard`
now owns it. Losing this is silent: the process exits cleanly and the final spans
simply never arrive, which is the worst kind of observability bug.

**Invalid `RUST_LOG` falls back to `info` rather than failing startup.** A typo in a
filter directive should not stop a service booting; losing precision beats losing
the service. Asserted by `an_invalid_filter_falls_back_to_info_rather_than_failing`.

**`CaptureWriter` ships in the library, not in a test helper.** The redaction suite
has to exercise the same subscriber construction production uses — a capture writer
that only exists in tests invites a subscriber that only exists in tests, which is
the same mistake 0.2 corrected in the config loader.

## 2026-08-17 — tasks 0.4/0.5/0.6: the pool `search_path` trap, caught by the gate suite

**The compliance-gate suite's first run failed 10 of 16 — and three of the six
"passes" were false.** Root cause: `tenant_db()` did
`pool.execute("SET search_path TO t_acme, ...")`. `SET` without `LOCAL` applies to
**one pooled connection**, and the pool hands out others freely, so the first query
landed in the right schema and later ones silently did not.

This is precisely the hazard ARCHITECTURE §5.2 documents for production. It showed
up here first, in the test harness, which is the cheapest possible place to learn it.

The dangerous half was the false passes. `refused()` was `.is_err()`, so a statement
failing with *"relation does not exist"* counted as "the constraint refused it".
Three gates looked enforced while proving nothing at all. Replaced with
`refused_by_constraint()`, which asserts SQLSTATE class 23 (integrity constraint
violation) or P0001 (our `RAISE EXCEPTION` in the consent trigger) and **panics
loudly on class 42** (undefined table/column). A gate test that can pass for the
wrong reason is worse than no test.

Fix on the harness side: `PostgresHarness::pool_for_schema()` sets `search_path` via
`PgConnectOptions::options` at **connect time**, so every connection in the pool
carries it. Production's request path will use `SET LOCAL` inside a transaction
(`TenantConn`, 0.7); this is the connect-time equivalent for tests and the migrator.

Reversible: no. Do not reintroduce `.is_err()` as a refusal assertion, and do not
set `search_path` on a pool with bare `SET`.

### Smaller findings

**sqlx 0.9 requires dynamic SQL to be explicitly asserted safe.** `sqlx::query()`
now takes `impl SqlSafeStr`, which `&'static str` implements and `String` does not;
dynamic SQL must be wrapped in `sqlx::AssertSqlSafe`. Genuinely good ergonomics for
this codebase, where schema names are interpolated into DDL — the wrapper is a
grep-able marker at every site that needs auditing, and the schema name is validated
against `^t_[a-z][a-z0-9_]{1,38}$` before it gets there.

**The index count is 207, not the 206 measured during design.** The extra one is
`_sqlx_migrations`' own primary key; the design-time count came from raw psql with no
ledger. The assertion now excludes the ledger's indexes so the number still counts
migrations' own output.

**Migration count assertions.** `the_embedded_migration_counts_match_the_files_on_disk`
asserts 2 global and 8 tenant. The macro embeds at compile time, so a mismatch means
the binary and the repository disagree — a stale build, or a migration added but not
committed.

**`PgSslMode::Prefer`, not `Require`.** Loopback testcontainers and the dev stack
speak plain TCP; deployed Postgres should be behind TLS. A caller that must mandate
it puts `sslmode=require` in the URL, which wins. Reversible: yes.

## 2026-08-17 — task 0.7: `TenantConn`, and a test that was wrong about its own regex

**`TenantConn` has exactly one constructor and it begins a transaction.** No
`from_pool`, no `set_schema`, no escape hatch. `SET LOCAL` outside a transaction is a
silent no-op, so a `TenantConn` that could exist outside one would be a cross-tenant
read with no error attached. The gate suite already showed how easy that mistake is;
this makes it unavailable rather than discouraged. Reversible: no — adding a
non-transactional constructor would remove the only guarantee the type provides.

Cost: every tenant-scoped read runs in a transaction, single-statement ones included.
In Postgres that is close to free, and it buys an invariant nobody can forget under
deadline.

**`begin()` checks the schema exists before setting the path.** A missing schema is
silently ignored in a `search_path`, so setting it first would hide an unprovisioned
tenant behind a series of confusing "relation does not exist" errors, one per query.
New `Error::TenantNotProvisioned` because "not provisioned" is a 404 or a
provisioning bug while "query failed" is an incident.

**`TenantSlug` in dam-core is the only way to obtain a schema name.** The slug reaches
DDL and `SET LOCAL`, neither of which takes bind parameters, so validation cannot be
deferred to the query layer. Deserialisation goes through the same constructor, so a
slug arriving from JSON is validated rather than trusted.

### The test was wrong, not the code

`the_length_limit_matches_the_database_check_constraint` asserted a single-character
slug was valid, **citing the very regex that forbids it**: `^[a-z][a-z0-9_]{1,38}$`
is one leading letter *plus one to thirty-eight more*, so the floor is two characters.
Misreading a quantifier as covering the whole pattern is an easy error, and it was
sitting inside a test whose entire purpose was to pin the constraint.

That is a warning about the class of test that asserts agreement between two layers
by restating one of them from memory. Replaced with
`the_rust_validator_and_the_database_check_agree`, which feeds sixteen inputs to
`TenantSlug::new` **and** to Postgres's own `~ '^[a-z][a-z0-9_]{1,38}$'`, and fails
on any disagreement. Reserved names are the one permitted asymmetry — Rust refuses
`extensions` and `public` although the regex accepts them, because a tenant schema
shadowing either would break every qualified type reference in the tenant migrations.

Reversible: no. Do not replace the cross-check with a restated regex.

**`executor()` not `as_mut()`.** Clippy flagged the shadowing of
`std::convert::AsMut::as_mut`; the new name also reads better at the call site.
Reversible: yes.

## 2026-08-17 — task 0.8: provisioning order chosen for recoverable partial failure

Provisioning spans four things that cannot be one transaction: a control-plane row, a
schema, the migrator (its own connections), and seed data.

**Order: schema → migrations → seed → tenant row.** A schema with no tenant row is
inert — nothing looks for it, and a re-run adopts it. The reverse is actively harmful:
a tenant row pointing at a missing schema means every request for that tenant fails
deep in the stack instead of at lookup, and the tenant shows up in listings as though
it worked. Asserted by `a_failed_provisioning_does_not_leave_a_half_built_tenant`.

**Idempotent rather than transactional.** Every step is `ON CONFLICT DO NOTHING` or an
existence check, so a crashed CLI or re-run CI job adopts what exists. That is cheaper
than a rollback path which itself has to be correct under partial failure — the case
you can least afford to get wrong is the one that runs least often.

**The tenant row and its feature flags commit together**, so a tenant can never exist
without its DPIA-gated flags. `requires_dpia` is a property of the *feature*, set by
the seed rather than left to an operator; combined with the CHECK on `feature_flags`
that makes `face_identify` unenableable without a DPIA reference and a legal basis even
with direct database access. Verified in the live dev stack:
`face_identify enabled=false requires_dpia=true`. Reversible: no (D14).

**Seed data is deliberately minimal** — five metadata fields, three roles, one asset
group. A long opinionated default is something the customer then has to delete.
`alt_text` is included because accessibility (D10) needs a field for AI-generated alt
text to land in with provenance, and a tenant should not have to invent it.

**`admin` gets `all_asset_groups = true`, not an enumerated list**, so a group created
later does not silently fall outside the administrator's reach.

**`damctl migrate --all` records per-tenant failure rather than aborting.** A tenant
whose migration fails is marked `status = 'migration_failed'` and the run continues;
the command exits non-zero at the end with a count. One tenant's bad state must not
block the fleet's upgrade (§5.3). Reversible: yes.

Driven end to end against the dev stack: `migrate`, `provision-tenant` (twice —
returned the same id), a second tenant, `migrate --all` across both, and an
injection-shaped slug refused with the constraint message.

## 2026-08-17 — task 0.9: the fairness cap starved the worker, not the greedy tenant

**The most useful failure of the night.** My first design made `per_tenant` a mandatory
cap of 4 alongside `limit = 10`. The fairness test then showed a worker claiming
**5 jobs out of a requested 10 while 200 sat queued** — because the cap bound before
the limit did. That does not throttle the flooding tenant; it idles capacity while the
backlog grows. A queue that refuses to hand out available work is worse than an unfair
one.

**Fairness comes from rank ordering, not from a cap.** `row_number() OVER (PARTITION BY
tenant_id ORDER BY priority, run_after, id)`, and the batch is taken in rank order —
every tenant's next job before any tenant's job after that. That gives both properties
at once:

- one tenant alone fills the batch (10 of 10), so fairness costs no throughput;
- a quiet tenant's single job is rank 1 and lands in the first batch even behind 200;
- three tenants and `limit = 9` gives exactly 3 each.

`per_tenant` survives as an optional safety valve for a pathological tenant, defaulting
to `None`. Reversible: yes, but do not make it mandatory again.

**No `SKIP LOCKED`, and that is not a compromise.** Postgres rejects `FOR UPDATE` in
any query containing a window function, so the fair ranking rules it out. Correctness
comes from the `UPDATE`'s own `WHERE j.state = 'queued'`: under `READ COMMITTED` an
`UPDATE` re-evaluates its predicate after taking the row lock, so the second worker to
reach a row sees `'running'`, fails the predicate, and is excluded from `RETURNING`. Ten
workers over fifty jobs claim each exactly once
(`concurrent_workers_never_claim_the_same_job`). The loser gets a smaller batch and
asks again. Reversible: no — reintroducing `SKIP LOCKED` means giving up fairness.

**`reclaim_expired` promotes an out-of-attempts job to `dead`, not back to `queued`.**
A job that reliably kills its worker would otherwise be reclaimed forever, taking a
worker down each time. Reversible: no.

**`heartbeat` checks `locked_by`.** Without it a worker whose lease was already
reclaimed could keep renewing a job another worker now owns, and the two would race
with no error anywhere. New `Error::LeaseLost` so the caller knows to stop work rather
than retry.

**Backoff is capped at one hour.** The cap matters more than the curve: uncapped
doubling reaches days, by which point a transient outage has effectively lost the work.

### Two test bugs, both mine

- `queue_db(&["a", "b", "c"])` — single-character slugs, which `TenantSlug` correctly
  rejects (minimum is two). My own validator from 0.7 caught my test from 0.9.
- `EXTRACT(EPOCH FROM interval)` returns `NUMERIC` in modern Postgres, not `FLOAT8`.
  Needed an explicit `::double precision`.

## 2026-08-17 — task 0.11: CI, and cargo-deny found a real vulnerable TLS stack

The deny job earned its place on its first run: **three `rustls-webpki` advisories**
reachable from every binary through the S3 client.

`aws-config`'s default features activate `aws-smithy-http-client`'s `hyper-014`, which
pulls `legacy-rustls-ring` → rustls 0.21.12 + hyper-rustls 0.24.2 — carrying
CVEs for URI name constraints, wildcard name constraints, and a reachable panic in CRL
parsing. The modern `rustls-aws-lc` client was *also* active, so the vulnerable stack was
compiled and linked while never being the path we use.

Fixed by `default-features = false` on `aws-config` and `aws-sdk-s3` with an explicit
feature list. rustls 0.21 is gone from the graph entirely (`cargo tree -i rustls@0.21.12`
now finds no package) and the workspace still builds and tests clean. Reversible: no —
restoring default features re-adds a known-vulnerable TLS implementation to the binary.

Worth noting *how* this was found: not by reading changelogs, but by wiring the check
into CI and running it once. It would have shipped otherwise.

### Three smaller fixes, all things CI would have failed on

- **`command: check advisories bans licences sources`** — British spelling. Not a valid
  cargo-deny check name; the job would have failed on its first real run.
- **`wildcards = "deny"` flagged our own path dependencies.** `allow-wildcard-paths`
  exists for this but does not apply to *publishable* crates. Set `publish = false`
  across the workspace, which is accurate (these are internal) and also prevents an
  accidental `cargo publish`.
- **NCSA scoped to one crate rather than allowed globally.** `libfuzzer-sys` reaches the
  lockfile via an unenabled `rav1e` fuzzing feature and is never compiled —
  `cargo tree -i libfuzzer-sys` finds no path. A `[[licenses.exceptions]]` entry means
  that if it ever enters a real build the check fires again, instead of having been
  waved through years earlier by a blanket allow.

**`paste` (RUSTSEC-2024-0436) is ignored with a dated rationale.** A proc-macro that
expands identifiers at compile time: no runtime code, no network, no unsafe surface, so
"unmaintained" here means "finished". Reached through the metrics and symphonia trees.
The ignore list carries dates deliberately — a global severity threshold ("we accept
mediums") ages into "we accept anything", while a dated list forces a re-read.

### CI shape

Four parallel jobs: `lint` (fmt + clippy, cheapest first so a formatting slip does not
queue behind a container build), `test`, `deny`, and `schema` — which applies the
migrations to a real Postgres and asserts the object counts, so a migration that
silently drops a constraint fails there rather than in production.

**No `services:` block.** Every database test starts its own container through
testcontainers (D17), so suites stay parallel and no test depends on a shared fixture's
state.

A separate nightly workflow runs the S3 conformance suite against real AWS — the only
place actual Glacier semantics are exercised. It **skips rather than fails** when
credentials are absent: a red nightly nobody can fix trains people to ignore the
nightly.

**`.sqlx/` offline data is not committed yet**, because nothing uses the `query!` macros
— every query so far is runtime-checked `sqlx::query`. The `.mise.toml` `SQLX_OFFLINE`
setting is there for when that changes. Claiming the CI verifies offline data it does not
have would be worse than saying so.

## 2026-08-17 — tasks 1.1/1.3: BlobStore, the conformance suite, and FakeS3Store

**`GetOutcome` is an enum, not an error.** A cold read is a normal, expected outcome —
"this master is in Deep Archive" is what the product is *for*, not an exception. Making
it a variant means the compiler asks about it at every call site, which is how the
Drupal connector's "resolve a cold original to the proxy" behaviour becomes hard to
forget rather than easy. `into_bytes()` exists for the paths that genuinely cannot handle
cold — the enrichment pipeline reading a proxy, which is always hot. Reversible: no.

**Capabilities are declared and the declaration is checked.** The conformance suite skips
what a driver does not claim and asserts everything it does. Skips land in a `Report`
that the caller prints, because a silent skip is how a capability gap becomes a
production surprise — the suite stays green while covering less each release.

The skip message for storage classes says it explicitly: *echoing the header back
without changing behaviour does NOT count*. That is exactly what SeaweedFS does, and a
driver that claimed the capability on that basis would pass a test proving nothing.

**The fake does not claim versioning or object lock.** Object lock's whole point is that
the *server* refuses the delete, so a fake that refuses proves nothing. Those run against
SeaweedFS and the AWS nightly (§20.3). Under-claiming costs coverage honestly;
over-claiming would fake it.

**Restore state is derived from the clock, never stored.** `Ongoing → Available →
Expired` is computed on read, which makes it *impossible* for the fake to report an
available restore whose copy has actually lapsed. Storing the state would allow exactly
the bug that conflating restore state with storage class produces: an object that reads
as available forever and 403s the day the temporary copy expires.

**The keep-warm window runs from availability, not from the request.** A 48-hour Bulk
restore kept for 24 hours would otherwise expire before it arrived, and the caller would
never receive bytes they had paid for. Pinned by
`the_keep_warm_window_runs_from_availability_not_from_the_request`.

**The expiry boundary is exclusive.** At `expires_at` the copy is already gone, matching
S3's `expiry-date`. My test originally advanced to exactly the boundary and called it
"just under" — the implementation was right, the arithmetic was mine. Now asserted at
t+59 (available) and t+60 (gone), because an inclusive boundary would serve bytes for one
request more than AWS does and the difference shows up only as an intermittent
production 403. Reversible: no.

**Tier exemption is derived from the key, not looked up.** `Key::is_tier_exempt()` reads
the `p/`, `t/`, and `c2pa/` prefixes directly, so the rule holds even for an object whose
placement row is missing or stale. `permitted_class()` then clamps a requested class,
which means the lifecycle engine cannot tier the master proxy even if a policy predicate
says to. Reversible: no — this is the §2 invariant in code.

**Keys reject `..`, empty segments, and uppercase digests.** Keys are ours, not a
caller's, so validation is about catching our own construction bugs. An uppercase digest
matters specifically: it would produce a second key for identical content, defeating the
deduplication the content-addressed layout exists for.

**`conformance.rs` ships in the library with panic lints relaxed.** A `#[cfg(test)]`
suite cannot be run against a driver from another crate, and the AWS nightly needs it.
The allowance is scoped to that module alone.

**`TestClock` starts at a fixed instant, not `Utc::now()`,** so a failure reproduces with
the same timestamps tomorrow. It never advances on its own — a test that forgets to
advance sees a stopped clock and fails loudly rather than passing intermittently.

**D19: the SeaweedFS tag is 4.42, and the tag is load-bearing.** 3.80 answers
`PutBucketVersioning` with `501 NotImplemented` — versioning and object lock are recent
S3-gateway additions. This mattered more than a version bump normally does, because
without versioning a version-scoped delete fails with `AccessDenied`, which is
indistinguishable from a legal hold working. An earlier note in this file claimed the hold
was verified live on that basis; it was not — the request was refused for the wrong reason.
The same tag is now pinned in the harness and in `docker/compose.dev.yml`, because `latest`
in dev against a pin in tests is how a capability difference becomes a bug that only
reproduces on one machine. Reversible: yes, by moving the pin — but never downward past
4.3x.

**The test container runs with an identity config, not anonymously.** An unconfigured
SeaweedFS allows everything, which is simpler and would have made
`x-amz-bypass-governance-retention` fail permanently: SeaweedFS only honours the header for
an identity holding `BypassGovernanceRetention` or `Admin`, and an anonymous request holds
neither. Two identities are declared — one that may bypass and one that may not — so the
suite proves both directions. Without the second identity, "bypass refused" would prove
nothing about the permission, since it is also what an unimplemented bypass looks like.
Reversible: yes.

**Object lock lives beside `BlobStore`, not in it.** `set_legal_hold`, `set_retention`, and
the versioning calls are `S3Store` methods rather than trait methods, because a trait
method forces every driver to answer — and `FakeS3Store` cannot answer honestly, since the
point of a hold is that the *server* refuses. `RetentionMode` has no `Default` and no
`FromStr`: GOVERNANCE is correctable and COMPLIANCE is not, so a caller must say which it
means. `Bypass` is an enum rather than a `bool` for the same reason — a bare `true` at a
delete that overrides a retention policy is the argument that gets flipped by an
autocomplete. Reversible: yes.

**A multipart `Placement` reports no whole-object checksum.** A multipart ETag is a digest
of the part digests with a `-<count>` suffix, so it is not the digest of the object.
Returning it as `checksum` would have the integrity scrub compare two values that were
never meant to match, and the failure would look like corruption. `checksum: None` forces
the caller to use the streaming BLAKE3 (§6.4). Reversible: no.

**The 5 MiB part minimum is enforced at `upload_part`, not at completion.** S3 reports
`EntityTooSmall` from `CompleteMultipartUpload`, after every byte has crossed the wire. For
a 200 GB master that is an expensive way to learn the part sizing was wrong, so an
undersized part is refused on the *next* call — the moment it stops being allowed to be the
final one. Reversible: yes.

**`.mise.toml` exported four `DAMRS_S3_*` variables that match no config field.** The
config is nested and denies unknown fields, so `DAMRS_S3_BUCKET` was not a loose alias for
`storage.bucket` — it was a hard startup failure. `damd` could not have started in the dev
environment. Renamed to `DAMRS_STORAGE__*`. The strictness stays: a typo in a deployed env
var must fail loudly rather than silently fall back to a default. Reversible: n/a, a bug fix.

**Read resolution is lexicographic, not a cost minimum.** Readable-now first, then price,
then pool name. "Cheapest available placement wins" (TASKS 1.4) is only correct if
"available" is evaluated first: Deep Archive is the cheapest place in the estate to keep
bytes and its per-GB retrieval charge can be *lower* than Glacier IR's, so a resolver
ranking on price alone would route ordinary downloads into 12-hour restores while appearing
to save money. The name tiebreak exists so two identical requests resolve identically — an
unstable tiebreak makes a cache key and an audit log disagree. Reversible: yes.

**A disabled pool is still readable.** `storage_pools.enabled = false` retires a pool from
*new* placements only. Blocking reads as well would take every object already living there
offline, which is a data outage dressed as a configuration change. Retiring a pool's data
is a migration, not a flag. Reversible: yes.

**Write placement is decided by pool minimums, not by class name.** A thumbnail must not
land in Glacier IR — not because of the label but because of the 128 KiB minimum billable
size and the 90-day minimum duration, which together make a 20 KB thumbnail cost more there
than in Standard. `suits_small_permanent_objects()` tests the minimums directly, so a future
cheap class without minimums qualifies automatically and a name-based check does not have to
be revisited. Reversible: yes.

**`Rate` is an integer at 1e-12, four digits finer than the database.** Cost comparisons must
be exact so ties break deterministically. The scale is finer than `numeric(12,8)` because at
the database's own scale a single S3 GET (4e-7 currency units) divides to zero: request costs
silently vanished from every estimate, and two pools differing only in request price compared
equal. Conversion from the database is an exact ×10,000. Reversible: yes, but not downward.

**`PlacementState` lives in `dam-core`.** The vocabulary is shared between the layer that
loads placements and the layer that acts on them; a second definition is how the two drift
out of step with the `object_placements.state` CHECK. It is an enum rather than a boolean
because each non-`Present` state implies a different operator action — `Uploading` resolves
itself, `Missing` needs re-replication, `Corrupt` needs a scrub. Reversible: yes.

**Deduplication compares size, not just presence.** Under content addressing a key implies
its bytes, so an object of the wrong size at its own content-addressed key is corruption or a
truncated earlier upload. A presence-only check would read that as a cache hit and make the
corruption permanent — every subsequent upload of the correct bytes would skip the write too.
Verifying the full digest would be stronger, but it costs a download of the whole object on
every duplicate upload; size catches the realistic failure for one `HeadObject`. Reversible:
yes, by adding an optional deep-verify mode.

**An empty body is refused at the ingest boundary, not in the hasher.** BLAKE3 of nothing is
a perfectly valid digest, so content addressing alone would store a zero-byte asset and hand
it a key. The refusal has to sit where the upload is accepted. Reversible: yes.

**`Digest` normalises case instead of validating it.** An uppercase digest would produce a
second key for identical content, defeating deduplication. Making it unrepresentable beats
asking every call site to remember — `Key`'s own lowercase check stays as a second line.
Reversible: no.

**Promotion deletes staging only after the content object exists.** Until the copy succeeds,
the staging object is the only copy of the bytes. Deleting it first — or on a failed
promotion — destroys an upload that could have been retried or inspected. So a failed
promotion leaves the staged bytes in place and a timed reaper cleans up abandoned uploads: a
leak that costs storage, rather than a loss that costs the upload. Reversible: yes.

**`copy` is a `BlobStore` method, not an `S3Store` one.** A server-side copy is a data-plane
primitive both drivers can implement honestly, unlike object lock — and putting it on the
trait means the shared conformance suite covers it. It was otherwise the only trait method
the suite did not exercise, which is precisely how `FakeS3Store` would drift. The suite
asserts a copy leaves its source intact, because a driver that moved instead of copying would
destroy the only remaining bytes on a partial failure. The 5 GiB multipart threshold lives in
the driver, since it is S3's limit rather than every caller's problem. Reversible: yes.

**An upload id is ours and validated like a digest.** `Key::staging` refuses anything but
1–64 alphanumeric, `-` or `_` characters. Object keys are generated, never client-supplied, so
this is the one place a caller could otherwise steer a write outside its own tenant prefix.
Reversible: no.

**Sniffing never falls back to the client's declaration, and never consults the filename.** A
declaration that disagrees with the bytes is recorded in `declared_mismatch` and discarded; one
that cannot be corroborated is still discarded. Any fallback would make every check bypassable
by sending bytes we do not recognise, and a filename extension is attacker-controlled in exactly
the same way a `Content-Type` is. The mismatch is kept rather than dropped because it is the
only evidence that an attempt was deliberate. Reversible: no.

**Two separate hazard flags, because they imply different actions.** `is_dangerous()` marks
executables — never a creative asset, refused by default. `carries_active_content()` marks SVG
and HTML — legitimate assets (HTML5 creatives, icon libraries) that must never be served inline
unsanitised, because they execute with the privileges of whatever origin serves them. Collapsing
the two would either refuse real assets or serve an XSS payload. An SVG is classed `Image` so it
keeps its thumbnail pipeline; the delivery constraint rides alongside. Reversible: yes.

**An unrecognised format is stored, not refused.** A DAM is a store first: refusing would lose
the customer's file over our inability to preview it. It is `application/octet-stream` with
class `Unknown` and `is_processable() == false`. Reversible: yes.

**The resumable-upload tail lives in object storage, not in the process.** A TUS chunk may be
far below S3's 5 MiB part minimum, so chunks accumulate before becoming a part. Keeping that
remainder in memory would make an upload sticky to one node and lose it when the node restarts —
so it is a small object alongside the staging key, at the cost of one extra small read and write
per sub-minimum chunk. For an upload measured in minutes that is the right trade. Reversible: yes,
but only by giving up multi-node resumption.

**`ResumableSession` is a value the caller persists; the engine keeps no map of live uploads.**
Any node can serve any PATCH and a restart loses nothing. Reversible: yes.

**Multipart primitives go on a separate `ResumableStore` trait, not on `BlobStore`.** `BlobStore`
documents multipart as sitting *above* it, and `MultipartUpload` owns its part list and borrows
the store — right for a single pass, wrong for an upload spanning many requests and processes.
Widening `BlobStore` would have contradicted its own stated design. Reversible: yes.

**An offset conflict is an outcome, not an error.** It is the normal way a client whose connection
dropped mid-chunk finds out where to resume, and TUS answers it with a 409 carrying the
authoritative offset. It also covers the replay case: a client retrying a chunk whose response was
lost sends it at the *previous* offset, and accepting that would silently duplicate the bytes and
produce an object whose digest matches nothing the client can compute. Reversible: yes.

**A completion short of the declared length fails but leaves the session Active.** The client may
still send the rest; marking it failed would turn a slow upload into a lost one. Reversible: yes.

**Upload validation happens at finalisation, never at mint.** A presigned `PUT` hands the client
a URL and steps aside: S3 will not cap the size, will not constrain the type, and does not report
what arrived. So the checks that matter run after the bytes land at a staging key and before
promotion to a content-addressed key, because promotion is what makes an object real. The
declared MIME is compared and recorded; the declared *size* is enforced, since on the presigned
path it is the only cross-check available. Reversible: no — this is the security boundary.

**A refused upload's staged bytes are destroyed immediately.** Every other failure leaves staging
for the reaper, because those bytes may be retryable. A refusal is different: until the reaper
runs, a refused executable stays retrievable at a key the uploader already knows. The delete is
best-effort so a cleanup failure cannot mask the reason for the refusal. Reversible: yes.

**`Policy` is a value, not a set of constants.** Size caps and the executable refusal differ by
tenant and by plan, and a safe default that cannot be overridden becomes a support ticket. The
defaults are the safe ones, and a unit test asserts `refuse_executables` is on — a careless edit
to `Default` would otherwise be invisible. Reversible: yes.

**`upload_sessions` carries `tenant_id`, and it is a key prefix rather than a boundary.** The
first version derived the tenant from `dam_global.tenants` by matching `current_schema()`, so the
reaper could not rebuild a staging key unless the control-plane row still existed — which is
exactly the situation in which reclaiming storage matters most. Object keys already embed the
tenant uuid and `object_placements` stores whole keys rather than reconstructing them, so this
follows an existing pattern. Tenant isolation remains structural (D2): the column is the prefix
the reaper needs, not an authorisation check. Reversible: yes.

**The reaper reclaims storage before it updates the row.** If the process dies between the two,
the next pass finds the row still `active` and repeats a cleanup that is idempotent. The other
order orphans the multipart parts permanently, with nothing left in the database pointing at them
and the bill continuing. A single stuck upload is logged and skipped rather than aborting the
batch — a reaper that stops on the first failure stops reclaiming anything. Reversible: yes.

**A reaped session's row is kept, marked `terminated`.** Deleting it would answer a client asking
about its own upload with a 404 that is indistinguishable from "never existed". Reversible: yes.

**`dam-db` depends on `dam-store`.** The session type is storage vocabulary, `dam-store` does not
depend on `dam-db`, and a second definition in the database layer is how the two would drift out
of step with the CHECK constraints in `0009_uploads.sql`. Reversible: yes.

**`StorageClass` deliberately has no `Default`.** A test wanted `Default::default()` and it was
tempting to add; an implicit storage class is how an original silently lands in an archive tier
and starts a 180-day minimum charge. Every call site states the class. Reversible: yes.

**Subprocess limits are applied by the shell, not by `pre_exec`.** Setting an rlimit on a child
means `pre_exec`, which is `unsafe`, and the workspace is `#![forbid(unsafe_code)]` throughout. So
the runner spawns `sh -c 'ulimit …; exec "$0" "$@"' <program> <args…>`: the limits are a fixed
string built from numbers, and the program and arguments arrive as positional parameters. That is
what makes argument injection structurally impossible rather than a quoting exercise — `$0` and
`"$@"` are values to the shell, not text it re-parses, so a filename containing `; rm -rf /` stays
a filename. Reversible: yes, if the unsafe ban is ever relaxed.

**The sandbox declares which limits the platform actually enforces.** Measured: darwin rejects
`ulimit -v` and does not constrain the allocation at all, while Linux does both; and `ulimit -f`
counts 512-byte blocks on busybox but 1024-byte blocks on bash, with `POSIXLY_CORRECT` making no
difference. Rather than pretend, `capabilities()` states the platform's real behaviour and
`unenforced()` names every requested limit that is decorative — the same "capabilities declared,
not probed" discipline as the storage drivers. A protection that exists only in production is
discovered in production. Reversible: no.

**The file-size ulimit is the coarse bound; a post-run scan is authoritative.** Because the block
size differs by shell, one divisor is wrong somewhere: 512 under-caps nothing on busybox but lets
bash allow twice the request. 512 is used (under-capping would fail legitimate large derivatives on
Linux) and `Sandbox::oversized()` closes the window afterwards, so an overshooting derivative is
detected rather than stored. Reversible: yes.

**A timed-out subprocess still returns what it printed.** The first version discarded it, which
leaves an operator with "timed out" and nothing else — and a hung tool's last lines are the
diagnosis. Capturing it required the pipe readers to accumulate into shared buffers as they read,
because a sink populated on completion is empty in exactly the case that matters. Reversible: no.

**The child gets an allowlisted environment, not the parent's.** `env_clear()` plus PATH, HOME,
TMPDIR and `LC_ALL=C`. The parent holds storage credentials — `.mise.toml` alone puts
`AWS_SECRET_ACCESS_KEY` in the process — and a subprocess that inherits them turns an RCE in a
media tool into a bucket compromise. The test asserts the child's environment against an allowlist
rather than looking for specific leaks, because naming the variables we fear only catches the ones
we thought of. `LC_ALL=C` is not incidental: media tools read locale for number formatting and have
produced comma-decimal output that downstream parsers rejected. Reversible: yes.

**Dimensions are reported twice, and neither field is called `width`.** A phone stores a portrait
photo as 4000x3000 pixels with `orientation = 6`. Reporting those numbers makes every grid cell,
aspect ratio and thumbnail sideways — the most visible bug a DAM can have — and reporting only the
rotated size loses what the file actually contains, which the derivative pipeline needs. So
`stored_*` and `display_*` are both explicit. The naming is the point: a bare `width` is the field
somebody uses without deciding which one they wanted, and that decision is invisible in review.
Reversible: no.

**An EXIF orientation outside 1-8 is dropped, not applied.** Cameras have written 0 and 9. An
unknown transform would corrupt the derivative, and treating the file as upright is the recoverable
failure. Reversible: yes.

**`orientation: None` and `Some(1)` are kept distinct.** Both mean no work for the derivative
pipeline, but the first is "no EXIF at all" and the second is "the file explicitly says upright" —
a provenance record should not conflate them. Reversible: yes.

**The probe never decodes; anything that must decode checks a pixel budget first.** A 65535x65535
PNG header fits in a few hundred bytes and is about 50 GB decoded. Dimensions come from the header
(`ImageReader::into_dimensions`), and `perceptual_hash` enforces `DEFAULT_PIXEL_BUDGET` itself
rather than trusting the caller — "hash everything on ingest" is exactly what a worker will do. An
*unknown* size does not count against the budget, because refusing files whose dimensions cannot be
read would reject every format this path does not understand, and a DAM stores those. Reversible:
yes, the budget is a parameter.

**libvips is not installed.** It needs a system library (`brew install vips`), which is not a
mise-installable runtime, so it is flagged rather than installed. The pure-Rust path is what
ARCHITECTURE already calls the fallback and covers JPEG, PNG, WebP, TIFF, GIF and AVIF; RAW, PSD
and Office formats need the primary path. Reversible: n/a — pending a decision.

**Alpha is premultiplied before resizing, and the test that proves it had to be repaired.** Resizing
RGBA without premultiplying averages the transparent *black* around a logo into the visible pixels
beside it, leaving a grey fringe. The first version of that test used a square whose edge fell
exactly on a block boundary at the chosen scale factor, so no output pixel ever mixed the two — it
passed with premultiplication switched off. The geometry now guarantees straddling pixels, and the
mutation produces `Rgba([77, 77, 77, 77])`. Recorded because the lesson generalises: a test for an
edge-mixing property has to be built so that mixing actually happens. Reversible: no.

**A derivative carries no EXIF orientation.** The pixels are uprighted once during rendering; leaving
an orientation tag on the output would make a viewer rotate them again. All eight orientation values
are applied, including the four mirrors — a mirrored derivative is as wrong as a rotated one.
Reversible: no.

**Nothing is ever upscaled.** A 2048px rendition of a 64px source is blurry in a way that reads as a
defect, so the source dimensions are the ceiling and the caller decides whether the result is usable.
Reversible: yes.

**`op_hash` length-prefixes the colour profile and rendering intent.** §18.1 requires both in the
hash so the cache cannot serve a wrongly-converted rendition. Concatenated, `("srgbper", "ceptual")`
and `("srgb", "perceptual")` hash identically — and a collision here serves the wrong colour from
cache indefinitely. Reversible: no, changing it invalidates every cached derivative.

**Matting composites per pixel rather than drawing over a filled canvas.** The latter depends on
whichever blend mode the library defaults to, and getting it wrong produces a black background —
exactly the bug the matting exists to prevent. Reversible: yes.

**The §2 invariant is enforced by a type, not by a comment.** An enrichment stage takes an
`EnrichmentSource`, and its only alarm-free constructor refuses any key outside the proxy namespace.
The reason to spend a type on this: the failure is silent. Nothing breaks the day a stage starts
reading originals — the bill arrives at the next model upgrade, as a restore storm across the whole
archive, by which time the change that caused it is months old. A convention cannot survive that
feedback delay. Reversible: yes, but it would give up the only enforcement that does not depend on
someone remembering.

**The check is "is the proxy", not "is not the original".** A thumbnail is hot and cheap too, but it
is 400px — re-running an embedding model against one would silently degrade every vector in the
library, and nothing would report it. Thumbnails, derivatives, manifests and staging keys are all
refused. Reversible: no.

**Reading the original is possible, inconvenient, and self-recording.** C2PA verification at ingest
legitimately attests to the master's own bytes while they are still hot, so an escape hatch has to
exist. It sets `used_original` and requires a non-empty reason, and the database refuses the row
without one — a flag with no reason gets muted, and a muted alarm is worse than none because it
reads as "we are watching this". Reversible: yes.

**Proxy quality is 82, not 88.** Measured against a photo-like 4000x3000 master (3.1 MB as JPEG):
q75 → 424 KB, q82 → 561 KB, q88 → 766 KB, q92 → 999 KB. §2 budgets ~0.5 MB per asset for the
*entire* hot set, so 88 does not fit on the proxy alone. Below 82 would still satisfy the model half
of the proxy's job — embeddings downsample to 224–448px and never see JPEG artifacts — but not the
preview half. The measured table now lives in ARCHITECTURE §2 so the number is grounded rather than
asserted. Reversible: yes, but changing it means regenerating every proxy, which means reading every
original.

**A proxy is contained, never cropped.** `Fit::Cover` would discard image content that a future model
— or a person looking at a preview — would have wanted, and by then the original may be hours of
restore away. Reversible: no.

**Fixtures that reason about size need photo-like data.** My first proxy size test compared against a
synthetic PNG gradient, which compresses to 187 KB for 12 megapixels: the "master" was smaller than
its own proxy and the test failed for a reason unrelated to the code. Correlated noise over a
gradient, encoded as JPEG, is the fixture that behaves like a photograph. Recorded because it is the
second time a smooth synthetic fixture has made a test meaningless in this project.

**The lifecycle engine plans; a separate step executes.** A plan can be read, diffed and approved
before a customer's masters move somewhere they cannot be read back from for 48 hours. `dry_run` is
`true` in the only constructor, there is no `Default` impl, and no builder ends in execution — so
turning it off appears in a diff rather than being inherited. Reversible: yes, and it should not be.

**Every candidate appears in the plan exactly once, with a reason if it was skipped.** An object that
is neither moved nor explained is indistinguishable from one the engine forgot, and a dry run whose
output cannot be reconciled against a bucket listing is not worth reading. A test asserts
transitions + skips == candidates. Reversible: no.

**A truncated run reports how much was left.** `HaltReason::ObjectLimit` carries `remaining`, because
a run that quietly stops at its limit looks exactly like a policy that is working — the first
thousand objects move every night and the other four million never do. Reversible: no.

**A policy can never move an object toward a warmer tier.** Cold to hot is a restore: it costs
retrieval fees and, for Glacier and Deep Archive, hours. A tiering policy able to do it by accident
turns a configuration typo into a very large bill, so the engine refuses and says so. Coldness is
ranked by retrieval characteristics rather than by class name, so a future class slots in by what it
actually costs. Reversible: yes.

**The minimum-duration boundary is inclusive.** At the instant the minimum elapses the charge is
settled and the next hop is free; one second earlier it is not. Rejecting *at* the boundary would
cost a full minimum period per object, forever, on a schedule nobody would think to question.
Reversible: no.

**A null `last_accessed_at` ages from `placed_at`.** The two obvious readings are both wrong:
"infinitely recent" means nothing ever tiers, and "epoch" means everything tiers at once. An object
nobody has ever opened is precisely what a tiering policy is for, measured from when it arrived.
Reversible: no.

**An `only_superseded` policy halts as unsupported rather than matching nothing.** `object_placements`
is keyed `(object_key, pool_id)` with no version dimension, so a noncurrent version cannot be
identified. Returning zero candidates would be indistinguishable from "nothing is due", and a policy
that appears configured while doing nothing is never investigated — a quiet success raises no
questions. The halt names the missing schema. Reversible: yes, by adding the version dimension, which
is still a decision for review.

**The accessibility gate exists from the first UI commit, not the first audit.** D10 says WCAG 2.1 AA
is a release gate from the first UI commit, and the CI job was added in the same commit that
introduced the UI. Retrofitting an a11y gate means fixing a backlog before it can go green, and a gate
that cannot go green gets disabled. It earned its place immediately: on its first run axe found that
the SvelteKit scaffold ships with no `<title>` (WCAG 2.4.2), so every page would have been announced
to a screen reader by its URL. Reversible: no.

**The a11y suite asserts structure axe cannot see.** §14.2 is explicit that automated scanning catches
roughly 40% and is "nowhere near sufficient". A scan passes happily on a page with no landmarks and no
skip link, so the suite also asserts that the skip link is the first focusable element, that
activating it *moves focus* rather than only scrolling, that there is exactly one `main` and one `h1`,
and that the viewport permits zoom. Those are the ones a keyboard user notices immediately and a
scanner never will. Reversible: no.

**The skip link is positioned off-screen, not `display: none`.** Hiding it until focus is the common
implementation and it is wrong: `display: none` and `visibility: hidden` remove the element from the
accessibility tree, so a screen-reader user never encounters it and the affordance exists only for
sighted keyboard users — who need it least. `main` also carries `tabindex="-1"`, without which the
browser scrolls but leaves focus in the header, and the user is told they have skipped when they have
not. Reversible: no.

**`:focus-visible` is styled globally.** Tailwind's preflight removes the browser's default outline,
and per-component focus styling is a rule each new component has to remember. Nobody tests with a
keyboard by accident, so the indicator is defined once in the base layer. Reversible: yes.

**`check:web` is a separate task from `check`.** The Rust gate runs on every edit; adding a browser
install and a Vite build would make the inner loop minutes long. CI runs both, and `check:all` exists
for a pre-push sweep. Reversible: yes.

**shadcn-svelte adoption is deferred to F.2.** Its `init` writes the design-token layer, which is what
F.2 defines from the UI spec — running it now would mean writing those values twice and reconciling
them later. bits-ui is installed and exercised now, since §14.1 says the DAM-specific components
compose bits-ui primitives directly rather than waiting for a shadcn wrapper. Reversible: yes.

**Each state dimension gets its own perceptual channel, and the tests pin the assignment.** Tier uses
form (glyph and border), rights use semantic colour, provenance stays neutral, confidence is a
magnitude. The reason to assert this in code rather than document it: giving tier a colour of its own
looks like a restyle, but it takes the channel rights depends on — and a grid where "archived" is
amber and "expiring" is amber is a grid where nobody trusts either badge. So there are direct
assertions that all tiers share one neutral token, that each rights state has a distinct one, and that
no provenance state borrows a rights colour. Reversible: yes, but not silently.

**`rights_state = unknown` is never styled like `allowed`.** It is the column default, and the schema's
AI-gate comment is explicit that unevaluated rights are not permission. So unknown gets its own hue —
deliberately not a paler green — and reports `blocksDistribution: true` alongside `denied`. Rendering
it like cleared would turn every unevaluated asset into an apparently licensed one. Reversible: no.

**The state names are copied from the CHECK constraints and asserted against them.** A state the
backend can produce but the UI cannot render shows as no indicator at all, which reads as "no
restriction" — the most dangerous possible default for a rights badge. F.3 will generate these types
from OpenAPI; until then the tests are what keep them in step. Reversible: yes, and F.3 should remove
the duplication.

**A `/style` route renders every variant, so axe checks every token pair's contrast.** One scan covers
the whole palette on every CI run. Verified to have teeth: lightening a single foreground token to 82%
lightness makes it fail with `color-contrast (serious)`. Checking a tint-plus-hue combination by eye is
how something that measures 3.9:1 ships. Reversible: yes, but the page is cheap and the coverage is
not replaceable by review.

**A null confidence renders no meter, not an empty one.** A tag with no score is usually one a human
applied. An empty bar would claim the model was certain the tag was wrong, which inverts the meaning of
the data. Reversible: no.

**Component tests use `it.each` rather than looping inside one test.** `vitest-browser-svelte` mounts
into a shared container, so repeated renders in a single test leave several copies in the DOM and every
locator matches more than one element — which surfaces as a 15-second timeout, not a duplicate-match
error. Worth recording because the symptom points nowhere near the cause. Reversible: yes.

**The OpenAPI document is checked in, not generated on demand.** For the same reason a lockfile is: a
reviewer should see the wire contract change in the diff rather than discover it at runtime. The Rust
suite asserts the checked-in copy matches what the code emits, so a forgotten regeneration fails
`mise run check` before a push. Emission is deterministic and pretty-printed — a document whose key
order varied between runs would fail CI at random and be disabled within a week, and a single-line
document would make every change unreadable. Reversible: yes.

**Drift is caught at three layers, because each catches what the others cannot.** A stale
`openapi.json` fails the Rust suite; a stale generated client fails a web test that reads
`openapi.json` directly; and a *current* client with an unhandled variant fails `svelte-check` through
the exhaustive `Record<State, Meta>` tables. The middle one matters most and is the least obvious: a
stale generated union type-checks perfectly against out-of-date constants, so TypeScript alone cannot
see it. Verified by adding a variant upstream and watching each layer fire in turn. Reversible: no.

**`dam-core::rights` defines the vocabulary and deliberately nothing else.** No
`blocks_distribution()`, no method answering whether a state permits anything. That is enforcement, it
belongs at the distribution chokepoint (D12), and the predicate deciding it is task 0.10 — stopped
pending the decisions in NEEDS-REVIEW.md. A convenience method here would quietly become the
definition, in the one layer with no idea who is asking or what for. The frontend's
`blocksDistribution` is a display hint that dims a button, and it is deliberately more conservative
than any server rule. Reversible: no — this is the line between vocabulary and policy.

**The wire enums live in `dam-core`, which now depends on `utoipa`.** A domain crate knowing about
OpenAPI is mild layering impurity; a second definition of these enums in `dam-api` is exactly the
drift F.3 exists to make impossible. The impurity is a derive macro, the alternative is a class of bug.
Reversible: yes.

**The generated client is excluded from Prettier and the CI drift check compares raw generator
output.** Formatting machine output would make the diff depend on two tools agreeing about formatting
forever, which is a flake waiting for a Prettier release. Reversible: yes.

**The grid container holds the collection's truth; each row holds its absolute position.** Virtualisation
removes rows from the DOM and assistive technology reads the DOM, so `aria-rowcount` and `aria-colcount`
are computed from the total while each rendered row carries an absolute `aria-rowindex`. A grid that
reported its rendered row count would announce a hundred-thousand-asset library as twenty items, and a
grid that numbered rendered rows 1..n would claim every scroll position was the top of the list. Neither
has a visual symptom. Reversible: no.

**The tier a user sees is derived server-side, not in the UI.** It comes from two independent columns —
storage class and restore state — and the mapping contains the trap the schema warns about twice: a
restore does not change the storage class, so an *expired* restore of an archived object is archived
again. Deriving it in TypeScript would mean reimplementing that rule, and getting it wrong leaves the
download button enabled until the day someone presses it. Reversible: yes.

**Arrowing past an edge holds position rather than wrapping.** In a grid, a wrap moves the eye the full
width or height of the viewport. The keypress is still consumed so the page underneath does not scroll.
Reversible: yes.

**Selection uses `SvelteSet` rather than replacing a plain `Set`.** Cloning is O(n) per click, and a
shift-range selection across 40,000 assets does it on every keystroke — which is exactly the case a 100k
grid exists to survive. eslint's `prefer-svelte-reactivity` prompted it; the performance argument is what
settled it. Reversible: yes.

**Keyboard handling lives on the grid container, not on each cell.** That is what the WAI-ARIA grid
pattern prescribes, and a per-cell handler would double-fire. The `svelte-ignore` this requires must be
in a comment of its own: the directive treats every following token as another rule name, so an inline
explanation becomes a list of invented rules. Reversible: no.

**Assert on layout, not on serialised CSS.** Chrome re-serialises a large px length in exponential form
(`3e+06px`), which reads as invalid CSS while laying out correctly. An assertion on `style.height`
reported a bug that did not exist. `getBoundingClientRect` is the honest measurement, and a probe test
pins the platform behaviour so a future clamp is caught rather than assumed. Reversible: no.


---

## Delegated decisions, adopted 2026-08-18

Asked to "complete m0 and m1 and then complete m2 and m3" with every open question already carrying a
recommendation. Recorded here as adopted-by-delegation rather than separately approved, so a later
reader can tell which calls were mine.

**ABAC 1 — multiple roles combine as a union.** A user with `contributor` on {A,B} and `reviewer` on
{B,C} sees {A,B,C}. Intersection would mean granting someone an extra role *reduced* their access,
which no administrator expects. Reversible: yes.

**ABAC 2 — an unreleased or expired asset stays visible but is not downloadable.** Someone has to find
an expired asset in order to renew its licence, and a librarian needs to see next week's embargoed
campaign in order to tag it. The download refusal carries a reason code so the UI can say "licence
expired 14 Aug" rather than silently omitting the asset — an asset that vanishes on expiry is one
nobody renews. Reversible: yes.

**ABAC 3 — `requires_eula` gates download and derivative delivery, not visibility.** Browsing is what
tells someone the EULA is worth accepting; gating search results makes an unaccepted EULA look like an
empty library, which reads as a broken product. Reversible: yes.

**ABAC 4 — rule-based groups are evaluated live inside the predicate.** Correct-then-fast: a
materialised membership table is faster but lets access lag a metadata change. Revisit at M2 when the
Tantivy side is measurable; if it shows up in p99 latency the fix is materialisation *with a stated
staleness bound*. Reversible: yes, and expected to be revisited.

**ABAC 5 — `all_asset_groups` bypasses group scoping and release windows, but not expiry, legal hold,
or `rights_state = 'denied'`.** An administrator manages the library, so unreleased assets must be
reachable; a lapsed licence is a legal fact about the asset rather than a permission anyone holds.
Under the alternative, "administrator" silently becomes "may commit a rights violation" — the exact
failure D12 exists to prevent, and invisible in an audit because the download would look authorised.
Reversible: yes, but this is the one I would want re-confirmed before it changes.

**C2PA 1 — damrs signs as one identity per deployment, not per tenant.** A C2PA signature attests to
who *performed the transform*, and that is the service, not the customer whose asset it was. Per-tenant
certificates would also mean provisioning a CA-issued certificate per tenant, which is operationally
infeasible. The tenant travels as assertion metadata instead. Reversible: yes.

**C2PA 2 — development may sign with the c2pa-rs test certificate; anything else refuses.** Signing is
enabled only when a certificate is configured, and a test certificate is refused unless
`environment = development`. A test-signed credential in production is worse than none: it looks like
provenance and verifies against nothing. Reversible: no.

**C2PA 3 — an inbound manifest that fails validation is accepted, recorded, and not re-signed.**
Rejecting the upload would stop a customer ingesting their own archive if any of it was re-saved by a
tool that broke the chain. Stripping is forbidden by D13. So the asset exists, the broken chain is
visible in `provenance_manifests`, and derivatives carry no credential. Reversible: yes.

**Search 1 — Tantivy ranks, Postgres authorises.** The access predicate is rendered into the Tantivy
query, and that filter is a *narrowing optimisation, not the authority*. An asset's group membership is
written into its document at index time and changes in Postgres the instant an administrator saves; between
those two moments the index is stale, and a stale index is **permissive** — it still says the asset is in
the group. So results are hydrated through Postgres with the same predicate applied there. §12 asks for one
predicate and three consumers; it does not say an eventually-consistent index may be the gate on a governed
library, and it must not be. Reversible: no.

*Consequence for 2.7:* facet counts computed purely from Tantivy inherit the same staleness, so they can
over-count. That has to be addressed there rather than assumed away.

**Search 2 — the tenant's metadata is one Tantivy JSON field, not one field per definition.** A Tantivy
schema is fixed at index creation; `field_defs` is not — adding a metadata field is something a customer
does on a Tuesday afternoon. One Tantivy field per definition would make every field addition a **full
reindex** of that tenant: hours for a million-asset library, with search degraded throughout. The JSON field
means a definition change alters what is written and never the schema. The price is that per-field boosting
moves from the schema to the query, which is a runtime choice rather than a migration. Reversible: yes, but
only via a reindex — which is the cost being avoided.

**Search 3 — a clause the index cannot answer is refused, not dropped.** Taxonomy and collection membership
are relational and absent from the index, and substring matching would disagree with SQL's `ILIKE` at the
margins. Silently dropping a filter clause returns **more** than the caller asked for, and the extra rows
look like ordinary results. `dam_search::query::render` returns `Unsupported` naming the clause. Reversible:
yes — each becomes supported when it can be rendered *and* covered by the differential test.

**Taxonomy 1 — terms are deprecated and superseded, never deleted.** `asset_tags` cascades on term
deletion, so "retiring a duplicate term" would silently untag every asset that used it, and nobody would
notice until a search came back empty. A merge therefore retags the assets to the survivor and retires
the source with `superseded_by` pointing at it, so an id stored outside this database — a saved search, a
Drupal field, an API client's cache — still resolves to something meaningful. Migration 0012. Reversible:
the columns yes; the policy no, because reverting means the untagging is back.

**Taxonomy 2 — deprecating a term with live children is refused rather than cascaded.** Cascading would
retire terms the operator never asked about; leaving them would put active terms under a retired ancestor,
which no rollup query renders sensibly. Refusing names the problem and the operator retires bottom-up,
which is the order they would work in anyway. Reversible: yes.

**Taxonomy 3 — slugs are unique per taxonomy, so a vocabulary cannot have the same leaf label twice.**
Not a decision I made: `taxonomy_terms_slug_idx` in 0001 is `UNIQUE (taxonomy_id, slug)` rather than
`(parent_id, slug)`. Recorded because the consequence is easy to miss and a customer will meet it — there
can be no "Other" under two different parents, and no `outdoor.beach` alongside `indoor.beach`. It also
makes a path collision structurally unreachable, since a path's last label *is* the slug; the `PathTaken`
guard in `move_term` stays anyway, so relaxing the index later cannot silently produce a half-applied
UPDATE. Left as it is for now: relaxing it later is an ordinary migration, while tightening it later would
break existing data. Worth a product decision before a real vocabulary import. Reversible: yes.

**C2PA 4 — the workspace MSRV rises from 1.85 to 1.88 to take c2pa 0.90.** Cargo silently resolves to
c2pa 0.58 under a 1.85 floor, which is a materially older API. Nothing is published from this workspace
and every build already uses the pinned 1.94 toolchain, so the floor was a declaration rather than a
constraint anyone was meeting. Reversible: yes, by dropping the dependency.

**C2PA 5 — `rust_native_crypto` instead of the default `openssl`, and `fetch_remote_manifests` stays
off.** The first removes a system OpenSSL from every machine and container that compiles this. The second
matters more: with it, the reader dereferences a URL found *inside an uploaded file*, which on an ingest
path is a server-side request forgery primitive handed to anyone who can upload. Cloud-hosted manifests
are not worth that. Reversible: the crypto backend yes; the remote fetch no.

*Cost, recorded because the earlier estimate was wrong:* c2pa pulls **245** crates, not the 83 I
estimated when assessing feasibility. It is still the only maintained Rust implementation, and D13 is not
optional, so the conclusion stands — but the number should not go unremarked.

**The access predicate renders into the query, never as a post-filter.** §7 gives the reason: pagination
counts alone disclose the existence of assets a caller cannot see. The two are indistinguishable by row
set and differ only in the count, so there is a test comparing `count(*)` with the rows and another
paginating the filtered set. Reversible: no.

**A predicate matching nothing renders as `(false)`, never as an omitted filter.** An omitted group
clause is a full scan of the tenant's library and is one early `return` away. Mutation-checked: rendering
`(true)` fails two tests. Reversible: no.

**Group membership renders as `IN (SELECT …)` rather than a join.** A join returns an asset once per
matching group, which inflates counts and breaks pagination — but only once somebody grants overlapping
groups, so it would ship. Reversible: yes.

**Release and expiry are deliberately absent from the visibility filter.** They gate distribution, not
visibility, and adding them here is the obvious optimisation and the wrong one. A unit test asserts the
rendered SQL mentions neither column. Reversible: no.

**A granted group with a rule predicate is refused, not ignored.** Evaluating one needs the query IR
(task 2.4). Ignoring it would grant less access than configured — fail-closed, but silently, so the
first symptom is an asset that should have been visible and was not. Same discipline as the lifecycle
engine's `only_superseded` halt. Reversible: yes, when 2.4 lands.

**A dangerous comment on an access-control column, corrected in migration 0011.** `0001` said, directly
above `roles.asset_group_ids`: "Empty array = all groups. Explicit rather than null so the 'no access' and
'all access' cases cannot be confused." That contradicts itself and the `all_asset_groups` boolean beneath
it — and under its reading, a role created with the column defaults would grant **every group in the
tenant**, which is the most dangerous default available. The behaviour was already correct in
`dam_core::policy`; 0011 attaches the right semantics with `COMMENT ON COLUMN`, which travels with the
schema so `\d+ roles` shows it. Editing 0001 in place would have broken its sqlx checksum for every
already-migrated database. Reversible: n/a, a documentation fix.

**API keys are hashed with BLAKE3, not argon2.** A key here is 256 bits from a CSPRNG, so guessing is not
a threat model: a password hash would buy nothing and cost a deliberate ~100 ms on *every request*. The
digest is unsalted, which would be wrong for a password and is right here — a salt defends against
precomputation over a dictionary, and there is no dictionary for 256 random bits. It also makes
authentication a single lookup against the `UNIQUE (key_hash)` index that already existed. Reversible: no,
without re-issuing every key.

**Every authentication failure looks the same.** Unknown, revoked, expired, and belonging-to-a-deleted-
tenant all return `Ok(None)`. Distinguishing them would tell a prober which of their guesses had the right
*shape*, and the shape is the cheap half to brute-force. Reversible: no.

**`last_used_at` is written at most hourly, not per request.** The column exists to find keys nobody uses.
Writing it on every request turns every read-only endpoint into a write and costs a row of WAL per API
call — a price nobody chose. Hourly resolution answers the question the column exists for. Reversible: yes.

**Key scopes intersect with the identity's permissions and never add.** That is what makes a key safe to
paste into a CI job. A union would let anyone escalate their own privileges by writing a broader scope on a
key they issue themselves. A grant left with no permissions after intersection is dropped entirely, because
it would otherwise still widen the group union for a different action. Reversible: no.

**A membership naming a deleted role contributes nothing rather than failing the request.** Failing would
lock a user out over an administrator's tidy-up. Reversible: yes.

**`is_tenant_admin` synthesises a grant rather than requiring a role row.** It is a shortcut on the
membership, so something has to turn it into grants; per ABAC 5 it clears group scoping and release windows
and nothing else. Reversible: yes.

**libvips and ffmpeg are installed through mise's conda backend, not Homebrew.** Not a preference: on
this machine `brew install vips` fails at `brew link freetype`, because a Codex runtime has placed a
symlink at `/opt/homebrew/opt/freetype/lib/libfreetype.6.dylib` pointing into its own bundled poppler.
Brew needs that path to be a symlink into its Cellar, so the two cannot coexist — and taking it would
silently repoint another tool's dependency at a different freetype. mise installs under
`~/.local/share/mise` and touches nothing shared. Both are pinned, because a loader appearing or
disappearing between vips builds changes which formats the probe claims to support. Reversible: yes.

**libvips is used through its CLI in the sandbox, not through the Rust binding.** The workspace had
declared the `libvips` crate; it is now documented as unused and the CLI path is the primary one. Two
reasons, and the second is the decisive one:

- An in-process binding puts a libvips CVE inside `damd`'s own address space, so a malformed RAW file
  corrupts the worker rather than a subprocess.
- **libvips marks 14 of its own loaders "untrusted"** — `dcrawload`, `magickload`, `pdfload`,
  `svgload`, `openslideload` among them — which is to say, precisely the formats a DAM most needs are
  the ones its own maintainers flag as risky on hostile input. Running them out of process, under the
  rlimits, wall clock, escape-proof temp dir and cleared environment `dam_media::sandbox` already
  provides, is the containment those warnings are asking for.

The cost is a process spawn per operation, tens of milliseconds, against derivative generation that is
already asynchronous and measured in seconds. Reversible: yes, but the security argument would have to
be answered.

**Colour-management tests assert on pixels, never on embedded metadata.** An embedded profile proves a
profile is embedded, not that the transform ran — and swapping the tag without converting the pixels looks
correct in every metadata check while being wrong on screen. Measured: Display P3 red `230 49 35` becomes
`251 0 5` in sRGB. An earlier version of this check compared profile *bytes* and would have passed
regardless, because the two profiles share an identical ICC header. Reversible: no.

**`vipsthumbnail` is invoked with the `>` size modifier, because it upscales by default.** Measured: a
64x48 source asked for 2048x2048 came back 2048x1536, while the pure-Rust path caps at the source. Two
renderers disagreeing on every asset smaller than the requested size is the kind of divergence nobody
notices until they compare two derivatives of one file. Reversible: no.

**Rendering intents are a no-op between matrix profiles, and there is a test saying so.** Intents diverge
only for LUT-based profiles: P3 and sRGB are both matrix/TRC with a shared D65 white point, so every
intent reduces to the same matrix. CMYK is table-based and does diverge — measured, relative
`232 31 42` against perceptual `232 0 0` — which is exactly the case D11 exists for, since "CMYK is
converted at delivery, never at ingest" is about print masters. Recorded as a test rather than a comment
because the next person to look at this will reasonably expect P3 to behave like CMYK and conclude the code
is broken when it does not. The corollary for the cache: for matrix conversions, `op_hash` including the
intent stores duplicate derivatives — wasteful, not wrong, and the price of one hash covering the case
where intent matters. Reversible: yes.

**Cover fit uses attention-based cropping, not a centre crop.** A centre crop of a product shot routinely
cuts the product in half, and vips already carries a saliency model. Reversible: yes.

**Bulk batch selection is on item state, not on the `resume_after` cursor (2.10).** The schema's comment
calls `resume_after` a "cursor for resumption", and my first version used it that way: the batch query
filtered `asset_id > resume_after`, and `record_outcome` advanced the cursor to the greatest id it had
recorded. That is correct only if outcomes are recorded in id order. A worker that fans a batch out
concurrently records in *completion* order, so recording the highest id first steps the cursor past every
lower pending item — those items are never served again, `done + failed = target` never holds, and the
operation can never legitimately finish. Selection is on `state = 'pending'` now, which cannot skip a row it
has not seen an outcome for; migration 0013 adds `asset_id` to the pending partial index so the batch order
still comes from the index rather than a sort. `resume_after` is still written and is still the progress
marker an operator reads — it just no longer decides what is served. Reversible: yes, but the concurrency
argument would have to be answered. Found by a surviving mutation: deleting the cursor from the query did
not fail any test, which is what sent me looking for what the cursor was actually buying.

**A refused eval query fails the run rather than being skipped (2.9).** An eval harness that drops the
queries it cannot parse reports a mean over a shrinking sample, and the number *improves* as more of the
corpus breaks — so the harness is worst exactly when it is most needed. `Run::is_trustworthy` is false while
any refusal remains and `damctl eval` exits non-zero, which makes a corpus that has drifted out of step with
the schema a build failure instead of a quietly better score. The same reasoning makes `--min-ndcg` fail on
an unscoreable corpus rather than pass it. Reversible: yes.

**`damctl eval` runs under an unrestricted access predicate, and says so in its own docs.** A run under a
restricted scope measures the ranking and the access filter together, and a regression that turns out to be
a permission change is a different bug from a regression in relevance. The predicate is still rendered —
`dam_search::query::search` is a second consumer of it, and §12's warning is that a second consumer is where
divergence appears — it is simply an unrestricted one. Reversible: yes; a `--as-role` flag would be the
natural extension when there is a reason to measure the two together.

**`dam_db::tenant_conn::single_tenant_pool` exists for `damctl` only, and the reason it is safe is narrow.**
`TenantConn` has exactly one constructor and it begins a transaction, precisely so no code can hold a
tenant-scoped handle outside one — a runtime `SET LOCAL` outside a transaction is a silent no-op, and on a
pool it reaches one connection while later queries take another. `single_tenant_pool` sets `search_path` as a
**connect option**, so Postgres applies it as every connection in the pool starts up; there is no window in
which a connection from it has a different path. It is still wrong for the server, where a pool per tenant at
a thousand tenants is a thousand idle connection sets, and the doc comment says so. Reversible: yes.

**A reindex indexes soft-deleted assets, with the flag set.** `query::render` excludes `deleted` on every
query, so a tombstone in the index is invisible to search; and an undelete then needs no reindex, because the
document is already there and only the flag has to flip. Skipping tombstones makes undelete a reindex, which
on a large tenant is minutes of stale search for an operation a user expects to be instant. The cost is index
size proportional to deletions ever made, which a periodic full rebuild reclaims. Reversible: yes.

**Migration-count assertions read the embedded count rather than a literal.** Two tests hard-coded `12` and
had to be edited by rote for migration 0013 — and a number edited by rote is a number nobody checks. They
compare against `migrate::tenant_migration_count()` now, which still catches the case the test was written
for (a migration file added but not committed, or a stale build, since the macro embeds at compile time)
without inviting a mechanical edit every time. Reversible: yes.

**The API surface is composed in one file, and the layers that must apply to everything live there.**
`dam_api::app::router` merges each feature router and then applies the request timeout, the JSON body limit,
`X-Content-Type-Options: nosniff` and CORS *outside* the merge. A handler that forgets a timeout holds a
connection until the client goes away, and a route added later would inherit nothing if each router set its
own. The `nosniff` header is not decoration: a delivery response carries a content type the uploader
influenced, and a browser that sniffs its way to `text/html` on an uploaded file is stored XSS. Reversible:
yes.

**CORS is permissive outside production, configured inside it, and that is defensible for a specific
reason.** The credential is a bearer token in a header rather than a cookie, so there is no ambient authority
for a hostile origin to ride — a cross-origin request without the header is anonymous, and one with it had to
be given the key. Written down rather than left as an unexamined `Any`, because the argument stops holding the
moment anything moves to cookie auth. Reversible: yes.

**`damd` refuses to start when several tenants are active.** The delivery path resolves its tenant from
configuration rather than from the signed claim (3.x moves it into the token). A process serving several
tenants would mint delivery URLs against the wrong tenant's objects, and the symptom would look like a caching
bug rather than a cross-tenant read. Refusing at startup is the only point where that is cheap to notice.
Reversible: yes, and it becomes unnecessary once the tenant is in the claim.

**The asset page's total comes from a window function in the same statement as the rows.** §7 says pagination
counts alone disclose the existence of assets a caller cannot see. Two statements would also be two snapshots,
so a concurrent upload between them makes the total disagree with the page it describes — a scrollbar that
does not match its own contents. Mutation-verified: counting without the predicate fails two cases.
Reversible: no.

**Placements are joined with a `LEFT JOIN LATERAL … LIMIT 1` that picks the warmest present copy.** A plain
join duplicates a replicated asset, because `object_placements` is keyed `(object_key, pool_id)` — it appeared
twice in the grid and twice in the window count. Warmest rather than first: the tier answers "can this be
fetched now", and a Deep Archive replica of an object that is also in Standard must not report `archive` and
disable a download that would have worked. `object_key` breaks the remaining tie so the choice is
deterministic; an order that varied between statements would make a tier flicker between two page loads with
nothing having changed. Reversible: no.

**A metadata PATCH validates the patch, not the merged document.** `Mode::Patch` is what makes editing one
caption possible on an asset missing a required field — which is every asset the moment an administrator adds
one. Validating the merge instead would refuse the edit and the message would name a field the user was not
touching. Clearing a required field is still refused, because a present `null` is an instruction rather than an
absence. Reversible: no.

**`AssetSummary.thumbnail_url` is `None` until the internal-preview rights question is answered.** See
`NEEDS-REVIEW.md`. Deliberately not worked around: every available workaround is either a second path around
the D12 chokepoint or a silent decision that internal display is not distribution. Reversible: it is the
absence of a decision, so yes.

**`damctl issue-key` prints the plaintext once and stores only a hash.** A key an operator can read back out
of the database is a key a database backup hands over. The prefix goes to the log so a key can be recognised
in an audit; the secret goes to stdout and nowhere else. Reversible: no.

**The metadata JSON field is indexed with the `raw` tokeniser, not the default one.** A field query is an
*equality* test, and SQL renders it as jsonb containment — which compares the whole value, case-sensitively.
Under the default tokeniser a stored `"Acme Corp"` indexed as `acme` + `corp`, so `brand:acme` matched it in
Tantivy and not in SQL: measured on a real corpus, 22 results against 11 for one query. That is §12's
divergence, and it survived the differential suite because every fixture value there was a single lowercase
word. The literal is no longer lowercased either — the two changes are one fix. Free text is unaffected: it
searches the separate `text` blob, which stays analysed. One field for matching values exactly, one for
searching prose. Reversible: no — reverting either half reintroduces the divergence, and both halves are
mutation-verified.

**The UI authenticates with an API key in `localStorage`, and the exposure is written down rather than
hidden.** The backend has `api_keys` with a hashed secret; it has no session endpoint. A bearer token in a
header is what makes the permissive dev CORS policy defensible — a cross-origin request without the header is
anonymous — but it has to be somewhere script can read, so an XSS on this origin can exfiltrate it. A cookie
would move the risk to CSRF rather than remove it. The real fix is a session endpoint issuing a short-lived
`HttpOnly` `SameSite=Strict` cookie, which is a backend change and not in this milestone. Two mitigations are
deliberate: only the key's *prefix* is ever displayed — the same thing `api_keys.key_prefix` stores and an
audit log shows — and forgetting it is one click. Reversible: yes, and it should be.

**`GET /fields` exists because a client cannot guess a field's shape.** `multivalued` decides whether a value
is a string or an array, and an editor that inferred its field list from facet keys had no way to know: it
sent `"blue, red"` to a field that takes an array and got a 422 about delimiters that a user can do nothing
with. Found by editing a multivalued field in a real browser. The endpoint requires `Read` rather than
`Manage` — a schema is not secret, and every reader needs it to render a form or a filter rail. Reversible:
no; a client without it cannot write a multivalued field correctly.

**The filter rail edits the query string rather than filtering results.** A rail holding its own state beside
the search box leaves a user with two filters and no way to see the whole thing, and "copy this search" copies
half of it. One string means what the user sees is what the server got — and it makes a search shareable
through the URL. The cost is that the rail has to write the shorthand language correctly, including quoting a
value with a space: `brand:Acme Corp` parses as a brand filter plus the free text "Corp". That is 13 unit
cases rather than a comment. Reversible: yes.

**Uploads run one at a time, and the client half of TUS is hand-written.** `tus-js-client` would hide the four
lines that matter — resuming from the server's offset rather than the client's idea of it — and those decide
whether a 4 GB upload survives a dropped connection. One at a time because eight-MiB chunks times six parallel
uploads is six times the memory and the multipart state for no gain on a single connection, and a queue showing
six bars at 40% tells a user nothing about when they can leave. Reversible: yes.

**The web gate mocks the API rather than running `damd`.** The Rust gate already proves the endpoints against a
real Postgres, and both sides' types are generated from one `openapi.json` — so a divergence is a build error
rather than something an integration test would catch. What only a browser can test is the browser half: a
facet click rewriting the query, a 422 landing next to the field it names, a grid reporting the collection's
row count rather than its rendered one. Keeping Docker out of the web gate is also what keeps it fast enough
to run on every push. Reversible: yes.

**The delivery tenant can be named in configuration, and is inferred only when unambiguous.** `damd` refused
to start on a dev database that had grown a second tenant — correctly, since delivery resolves its tenant
from configuration rather than from the signed claim, and guessing would mint URLs against the wrong tenant's
objects. But "delete a tenant" is not an acceptable way past that, so `server.delivery_tenant` names it. The
refusal stays for the unset-and-ambiguous case and now says which slugs it found and which variable to set.
Naming it is also the right posture for a deployment that later grows a second tenant: the answer does not
silently change under it. Reversible: yes, and it becomes unnecessary once 3.x puts the tenant in the claim.

**An internal preview goes through the D12 chokepoint and does not consult the rights verdict (A.7).** Adopted
on your instruction — "we should see thumbnails" — and implemented as a `Purpose` **signed into the delivery
claim** rather than as a bypass. The argument: an asset with no licence is `RightsState::Unknown` and unknown
denies (2.8, deliberately), and an unlicensed asset is the normal state of a freshly uploaded one and of an
entire migrated archive on day one — so gating the grid's thumbnails on the distribution verdict leaves a new
tenant with no thumbnails at all. The reasoning that resolves it is already in this repository, made for a
different gate: 2.8 records that the AI gates are answered independently of the distribution verdict, "since a
territorial restriction says nothing about internal cataloguing". ARCHITECTURE §2 points the same way — a Deep
Archive asset "is a first-class search result **with a working thumbnail**". Three restrictions keep it from
being a hole, checked at the mint *and* at delivery: a known built-in profile only, an identity required, a
share link refused. Downloads are unchanged and a test asserts both on one asset. Reversible: yes — the purpose
is one field in the claim, and flipping the policy is one branch.

**The token format version is refused rather than defaulted across a purpose change.** A v2 token carries no
purpose, and defaulting a missing one is wrong in a different direction each way: `Distribution` breaks every
outstanding preview URL, `InternalPreview` lets a token minted before the field existed skip the rights check.
Outstanding v2 tokens stop verifying, which is correct — they were issued for at most 24 hours. Reversible: no.

**An unfalsifiable branch was deleted rather than left in.** The preview restriction also required the
profile's role to be `thumbnail`, `preview` or `proxy`. Every built-in profile is proxy-class, so no test could
distinguish that check from its absence — a mutation removing it survived. `by_name` over `profiles::ALL`
already excludes both things the check was for (the original is not a profile; a tenant-defined render is not in
`ALL`), so the branch is gone. An untested branch is one that will be wrong when it finally matters.
Reversible: yes, and it should be revisited when a non-proxy built-in profile exists.

**The ingest pipeline is a library, not the worker binary.** `dam-pipeline` holds finalisation, derivation and
the queue consumer, because both stages need a real object store and a real database and an integration test
cannot reach a binary's private modules. It also means `damctl` can run a stage by hand, which is what makes a
stuck asset recoverable without a queue. Reversible: yes.

**Every pipeline stage is idempotent, because the queue leases rather than deletes.** `jobs::claim` is
at-least-once by design — a SIGKILLed worker must not lose its work — so a stage that is not safe to run twice
turns that into duplicate assets or double-charged storage. Finalisation keys on the session's `asset_id`,
derivation on `(asset_id, op_hash)`. Reversible: no.

**A permanent failure is pushed to its final attempt instead of being retried.** A malformed file will not parse
on the fifth try. `pipeline::Error::is_transient` decides, and the queue's backoff is reserved for the failures
backoff helps. Discovered to matter immediately: the first real upload failed on a missing storage pool and went
to `dead` in one step with an accurate message, rather than after four retries and twenty minutes. Reversible:
yes.

**A format no renderer can read is reported, not failed.** A DAM stores text files, spreadsheets and archives,
and none has a thumbnail; the grid draws a placeholder. The first version returned a *transient* error for a
`.txt`, so the queue would have retried it five times and dead-lettered it — caught by a test. A **missing
tool** is the case that does fail the job, because that is a deployment mistake rather than a fact about the
file, and the two are now separate variants. Reversible: no.

**`upload_sessions.content_hash` makes finalisation resumable across its one unavoidable gap.** Promoting an
object and recording an asset cannot be one transaction — one is an object store, the other is Postgres — so a
failure between them leaves the bytes promoted, staging gone, and no asset row. The retry then failed
*permanently* with "object not found" on an upload whose bytes were safely stored the whole time. Recording the
digest as soon as the promotion succeeds closes it: a re-run reads the column, skips the promotion, and records
the asset. Found by running the real pipeline, not by a test. Reversible: no.

**Provisioning creates the tenant's hot storage pool.** A tenant without one cannot ingest: finalisation records
a placement and refuses without a pool, which is exactly how the first real upload failed on a tenant
`provision-tenant` had just created. The pool description is passed in rather than read inside `dam-db`, which
has no business knowing how the deployment is configured, and `credentials_ref` is a *reference* — a credential
in that column would be a credential in every backup. Reversible: no.

**`server.public_url` makes delivery URLs absolute, and its absence makes them root-relative.** A browser
resolves `/d/<token>` against the *page's* origin, which in development is a Vite port — a 404 from the wrong
server, which is how this was found. Optional rather than required, because a required value is one that is
wrong in every deployment behind a proxy until somebody sets it. The web client resolves a relative URL against
the API base it already holds, so both shapes work. Reversible: yes.

**The delivery route needs a pool pinned to the delivery tenant's schema.** It reads `assets`, `derivatives`,
the rights tables and `share_links` unqualified, so the shared global pool resolves none of them — it failed
with `relation "derivatives" does not exist` the first time there was a real derivative to serve. The delivery
route serves exactly one tenant by construction (`damd` refuses to start otherwise), so a pinned pool is the
right shape rather than a compromise. A second `DeliveryState` built from the global pool was the actual bug,
and it also meant two keyrings — the next failure once one rotated. There is one now, shared with the asset
endpoints that mint preview tokens. Reversible: yes; it goes away when 3.x puts the tenant in the claim.

**`RUST_LOG` named the project rather than the crates.** `damrs=debug` matched no crate, so every binary logged
nothing while looking configured — the worker started, ran jobs and reported none of it. Now
`damd=info,dam_worker=info,dam_api=info,dam_pipeline=info,dam_db=info,sqlx=warn`. Reversible: yes.

**A key only authenticates while its tenant is `active`.** The query joined `dam_global.tenants` and checked
nothing about it, so the join proved the row existed and nothing more — a tenant `suspended` for non-payment or
abuse kept every one of its API keys working, which is the one thing suspension is for. `provisioning` is
refused because the schema may not exist yet, `deprovisioning` because it is being torn down, and
`migration_failed` because its schema is at an unknown version and every later query would fail from inside a
handler. `damd` already filtered on `active` when resolving its delivery tenant; authentication now agrees with
it. The refusal is indistinguishable from an unknown key, per the module's existing posture: the place to
explain a suspension is the billing page, not an API that has just refused a credential. Found by a surviving
mutation — turning the join into a `LEFT JOIN` broke no test, which meant nothing asserted the tenant side at
all. Reversible: no.

**`paths::claim_queued` is now `due_for_delivery`, because it never claimed anything.** It carried
`FOR UPDATE SKIP LOCKED` and a comment saying that let several workers drain without overlapping. The statement
runs on a pool, so it is its own transaction and the row locks release the instant it returns — `SKIP LOCKED`
protects nothing between two *calls*, only within a transaction a caller holds open. Deleting the clause changed
no behaviour, which is how it was found. The accepted design is unchanged (rows stay `queued`; an in-progress
state would need a crash-recovery sweep; the provider idempotency key covers the duplicate), but the name and
the clause promised exclusivity the code cannot give — and the notification worker does not exist yet, so a
future author would have inherited the assumption and sent every notification once per worker. Reversible: yes,
and the alternative is a real lease like `jobs::claim`.

**A caller with no identity owns no saved search.** The ownership test was
`owner_id IS NOT DISTINCT FROM $1`, so a caller with no identity matched every *ownerless* search while an
identified caller matched none of them. Neither half is a coherent rule and it was nobody's decision — it reads
as NULL-handling politeness. Plain `=` gives the only coherent reading: an identity-less caller owns nothing, and
an ownerless search is reachable only by being shared. Narrowing rather than widening, which is the safe
direction for an access predicate, and consistent with `caller::authorize` already refusing an identity-less
caller outright. Reversible: yes.

**Deleting the staging object last is defensive, not load-bearing — and the comment said otherwise.** What makes
a crash between the promotion and the asset insert recoverable is `upload_sessions.content_hash`, written the
moment the promotion succeeds. A mutation that unstages before the commit survives the suite for exactly that
reason. The ordering costs nothing and still helps for a failure landing before the digest is recorded, so it
stays — but an overclaiming comment is itself a defect, because it tells the next reader a property is enforced
when nothing enforces it. Reversible: n/a, it is a comment.

**Four untested security properties, each now pinned by a test:** an *expired* share must stop the delivery URLs
it already issued (revocation and exhaustion were covered; expiry, the commonest of the three, was not);
`consume_download` has its own copy of the expiry condition — deliberately, because the decrement must be atomic
with the check — and a duplicated condition needs its own test; a restore plan costing exactly the approval
threshold does not need approval, which is the reading that makes a threshold of 0 mean "approve everything";
and re-indexing an asset must leave one document, since Tantivy has no update and a missing delete-by-term puts
the same asset in every search result twice.

**The UI is dark by default, and that is an argument about images rather than taste.** Every serious image tool
is dark — Lightroom, Capture One, Bridge, Resolve — because a bright surround biases colour and tone judgement.
A librarian deciding whether a proof matches a brand colour makes that judgement against whatever chrome we put
around the image, so a light-grey UI makes the app an instrument that lies. Simultaneous contrast is not a
preference. Light remains complete and fully supported, because the Drupal picker (§11.2) is a guest inside
Drupal's own admin theme and cannot impose a dark surround on a page it does not own. Reversible: yes — it is a
token set, and `data-theme` already switches it in both directions.

**Every colour token is defined in both palettes, and none only inside a media query.** A colour whose only
definition sits in `@media (prefers-color-scheme: …)` or a `[data-theme]` block never applies in the un-stamped
"system" state — most viewers — and the page then renders one theme's text on the other theme's ground. So
`:root` carries the complete dark palette and the light overrides restate the same names, twice: once guarded
`:not([data-theme="dark"])` so an explicit light choice beats a dark OS, and once under `[data-theme="light"]`
so the toggle wins the other way. Reversible: no; the alternative is the bug.

**The axe contrast scan forces both themes.** It previously scanned whichever colour scheme Chromium defaulted
to, so the dark palette was never contrast-checked — which is exactly how a broken dark theme reaches a release.
Each theme has its own value for every token, so one scan cannot stand in for the other, and both `data-theme`
stamps are also scanned against the opposite OS setting. Reversible: no.

**Images sit in a checkerboard well.** It disambiguates a transparent PNG from one with a white background,
which is why every image tool uses the convention — and it fixed a defect the first dark screenshot exposed: a
dark photograph on a dark cell has no visible edge, so the grid read as one empty box beside eleven placeholders
while the image was loading perfectly. Diagnosed by sampling the decoded pixels rather than by eye, which is
also how an initial "the thumbnail is transparent" conclusion was disproved. Reversible: yes.

**The lightbox is a `<dialog showModal()>`.** The focus trap, the inert background, `Escape`, the top layer and
`::backdrop` all come from the platform. The trap is the one hand-rolled modals reliably get wrong: a div-based
modal leaves the page behind it tabbable, so a keyboard user tabs out of the "modal" and operates a UI they
cannot see. The one thing `<dialog>` does not do reliably under a framework is restore focus when the element is
*destroyed* rather than closed by the user, which is what happens when a parent unmounts it — so that is
explicit. Reversible: yes, and there is no reason to.

**The lightbox steps only within the loaded page.** Stepping past the last fetched row would need a fetch
mid-gesture, and an arrow key that sometimes pauses for a network round trip feels broken. The controls are
hidden at the edges so the affordance matches what it can actually do. Reversible: yes — prefetching the next
page would remove the limit.

**`preview_url` is on the detail endpoint only.** A list response of sixty preview URLs would mint sixty tokens
for images no grid draws — the grid uses the thumbnail — and a lightbox opens one asset at a time. Reversible:
yes.

**Bulk targets are filtered through the caller's Manage predicate on every request, and out-of-scope ids fall
out silently.** A bulk request is a client-assembled id list, and a caller scoped to one corner of the library
must not act on the rest by naming it. Falling out silently rather than erroring is deliberate: a stale grid
legitimately holds an id re-scoped a moment ago, and refusing the whole request over it makes bulk work flaky
exactly when the library is busy. The caller learns a *count* of what fell out and nothing more — which ids, and
whether they exist at all, is §7's disclosure applied to writes. Preview and creation share one filter so the
confirmation dialog's number is the operation's number. Reversible: no.

**Unexecutable bulk kinds are refused by name, at the API (422) and again in the executor (permanent).** The
schema's kind vocabulary is wider than what can run — `download_zip`, `tier`, `restore` need machinery that does
not exist yet — and an operation that "completed" while doing nothing would put a success in the history for
work that never happened. The API refusal is a message in the requester's face; the executor refusal covers
operations created by a future path that skips the API. Reversible: yes — each kind becomes executable by
gaining an `Action` arm.

**An invalid bulk metadata patch fails before any item is touched.** The patch is identical for every target, so
a malformed one would fail all 40,000 items identically; validating once and failing permanently with the field
named beats recording the same failure per row. The claim "no asset was touched" is asserted in a test, not just
stated in the message. Reversible: no.

**The bulk delete's guards live in the UPDATE's own WHERE.** `deleted_at IS NULL AND NOT legal_hold` in the same
statement as the change, so a hold cannot land between a check and the delete. A skip then says which guard
held — "legal hold blocks deletion" vs "already deleted" — because a silent skip is indistinguishable from a
bug. Reversible: no.

**Svelte effects that react to one prop read everything else under `untrack`.** The bulk bar's
abandon-on-selection-change effect also read `flow`, making `flow` a dependency — so setting it to `confirm`
re-ran the effect, which saw `confirm` and reset it to `idle` in the same tick. The dialog closed itself, and
only the e2e caught it. An effect's dependency set is whatever it reads; the comment at the site now says which
read is the trigger and why the rest are untracked. Reversible: no, it is the fix.

**The share token is shown once and stored as a digest.** The same posture as an API key, for the same reason:
a table of recoverable tokens is a table of live credentials, and a share list that can re-show links makes
every reader of that page a holder of every link. A lost link is revoked and re-created. Reversible: no —
storing recoverable tokens is a one-way weakening.

**The portal discloses why a link is dead.** "Revoked", "expired", "download limit reached" go to the
recipient verbatim, where the delivery endpoint stays flat. The difference: a delivery URL is an ambient
string, but a share token was *sent to* the person holding it — they are the intended audience, and "ask the
sender for a fresh link" is only actionable when the page says which of those it is. Passcode refusals still
distinguish required from wrong (401 both ways), never whether the token exists with some other flaw.
Reversible: yes — flatten the reasons any time.

**Portal passcodes travel in the POST body, never the query string.** A passcode in a URL lands in server
logs, browser history, and Referer headers; the portal's view and download are POSTs so the credential rides
in the body. The token itself is in the path by necessity — it is the capability being exercised — which is
why it is high-entropy and revocable rather than secret-plus-identifier. Reversible: no.

**The share's download limit is consumed only after the rights check passes.** A recipient whose download was
refused for rights has not downloaded anything; a decrement-then-check would spend the limit on refusals and
exhaust a link that never delivered a byte. The consume is a single atomic UPDATE with the limit in its WHERE,
so two racing downloads cannot both take the last slot. Reversible: no.

**A share on an EULA-gated asset fails closed on the portal.** `requires_eula` has no portal acceptance flow
yet, and rendering the file anyway would make the gate decorative. Both view and download refuse until the
flow exists. Reversible: yes — it becomes a portal acceptance step.

**A field key is immutable; renaming is define-new, backfill, remove-old.** The key is the JSONB member name
every stored value already sits under, so renaming the definition in place would orphan all of them at once
and invisibly — the validator only runs on write, so nothing would ever notice. `Amendment` therefore has no
`key` at all, which makes the rule structural rather than a check somebody can forget. Reversible: yes, a
renaming path can be added later as a bulk operation.

**A field's kind cannot change once any asset carries a value for it — soft-deleted assets included.** Values
already written were validated under the old kind and nothing re-validates them, so a text→int change leaves
rows describing themselves with a rule they do not satisfy. Soft-deleted assets count because a delete does
not touch `asset_metadata`: restoring one after a kind change produces exactly the mismatch the lock exists to
prevent. Mutation testing found this — the first implementation used the live count. The refusal carries the
number, because "you cannot" without "because 40,000 assets" is not a decision anybody can make. Reversible:
no; a converting backfill would be the way to relax it.

**Removing a field definition keeps its stored values.** The row goes, the JSONB members stay. That makes a
mis-clicked removal on a large tenant recoverable — re-defining the same key with the same kind brings the
data back, unedited — where deleting the values would make it a restore-from-backup event. The cost is JSONB
members no definition describes, which every reader already ignores: the validator refuses unknown keys on
*write*, and forms and facets enumerate the catalogue. The response reports how many assets are affected, so
the UI can say what is going dark. Reversible: yes, in both directions.

**Turning `required` on is allowed and reported, not refused.** Refusing it would make a schema unfixable on
a library that predates the rule. But an administrator who has just made forty thousand assets unsaveable
should learn it at the moment of the edit rather than one 422 at a time, so the response carries the count of
assets that now lack a value. The count is computed only when requiredness actually changes: re-reporting it
on an unrelated label edit would read as a consequence of that edit. Reversible: yes.

**A reorder must name every field exactly once.** Display order is a total order, so a client sending half
the schema has a stale copy — and applying it would reposition fields whose current place that client never
showed anybody. Refused as 422 with the expected and given counts. Reversible: yes.

**Schema statuses split "fix the request" from "the world refuses it".** A malformed key or unknown kind is
422; a duplicate key, a taken alias, or a kind locked by stored values is 409. That is the distinction which
tells a client whether to show field errors or to show the administrator what is in the way — and both carry
the refusal's own sentence, because these reach a person in a form and each one names its own fix.

**Reading the schema needs Read; editing it needs Manage.** A schema is not secret and no form can be drawn
without it, so `GET /fields` stays open to any reader — including the integration key handed to a website
build. Editing has a different blast radius: a definition is what the validator refuses payloads against,
what the search renderer decides textual-ness from, and what every form is built from. Reversible: no,
loosening this would let a read-only key reshape the tenant.

**The API client reads `reason` before falling back to a status sentence.** Found by an e2e case: `describe`
looked for `message`, the server sends `reason`, so every refusal that had something specific to say — a
schema key already taken, a share link revoked, a kind locked by 12 assets — reached the user as "Request
failed (409)". One line in one place, and it fixes the whole app at once. Reversible: no, it is the fix.

**A metadata type is a selection over the field vocabulary, not a second vocabulary.** `field_defs` remains the
one place a key has a kind and a validation rule — the schema-admin refusals exist to protect exactly that, and
a per-type copy of a definition would reintroduce the divergence they prevent. So `description` shared by the
image and video types is one definition reached two ways, and a value written under either is readable under
both. The payoff is that `dam_core::fields::validate` needed no change: it takes a slice of definitions, and
choosing the slice is a database concern. Reversible: yes, but per-type definitions would be a one-way loss.

**Metadata-type resolution has no dead end.** The asset's own type, else the tenant's default, else the whole
vocabulary. The third step is what makes migration 0015 a no-op for every existing tenant — opting in is
defining a type — and it exists because a field list is what the metadata form enumerates: a resolution that
returned nothing would not lose data, it would *hide* it, with nothing to alarm on. The types table is created
empty for the same reason; seeding built-in types would narrow every existing asset's form in a migration.
Reversible: yes.

**A write is scoped to the asset's own form.** A field the asset's type does not include is refused, with the
same `unknown_field` code a typo gets — a distinct "not in this type" code would disclose the rest of the
tenant's schema to a caller holding one asset, and the fix is identical either way. Without this a type is
decoration: the value lands in the JSONB and no form ever displays it again, which is the silent-discard
failure `unknown_field` was introduced to prevent. Reversible: no.

**A bulk item outside its asset's type fails per item rather than failing the operation.** With types,
"applicable" stops being a property of the patch alone: a field on the image form is not on the archive form,
and a selection spanning both is legitimately partial. The pre-flight check still fails the whole operation for
what is wrong for everyone — a typo, a bad kind, an out-of-range value — and this is the per-item half, which
is what `partial` exists to report. Writing anyway would make the bulk path and the single-asset path disagree
about what the schema means. Reversible: yes.

**Ingest picks the metadata type from the sniffed mime's media class.** Null is valid and resolves via the
fallback, but leaving every asset null would make types something an administrator applies by hand to a library
that already exists. The class vocabulary (image/video/audio/document/archive) is deliberately coarser than
mime: it exists so the choice is predictable to the person who set up the types. Reversible: yes.

**A missing metadata type is a 404 when the path named it and a 422 when the body did.** `DELETE
/schema/types/{id}` on an unknown id is a 404 because the id is what was addressed. `PUT
/assets/{id}/metadata-type` with an unknown id in the body is a 422: the path addresses the asset, the asset
exists, and nothing is missing — the request is wrong. The alternative, treating it as a clear, is worse than
either: the asset would fall back to the default and the caller would believe the type had been applied.
Reversible: yes.

**A type's field list is replaced wholesale, never merged.** The list is ordered, and "add this one" computed
against a client's stale copy would silently drop whatever it had not seen — the same reasoning that makes the
field reorder refuse a partial list. The UI sends the whole list on every toggle, which an e2e case asserts.
Reversible: yes.

**The metadata editor is filtered to the asset's own field list.** It had been handed the tenant's whole
vocabulary, which with metadata types means offering fields the API refuses — the error then lands on an input
the user was invited to fill in. The resolved list comes from the same endpoint the picker reads, so the form
and the validator cannot disagree. `null` (no types defined) still means "every field", which is the
pre-types behaviour. Reversible: no, it is the fix.

**TASKS.md leads with a rollup.** The file is fifteen hundred lines of narrative across nine sections, which
is the right shape for the reasoning and the wrong shape for "where are we". A table at the top, updated per
slice, plus the next-up order and the parked questions. Reversible: yes.

**Categories reuse the taxonomy machinery rather than getting their own hierarchy.** `taxonomies.kind` already
admitted `'category'`, `taxonomy_terms` already had an ltree path with a GiST index and a supersession chain,
`asset_tags` was already the asset↔term join, and `query_sql::push_term` already filtered by a term including
its descendants. A parallel `categories` table would have duplicated all of that — four places to keep in step
instead of one. `kind` is what keeps a vocabulary (a field's value set) out of the browse tree and a category
out of a field picker. Reversible: yes.

**Category counts never use `taxonomy_terms.asset_count`.** That column is a denormalised global number, and
§7 says counts disclose: showing it to a scoped caller would tell them how much of the library they cannot see.
Counts run through the same `Planned` the search does, so they reflect both the caller's scope and any active
query — a tree that counted the whole library while a query was active would offer branches leading nowhere.
Reversible: no, the global number is wrong for this purpose.

**A rollup counts distinct assets.** An asset filed under two leaves of one branch counts once for the branch.
A plain count would show more assets in a branch than the library contains, with nothing on screen to reveal
it. Same reasoning as the taxonomy facet counter, which had already solved this. Reversible: no.

**Filing an asset writes `confirmed`/`human`, not `suggested`.** Filing something is a decision, and a
suggested row would sit in the AI review queue waiting for somebody to approve what a person already did.
`reviewed_by` records who. The write is an upsert, so filing is idempotent (a state, not an event) and a person
filing what a model had merely suggested *confirms* that row rather than creating a second history. Reversible:
no.

**Unfiling deletes the placement rather than marking it rejected.** Rejection is model feedback and belongs to
the tag-review surface; taking an asset out of a category is filing. Conflating them would teach the tagger
from an action that said nothing about whether its suggestion was any good. Reversible: yes.

**Migration 0016 drops the taxonomy-wide unique slug index.** It forbade an ordinary category tree — "Yellow"
under both Exterior and Interior, "Overview" under every product line — while adding nothing: uniqueness on
`(taxonomy_id, path)` already says no two *siblings* share a slug, because a path is the parent's path plus the
slug. No query in the workspace resolved a term by slug alone. Relaxing a uniqueness constraint is one-way in
practice, and that is accepted: the alternative is a DAM that cannot express a filing hierarchy. The taxonomy
suite's case that documented the old limit is inverted, and `move_term`'s `PathTaken` guard — kept by an earlier
iteration precisely so this relaxation could not half-apply a subtree rewrite — is now reachable and tested.

**`check_groups_are_renderable` runs on a tenant connection, not the global pool.** It queries `asset_groups`,
which lives in the tenant schema, so on a global-pool connection the unqualified name resolved against
`dam_global` and the query failed — turning every request by a group-scoped caller into a 500. The check exists
to protect exactly those callers. It hid for two reasons worth recording: the `dam_db` unit tests call it with a
*tenant*-scoped pool, where it works, and no API test had ever built a group-scoped caller — an administrator
has `all_asset_groups`, which short-circuits the check before the query. Reversible: no, it is the fix.

**A configuration this build cannot honour is 501 with a body, not 500.** Refusing a caller scoped to a
rule-based asset group is deliberate (decision 4: refuse rather than approximate, because ignoring the rule
would silently grant less access than configured). But collapsing it into `Refusal::Internal` defeated the
purpose — the refusal exists so a half-supported configuration is *loud*, and a bare 500 is indistinguishable
from a crash: an operator saw an incident where the honest answer was "this group needs a feature that does not
exist yet". `Refusal::Unsupported` now answers 501 and names the group, and it is logged at `warn` rather than
`error` because somebody should fix it and nothing is broken. This is the only refusal in `caller` that carries
a body, justified because it describes the deployment rather than the tenant's data. Reversible: yes.

**Category reads and the uncategorised worklist are scoped to the caller, and browse counts ignore the active
query.** The first is §7: a rollup reporting the true total would tell a group-scoped caller how large the part
of the library they cannot reach is. The second is a deliberate split — the tree is browse chrome and counts
everything the caller may see, while narrowing counts to the current search is what the search endpoint's facets
already do. Reversible: yes.

**Categories filter through the query string, not a separate parameter.** `in:exterior.yellow` is a shorthand
selector, so the whole filter stays in one string. That is the filter rail's own stated rule — it edits the
query rather than keeping state beside the text box, because two filters with no way to see the whole thing
makes "copy this search" copy half of it. A `category=` alongside `q` would have been exactly that split. The
cost is a parser change and a schema that carries category paths; the benefit is that a category filter is
typeable, shareable and negatable (`-in:exterior`) like everything else. Reversible: yes, but the parameter
would reintroduce the split.

**`in` is a reserved selector name.** A tenant that defined a field called `in` would otherwise shadow the
browse tree, and the rail's own links would stop working for a reason invisible from either side. The selector
wins over a field of the same name, and a test pins it. Reversible: no.

**`in:` always includes descendants.** "In Exterior" colloquially means everything filed beneath it, which is
what clicking a branch in a browse tree means — and it is why category paths are `ltree` in the first place.
There is no syntax for "this category only" yet; if one is ever wanted it should be the marked case, not the
default. Reversible: yes.

**Category paths are loaded lower-cased, and retired categories are not filterable.** Case-folding matches every
other selector value: a user typing the label they see is not wrong, and an ltree label may legitimately carry
capitals. Retired categories are excluded because a retired category cannot take new assets, so offering it as a
filter would keep it alive in the one place somebody would notice — the opposite of retiring it. Vocabulary
terms are excluded too, or `in:` would silently start filtering by an AI tag vocabulary. Reversible: yes.

**A relational query is answered in SQL, and the response says it is not ranked.** Category and collection
membership are joins, not index terms, and `dam_search` refuses them by name rather than dropping them — a
dropped filter returns *more* than the caller asked for, which over a governed library is the wrong direction to
be wrong in. So `/search` routes those queries to `assets::page_matching` instead. SQL has no relevance score,
so the page comes back in recency order, and `AssetPage.ranked` reports that: a grid claiming "ranked by
relevance" over a `created_at` ordering lies about the one thing a reader might act on. The check is recursive,
because a relational clause nested under a conjunction is still relational — without that, `in:exterior harbour`
went to the index where the category half was refused, so the *composition* broke while each half worked.
Reversible: yes.

**A category tree is not also a facet.** Once a real tree existed the rail rendered it twice: once as the
hierarchy with rollup counts, and once as a flat list of leaves under a heading that was the taxonomy's UUID,
because a facet key is an id and no label can be derived from one. `taxonomies.kind` already records the
difference, and the facet query now excludes categories. Vocabularies are still facets — that is what a facet is
for. Reversible: yes.

**`text-accent` for text on the page, `text-accent-fg` only on an accent surface.** The selected category's
label used the latter and rendered *exactly* the background colour — invisible, and it was the one thing the
highlight existed to emphasise. Worth recording because the token name invites the mistake, and because the axe
scan did not catch it: a 1:1 contrast ratio on a styled button slipped through, so there is now a direct
colour-comparison assertion for the selected state rather than reliance on the scan. Reversible: no.

**An empty result is a `status`, not a grid with zero rows.** A previous iteration deliberately kept
`role="grid"` on the empty state so assistive technology would not be confused by a region changing identity.
The intent was right and the implementation was not: `grid` requires `row` children, so an empty one fails
`aria-required-children`, and a screen reader announced "grid, 0 rows" and then read a sentence sitting in no
cell. Found by the first fixture that returned zero assets. Reversible: no.

**An upload profile is a row because three readers need it at three different times.** The uploader needs the
form and the required-field rule before any bytes move; finalise needs the defaults and the metadata type when
it writes the asset; enrichment needs to know whether machine tagging was permitted, in a worker running long
after the reaper has taken the session row. Only the last can be answered from the asset and only the first from
the request, so the profile id is recorded on *both* the session and the asset. Reversible: yes.

**A profile's defaults are metadata, so the metadata validator checks them.** `Writer::Human`, because a profile
is an administrator's intent rather than a model's guess — so it is bound by `read_only` and not by
`ai_writable`; and `Mode::Patch`, because defaults fill fields rather than claiming to be a complete record.
Checked at save time so a default that can never apply never becomes a stored setting, and *again* at apply time
because a field definition can change in between — a default that has quietly become invalid must fail where
somebody can see it rather than be dropped from every upload from then on. At ingest that failure is
`Permanent`: the profile is misconfigured, so retrying would fail identically while saying nothing. Reversible:
no.

**A default fills only absent keys.** A profile that overwrote what the uploader supplied would silently discard
their work, which is the opposite of what a default is for. Reversible: no.

**The profile's metadata type wins over the mime's media class.** A profile is an explicit statement about an
intake; a class is a guess from the bytes. A profile naming no type falls through to the guess, which is the
Q.1 behaviour. Reversible: yes.

**`require_complete` is a client-facing rule, not enforced at finalise.** By the time finalise runs the bytes
are staged, and refusing there would strand an upload over metadata a person could have supplied. The library's
answer to "which assets are incomplete" is the worklist query, asked where an incomplete asset can actually be
fixed. Reversible: yes — enforcing it at the API boundary is additive.

**An upload naming a profile that no longer exists falls back rather than failing.** A session can outlive an
administrator's tidy-up, and refusing the upload at that point would strand staged bytes over a configuration
change nobody told the uploader about. Reversible: yes.

**Listing upload profiles needs Read, not Manage.** The uploader must render the profile picker and honour
`require_complete` before it can upload anything, so a client that could not list profiles could not obey them —
the rule would exist only in the database. Editing stays Manage: a profile decides what is true of everything
arriving from a source. Reversible: no, tightening it would break the uploader.

**An upload names its profile by key, in `Upload-Metadata`.** A key rather than an id, because a client that
knows its intake by name should not have to look up a uuid first, and an id in a header is one more thing for an
integration script to get wrong. The resolved id is stored on the *session*, because finalise runs from a queue
long after the request and the profile has to be recoverable from the row. Both intakes resolve it — TUS and
presigned — or which endpoint a client happened to use would decide whether its intake's rules applied.
Reversible: yes.

**An unknown profile key does not refuse the upload.** It resolves to nothing and finalise falls back to the
tenant's default. The bytes are the point: a mistyped profile is recoverable afterwards, while a refused upload
is somebody's work lost. `None` and "a named profile that resolves to nothing" are deliberately the same outcome
here, and a unit case pins that an absent key parses as `None` rather than as an empty name. Reversible: yes.

**A profile's defaults are re-validated on amend, not only on create.** Otherwise a profile could be edited into
a state that breaks every upload from that source, with the person who could fix it never seeing why. Mutation
testing found this: nothing covered the amend path. Reversible: no.

**Every documented path is asserted to be mounted on the app router.** Each endpoint's own suite builds *its*
router directly, so a module can be completely tested and still not be served — which is exactly what happened
to `/upload-profiles`: seven passing API cases, a correct OpenAPI entry, and a 404 from the running server. The
document and the router are two lists that must agree, and nothing compared them. The guard probes each path
with its own documented method and no credentials: a mounted route refuses with 401/403 because authentication
runs first, while an unmounted path is a 404 from axum's routing table. No database, no handler runs.

Its first version was useless and mutation testing said so: it probed with OPTIONS, which the CORS layer answers
for *any* path, so every probe returned 200 and the guard passed with three routers deleted. Worth recording
because it is the second time here that a test needed testing. Reversible: no.

**The uploader offers only fields a profile may write, and clears rather than blanks.** A read-only field in the
defaults form would invite a refusal the person could not have predicted, since the validator rejects it. And an
empty value is dropped from the payload rather than sent as `""` — the validator accepts an empty string for a
text field, so sending it would silently default every asset from that intake to blank. Reversible: no.

**The uploader preselects the tenant's fallback profile.** That is what the server applies when nothing is
named, so showing "no profile" while a profile silently took effect would misdescribe what happens. "No profile"
is offered last and explicitly, as the deliberate choice. Reversible: yes.

**`require_complete` is applied by the uploader, and the drop target is gated too.** Disabling the file input
alone would have left the drag-and-drop path working — making the mouse route quietly more permissive than the
keyboard one, which is the accessible one. Both are gated on the same condition. Reversible: no.
