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
