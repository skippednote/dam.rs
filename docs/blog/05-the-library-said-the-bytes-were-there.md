# The library said the bytes were there and only the download disagreed

We did not schedule a fault-injection exercise. The disk under a single-node object store filled during a multi-tenant load run, the container restarted, and the last few minutes of writes did not survive. Postgres did. The API continued to describe hundreds of assets as active, complete, and downloadable, which was a remarkably calm account of a library that had just lost its objects.

> [!TLDR]
> Database rows, queue entries, and signed tokens are claims about external reality, not proof of it. Load testing dam.rs exposed missing objects that still looked healthy, background jobs that were queued but could never run, and valid delivery tokens that named no tenant. The fixes added independent verification at each boundary: storage scrubbing, bounded queue progress, and tenant-bound signed claims.

Three defects surfaced during that work. They affected storage integrity, job scheduling, and delivery isolation, so their fixes are necessarily different. The reusable diagnostic is the same: identify what the system believes, then find a second signal capable of proving the belief false.

```press-diagram
{"type":"mapping","title":"Claims need witnesses","fromLabel":"system claim","toLabel":"independent witness","from":[{"label":"object present"},{"label":"job runnable"},{"label":"token local"}],"to":[{"label":"store probe"},{"label":"band progress"},{"label":"tenant claim"}],"links":[[0,0],[1,1],[2,2]],"footer":"A second copy of the same assumption is not verification."}
```

## The database survived and the objects did not

The load run ingested a few thousand assets across five tenants. When storage filled, the database had already committed asset and placement rows for writes the object store later lost or could no longer serve.

At one investigation checkpoint, 608 objects were missing outright. Roughly 80 more were still listed at their recorded sizes but returned no usable body. One asset claimed hundreds of kilobytes and downloaded as zero bytes. A later first scrub reported 609 missing placements. Those are observations from different checks, not a precision claim that the incident held perfectly still while we counted it.

Every affected asset looked ordinary through the metadata API. The placement table was treated as authority by delivery, tiering, and metering. If the row said the object existed and weighed 669,598 bytes, every higher layer repeated that claim.

The irritating part was that the schema already had the right vocabulary:

```sql
state text NOT NULL DEFAULT 'present'
  CHECK (state IN (
    'uploading', 'present', 'transitioning',
    'missing', 'corrupt', 'deleting'
  )),
remote_checksum  text,
last_verified_at timestamptz,
verify_failures  int NOT NULL DEFAULT 0
```

`PlacementState::Corrupt` was documented as the state that needed a scrub. Nothing wrote it. `last_verified_at` appeared in the migration and nowhere in the Rust path that mattered. We had designed a language for reporting divergence and omitted the process that speaks it.

### The scrub asks the object store directly

The integrity pass takes a bounded window of placements, ordered by `last_verified_at ASC NULLS FIRST`. Never-checked objects go first; later passes revisit the oldest. This makes the window control cycle time rather than coverage.

For each placement it performs `HEAD` and compares:

1. recorded size against remote size;
2. previous server-side checksum against the current one, when both exist;
3. a one-byte ranged read for non-empty objects whose metadata otherwise agrees.

It deliberately does not download and re-hash every object. A whole-library read turns verification into a recurring egress job. Expensive checks are the first checks operators disable when the bill arrives.

The main decision is visible in the probe:

```rust
match store.head(&key).await {
    Ok(state) => {
        let (verdict, checksum) =
            verify(store, &key, &placement, &state).await;
        verdicts.push((placement.object_key, verdict, checksum));
    }
    Err(dam_store::Error::NotFound(_)) => {
        verdicts.push((placement.object_key, Verdict::Missing, None));
    }
    Err(error) => {
        tracing::debug!(%error, "store probe was unreachable");
    }
}
```

`NotFound` is a storage finding. A timeout, connection refusal, or backend error is not. Marking every unreachable object as missing would turn a network incident into a false data-loss report. Operators would learn to ignore the report just before a real loss needed their attention.

### The one-byte probe found the boundary of the claim

Metadata can agree while the body remains unreadable. For a non-empty object, the scrub requests byte zero. A successful response containing no bytes is impossible for a healthy object and becomes `corrupt`.

On the local SeaweedFS failure shape, the roughly 80 unreadable objects errored instead of successfully returning an empty body. The scrub therefore does not flag them. This is a documented limitation, not a renamed success.

One failed read cannot distinguish corruption from transient weather. Catching that class reliably needs history, such as consecutive probe-failure count, failure spacing, and a threshold before state changes. The migration already has `verify_failures`, but the first integrity slice did not yet use it for that policy.

The honest result is narrower: absent objects are detected reliably when the backend answers `NotFound`; unreadable objects are detected only when the backend returns a contradictory successful response or comparable evidence.

### Repair must clear the finding

Integrity state is re-derived on every pass. If an operator restores or re-replicates a missing object, the next successful probe returns the placement to `present`. Latching a bad state forever would make the tool report only deterioration and hide successful recovery.

The standing report is calculated from placement state across the library, not from one pass's counters. A pass that checked 5,000 healthy objects does not erase 600 findings discovered yesterday.

## Strict priority made queued work permanently unrunnable

The storage incident led us to search for a known filename. The asset existed in Postgres and was absent from search. The queue explained why: 1,280 index jobs and 1,280 similarity jobs had `attempts = 0` and `run_after` timestamps roughly half an hour in the past. They had never been claimed.

Upload finalisation fans out derivative work at priority 40. Indexing runs at 50 and similarity at 70. Lower numbers run first. During sustained ingest, the derivative band replenished faster than workers drained it. Strict priority therefore made background jobs eligible but unreachable.

This was not a slow queue. Slow queues make progress. This queue had an unbounded wait for lower bands.

### Tenant fairness did not imply priority fairness

The claim query already ranked jobs per tenant with `row_number() OVER (PARTITION BY tenant_id ...)`. That prevents one tenant importing 100,000 assets from placing all of its jobs ahead of another tenant's next thumbnail.

The test proved fairness across tenants. It said nothing about progress across priority bands inside those tenants. We had covered one starvation axis and inferred the other.

### A slot for every band cannot fit the worker

The first tempting fix is to reserve one slot per priority band. The worker claims four jobs at a time and the system has seven bands. Giving one slot to each background band cannot fit; giving the three available slots away would leave only one for interactive work and still omit bands.

Priority aging was another option. A background job could become numerically more urgent as it waited. That works mechanically, but it violates the documented contract that values below 50 are interactive. A priority whose meaning changes with wall time becomes difficult to reason about in incidents.

The implemented rule reserves one quarter of batches of at least two jobs for the pooled background band, ordered oldest first. At limit four, one slot advances background work. If no background work is eligible, urgent work uses the whole batch. At limit one, priority remains strict and predictable.

```rust
let reserve = if opts.limit >= 2 {
    (opts.limit / 4).clamp(1, opts.limit - 1)
} else {
    0
};
```

The SQL chooses reserved rows first, then fills the remainder with the normal tenant-fair ordering:

```sql
starved AS (
  SELECT id FROM fair
  WHERE priority >= $6::smallint
  ORDER BY run_after, id
  LIMIT $5
),
urgent AS (
  SELECT f.id FROM fair f
  WHERE NOT EXISTS (
    SELECT 1 FROM starved s WHERE s.id = f.id
  )
  ORDER BY f.rn, f.priority, f.run_after, f.id
  LIMIT GREATEST($4 - (SELECT count(*) FROM starved), 0)
)
```

The guarantee is bounded progress rather than equal service. A small background band waits for older background jobs ahead of it, but a continuously replenished interactive band can no longer block it forever. On the measured backlog, background completion moved from zero in 35 minutes to eight jobs in eight minutes.

That metric is more meaningful than queue length. A growing queue may be normal during a bulk ingest. A band whose oldest job age increases while its completion counter stays flat is starvation.

## A valid signature belonged to no tenant

The third defect lived in signed delivery URLs. The token carried asset ID, transform, channel, territory, identity, share ID, purpose, expiry, and signing-key ID. It did not carry tenant ID. The delivery process chose a tenant from its own configuration.

That is adequate only while one deployment and one tenant are inseparable. Two deployments can legitimately share key material after a backup restore, staging clone, or disaster-recovery cutover. A token issued by one verifies under the other's keyring.

Without a signed tenant, the receiving process resolves the asset UUID in its configured library. Usually that returns a 404 for the wrong reason. If the same UUID exists in both libraries, it can serve the wrong asset with a valid MAC.

The fix added `tenant_id` to the versioned, length-prefixed claim before the asset ID and checked it against the served tenant before any asset lookup. The token format version moved from 3 to 4. Version 3 tokens are refused rather than reinterpreted because their bytes have no tenant meaning.

The regression test is mutation-verified. With the tenant comparison present, a token for another tenant receives the flat not-deliverable response. Remove the comparison and the same request returns 302 with an object-store URL. That result demonstrates why merely carrying the field would not be enough.

This is a different kind of independent witness from the storage scrub. The token itself now names the namespace in which its remaining fields are meaningful, and the receiving deployment compares that claim with its own boundary.

## The test gate had its own unwitnessed claims

The repository also contained a workflow file that looked like a gate while GitHub Actions was disabled at repository level. It had not executed across 137 commits. Enabling it found a sequence of environment-dependent failures:

- a floating Rust toolchain moved underneath lint;
- a minimal Rust profile omitted `rustfmt`;
- Vitest browser tests ran before Chromium was installed;
- roughly 150 Rust test binaries exhausted runner disk;
- timestamp precision differed between Rust nanoseconds and Postgres microseconds;
- a file-size limit test assumed the macOS shell's block size on Linux.

The AWS archive workflow had a quieter version of the same problem. It could exit successfully when credentials were absent, leaving a green scheduled run that had tested no restore. Missing credentials now fail and state what coverage is unavailable.

CI configuration is executable production code for the development process. A workflow file in Git is a claim. A recorded run on the intended platform is the witness.

## What we changed in the operating model

The fixes added new checks, but the larger change was to monitor progress and agreement rather than only state.

For storage:

- track unverified placement count;
- track standing missing and corrupt counts;
- separate unreachable probes from findings;
- alert when scrub coverage stops advancing.

For queues:

- expose queued and completed jobs by kind or priority band;
- track oldest eligible age;
- distinguish a deep queue from a stationary band;
- exercise sustained arrival in tests, not only finite fixtures.

For delivery:

- log wrong-tenant claims without revealing details to the caller;
- keep token versions explicit;
- test the negative path by removing the check;
- avoid sharing signing keys across environments unless rotation and recovery require it.

For CI:

- fail when mandatory credentials or prerequisites are absent;
- pin the toolchains that define the gate;
- run in clean environments where cached browsers and local defaults cannot mask ordering defects.

None of those checks proves the whole system correct. Each is positioned where one store of truth can disagree with another.

## The sharp edges remain

A scrub is sampled and delayed. An object can disappear after verification and before delivery. A full cycle over a very large library may take days unless concurrency and request budgets are tuned.

The one-byte probe adds a request per object and still cannot classify repeated transport errors without historical policy. Server-side checksums help only when the backend stores and returns them. Multipart ETags are not whole-object hashes and must not be treated as such.

The queue reserve is a policy choice. Twenty-five percent may be too much under interactive load or too little during reindexing. It needs metrics and workload-specific tuning. The important invariant is that each admitted band advances within a bounded wait.

Tenant-bound claims still operate inside a process currently pinned to one tenant schema for delivery. Carrying tenant identity closes cross-deployment acceptance and prepares multi-tenant resolution; it does not by itself implement a safe pool-per-tenant router.

These limitations are useful because they say where the next independent witness is still missing.

## FAQ

### How can a database say an object exists when object storage lost it?

They are separate systems with no shared transaction. The database can commit a placement while the object write is later lost, truncated, or rolled back by the storage backend. A periodic scrub must compare the row with the store.

### Why is `HEAD` not enough for object integrity?

`HEAD` can detect absence and size mismatch, and sometimes exposes a stored checksum. It cannot prove readable content on backends that report correct metadata for a damaged body, so a bounded ranged read or a stronger periodic verification tier is still needed.

### How do you prevent low-priority job starvation?

Reserve a bounded portion of each multi-job claim for pooled background work ordered by age, while letting urgent work reclaim unused reserve. Then monitor oldest-job age and completion by band so progress is observable.

### What should be inside a multi-tenant signed delivery token?

At minimum, the tenant, asset, purpose, transform, channel, territory, expiry, and relevant identity or share context must be covered by an unambiguous versioned signature. The receiving service must still compare those claims with live state. A database row, a queue state, or a valid MAC becomes trustworthy only after something independent is capable of disagreeing with it.
