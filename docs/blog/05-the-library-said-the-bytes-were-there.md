# The library said the bytes were there and only the download disagreed

We spent a week load-testing dam.rs with a few thousand assets across five tenants. It found three
defects. They look unrelated — a storage integrity problem, a queue scheduling problem, and a security
problem in signed URLs — and they have the same shape underneath, which is why they are in one post.

In each case **the system was confidently reporting a state that was not true**, and nothing in it was
positioned to notice.

## One: 608 objects gone, every one of them reported healthy

Our load run filled the disk on the machine running our local object store. The container was killed and
restarted. The writes from its last three minutes did not survive.

Postgres had flushed its rows and kept every one of them.

What that left: 608 assets whose objects no longer existed, and around 80 more that were listed at their
recorded size and served nothing at all. The API reported all of them `active`, with their original
filenames and byte counts. One claimed 669,598 bytes. Downloading it returned zero.

The database and the object store had diverged, and **nothing in the system was in a position to
notice**, because nothing had ever asked. Every read path trusts the placement rows: delivery resolves a
key from them, the tiering sweep moves what they name, metering bills the bytes they claim. All of it
trusting a record nobody verifies.

The part that stung: the schema had been ready for this since the first migration. `object_placements`
carries a `state` column whose CHECK constraint permits `missing` and `corrupt`. It carries
`remote_checksum` and `last_verified_at`. The enum in the code documents `Corrupt` as the state that
"needs a scrub."

**Zero writers.** For any of it. `last_verified_at` did not appear in a single line of Rust. We had
written down the vocabulary for describing this failure and never written the code that produces it,
which is a specific and common way to be wrong: the design remembered, the implementation did not, and
the schema looked complete enough that nobody noticed the gap.

So we built the scrub. A HEAD per placement, comparing recorded size and — where the backend reports one
— the server-side checksum, walking the library oldest-verified-first so consecutive passes cover
everything rather than re-checking the same page. Deliberately not re-downloading and re-hashing: that
is egress across the entire library on every pass, and a check that expensive is a check that gets
turned off.

Run against the database that produced the problem, it found 609 missing — independently matching what
the failed derivative jobs had implied — and came back clean on the one tenant that had not been
ingesting when the disk filled.

**What it still cannot see, and why we wrote that down.** A store that cannot be reached is not a
finding. Only a probe that *succeeds and returns nothing* counts as corruption, because that is the one
answer no working backend can give. A probe that errors is weather — one failed read cannot be
distinguished from a network blip, and recording it as data loss would fill the report with noise until
nobody believed it.

That rule has a cost, and we found it by measuring rather than assuming. Those ~80 truncated objects
fail by *erroring* on our local store, so the scrub does not flag them. We added a first-byte probe
specifically to catch them, watched it not work, and rewrote the module documentation to say so instead
of keeping the more confident wording we had written first. Telling a damaged object from a flaky one
needs the same probe failing across several passes — a column and a decision that slice does not have.

The honest claim is narrower than the one we wanted to make: missing objects are detected reliably,
unreadable ones only when the backend answers rather than errors.

## Two: half a library invisible to search

Same load run, different symptom. We searched for an asset by its filename and got nothing back. The
asset was in the library. It was in the database. `SELECT` found it immediately.

1,280 indexing jobs were sitting in the queue with `attempts = 0` and a scheduled time half an hour in
the past. They had never been claimed. Not once.

The queue orders strictly by priority. Derivative generation runs at priority 40 because somebody is
usually waiting on a thumbnail; indexing runs at 50; similarity hashing at 70. Every upload fans out into
derivative jobs, so on a busy tenant that band **is never empty**. Strict priority combined with a
higher band that keeps refilling is not a queue. It is starvation with a schedule.

The queue already had fairness — between tenants. One tenant bulk-importing 100,000 assets cannot starve
another tenant's thumbnails, and there is a test that proves it. We had thought carefully about one axis
of starvation and not noticed that priority is another.

The fix reserves a quarter of every batch for the background band, pooled and ordered by age. Two things
we got wrong on the way to it are worth more than the fix:

**A slot per band does not work at the batch size that exists.** The worker claims four jobs at a time.
Reserving one slot per priority band would leave interactive work one slot in four *and* still starve
the lowest bands, because three slots cannot cover seven of them. We wrote that version first.

**Aging the background band into the interactive one would have worked, by contradicting the
documentation.** `JobSpec::priority` says to reserve below 50 for interactive work. Aging jobs across
that boundary fixes the symptom by making the contract untrue, which is how a codebase's stated rules
quietly stop describing it.

The reserve is bounded rather than guaranteed: a job in a small band waits for the background work older
than it and no longer. That is what a queue means. Before the fix, the wait was unbounded — zero
background jobs completed in thirty-five minutes. After it, eight in eight minutes, on the same backlog.

## Three: a token that named no tenant

Covered in [post three](03-the-green-badge-is-not-permission.md), so briefly: our signed delivery URLs
carried asset, transform, channel, territory, identity, share link and expiry — and no tenant. The
delivery process resolved which library to look in from its own configuration.

Two deployments sharing a signing key — a staging environment restored from a production backup, a DR
site — produce tokens that verify perfectly against each other and resolve against the wrong library.

The test asserts a 404. With the check removed it returns **302 and serves the file.**

## What the three have in common

Each was a place where the system held a belief nobody checked.

The placement rows asserted objects existed. Nothing asked the store.
The queue asserted every job would eventually run. Nothing measured whether any band was advancing.
The delivery token asserted a request was legitimate. Nothing checked it was legitimate *here*.

All three passed their tests, because the tests asserted the same beliefs. And all three were invisible
until we ran the system at a scale and duration where the belief and the reality had time to come apart:
thousands of assets, several tenants, a worker running for hours, and — critically — a disk that filled
up while we were not looking.

That last one was an accident and it was the most valuable part of the week. **We did not plan a
fault-injection exercise. We ran out of disk.** The resulting damage was more realistic than anything we
would have designed, because it was uneven in ways a deliberate test is not: some objects gone, some
truncated, one tenant untouched, and the database perfectly intact and perfectly wrong.

## The one we would not have found any other way

There is a fourth thing, and it belongs here because it is the same failure applied to the tooling.

Our CI pipeline had never run. Not "ran and passed" — never executed, for 137 commits, because GitHub
Actions was disabled at the repository level. The workflow file sat in the tree looking like a gate.

Turning it on found seven real problems in a row, none of them in the product: a lint job pinned to a
floating toolchain that failed on a compiler released after the code was written; a package manager
installing Rust with a minimal profile that has no `rustfmt`; unit tests that launch a browser installed
by the *next* step; 150 test binaries that do not fit on a runner's disk; and two tests that could only
ever pass on macOS — one comparing a nanosecond timestamp against a database column with microsecond
precision, one assuming `ulimit -f` uses 1024-byte blocks when the Linux shell uses 512.

Every one of those was invisible locally, which is the point. A laptop has the components already
installed from some earlier session, the browser already cached, the clock granularity that happens to
round the right way.

We had also been running the AWS conformance suite as a nightly job that reported success for months
while executing nothing.

A test suite's silence means nothing until you have confirmed it can speak.

---

*Previous: [Your bucket, your keys, your bill](04-your-bucket-your-keys-your-bill.md)*
*Next: [A grid that holds a hundred thousand assets](06-a-grid-that-holds-a-hundred-thousand-assets.md)
— the Svelte implementation, and why accessibility was a gate from the first UI commit.*
