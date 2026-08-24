# The green badge is not permission

The rights screen was already green when I found the hole. The asset showed a valid licence, the intended territory matched, and the download control looked correct. Then I followed the URL instead of the interface and reached the more important question: what recomputes that answer when the licence changes after the URL has been issued?

> [!TLDR]
> Digital asset rights enforcement must happen at the delivery boundary, not only in the user interface or when a signed URL is minted. A signature proves that a request was issued intact; it does not prove the asset is still licensed for this channel, territory, identity, tenant, or share. Re-evaluating current rights before every short-lived object-store redirect makes expiry and revocation apply to URLs already in circulation.

A rights badge is valuable communication. It tells a person whether the system expects a use to be allowed and gives them a place to inspect the reason. It becomes dangerous only when the architecture treats the badge as the control.

The simplest test is blunt: if somebody ignores the badge and calls the download route directly, what stops the bytes?

## Every upstream check can be bypassed

Rights controls are often added where they are easiest to see. Those locations are useful, but none is sufficient on its own.

### A badge is advice

A person can miss it, misunderstand it, or use another client. An API consumer may never render it. The colour cannot be the enforcement boundary because colour does not participate in delivery.

### A disabled button protects one interface

If the underlying route remains callable, disabling the button changes the page rather than the permission. A user who has seen one delivery URL may also understand enough of its shape to construct another.

### A handler-level check protects one route

A DAM has many ways to release bytes: a direct download, a share, a portal, a bulk package, a CMS rendition, a connector, or a transformed preview. If each route remembers to call a policy helper, the real policy is the set of routes whose authors remembered.

### A signed URL can preserve the wrong decision

A signed token proves that the signed bytes were not altered. If the token contains `allowed=true`, the signature preserves a verdict made in the past. A licence withdrawn one minute later remains effectively valid until that token expires.

The safer interpretation is narrower: a signed URL is permission to attempt delivery under specific terms. Entitlement is computed from live state when the attempt arrives.

## One authorisation path, then a short redirect

dam.rs routes downloads and renders through one delivery handler. The object bucket is private. After the handler verifies the request and evaluates current policy, it returns a presigned object-store URL with a 30-second lifetime. The application does not proxy large media bodies, but it does control the only supported path to a bearer credential that can fetch them.

```press-diagram
{"type":"sequence","title":"Delivery decision","actors":["client","delivery","Postgres","object store"],"messages":[{"from":0,"to":1,"label":"signed claim"},{"from":1,"to":1,"label":"verify HMAC"},{"from":1,"to":2,"label":"load rights"},{"from":2,"to":1,"label":"current terms","reply":true},{"from":1,"to":3,"label":"presign GET"},{"from":3,"to":1,"label":"30s URL","reply":true},{"from":1,"to":0,"label":"302 redirect","reply":true},{"from":0,"to":3,"label":"GET bytes"}]}
```

The order is deliberate:

1. Verify the signature and token format.
2. Refuse a token for another tenant.
3. Re-check any share or connector state.
4. Re-check identity access.
5. Evaluate current rights for the signed channel and territory.
6. Resolve the exact signed transform.
7. Issue the short object-store redirect.

Checking rights before resolving the rendition also avoids an existence oracle. A caller with no entitlement should not learn whether a particular derivative exists by observing a different error.

The 30-second redirect is a concession, not a proof of perfect revocation. Once issued, that object-store URL is a bearer credential the application cannot call back. The window is sized for a browser to follow a redirect rather than for a person to reuse it later. Immediate revocation is therefore bounded by those seconds, while the longer-lived dam.rs token is re-evaluated on every use.

## Carried terms, computed verdict

The distinction between carried and computed data is the core of the design.

The token carries facts about the request:

- tenant and asset identity;
- distribution or internal-preview purpose;
- requested transform;
- channel and territory;
- user identity, when present;
- share-link identity, when present;
- expiry and signing-key identity.

The token does not carry the effective rights verdict. That verdict depends on mutable records: licences, scopes, releases, legal holds, usage caps, membership, share revocation, and time.

The hardened claim type makes the boundary visible:

```rust
pub struct DeliveryClaim {
    pub tenant_id: Uuid,
    pub asset_id: Uuid,
    pub purpose: Purpose,
    pub transform: String,
    pub channel: String,
    pub territory: String,
    pub identity_id: Option<Uuid>,
    pub expires_at: DateTime<Utc>,
    pub share_link_id: Option<Uuid>,
    pub key_id: String,
}
```

Each signed field closes a concrete edit.

### Transform prevents thumbnail escalation

If `transform` sits outside the signature, a caller can change `thumb-256` to `original`. The signature still verifies because it never covered the part that selects the bytes.

### Channel and territory select the contract terms

A licence can allow editorial use and refuse advertising, or allow one market and exclude another. Leaving those fields editable lets the caller select the rule under which the request will be judged.

### Purpose prevents a policy downgrade

dam.rs distinguishes distribution from an internal preview. A signed-in tenant member may need a small proxy in the asset grid before a licence has been attached. That is internal cataloguing, not publication.

The exception is narrow: internal preview requires a named identity, refuses share links, and permits only known proxy-class transforms. It can never request the original. Purpose is signed so a distribution request cannot be edited into `InternalPreview` to skip the rights verdict.

### Share identity makes revocation reach issued URLs

Revoking a share page is not enough if that page has already minted delivery tokens. Carrying the share ID lets the delivery handler look up its current state. Revocation then affects outstanding tokens instead of waiting for their own expiry.

### Tenant identity fixes the namespace

Asset UUIDs are meaningful only inside a tenant. A token without a tenant relies on deployment configuration to decide which schema and object prefix to use. Two deployments sharing a signing key, such as a staging environment restored from production or a disaster-recovery site, can then accept each other's valid tokens.

If the same asset UUID exists in both libraries, the signature remains valid while the receiving deployment resolves the wrong tenant's bytes. The clean audit trail is part of the problem: nothing looks forged.

The tenant now appears in the canonical signed payload before the asset ID and is compared with the tenant served by the delivery process. A mutation test removes that comparison; the expected 404 becomes a 302 and the file is served. That is the kind of test worth keeping because it demonstrates the exploit, not only the intended branch.

## The signature format has to preserve meaning

Signing the right fields is insufficient if two different claims can serialize to the same byte sequence.

A delimiter format can be ambiguous. If fields are simply joined with `|`, a value containing the delimiter or a shifted boundary can make distinct claims render identically. The dam.rs token uses a version byte and length-prefixed fields. Distinct field sequences therefore produce distinct canonical payloads.

Token versions are refused rather than guessed. Adding tenant identity changed the meaning and layout of every field after it. Reading an old token under the new layout could interpret asset bytes as a tenant UUID and shift the remainder into plausible but false values. The signature would still match the byte string. It cannot tell the parser what those bytes meant when signed.

That is why a format version is a security field. When the layout changes, old tokens fail as `WrongVersion`. The maximum token lifetime is 24 hours, so the compatibility cost is bounded and smaller than supporting a layout that lacks a new security property.

HMAC comparison is constant-time through the `subtle` crate. The claimed key ID is used only to select candidate verification keys; it confers no authority before the MAC matches. A keyring retains retired keys long enough to verify outstanding tokens, while new tokens use the first active key.

## Rights are an intersection, not a friendly merge

An asset may have several licences and releases. Combining them with a union would let one permissive record erase restrictions from another. dam.rs takes the intersection: the most restrictive applicable term wins.

The evaluator handles several rules that are easy to state incorrectly:

- Unknown rights deny distribution.
- An excluded territory beats a `WORLD` inclusion.
- A licence with no scopes grants nothing.
- A scope with an empty channel list means all channels.
- Legal hold blocks distribution as well as deletion.
- An expiring licence is a distinct verdict so renewal can happen before denial.
- Download and impression caps are evaluated against recorded use.

The exclusion order appears directly in code:

```rust
fn covers_territory(&self, territory: &str) -> bool {
    if self.excluded_territories
        .iter()
        .any(|t| t.eq_ignore_ascii_case(territory))
    {
        return false;
    }
    if territory.eq_ignore_ascii_case(WORLD)
        && !self.excluded_territories.is_empty()
    {
        return false;
    }
    self.territories.iter().any(|t| {
        t.eq_ignore_ascii_case(WORLD)
            || t.eq_ignore_ascii_case(territory)
    })
}
```

The second condition matters. A caller asking for `WORLD` cannot be satisfied by "worldwide except China." Checking only whether the inclusion list contains `WORLD` would turn an explicit exclusion into permission.

## Refuse forgery and policy differently

A flat 404 is useful for malformed, expired, bad-signature, wrong-tenant, and revoked-share tokens. Telling an unauthenticated caller which component failed helps them refine the next attempt and may reveal whether an asset or share exists.

A rights denial is different. The caller is often an authenticated employee who can see the asset and needs to understand why distribution is blocked. Returning "not found" for an asset visible in the adjacent panel does not protect anything. It produces a support request.

dam.rs therefore returns a 403 with machine-readable rights state and reason codes such as `no_license`, `legal_hold`, expired licence, excluded territory, or disallowed channel. The UI can explain what needs correction without parsing prose.

The rule is:

- Collapse distinctions about existence until the caller is established.
- Explain distinctions about terms to a caller already entitled to inspect the asset.

That division also helps monitoring. A rise in signature refusals suggests scanning, clock skew, or key rotation trouble. A rise in rights denials suggests expired contracts, missing releases, or a client asking for the wrong intended use. Combining them into one status would hide both diagnoses.

## What this design costs

Every delivery performs indexed reads and policy evaluation before object-store I/O. Rights caches can reduce the work, but invalidation must fire on every input that changes the verdict. A stale cache is simply a carried verdict under another name.

The single delivery boundary also demands discipline. A future bulk exporter cannot read object keys directly because doing so is convenient. Connectors cannot mint unrestricted S3 URLs. The code review question for any feature that releases media is "where is its delivery claim?"

The internal-preview exception needs continued pressure. New transforms must not automatically become preview-safe. Anonymous access must remain distribution. A tenant-defined conversion may be perfectly small and still encode sensitive or full-resolution material, so only built-in profiles are accepted.

Finally, this is rights enforcement software, not legal interpretation. The system can apply terms it has been given. It cannot determine whether a contract was entered correctly, whether a jurisdiction recognises it, or whether the requested channel vocabulary matches counsel's intent. The data model makes those decisions enforceable; it does not make them for the organisation.

## FAQ

### Is a signed URL proof that an asset may be downloaded?

No. It proves that the signed request was issued and not altered. Current licence, release, access, share, tenant, and hold state still need evaluation when the URL is used.

### Where should DAM rights enforcement happen?

At the point that authorises bytes to leave private storage. UI badges and issue-time checks should provide early feedback, but the delivery boundary must recompute the verdict.

### How can rights revocation affect an existing URL?

The long-lived token carries request terms rather than an `allowed` verdict. Each use loads current rights and share state, so a lapse, withdrawal, or legal hold changes the response without changing the token.

### Why allow previews of an unlicensed asset?

A freshly uploaded or migrated asset may need internal cataloguing before rights data is complete. A small, identity-bound, non-shareable proxy can support that work without permitting distribution of the original. The exception stays safe only because its purpose and transform are signed and narrowly constrained.

If ignoring the green badge still reaches the master, the product has a rights interface. It does not yet have rights enforcement.
