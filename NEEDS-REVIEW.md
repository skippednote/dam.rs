# Open — may an administrator read a private comment? 2026-08-19 (Q.6)

**Q.6a is built with the strict rule — a private comment is readable by its author and the people it names, and
by nobody else — and I want your view before anything relaxes that.**

The strict rule is implemented and mutation-tested. `everything()`, the widest access predicate the system can
compile, does not open a private comment: there is no administrator path to one, deliberately.

I did not decide this the way I decided the engagement disclosures earlier today. There, minimum disclosure was
plainly the reversible direction and I logged it in `DECISIONS.md` and continued. Here both answers are
defensible, and the choice is about what the product *promises*:

- **An admin can read them.** Moderation, legal discovery and offboarding all eventually need it. A DAM under a
  retention or e-discovery obligation cannot have a store of text nobody can produce. And an administrator who
  can already read every asset arguably reads every note about them.
- **An admin cannot read them.** "Private" is a word users act on. If it means "private unless somebody with the
  right role looks", then the honest label is "restricted", and calling it private is a promise the system does
  not keep. Once made, that promise cannot be un-made retroactively — anybody who wrote a private note under the
  strict rule wrote it under the old promise.

What makes this worth stopping on rather than logging: the permissive direction is **not reversible**. Adding an
admin reader later is additive and affects only notes written afterwards, if the UI says so. Removing one is
impossible for everything already written under it.

Three things follow from your answer, and none of them are built yet:

1. Whether `comments::read` and `comments::on_asset` take a "may override visibility" capability at all. Today
   they take a reader identity and nothing else, which is what makes the strict rule structural rather than a
   policy check somebody can forget.
2. Whether an override read is *audited*. If an admin can read private comments, every such read should be a row
   in `audit_log` — an unlogged override is indistinguishable from a leak after the fact.
3. What the UI says when composing a private comment. Under the strict rule it can say "only you and the people
   you name"; under the permissive one it must not.

Until you say otherwise the strict rule stands, and Q.6b/Q.6c will be built on it.

---

# Answered — thumbnails, 2026-08-18 (A.7)

**You said: "We should see thumbnails. We can worry about ai gates later."** So the internal-preview reading is
adopted, and it is implemented as proposed rather than as a shortcut. What shipped:

1. **`Purpose` is signed into the delivery claim** — `Distribution` or `InternalPreview` — and the token format
   version went 2 → 3. A v2 token is now *refused* rather than defaulted, because defaulting a missing purpose
   is wrong in a different direction each way: `Distribution` breaks every preview URL, `InternalPreview` lets a
   token issued before this existed skip the rights check.
2. **`InternalPreview` skips the rights verdict and nothing else.** It is still signed, still verified at the
   chokepoint, still access-checked, still the only path to the bytes. D12's "one code path" holds; the
   chokepoint now knows what it is being asked for.
3. **Three restrictions, enforced at the mint *and* at delivery:** a known built-in profile only (so never the
   original, never a typo, never a future tenant-defined render), an identity is required, and a share link is
   refused outright — a share is distribution by definition.
4. **Downloads are unchanged.** A `Distribution` token over an unlicensed asset is still `403`, and there is a
   test that asserts both on the same asset in the same run: the preview is served and the download is refused.

All seven mutations of those restrictions now fail a test. Two survived the first pass and both were tests
passing for the wrong reason — worth recording, because they are the failure mode this whole discipline exists
to catch:

- The share-link case invented a share id that did not exist, so `shares::is_live` refused the token before the
  preview restriction was ever consulted. It uses a **live** share now.
- The refusal also required the profile's role to be proxy-class, and that branch was **unfalsifiable**: every
  built-in profile is proxy-class, so no test could tell the check from its absence. The branch is gone rather
  than left untested — when a non-proxy built-in exists, that is a decision to make then, with a test that can
  see it.

The AI gates are untouched, as you said. They stay `false` without a licence.

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

---

## `max_downloads` now refuses (Q.12) — a caps enforcement change, not a new decision

**Not blocking.** Implemented, and flagged because it changes outcomes for any tenant that already has a
capped licence scope.

`license_scopes.max_downloads` has existed since migration 0005 and the evaluator has summed
`rights_usage.downloads` against it since the same day. Nothing ever wrote a download row, so the cap permitted
an unlimited number — exactly what that migration's own comment warned about for `max_impressions`: "Without it,
`max_impressions` is decoration."

Q.12 writes the ledger, so the cap now bites. Three consequences worth a human's eye:

1. **A licence that says "50 downloads" starts refusing at 50.** That is what the licence says, and the
   direction is conservative, but a tenant who set a cap believing it decorative will see refusals they did not
   see yesterday. The refusal names `downloads_exhausted`, so it is explicable rather than mysterious.

2. **Historic downloads are not in the ledger**, because nothing recorded them. Every cap therefore starts
   counting from now rather than from the licence's beginning. The alternative — backfilling from `events`,
   where download events *were* recorded — would attribute each to a licence scope by guessing which one applied
   at the time, and a guess in a rights ledger is worse than a gap. If you want the backfill, it is a damctl
   command and a decision about attribution, not a code change.

3. **A download is recorded before the URL is minted**, so a mint that then fails over-counts a cap by one. The
   reverse order would under-count, which permits more than the licence allows. I chose the conservative
   direction; say if you would rather have the other.

**The narrower question I did decide** and would flag if you disagree: when several scopes cover a usage, the
download is attributed to the one with the most headroom, counting an uncapped scope as unlimited. That matches
what `downloads_remaining` reports (a maximum), so the figure a caller watches goes down by one per download. It
also means that while an uncapped alternative covers the usage, a capped scope's allowance is untouched — which
is what "scopes are alternatives" already means in the evaluator, not a loophole added here.

---

## Orders: I chose the design that delegates nothing (Q.13)

**Not blocking.** Implemented, and flagged because the alternative is the one most systems pick and it is an
access-control decision ARCHITECTURE does not settle.

An order lets somebody who may *see* assets but not take them ask for them. There are two ways to make that work:

1. **Approval grants the requester a download right** on those assets, until an expiry. This is a fourth kind of
   grant — neither a role nor a share — with its own scope, its own lifetime and its own interaction with the
   rights evaluation at delivery. ARCHITECTURE settles roles (§7, §12) and share links (3.4); it says nothing
   about this.

2. **Approval leads to a share link.** The requester asks, an approver decides, and fulfilment creates a share
   addressed to the recipients: a token, an optional passcode, an expiry, a download cap, revocation, and rights
   re-evaluated on every delivery. Who may take bytes is answered exactly where it was already answered.

**I implemented (2).** It needs no new concept, it is reversible, and it composes with what the last two slices
added — the order carries the intended use (Q.12), so the pickup's downloads land in the ledger as declared, and
it carries a format (Q.11), so an approver agrees to hand over a 2048px JPEG rather than a 40 MB master.

What you get with (2) that (1) would not give you: revoking a pickup is revoking a share, which already works and
already stops URLs it has issued. What you lose: the requester picks up through the share portal rather than
seeing the assets appear in their own library. If that is the wrong trade for your users, (1) is the change, and
it is a change to the access model rather than to this feature.

**Two narrower decisions inside it**, either of which I will change on a word:

- **Self-approval is recorded, not prevented.** A person who holds the permission to approve does not need an
  order, so a self-approval is either a tenant where that is the normal path or something a reader of the trail
  should see. `self_approved` is on every order for that reason. Prohibiting it would be inventing a policy that
  belongs to a tenant.

- **An approver cannot approve an order containing assets outside their scope** — they get a 403 with a count.
  Agreeing to hand over something you cannot inspect is a signature on a blank page. Rejection has no such
  requirement, because otherwise an order could reach a state nobody is able to close.

**Not built yet:** fulfilment. An approved order sits at `approved` with no share, and the interface says
"the pickup is being prepared" rather than pretending otherwise. That is the next slice: packaging, the
multi-asset portal view (the portal currently handles single-asset shares only), and the metadata export.

---

## The metadata export stops at the tenant's edge (Q.13d)

**Not blocking.** Implemented the narrow way, and flagged because the wider way is what Acquia does and it needs a
concept damrs does not have.

An order can be placed with `include_metadata`, and `GET /orders/{id}/metadata.csv` gives the requester (or an
approver) a spreadsheet of the ordered assets: the file facts plus every declared field, one column per
`field_defs.key`. That is safe by construction — somebody signed in is exporting metadata they can already read
in the DAM, which is not a disclosure at all.

**What it deliberately does not do is put that CSV in the pickup.** An external recipient collecting an order
would then receive descriptive metadata — captions, credits, internal notes, whatever a tenant keeps in its own
fields — and `field_defs` has no notion of which fields an outsider may see. The options were:

1. **Invent a visibility flag** (`field_defs.external_visible`, say) and default it closed. Then the export is
   useful in the portal for the fields somebody deliberately opened, and every tenant has a new column to
   curate. This is the real answer, and it is a schema change plus a screen.
2. **Export everything to the portal.** One line of code, and the first time a tenant discovers their internal
   notes went to an agency it is a support call at best.
3. **Keep it inside the tenant** — what I did. The requester gets the spreadsheet and forwards what they choose,
   which is the status quo made explicit rather than automated.

If you want (1), say so and it is a slice: a column, an admin toggle beside the existing field settings, and the
portal gaining the export. I did not want to pick a default for what an outsider may read.

---

## The built-in `admin` role's permissions are decorative (found while designing M5a·4)

**Blocking a decision, not a slice.** Nothing is broken today in a way that grants too much — the failure is the
safe direction — but a built-in role that confers nothing is a bug somebody will hit, and the fix widens access,
which is not mine to choose.

`provision.rs` seeds three roles. The administrator's permissions are written as wildcards:

```
("admin", "Administrator", vec!["asset:*", "metadata:*", "tenant:*", "rights:*"], true)
```

Nothing expands them. `Grant::permits` is exact string equality (`crates/dam-core/src/policy.rs:93`), and so is
the narrowing check a feature does against `caller.permissions` (`dam_db::conversions::permitted_for`). So a
person assigned the built-in `admin` role, and *not* flagged `is_tenant_admin` on their membership, holds no asset
permissions at all: `asset:*` never matches `asset:read`.

Why it has not shown up: `auth.rs` synthesises `asset:read`/`asset:download`/`asset:manage` for
`is_tenant_admin` members directly, which is the path every test and every live check has taken. The role row is
only consulted for members who are *not* tenant admins, which is exactly the case the wildcards were written for.

Three ways out, and they mean different things:

1. **A wildcard matcher** — one `holds(held, wanted)` helper used by both `Grant::permits` and every
   `caller.permissions` check, matching `asset:read` against a held `asset:*`. This makes wildcards a real
   feature of the roles table, available to a tenant's own roles, and it means a role written today
   automatically confers permissions invented next year. That is what a wildcard means; it is also a widening
   that nobody has agreed to.
2. **Enumerate the seed** — replace `asset:*` with the three strings that exist. Smallest change, no new
   semantics, and every future permission has to be added to the seed by hand. A tenant who wrote `metadata:*`
   into their own role still gets nothing, silently.
3. **Refuse wildcards at the edge** — a CHECK or an API validation that rejects a `*` in `roles.permissions`, so
   the table cannot promise what the code does not honour, and fix the seed as in (2).

ARCHITECTURE §7 and §12 do not settle this: they specify one compiled predicate and the coarse `Action` axis,
and say the fine-grained strings are for narrowing — neither says whether a string may be a pattern. I have
changed nothing and added no matcher.

My recommendation is (1) plus (3)'s validation *inverted*: make wildcards real, and document that a wildcard
confers future permissions in its namespace, because that is what an administrator role is for. But it widens
access, so it waits for you.

---

## A portal backed by a live query would publish assets nobody decided to publish (Q.14)

**Blocking one source, not the feature.** Portals ship: `migrations/tenant/0030_portals.sql` has the table, the API
creates them, and the four Acquia kinds (Standard, Brand, Video, Channel) are there as presentations. What is
refused is one of the three sources the schema anticipates, and the reason is an access-control question
ARCHITECTURE does not settle.

A portal is visible to people with **no account**. Its set can come from three places:

1. **A collection** — implemented. Somebody with Manage put each asset in it. That act *is* the publication
   decision, and it is made per asset by a person.
2. **A saved search** — refused. A saved search is a live query, so a portal backed by one publishes every future
   asset that happens to match. Nobody decides; a rule does. An asset uploaded next month with `brand:acme` in
   its metadata would appear on a public page because a query written in March said so.
3. **A media class** ("every video") — refused, for the same reason and more broadly.

Rights still bite: every preview and download is evaluated at the delivery chokepoint, so an unlicensed asset is
listed and cannot be taken. That is a real mitigation and not a sufficient one — an unreleased campaign under
embargo is often *licensed*, and the harm is the exposure rather than the download.

Three ways to make the live sources safe, and they are different products:

1. **A publication flag per asset.** `assets.published_at`, set by a person, and a live-query portal shows only
   assets that carry it. The query then narrows an explicitly published set rather than defining one. This is the
   version I would build, and it is a migration plus a bulk action plus a column on the grid.
2. **A rights floor.** Only assets whose `rights_state = 'allowed'` for `web`/`WORLD` appear. Cheap, and wrong in
   both directions: it publishes anything with a broad licence, and hides cleared assets whose evaluation is stale.
3. **Accept it, and say so loudly.** The tenant chose a live query; the portal screen warns that new matching
   assets will appear publicly, and the audit log records each first appearance. Fastest, and it makes an
   irreversible disclosure depend on somebody reading a warning.

Both are fields on the create request and both answer 422 with a sentence naming this note — asking gets the
decision back rather than a complaint about a missing field. Tell me which of the three you want and it is a
slice; I did not want to pick a default for what becomes visible to the public internet.
