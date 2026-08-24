# Your DAM bill grows with your library. Your library only grows.

Every digital asset manager we have looked at meters the same four things: how many people can sign in,
how many assets you hold, how many bytes those assets occupy, and how many bytes leave the building.

Look at that list next to what a DAM is for and the problem is structural rather than commercial. A DAM
exists so that an organisation stops losing its own work. Every successful year adds photographers,
campaigns, product lines and territories — more people, more assets, more bytes, more downloads. The
product is priced on exactly the axes it exists to grow.

That is not a complaint about vendors being expensive. It is an observation that the meter and the
mission point in opposite directions, and that the gap between them widens with tenure. The customer who
has used the system longest, has the most in it, and is least able to leave, is the one paying most.

## The four meters, and what each one costs you in behaviour

**Seats.** Priced per named user, so access becomes a budget line. The predictable outcome is shared
logins and a "marketing" account that six people use, which quietly destroys the audit trail — the thing
you bought a governed system for. Or the reverse: freelancers and agencies never get accounts, so assets
travel by file share and email, and the DAM stops being where the library actually lives.

**Assets.** Priced per record, so the archive becomes a liability. Teams start deleting to stay under a
tier, and what gets deleted is whatever nobody has opened recently. That is not the same set as whatever
nobody will need. A model release from four years ago is dead weight until the day a licence is
challenged, at which point it is the only document that matters.

**Storage.** Priced per gigabyte, usually at a flat rate that does not distinguish a thumbnail from a
6K master. This is the meter that punishes quality: shooting raw, keeping mezzanines, retaining the
version history that provenance depends on.

**Bandwidth.** Priced per byte delivered. A DAM that is doing its job is serving assets into web pages,
product feeds, partner portals and CMS renders all day. Success here is indistinguishable from cost.

None of these is unreasonable in isolation. Together they describe a system where every measure of the
product working is also a measure of the invoice.

## Deletion is not the escape hatch it looks like

The obvious answer to per-asset and per-gigabyte pricing is to hold less. In a governed library, you
mostly cannot.

Rights records have to outlive the asset they describe: proving you *had* a licence in 2024 is a
different question from whether you have one now, and it is the question that gets asked. Provenance is
a chain — remove a link and the derivatives downstream of it can no longer establish where they came
from. A legal hold is an instruction from counsel that a specific set of assets does not move, does not
change and does not get deleted until they say otherwise, and it does not care what pricing tier you are
on.

So the assets accumulate because they must. The pricing model treats that accumulation as growth in
consumption. The organisation experiences it as growth in obligation. Those are not the same thing, and
only one of them shows up on the invoice.

## What it costs to leave

The cost question people model is the annual fee. The one that decides whether they ever change anything
is the exit.

**Egress.** Getting your library out means downloading every byte you have accumulated, and bandwidth is
metered. The bill for leaving scales with how long you have stayed — which is an unusual property for a
switching cost to have, and a very effective one.

**Renditions.** A DAM generates derivatives: web sizes, print profiles, video proxies, format
conversions. These are usually reproducible in principle and expensive in practice, because reproducing
them means knowing every transform that was ever configured and re-rendering the whole library through
it. Most migrations end up pulling the renditions too, which multiplies the egress.

**Metadata semantics.** This is the one that actually traps people. Your metadata comes out — most
systems will export it. What does not come out is what it *meant*. A controlled vocabulary whose terms
map to a taxonomy defined inside the vendor's admin UI, a rights state that is a label rather than a
rule, a category tree whose enforcement lived in a screen you no longer have access to. You receive
strings. You had a system.

**Integrations.** Every connected site, feed and CMS referencing assets by the vendor's URL scheme.
Migrating the DAM means either rewriting every reference or maintaining a redirect layer indefinitely.

Add those together and switching cost is not a number you pay once. It is a number that grows every year
you do not pay it.

## The asymmetry, stated plainly

A vendor's revenue from a customer grows with:

- the number of people who depend on the system,
- the number of assets in it,
- the number of bytes they occupy,
- the traffic they serve,
- and the difficulty of taking any of it elsewhere.

Every one of those is also a measure of how thoroughly the customer has committed. There is no point on
that curve where the customer's leverage improves. That is the whole shape of the problem, and no amount
of negotiating a per-seat rate changes it, because the rate is not what is compounding.

## What we think the alternative looks like

Not "cheaper". Structurally different, in three specific ways.

**Storage you already own.** The assets live in your S3-compatible bucket, in your account, under your
keys. Storage costs what your cloud provider charges, at whatever rate you have negotiated, on whatever
class you choose. The DAM is a program that reads and writes objects; it is not the landlord of your
bytes. When you stop running it, the bytes do not move — they are already where they were.

**Cold tiering that does not cost you the library.** The economics of a media archive are dominated by
the fact that most of it is rarely touched and none of it can be discarded. Archival storage classes are
an order of magnitude cheaper than standard, and the reason most systems cannot use them properly is
covered in the next post: they tier the object and lose the asset. Keeping cold originals fully
searchable, with metadata and previews intact, is what makes the cheap class usable rather than
theoretical.

**No exit event.** If the objects are in your bucket and the metadata is in your Postgres, in a schema
you can read, then "migrating away" is not a project. This is a property of the architecture, not a
promise in a contract — the point is that there is no leverage to accumulate, because there is nothing
you would have to buy back.

The honest version of this argument is not that a self-hosted system is free. You run Postgres. You pay
for storage and for the requests against it. Somebody operates it, and that somebody costs more per hour
than a SaaS seat. For a small library with a handful of users, a hosted product is very likely the right
call and the arithmetic says so.

The argument is that the costs should scale with *what you actually consume*, not with how committed you
have become. A library that doubles should roughly double its storage bill, not move you into a tier
that reprices your seats.

---

*Next: [Cold storage you can't search is a filing cabinet in a
warehouse](02-cold-storage-you-cant-search.md) — the archival limitation that makes the cheap storage
class unusable, and the arithmetic nobody puts in a proposal.*
