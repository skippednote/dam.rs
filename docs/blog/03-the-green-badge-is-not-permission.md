# The green badge is not permission

Most DAM platforms will show you an asset's rights status. A green tick, a label, a licence expiry date
in the sidebar. This is genuinely useful and it is not the same thing as enforcement, and the gap between
those two is where organisations get sued.

The question worth asking of any system that displays rights: **what happens if the person ignores it?**

In a system where rights are metadata, the answer is that they download the file. The badge was advice.
The download button did not consult it.

## Where the check has to happen

There is exactly one moment that matters, and it is not the moment the badge renders. It is the moment
bytes leave the building.

Everything else is upstream of the decision and can be bypassed:

- **A badge in the detail panel** is bypassed by not looking at it.
- **A disabled download button** is bypassed by anyone who can construct the URL it would have called,
  which in most systems is anyone who has seen one working download URL and can pattern-match.
- **A permission check in the API handler** is closer, but only covers the paths that remembered to call
  it. A DAM has many ways to get bytes out: direct download, a share link, a portal, a CMS render, an
  export, a bulk zip, an integration pulling a rendition. Each of those is a path someone has to
  remember to guard.
- **A signed URL with no rights context** is the one that looks safest and is the most quietly broken.
  It proves the URL was issued by the system. It says nothing about whether the licence was still valid
  when somebody actually clicked it — and a signed URL with a 24-hour life is a 24-hour window in which
  a rights withdrawal has not taken effect.

The design conclusion we reached is that there should be **one chokepoint** through which every byte
leaves, and the rights verdict is computed *there*, at request time, rather than being carried in the
request.

## Carried versus computed

This is the distinction the whole design turns on, and it is subtle enough to be worth stating slowly.

A **carried** verdict is one decided when the URL was minted and baked into the token. It is fast, it is
cacheable, and it is wrong the moment anything changes. If a licence lapses at midnight, every URL
issued before midnight continues to work until it expires on its own. "Revoke" means "revoke,
eventually."

A **computed** verdict is one derived at the moment of delivery from the current state of the rights
tables. It costs an indexed read per delivery. It has the property that a licence withdrawn at 11:59
stops working at 11:59, for URLs already in circulation.

Everything a governed library promises about rights — expiry, withdrawal, territory restrictions,
channel restrictions, embargo — depends on the second reading. A rights system built on carried verdicts
promises things it structurally cannot deliver, and the failure is silent: nothing errors, the asset
just keeps being served after it should have stopped.

## What the token can and cannot say

If the verdict is computed at delivery, what is the token even for? It establishes the *terms of the
request*, and this turns out to be most of the security surface.

A signed delivery URL in our design carries: the asset, the tenant, what the URL is for, the transform,
the distribution channel, the territory, the identity it was issued to, the share link it came through,
and the expiry. All of it inside the signature.

The reason each one is signed is a specific attack:

**Transform.** If the transform is a query parameter outside the signature, a thumbnail URL becomes a
request for the master by editing one string. This is the most obvious attack on a delivery URL and the
cheapest to get wrong.

**Channel and territory.** These select *which licence terms apply*. A licence may permit editorial use
and forbid advertising, or permit the UK and not the US. If those are editable, the caller picks the
terms they are judged under, which makes the rights engine decorative.

**Purpose.** We distinguish an internal preview from a distribution. Both go through the same
chokepoint, but only one is a distribution and only one runs the full rights check — an internal preview
of a proxy, to a named signed-in identity, is not a publication. If purpose were a query parameter,
anyone could downgrade a distribution URL to a preview and skip the rights check entirely.

**Share link.** Carried and re-checked, which is what makes revoking a share take effect on URLs it has
already issued. Without it, revocation waits for every outstanding token to expire.

**Tenant.** This one we got wrong initially and it is instructive, so it gets the next section.

## The tenant we forgot to sign

Our delivery claim carried asset, transform, channel, territory, identity, share link and expiry — and
no tenant. The delivery process resolved which tenant's library to look in from its own configuration.

For a single-tenant deployment that is fine and it is what we shipped. The bug is what it permits when
two deployments share a signing key, which is not exotic at all: a staging environment restored from a
production backup, a disaster-recovery site, a second region.

In that situation a token minted by one deployment verifies perfectly against the other's keyring —
same key, valid signature, unexpired. And because the token named no tenant, the receiving process
resolved the asset id against *its own* library. Either you get a 404 for entirely the wrong reason, or,
if the two libraries happen to share an asset id, you serve the wrong tenant's asset with a valid
signature and a clean audit trail.

We added the tenant to the claim and a check that it matches. The test we wrote for it asserts a 404;
with the check removed it returns **302 and serves the file**, which is how we know the test is worth
having.

Two details worth keeping from that change:

The tenant goes **first in the signed payload**, before the asset id, because every field after it is
meaningless until you know which library you are in. Two tenants can hold assets with the same
identifier.

And the token format version had to be bumped, with old tokens **refused rather than reinterpreted**.
The payload is length-prefixed, so a token from the previous format read under the new rules would land
its asset id where the tenant is expected and shift every field after it — producing a plausible tenant
it was never issued for, with a signature that covers the bytes rather than their meaning. A signature
proves nobody edited the bytes. It does not prove you are reading them the way the signer wrote them.
That is what the version byte is for.

## Refusing well

Once rights are enforced rather than displayed, the interesting design question becomes what a refusal
looks like.

The instinct is a flat 404 for everything, on the grounds that any detail is a hint. We do that for
forgery — a bad signature, an expired token, a revoked share and a token for the wrong tenant all
produce the same flat refusal, because distinguishing them tells an attacker which part of their attempt
to fix.

But a rights refusal is different, and treating it like forgery is a failure of a different kind. The
person being refused is usually an employee who is allowed to see the asset, is looking at it right now,
and wants to use it. Telling them "not found" for an asset visibly on their screen is not security. It
is a support ticket, and an accurate one.

So a rights denial says what was denied and why — no licence, expired licence, wrong channel, wrong
territory, embargo not yet lifted — with the specific reason codes. The distinction we settled on: the
system collapses answers about *existence* into one refusal, because the gap between "you may not see
it" and "it does not exist" is an existence oracle. It does not collapse answers about *terms*, because
the caller has already been shown the asset and what is being withheld is a verdict they can act on.

## What this costs

An indexed read per delivery, on a request that is already doing object-store I/O. In practice it does
not register.

What it costs in design terms is more interesting: every path that serves bytes has to go through the
one chokepoint, with no exceptions for the convenient case. Every time somebody adds a feature that
needs to hand a file to a person — an export, a zip, a new portal type, a connector — the honest
implementation is to route it through the same door rather than reach past it. That is a discipline, and
the only thing that keeps it is having exactly one place where bytes leave, so that reaching past it is
visible in review.

The payoff is being able to answer the question that started this post. What happens if someone ignores
the badge?

They get a 403 with the reason.

---

*Previous: [Cold storage you can't search is a filing cabinet in a
warehouse](02-cold-storage-you-cant-search.md)*
*Next: [Your bucket, your keys, your bill](04-your-bucket-your-keys-your-bill.md) — what changes when
the DAM is a program rather than a landlord.*
