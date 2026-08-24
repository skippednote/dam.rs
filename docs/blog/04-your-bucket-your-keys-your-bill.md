# Your bucket, your keys, your bill

The three posts before this one were about what goes wrong. This one is about the specific architectural
choice that addresses most of it, which is duller than it sounds: **the assets live in object storage
you own, and the DAM is a program that reads and writes them.**

That is not a feature. It is the absence of one — the vendor is not in the storage business on your
behalf — and almost everything else follows from it.

## What "S3-compatible" actually buys

Not portability in the abstract. Four concrete properties.

**The bytes are already where they will stay.** If you stop running the software tomorrow, nothing moves.
The objects sit in the bucket they have always sat in, under keys derived from their content hashes, in
the account you already had. There is no export step, because there is nothing to export from.

**You choose the storage class, and the rate.** Standard, infrequent access, Glacier, Deep Archive — and
at whatever rate your organisation has negotiated with its cloud provider, which for anyone at scale is
not list price. A DAM that resells you storage cannot pass that through. One that uses your bucket
cannot help but.

**Egress is between you and your provider.** Serving a million thumbnails is a line on a cloud bill you
already understand and already have commitments against. It is not a metered feature of the DAM, which
means the system doing its job is not a reason for the invoice to grow.

**"S3-compatible" means more than AWS.** The same protocol is spoken by MinIO, Ceph, SeaweedFS, Backblaze
B2, Cloudflare R2, Wasabi and every major cloud's object store. That covers on-premise for the
organisations that require it, and it covers the ones whose egress economics are the entire reason they
are shopping. We develop against SeaweedFS locally and test against real AWS, which keeps us honest about
the difference between the protocol and any one implementation of it.

## Capabilities, declared rather than assumed

Writing against "S3-compatible" as though it were one thing is how you ship something that works on AWS
and breaks on MinIO.

The backends genuinely differ. Object Lock, versioning, storage classes, server-side checksums on HEAD,
restore semantics — each is present in some implementations and absent in others. A driver that assumes
them produces failures that look like corruption. A driver that assumes their absence gives up features
that are available.

So each driver declares what it supports, and the layers above ask rather than guess. This shows up in
small honest places: the integrity scrub compares server-side checksums *where the backend reports
them*, and falls back to size where it does not, because SeaweedFS does not return a checksum on HEAD
and pretending otherwise would mean either a false alarm on every object or a check that silently does
nothing.

The same discipline applies to the archival tests. The local server can prove the wire protocol and
object lock. It cannot prove a real `RestoreObject` against Deep Archive, and it says so by *skipping*
those cases rather than passing them. Which is why the AWS conformance run asserts that the skip count
is zero — against real S3, a skip would mean our capability detection had decided a backend could not do
something it demonstrably can.

## Bring your own key

Encryption at rest with a customer-managed key is usually where a hosted DAM's story gets vague. If the
vendor holds the key, "encrypted at rest" describes their operational hygiene, not your control. The
distinction becomes concrete during a subpoena, a breach notification, or an exit.

Because the bucket is yours, SSE-KMS is a property of the objects rather than a feature the DAM has to
grant. Every object-creating call carries the KMS key when one is configured — and "every" is doing real
work in that sentence. We have seven distinct code paths that create objects: simple puts, multipart
uploads, part uploads, completions, server-side copies for tiering, and the rest. Six out of seven is
not encryption at rest. It is encryption at rest with a hole in it, and the hole is wherever the least
common code path is.

That one is enforced by a test that reads the driver's own source and asserts the count, which is an
unusual thing to do and the right response to a mistake that is invisible in review.

## Content addressing, and what it removes

Objects are stored under keys derived from the BLAKE3 hash of their content, under a per-tenant prefix.

This is a small decision with a long tail of consequences:

**Deduplication is free.** The same file uploaded twice is one object. In a library with campaign assets
that circulate between teams, this is not a marginal saving.

**A delivery URL cannot name an object the hash does not account for.** The key is derived rather than
stored, so there is no field to tamper with that would point at different bytes.

**Integrity checking has something to check against.** A scrub can ask whether the object under this key
is the size and shape the database claims — which is the subject of the next post, and turned out to
matter more than we expected.

**Tenant isolation is structural.** Every key sits under its tenant's prefix, built by a function that
takes a tenant id rather than a string. A caller passing a prefix would be one concatenation away from
naming another tenant's object; a caller passing a tenant id cannot be. When we load-tested five tenants
we verified all 2,558 object keys sat under the right prefix, and the reason that check was cheap to
write is that there was only one place keys are built.

## The part we had to prove rather than assert

An architecture argument is easy to write and easy to believe. The archival path in particular is where
"S3-compatible" claims tend to outrun what has actually been tested, because testing it properly means
waiting on real restores and paying for real storage.

So the conformance suite runs against real AWS: twenty cases, none skipped, and a Glacier restore that
completed in **76.7 seconds** and returned the original bytes.

Getting there involved discovering that the nightly job meant to be running this had been green for
months while executing nothing — first because it named a Cargo feature that did not exist, and then,
after that was fixed, because a missing-credentials check exited early and logged a warning instead of
failing. Both times the job reported success. A warning in a scheduled job nobody opens is
indistinguishable from coverage, and the fix is that absent credentials now fail the run rather than
skip it.

We mention it because it is the most transferable lesson in this series: **a test suite's silence is
only meaningful if you have checked that it can speak.** Ours could not, twice, for months.

## Where this is the wrong choice

Self-hosting has real costs and the argument is weaker if we skip them.

You run Postgres, and you are responsible for backing it up and for the restore drill actually working.
You hold the KMS key, which means losing it is your problem and no support ticket recovers from it. You
patch things. Somebody is on call, and that somebody is more expensive per hour than a seat.

For a team of five with a few thousand assets and no archival pressure, a hosted product is very likely
the correct decision and the arithmetic will say so plainly.

The architecture starts paying when the library is large enough that storage class matters, when
retention obligations mean assets accumulate faster than they can be deleted, when rights enforcement
has to be real rather than displayed, or when the exit cost of the current system has become the reason
nobody will discuss changing it.

That last one is the tell. If the honest reason for staying on a platform is what leaving would cost,
the pricing model has finished doing its work.

---

*Previous: [The green badge is not permission](03-the-green-badge-is-not-permission.md)*
*Next: [The library said the bytes were there and only the download
disagreed](05-the-library-said-the-bytes-were-there.md) — three things a week of load testing found, and
what they have in common.*
