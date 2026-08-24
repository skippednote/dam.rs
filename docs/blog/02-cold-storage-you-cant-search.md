# Cold storage you can't search is a filing cabinet in a warehouse

Archival storage is roughly an order of magnitude cheaper than standard, and a media library is close to
the ideal workload for it: enormous, mostly untouched, and impossible to delete. On paper this is the
easiest cost decision in the building.

In practice most DAM platforms either do not offer archival tiering at all, or offer it in a form that
makes the archived assets stop being part of the library. The second is worse than the first, because it
looks like a feature.

## The failure mode: tiering the object, losing the asset

The naive implementation moves the object to a cold storage class and treats the asset as gone until
somebody restores it. What that does to the library:

- **It disappears from search.** The index either drops the record or the search UI filters it out,
  because the system cannot produce a thumbnail for it.
- **Its metadata becomes unreachable**, or reachable only through an admin screen, because the read path
  for an asset assumes the bytes are available.
- **Its previews stop rendering**, so every grid, picker and collection containing it shows a broken
  tile or a placeholder.
- **Anything referencing it breaks** — collections, portals, a CMS page that embedded a rendition.

The result is a library with holes in it. Users learn that "archived" means "gone", and the entirely
rational response is to stop anyone from archiving anything. The cheap storage class sits unused,
because using it costs you the product.

This is why so many DAM archives are just a big Standard-class bucket that nobody is allowed to touch.
Not because the storage economics are hard, but because the software made tiering and usability mutually
exclusive.

## What separates the two

The mistake is treating the storage class as a property of the asset rather than a property of one
placement of its bytes.

An asset is a record: filename, dimensions, capture date, rights state, provenance chain, keywords,
categories, version history, the collections it belongs to, who has looked at it. None of that lives in
the object store. All of it is perfectly available whatever storage class the original happens to be
sitting in, because it is rows in a database.

What actually becomes unavailable when an original goes cold is **the original bytes**, and nothing
else. So:

- **Search still works**, because search reads the index and the index reads the database.
- **Metadata still works**, for the same reason.
- **Previews still work**, provided the derivatives were kept in a warm class — which is the whole
  trick, and it is cheap. A 200KB proxy for a 60MB master is a third of a percent of the storage.
  Keeping every proxy hot while every original goes to archive is close to free and preserves the
  entire browsing experience.
- **The only operation that changes** is downloading the original, which becomes "request a restore,
  and we will tell you when it is ready".

That last one is a real change and should be presented as one. A person who clicks download on a cold
asset needs to be told it is archived, roughly how long it will take, and then actually notified. What
they must not get is a spinner, a generic error, or a file that silently isn't there.

The design rule we ended up with: **cold changes the latency of the bytes, never the visibility of the
asset.**

## The arithmetic nobody puts in a proposal

Archival classes are cheap per gigabyte-month and carry two costs that do not appear in the headline
rate. Both bite in ways that make a naive tiering policy worse than no policy.

**Minimum billable duration.** On AWS, Glacier Flexible Retrieval bills a minimum of 90 days per object
and Deep Archive bills 180 — whatever actually happens to the object. Store a file in Deep Archive and
delete it an hour later and you are billed as though it sat there for six months. Small per object.
Ruinous as a policy, if the policy churns.

The consequence is that **a tiering rule which moves objects back and forth is worse than one that never
moves them at all.** An "archive anything untouched for 30 days" rule sounds prudent and is a way to pay
the 90-day minimum repeatedly on the same asset every time someone opens it and it cycles back. The
minimum duration is what makes tiering a decision about an asset's *lifecycle* rather than its recent
activity, and any engine that does not model it is quietly generating charges nobody is reading.

We ended up encoding this directly: a placement carries the timestamp at which its minimum duration
expires, and the lifecycle engine refuses to move an object that has not served it out. Not as an
optimisation — as a correctness rule, because the alternative is a policy that bills for its own
churn.

**Retrieval cost and retrieval time.** Getting bytes back costs money per gigabyte and takes time, and
the two trade against each other. Expedited retrievals from Glacier land in minutes and cost most; Bulk
takes up to twelve hours and costs least; Deep Archive's standard tier is measured in half-days. A
system that offers "restore" without surfacing which tier it is using, what that will cost, and how long
it will take, is making a spend decision on the user's behalf and not telling them.

There is a second-order effect here worth naming. If restores are slow and invisible, people stop
trusting the archive, and the workaround is to keep a private copy of anything important on a laptop or
a share drive. Now you are paying for archival storage *and* you have an ungoverned shadow library,
which is precisely the condition the DAM was bought to end.

## Verified, not asserted

Everything above is design. The reason we can state it as fact is that the archival path is tested
against real AWS rather than a mock.

That distinction matters more than it sounds. It is straightforward to build a fake object store with a
controllable clock, assert that your tiering state machine transitions correctly, and go home. We have
that, and it is genuinely useful — it is how the restore-expiry and cost-estimation logic get exercised
without waiting hours. But it proves that our state machine agrees with itself. It cannot prove that
*AWS* reports what we expect at the moment a restored copy appears.

So the conformance suite runs against real S3. On the last run: twenty cases passed, none skipped, and a
Glacier restore completed in **76.7 seconds** and served back the original bytes byte-for-byte.

The "none skipped" is the assertion we care about as much as the passes. Both of the other drivers we
support — a local S3-compatible server and the fake — *skip* the restore-completion cases, because they
cannot honestly answer them. Against real AWS a skip would mean our capability detection had decided the
backend could not do something it demonstrably can, which is a bug in us rather than a fact about the
storage. Asserting that the skip count is zero is what makes the suite's silence meaningful.

We also learned to distrust the workflow that was supposed to be running this. It had been green every
night for months while executing nothing — first because it named a Cargo feature that did not exist,
then because a missing-credentials check exited before anything ran and logged a warning rather than
failing. A warning in a nightly job nobody opens is indistinguishable from coverage. Missing credentials
now fail the run.

## What this changes

For a library dominated by rarely-touched originals — which is most media libraries after year two —
tiering originals to archival while keeping every derivative and every row of metadata hot moves the
bulk of your storage onto the cheap class without removing anything from the library.

Users see the same grid, the same search results, the same previews and the same metadata. The
difference appears at exactly one moment: someone asks for the full-resolution original of something
nobody has touched in two years, and instead of an immediate download they get an honest "this is
archived, it will take about five minutes, we will tell you when it is ready."

That is a real trade-off and worth being explicit about. It is also a very different trade from "the
asset vanishes from search."

---

*Previous: [Your DAM bill grows with your library](01-your-bill-grows-with-your-library.md)*
*Next: [The green badge is not permission](03-the-green-badge-is-not-permission.md) — where rights get
checked, and why showing a status is not the same as enforcing one.*
