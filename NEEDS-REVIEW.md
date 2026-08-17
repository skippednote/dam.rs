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
