# Open — the UI's thumbnails need a rights decision, 2026-08-18 (API surface)

**Every asset in a fresh library has `rights_state = 'unknown'`, and `unknown` denies. So if the grid's
thumbnails go through the delivery chokepoint unchanged, a new tenant sees no thumbnails at all.**

This is not a bug in any layer — each piece is behaving as decided:

- D12: rights are enforced at the point of distribution, which is the signed-URL chokepoint.
  `delivery::issue` and `deliver` both call `rights::effective` and refuse anything that is not
  `allowed` or `expiring`.
- 2.8, on your recommendation: **unknown is not a soft yes**. An asset with no licence attached is
  `Unknown`, because the cost of guessing wrong is a rights claim made on a customer's behalf.
- An asset with no licence is the *normal* state of a freshly uploaded asset, and of an entire
  migrated archive on day one.

Put together: a thumbnail is a render of a derivative, a render passes the chokepoint, the chokepoint
asks about rights, and rights say unknown. A correct DAM UI is unusable.

ARCHITECTURE is close to settling this and does not quite. §2's tier table says a Deep Archive asset
"is a first-class search result **with a working thumbnail**; it just cannot hand over the 400 MB
original without notice" — that is the *tiering* answer, and the distinction it draws (proxy yes,
original with notice) is exactly the shape of the answer I think this needs. But it is about storage
class, not about rights, and I am not reading a rights conclusion out of a storage sentence.

**What I would do** (needs your yes, because it is rights enforcement):

1. **The signed claim gains a purpose,** signed into the token alongside the transform and channel, so
   a caller cannot flip it: `distribution` (today's behaviour, rights-checked at issue *and* at
   delivery) or `internal_preview`.
2. **`internal_preview` is restricted structurally, not by trust.** It may only name a proxy-class
   transform — `thumb_256`, `preview_1024`, the master proxy — never the original and never a
   tenant-defined render profile. `profiles.rs` already separates these, so the check is a match on a
   known profile rather than a string comparison.
3. **`internal_preview` requires an identity in the claim** and a live `asset:read` grant, and it is
   refused for a share link. A share is distribution by definition: an external recipient looking at a
   thumbnail of an unlicensed asset is the exact exposure the rights model exists to prevent.
4. **Everything else stays as it is.** Downloading an original, rendering for a channel, and every
   share-link delivery keep the full rights check. The chokepoint is still the only path; it just knows
   what it is being asked for.

The argument for it is already in this repo, made for a different gate: 2.8 records that the AI gates
are answered **independently of the distribution verdict**, "since a territorial restriction says
nothing about internal cataloguing". A thumbnail in the DAM's own grid, shown to a member of the tenant
who holds `asset:read`, is internal cataloguing by the same reasoning. What I am asking you to confirm
is that the reasoning transfers — because if it does not, the answer is that a DAM must not display an
asset it has no licence for even to its own librarians, and the product then requires a licence before
an upload is visible. That is a defensible position; it is just a very different product, and it is not
mine to choose.

**What is blocked, precisely.** `AssetSummary.thumbnail_url` is `None` for every asset until this is
answered — a shape the field already documents ("absent while a newly-uploaded asset is still being
processed"), so the grid renders its placeholder rather than breaking. Everything else in the API
surface and the UI proceeds: list, detail, search, facets, metadata editing, upload, collections. You
will be able to drive the whole UI; the cells will be placeholders.

**Cost of the wrong guess.** Choosing `internal_preview` when you wanted strict enforcement means
thumbnails of unlicensed assets were shown internally for however long it takes to notice — visible
only to tenant members with a read grant, and not distributable, but still a display we were not
authorised to make. Choosing strict enforcement when you wanted previews means the product looks broken
on day one for every customer, which is the failure that gets discovered by a prospect rather than by
us. Neither is recoverable by a refactor; both are one config flag apart if the purpose is in the claim
from the start, which is why point 1 is worth doing whichever way you answer.

---

# Open — one access-control question, 2026-08-18 (2.4)

**Rule-based asset groups are still refused, and I am not wiring them without your view.**

`asset_groups.predicate` is documented in the schema as holding "the same query IR the search layer
compiles, so a group is literally a saved search". 2.4 has now built that IR, so the missing piece is
nominally there — and `dam_db::access::check_groups_are_renderable` still refuses a rule-based group,
exactly as it did before.

I am leaving it refused because connecting the two creates a recursion ARCHITECTURE does not settle:

- Every user query carries an `AccessPredicate`, which is expressed in terms of **asset-group
  membership**.
- A rule-based group's membership would be defined by a query.
- So a group whose rule referenced group membership — directly, or through another group — would need
  its own membership resolved in order to resolve its own membership.

That is not a hypothetical. `Query` today can express `InCollection`, and a collection is a saved set;
adding a `InGroup` clause is the obvious next request, and at that point the cycle is one configuration
away. The failure mode is a hung request or an unbounded query, and it would be reachable by an
administrator editing a group definition rather than by an attacker — which makes it more likely, not
less.

**What I would do** (needs your yes, because it is an access-control semantic):

1. A group's rule predicate is evaluated **without** an access filter. It *defines* access; filtering it
   by access is circular. This means a rule predicate is a privileged object and only an administrator
   may write one — which is already true of `asset_groups`.
2. A rule predicate may not reference group membership at all, enforced by validating it against a
   restricted subset of `Query` rather than by documentation. Enforced structurally, the cycle cannot be
   configured.
3. Group membership is materialised by a worker into `asset_group_members` rather than evaluated live, so
   the request path keeps the single indexed subquery it has now. Decision 4 in DECISIONS.md currently
   says rule-based groups are "evaluated live"; that is the part I would want to change, and it is why
   this is a question rather than a task.

Point 3 contradicts a recorded decision, which is why I have not simply taken the reversible option. The
alternative — live evaluation — is a nested access-filtered subquery per request per group, and I do not
think it holds up at the pagination-count level §7 cares about.

Nothing is blocked on this: explicit groups work, and a rule-based group fails closed with a message
naming the gap.

---

# Answered — decisions delegated, 2026-08-18

You said "complete m0 and m1 and then complete m2 and m3". Every open question below carried a
recommendation, so I am reading that as: proceed on the recommendations. Each is now recorded in
DECISIONS.md as adopted, with the fact that it was delegated rather than separately approved — so a
later reader can see which calls were mine.

**Two questions had no recommendation and I have now made one** (both in DECISIONS.md, both
reversible): C2PA signs as one damrs identity per deployment rather than per tenant, and signing is
refused outside development unless a real certificate is configured.

**One item I still cannot do:** `brew install vips`. My write approval is scoped to this repository
and a Homebrew install writes to `/opt/homebrew`. Run `! brew install vips` when convenient and the
RAW/PSD/INDD path activates; everything else in 1.7 proceeds without it.

The original text is kept below, because the reasoning behind each recommendation is the record of
why the system behaves the way it does.

---

# Blocked — needs your decision

Task **0.10 (ABAC predicate compiler)** is stopped per the standing rule: stop if a task
touches rights enforcement, consent, provenance, or access control in a way
ARCHITECTURE.md does not already settle.

Everything else in M0 is done. 0.11 (CI) proceeded — it does not depend on this.

---

## Why this one stopped

The **mechanism** is settled and I would not need to ask about it:

- one predicate, compiled once, reused by SQL, Tantivy, and MCP (§12)
- applied at query time, never as a post-filter, because pagination counts alone
  disclose the existence of assets a caller cannot see (§7)
- inputs are asset groups, release/expiry windows, and EULA acceptance (§12)

The **semantics** are not, and five of them decide whether an unapproved, unreleased, or
expired asset can be seen or fetched. That is the core of what the rights work exists to
prevent, so a wrong guess here is a compliance problem rather than a refactor.

---

## The five decisions

Recommendations are mine; say "all recommended" and I'll implement exactly that.

### 1. How do multiple roles combine?

A user with `contributor` on groups {A,B} and `reviewer` on groups {B,C}.

- **(a) Union → {A,B,C}.** Standard RBAC. Adding a role never removes access, which is
  what makes roles composable.
- (b) Intersection → {B}. Safer, and surprising: granting someone an extra role would
  *reduce* what they can see.

**Recommend (a).** It is what every RBAC system does and what an administrator will
assume. Note it interacts with 5.

### 2. Does an unreleased or expired asset disappear, or stay visible but unusable?

`assets.release_at` in the future, or `expires_at` in the past.

- **(a) Visible to anyone with `asset:read`, but not downloadable.** Someone has to be
  able to find an expired asset in order to renew its licence, and a librarian needs to
  see next week's embargoed campaign in order to tag it.
- (b) Invisible unless the caller holds a manage permission.
- (c) Invisible to everyone but administrators.

**Recommend (a)**, with the download gate carrying the reason code so the UI can say
*"licence expired 14 Aug"* rather than silently omitting the asset. This matches the UI
spec, where expiry is a chip on a visible asset rather than a disappearance — and an
asset that vanishes on expiry is one nobody renews.

### 3. Does `roles.requires_eula` gate visibility or only download?

- **(a) Download and derivative delivery only.** Browsing is what tells someone the EULA
  is worth accepting.
- (b) Everything, including search results.

**Recommend (a).** (b) makes an un-accepted EULA look like an empty library, which reads
as a broken product rather than a gate.

### 4. Rule-based groups: evaluated live, or materialised?

`asset_groups.predicate` is a saved query; `asset_group_members` is explicit membership.

- **(a) Union of both, predicate evaluated live** inside the access predicate.
  Always correct, and nests a subquery inside every ACL check.
- (b) Materialise predicate groups into `asset_group_members` on a schedule.
  Faster, and access can lag a metadata change by up to one cycle.

**Recommend (a) now, revisit at M2** when the Tantivy side is measurable. Correct-then-fast
beats a lag window in an access check — but flag that if it shows up in p99 latency, (b)
is the fix and it needs a stated staleness bound.

### 5. Does `all_asset_groups` (admin) bypass release, expiry, and EULA too?

The one I am least willing to guess at.

- **(a) Bypasses group scoping and release windows, but NOT expiry, legal hold, or
  `rights_state = 'denied'`.** An administrator manages the library, so unreleased assets
  must be reachable; but a lapsed licence is a legal fact about the asset and not a
  permission an administrator holds.
- (b) Bypasses everything. Simple, and means an admin account can download an asset
  nobody is licensed to use.
- (c) Bypasses nothing beyond group scoping.

**Recommend (a).** Under (b), "administrator" silently becomes "may commit a rights
violation", which is exactly the failure D12 exists to prevent — and it would be invisible
in an audit because the download would look authorised.

---

## What I will build once you answer

- `dam-core::policy` — `Grants` (from roles) → `AccessPredicate`, pure and unit-testable
- SQL rendering into a `WHERE` fragment for `TenantConn` queries
- Tantivy filter rendering from the same `AccessPredicate` value
- a differential test asserting both back ends return **identical** asset sets for the
  same grants over the same corpus, because §12's "one implementation, three consumers"
  is only true if something checks it
- a test that pagination counts do not differ between a caller who can see an asset and
  one who cannot — the §7 leak, asserted rather than assumed


---

## Task 1.6 — the TUS HTTP surface is blocked on the same gap as 0.10

**Status:** everything in 1.6 that does not require an HTTP layer is done — the resumable
engine, `upload_sessions`, the session repository, the reaper, sniffing, and finalisation. The
remaining pieces are the TUS endpoints and presigned URL minting, and both stop here.

**Why.** A TUS surface is not just header parsing. `POST /uploads` has to answer three questions
before it creates anything:

1. **Who is asking?** There is no authentication layer yet. No task in M0 or M1 schedules an API
   skeleton — I checked; `TASKS.md` has no axum, router, or auth task in the overnight scope.
2. **Which tenant?** Tenant resolution from a request is the `TenantConn` invariant's entry
   point (§5.2), and getting it wrong is a cross-tenant data leak rather than a bug.
3. **May they upload here?** Upload permission is an ABAC predicate, and **0.10 is already
   blocked** pending the five decisions above.

Minting a presigned PUT has the same shape: the mechanism is settled (`presign_put` exists, the
staging key layout is settled, finalisation validates what arrives), but handing a client a URL
that writes into a tenant's prefix *is* the authorisation decision. Building it now would mean
inventing an auth model in a handler, which is exactly where an access-control model should never
be invented.

**What I need.** Either sign off on `NEEDS-REVIEW.md`'s five ABAC questions so 0.10 can proceed
and the API layer can be built on it, or say explicitly that a provisional unauthenticated
surface — bound to localhost, with a hard-coded tenant, marked as scaffolding — is acceptable for
the overnight run. I did not assume either.

**What proceeding without a decision would cost.** An HTTP surface written against an invented
auth model tends to leak that model into every handler, and unpicking it later is the expensive
kind of refactor. Everything below the HTTP layer is finished and tested, so nothing is blocked
except the endpoints themselves.

---

## Task 1.9 — C2PA: the mechanism is settled, the signing identity and the failure policy are not

**Status:** stopped before writing code. This is the provenance category I was told to stop on, and
two of the three questions below are not answered anywhere in ARCHITECTURE.md.

**What is settled.** D13 ("provenance is preserved and re-signed, never stripped"), D15 (AI
disclosure rides the same mechanism), §16's tool table naming `c2pa-rs` for verify + preserve +
re-sign, `0006_provenance.sql` with `provenance_manifests`, `provenance_actions`, `ai_disclosures`
and the `provenance_gaps` view, and `Key::manifest` for a detached manifest that survives its master
being tiered to Deep Archive. None of that needs a decision from you.

**Dependency check, so the cost is not a guess.** `c2pa` 0.90.15: 83 required dependencies, MSRV
1.88 (we build on 1.94), and **openssl is optional** — the default path is pure Rust (`rasn-cms`,
`rsa`, `p256/p384/p521`, `ed25519-dalek`). So no system library, and no repeat of the libvips
problem. It is a heavy crate but it is the reference implementation and D13 names it.

**What I need decided.**

1. **Whose signature is it?** Re-signing asserts that *someone* performed a transform. One damrs
   identity for the whole deployment, or one per tenant? This is not a code detail: a single
   identity means every customer's derivatives carry damrs's claim, and a per-tenant identity means
   provisioning and rotating a certificate per tenant. It also decides what a downstream verifier
   learns about who did what.

2. **Where does the production certificate come from?** §16 says "a damrs signing cert in KMS". A
   C2PA signature is only meaningful if the certificate chains to a trust list a verifier accepts,
   which means a real CA and an organisational decision. `c2pa-rs` ships test certificates, so I can
   build and test the whole pipeline against those — but then the code must **refuse to sign with a
   test certificate outside development**, and I want that refusal to be a decision you have made
   rather than one I inferred.

3. **What happens when an inbound manifest fails validation?** This is the one ARCHITECTURE does not
   address at all, and it is the sharpest. A file arrives with a C2PA manifest whose certificate is
   revoked, or whose hash no longer matches the pixels. Three options, with different consequences:
   - **Reject the upload.** Safest for provenance integrity; also means a customer cannot ingest
     their own historical archive if any of it was re-saved by a tool that broke the chain.
   - **Accept, record the failure, do not re-sign.** The asset exists, the broken chain is visible
     in `provenance_manifests`, and derivatives carry no credential. My recommendation.
   - **Accept and strip.** Explicitly forbidden by D13, listed only for completeness.

**What proceeding without a decision would cost.** Guessing (1) means either re-issuing certificates
for every tenant later, or unpicking a per-tenant scheme nobody wanted. Guessing (3) means an ingest
policy with compliance consequences chosen by default rather than deliberately — and it is exactly
the kind of default that is never revisited once assets have been accepted under it.

**What I did instead.** Moved to 1.10 (lifecycle engine), which §6.4 settles fully. 1.9 is the only
M1 task left blocked besides the TUS surface.
