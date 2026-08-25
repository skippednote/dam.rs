# Cold storage you can't search is a filing cabinet in a warehouse

The archive design looked trivial until I followed one click all the way through it. Move an old original to a cheaper storage class, keep its row in Postgres, and restore it when somebody asks. Then I opened the asset grid and realised that a system which needs the original to build its thumbnail has already lost the asset long before anyone clicks download.

> [!TLDR]
> Searchable cold storage works only when the asset record, search index, thumbnail, and proxy remain hot while the original changes storage class. Archiving should change the latency of the original bytes, not the visibility of the asset. A correct implementation also models minimum storage duration, restore state, expiry, price, and notification instead of treating `GLACIER` as a decorative label.

The architectural mistake is to treat "asset" and "original object" as synonyms. An asset is a record with rights, provenance, metadata, versions, relationships, and several representations. The original is one placement of bytes. Once those concepts are separated, cold storage stops being a second library and becomes one state in the first.

## The naive archive path removes the product

A common first implementation changes the object's class and lets the normal read path discover that `GET` no longer works. The consequences spread well beyond download.

- Search results omit archived records because the renderer cannot produce a tile.
- Collections and portals show placeholders where previews used to be.
- Metadata becomes reachable only from a separate archive screen.
- A CMS rendition fails because the transform tries to read the cold master.
- Users learn that "archive" means "make this disappear," so they stop using it.

The storage class may be cheaper, but the organisation has paid by making the archive operationally irrelevant. People compensate by keeping private warm copies on laptops and shared drives. The system now funds both an archive and a shadow library.

dam.rs uses a stricter invariant: only original masters may move to a restore-required class. Metadata, extracted text, embeddings, thumbnails, detached provenance manifests, and the master proxy stay hot. The browser can therefore show the same result before, during, and after a restore.

```press-diagram
{"type":"stack","title":"A searchable cold asset","layers":[{"label":"interface","nodes":[{"label":"search"},{"label":"metadata"},{"label":"preview"}]},{"label":"hot data","nodes":[{"label":"Postgres"},{"label":"index"},{"label":"proxy"}]},{"label":"cold data","nodes":[{"label":"original"}]}],"footer":"Only the original changes retrieval latency."}
```

That separation is also why the object key namespaces matter. Proxies and thumbnails can be marked tier-exempt from their key shape without another database lookup. A lifecycle policy that accidentally matches everything still cannot move the browsing substrate.

## Storage class and restore state are different facts

An archived S3 object does not become a Standard object when restored. For S3 Glacier Flexible Retrieval and Deep Archive, a restore creates a temporary accessible copy while the object remains in its archive class. AWS deletes that temporary copy after the requested availability period. The [AWS restore documentation](https://docs.aws.amazon.com/AmazonS3/latest/userguide/restoring-objects.html) is explicit about this distinction.

That requires two independent dimensions in the database:

- `storage_class` says where the durable object lives.
- `restore_state` says whether a temporary readable copy is requested, ongoing, available, or expired.

The schema refuses an `available` restore without an expiry:

```sql
storage_class      text NOT NULL DEFAULT 'STANDARD',
restore_state      text NOT NULL DEFAULT 'none'
                       CHECK (restore_state IN (
                         'none', 'requested', 'ongoing',
                         'available', 'expired'
                       )),
restore_expires_at timestamptz,

CONSTRAINT placements_restore_expiry CHECK (
  restore_state <> 'available' OR restore_expires_at IS NOT NULL
)
```

Conflating the two creates a delayed failure. If a completed restore is recorded by changing the class to `STANDARD`, the application believes the object is warm forever. The temporary copy later expires, and the next download returns a storage error against a row that still claims instant access.

The type system carries the same distinction. Restore-required behaviour is a property of the storage class, not a string comparison scattered through handlers:

```rust
pub fn requires_restore(self) -> bool {
    matches!(self, Self::Glacier | Self::DeepArchive)
}

pub fn min_duration_days(self) -> u32 {
    match self {
        Self::Standard | Self::IntelligentTiering => 0,
        Self::StandardIa | Self::OnezoneIa => 30,
        Self::GlacierIr | Self::Glacier => 90,
        Self::DeepArchive => 180,
    }
}
```

This avoids another easy error: S3 Glacier Instant Retrieval contains "Glacier" in its name but supports real-time `GET`. Classifying archive behaviour by a substring would send immediate-access objects through a restore workflow they do not need.

## The billing rules belong in the state machine

Archive pricing has at least four dimensions: byte-month storage, minimum billable duration, retrieval request cost, and retrieved bytes. There can also be minimum billable object sizes and metadata overhead.

Current AWS documentation lists a 90-day minimum for S3 Glacier Instant Retrieval and Flexible Retrieval, and 180 days for Deep Archive. Deleting, overwriting, or transitioning an object early still incurs the remaining duration charge. Glacier Instant Retrieval also bills objects at a minimum of 128 KB. These are provider facts, not guesses; the [S3 storage-class comparison](https://docs.aws.amazon.com/AmazonS3/latest/userguide/glacier-storage-classes.html) is the source dam.rs follows.

That changes lifecycle policy in two ways.

### Do not churn between classes

An "archive after 30 idle days" rule sounds conservative. If opening an asset moves its original permanently back to Standard and another idle period moves it cold again, the system can repeatedly start minimum-duration clocks. Recent access alone is not enough information to decide a transition.

Each placement therefore carries `min_duration_until`. The planner refuses a second move before that instant:

```rust
if let Some(until) = candidate.min_duration_until
    && now < until
{
    return Verdict::Skipped(
        SkipReason::MinDurationNotElapsed { until }
    );
}
```

A restore is not a warm transition. It leaves the durable class unchanged and creates temporary readability. This is both cheaper and more faithful to how S3 works.

### Do not tier tiny hot artifacts

A 20 KB thumbnail billed as 128 KB can cost more in an infrequent-access class than in Standard. It also gains retrieval charges while saving almost no storage. The lifecycle engine excludes proxy, thumbnail, manifest, and staging namespaces before it considers price.

The useful optimisation is asymmetric: large originals move; small browsing artifacts stay. Treating all objects attached to one asset as a unit erases that advantage.

## A restore is a workflow, not a retry

Once an original is cold, the download button cannot pretend the response is merely slow. The API has to represent a long-running, priced operation.

dam.rs splits the path into quote, request, approval when needed, worker polling, availability, and delivery. The person sees the class, estimated wait, estimated charge, and any restore already in flight before confirming.

```press-diagram
{"type":"sequence","title":"Restore workflow","actors":["user","api","worker","store"],"messages":[{"from":0,"to":1,"label":"request quote"},{"from":1,"to":0,"label":"cost and ETA","reply":true},{"from":0,"to":1,"label":"confirm"},{"from":1,"to":2,"label":"queue restore"},{"from":2,"to":3,"label":"RestoreObject"},{"from":2,"to":3,"label":"poll HEAD"},{"from":3,"to":2,"label":"available","reply":true},{"from":2,"to":1,"label":"mark ready"},{"from":1,"to":0,"label":"notify","reply":true}]}
```

The restore tier changes both time and cost. AWS currently documents these typical windows for S3 Glacier Flexible Retrieval: Expedited at 1-5 minutes for eligible objects, Standard at 3-5 hours, and Bulk at 5-12 hours. Deep Archive has no Expedited tier; Standard is typically within 12 hours and Bulk within 48. The [archive retrieval options](https://docs.aws.amazon.com/AmazonS3/latest/userguide/restoring-objects-retrieval-options.html) also note request-rate and large-dataset constraints, so none of these windows should be presented as a deadline.

A good interface says "typically" and reports the actual state. A spinner implies the current HTTP request may finish. A restore can outlive the browser session, the user's working day, or the worker process that initiated it. It needs a durable row and an eventual notification.

## Search must not touch the original

Keeping the database row is necessary but insufficient. Search and preview paths must also avoid accidental reads of the master.

The indexing pipeline works from hot metadata and proxy material. The asset grid receives a thumbnail URL for the hot derivative. The delivery handler checks archive state only when the signed claim names the original. A thumbnail claim should not return `202 Archived` merely because the master beside it is cold.

This boundary was easy to get wrong because the asset owns both objects. The implementation now resolves archive state for an original claim only. Otherwise a fully archived library would render every thumbnail as a pending restore, which is technically consistent with the asset's master and completely wrong for the bytes being requested.

The same rule applies to integrations. A page render must not trigger a restore by surprise. A connector either uses an existing warm rendition, receives an explicit archived response, or is configured to request restores. Restore permission is separate from read permission because starting retrieval spends money.

## Prove the provider behaviour, not only the planner

A fake store with a controllable clock is ideal for testing state transitions. It can advance a restore from requested to available, expire the temporary copy, and exercise minimum-duration boundaries in milliseconds. It proves that the planner agrees with itself.

It cannot prove that AWS accepts the request, reports the expected headers, remains in the original storage class, and eventually serves the same bytes.

The repository therefore has a separate, intentional AWS conformance command. It is excluded from the ordinary pre-push gate because it uses a real account, creates billable archive objects, and waits on an external restore. In one recorded run in `ap-south-1`, 20 cases passed, none skipped, and an Expedited Glacier restore became readable in about 76.7 seconds. That number is a measured observation, not an SLA.

The zero skips matter. Local S3-compatible stores honestly skip restore-completion cases they cannot implement. Against AWS, a skip would mean the driver had under-declared a capability the backend provides.

The workflow had previously been green while doing no useful work, first because it named a Cargo feature that did not exist and later because missing credentials exited successfully after printing a warning. It is now manual until a deliberate non-interactive identity is configured. Missing credentials fail instead of manufacturing a green archival claim.

## The limits we still carry

The design does not make archive access instant. A user who needs an original now may still wait hours. A bad policy can also create a restore queue large enough to hit provider quotas or surprise the budget even if every individual quote is accurate.

Cost estimates depend on region, provider, retrieval tier, object size, and current price tables. They need versioned inputs and a visible timestamp. A number calculated from stale pricing is more dangerous than no number because it looks authoritative.

Hot proxies are another obligation. If they are lost and the original is in Deep Archive, rebuilding previews now requires restores. The browsing substrate needs its own durability, backup, and scrub policy. "Keep proxies hot" is an architecture choice, not a substitute for operating them.

Finally, S3-compatible backends differ. Some echo a storage-class header without changing behaviour. Some do not return server-side checksums on `HEAD`. Capability declarations and conformance cases have to be per driver. The word "compatible" cannot stand in for evidence.

## FAQ

### Can an archived digital asset remain searchable?

Yes. Search uses metadata and an index, while the grid uses warm thumbnails or proxies. Only downloading or reprocessing the cold original needs a restore.

### Does restoring a Glacier object move it back to Standard?

No. For S3 Glacier Flexible Retrieval and Deep Archive, S3 creates a temporary readable copy and leaves the durable object in its archive class. The application must track restore state and expiry separately from storage class.

### Why not archive thumbnails with the original?

Thumbnails are small, frequently read, and may be subject to minimum billable object sizes in colder classes. Keeping them hot preserves browsing and often costs less than tiering them.

### What makes a cold-storage DAM trustworthy?

The library must remain searchable without touching originals, lifecycle rules must model billing constraints, restores must be durable and explicit, and the backend claims must be tested against the backend they name. Cold storage is useful only when the asset stays present while its largest bytes take longer to arrive.
