# Your DAM bill grows with your library. Your library only grows.

The first cost model I wrote for dam.rs had an uncomfortable result: a healthy digital asset library and an expensive one looked identical. More people could find the work, more originals were retained, and more sites consumed approved renditions. Every sign that the system was succeeding also pushed one of the common commercial meters upward.

> [!TLDR]
> DAM pricing often scales with seats, asset count, stored bytes, and delivery traffic, while a useful archive is expected to grow on all four axes. A durable cost model separates the organisation's real infrastructure consumption from the amount of work it has entrusted to the product. Storage ownership, searchable cold originals, and portable metadata do not make a DAM free, but they stop retention and exit from becoming the vendor's leverage.

This is not an argument that every hosted DAM is overpriced. For a small team, paying someone else to operate the database, media pipeline, security updates, and support desk is usually sensible. The problem appears later, when pricing is tied to commitment while the product's mission is to deepen that commitment.

## The meter and the mission point in opposite directions

A DAM exists because files accumulate faster than people can remember where they put them. A successful year adds campaigns, product launches, territories, photographers, agencies, releases, and revisions. The archive grows because the organisation is working, not because somebody forgot to clean it.

Four meters commonly appear in commercial plans. None is irrational by itself. Their combined behavioural effect is the issue.

| Meter | What the invoice observes | What the organisation starts doing |
|---|---|---|
| Named seats | How many people can enter | Share logins, exclude agencies, or route work around the DAM |
| Asset count | How many records remain | Delete old work before its future value is known |
| Stored bytes | How much quality and history is retained | Drop RAW files, mezzanines, or previous versions |
| Delivery traffic | How much the library is reused | Cache outside governance or send files through side channels |

The behaviour matters because it damages the properties that justified buying a governed system. Shared credentials weaken attribution. Files sent through email and generic file shares fall outside rights checks. Deleting an old model release can remove the evidence needed to explain why an image was used. A pricing limit becomes an architecture decision, made indirectly and usually without an architecture review.

```press-diagram
{"type":"mapping","title":"Two cost models","fromLabel":"commitment meter","toLabel":"operating cost","from":[{"label":"named seats"},{"label":"asset count"},{"label":"stored bytes"},{"label":"traffic"}],"to":[{"label":"identity ops"},{"label":"database work"},{"label":"storage class"},{"label":"network bytes"}],"links":[[0,0],[1,1],[2,2],[3,3]],"footer":"Commitment and consumption are related, but they are not the same unit."}
```

The mapping on the right is not automatically cheaper. It is simply inspectable. A storage bill can be reconciled to object placements and classes. A database bill can be reconciled to queries, backups, and compute. A per-seat tier is harder to connect to marginal cost, especially when the next person needs read-only access for one week.

## Deletion is not a complete cost-control policy

The obvious answer to asset-count and storage limits is to retain less. That works for disposable material. A governed library contains several categories that are not disposable on the same schedule as their original campaign.

### Rights evidence outlives current use

The question in a dispute is often not whether an image may be used today. It is whether the organisation had the necessary licence and releases when it used the image two years ago. Deleting the evidence because the creative is no longer active turns a storage saving into an evidentiary gap.

dam.rs therefore models rights as data with its own lifecycle. Licences, scopes, releases, intended-use declarations, and consumption records do not collapse into one badge on the asset row. A legal hold can also block distribution and deletion independently of ordinary retention.

### Provenance is a graph, not a label

A web rendition may descend from a retouched master, which descended from a camera original, with a detached credential and a sequence of transformations between them. Removing an intermediate record can leave the output bytes intact while making the explanation of those bytes incomplete.

### Historical versions answer future questions

Version history looks redundant until someone asks which logo was approved for a market, which crop a partner received, or whether the image on a page predates a withdrawal. "Nobody opened it recently" is not the same predicate as "nobody will need to establish what happened."

Deletion still belongs in the system. It needs policy, holds, review, and an audit record. Treating it as the main response to a pricing tier delegates those decisions to the invoice.

## Exit cost compounds with tenure

Annual licence cost is visible. Exit cost tends to remain hypothetical until the organisation tries to leave, by which point the library is largest and the integrations are most numerous.

### Bytes have to cross a boundary

If the vendor stores the masters, migration begins with a full egress. Derivatives may be reproducible, but reproducing them requires the exact transform definitions, colour profiles, codecs, and versioned behaviour that created them. Pulling existing renditions may be operationally safer, and it multiplies the bytes that must leave.

### Metadata exports preserve values more easily than semantics

A CSV can contain `editorial`, `WORLD`, and `approved`. It may not contain the rules that made those values meaningful: whether exclusions override inclusions, which vocabulary terms are deprecated but still resolvable, or whether an approval applies to the asset, one version, one channel, or one portal.

That distinction shaped dam.rs. The data model stores licence scopes, exclusions, validity windows, release status, declared use, and the resulting consumption ledger. The goal is not to make the schema elegant. It is to ensure an export can preserve decisions rather than only strings.

### URLs become part of other systems

CMS pages, partner portals, feeds, and design tools reference delivery URLs. A migration either rewrites those references or keeps a redirect and compatibility layer alive. The more useful the DAM becomes, the more callers depend on its identifiers.

Exit cost therefore grows with the same things that make the library valuable: history, structure, and reach. That is the lock-in mechanism worth modelling. A negotiated discount changes the slope of the current invoice. It does not remove the compounding migration surface.

## Measure consumption as levels and flows

The alternative is not "do not meter." An operator needs to know what a tenant consumes, and hard limits are appropriate for some variable-cost operations. The important part is to meter units that correspond to work the system or provider actually performs.

dam.rs separates levels from flows:

- A **level** is a current state, such as asset count or bytes stored by class.
- A **flow** is an event during a period, such as downloads, restore bytes, or model tokens.

That difference prevents a subtle accounting error. Today's storage level cannot be reconstructed accurately for an arbitrary day last March unless the system captured the level then. Download events can be counted historically because each event has a timestamp.

The metering query reads the current placement table for storage and the event ledgers for daily activity:

```sql
SELECT storage_class,
       coalesce(sum(size_bytes), 0)::bigint AS stored_bytes
FROM object_placements
WHERE state = 'present'
GROUP BY storage_class;

SELECT count(*) AS downloads
FROM rights_usage
WHERE source = 'download'
  AND recorded_at::date = $1;
```

The resulting daily row is upserted on `(tenant_id, day)`. Re-running a metering job replaces the measurement instead of adding it again. A worker retry should correct a report, not create a second invoice.

> [!IMPORTANT]
> Metering must use the operator's full tenant view, not the requesting user's filtered view. A curator who may see 30 percent of a library still belongs to a tenant storing 100 percent of its bytes. Using an access-scoped count for billing would make the same tenant have several different bills.

The quota schema allows each unit to choose soft or hard enforcement. Hard caps are reasonable for optional AI enrichment or restore spend. They are dangerous for ingest, where refusing an upload at the limit can strand active production work. One global "over quota" switch cannot express that distinction.

## The ownership boundary changes the curve

dam.rs puts originals and derivatives in an S3-compatible bucket selected by the operator. Metadata lives in Postgres. Search indexes can be rebuilt from that database. The application coordinates those systems, but it does not need to own the storage account.

That changes three parts of the cost curve.

### Storage classes become an operational choice

Large originals can move to an archive class while searchable metadata, thumbnails, and proxies stay hot. The cost then follows the actual temperature of the bytes instead of treating a rarely opened master and a frequently rendered thumbnail as identical storage.

### Delivery is a provider cost, not a product-success penalty

The organisation still pays for requests and network transfer. The difference is that those charges are visible in the cloud account, can use existing commitments, and can be changed by architecture. A CDN, a different object-store provider, or a regional design can alter the bill without renegotiating the DAM licence.

### There is no bulk export event for the objects

Stopping the application does not copy the bucket somewhere else. The objects are already in the account chosen by the operator. Migration still has real work: preserve metadata semantics, replace integrations, and stand up another delivery layer. It does not begin by buying back every byte.

## A cost model that survives contact with operations

I would compare hosted and self-operated DAMs over at least three years and include five categories:

| Category | Hosted model | Storage-owning model |
|---|---|---|
| Product fee | Subscription, tiers, overages | Software support or engineering |
| Infrastructure | Often bundled and opaque | Database, object storage, compute, CDN |
| Operations | Vendor-operated | Backups, upgrades, monitoring, incidents |
| Governance | Configuration and process | Configuration, process, and code ownership |
| Exit | Export, egress, remapping, URL migration | Metadata and integration migration |

Use at least three library-growth scenarios. A single average hides the shape that matters. Model a conservative case, the expected case, and a retention-heavy case where originals and versions accumulate faster than delivery traffic.

Do not count self-hosting labour as zero. Postgres needs tested restores. Object-store policy needs review. Signing keys and KMS keys need rotation. Media tooling needs security updates. Someone has to respond when a worker queue stops advancing at 02:00. A hosted service may be the cheaper choice once those costs are priced honestly.

The useful decision boundary is control. If a small library has ordinary rights requirements and no dedicated operator, buying the service is rational. If storage class, retention, data residency, enforced rights, or exit cost are strategic constraints, owning the data plane can be worth the operational load.

## What we rejected

We rejected two neat answers because neither survives the actual workload.

The first was "everything stays in Standard forever." It makes retrieval simple and makes archival economics irrelevant. It also charges the warm rate for a body of masters that becomes colder every month.

The second was "archive old assets outside the DAM." That saves on the main system while creating a second library with weaker search, rights, and provenance. Users then keep local copies of anything they may need quickly, and the organisation pays for cold storage plus an ungoverned warm shadow.

The workable middle is to keep the asset record and browsing substrate online while changing only the latency of the original bytes. That design is the subject of the next article, because getting the state model wrong turns a cheap object into a missing asset.

## FAQ

### Is a self-hosted DAM always cheaper than SaaS?

No. Small libraries often benefit from a hosted service because specialist operations, upgrades, and support are bundled. Self-hosting becomes compelling when storage, retention, rights enforcement, residency, or exit control outweigh the cost of operating the stack.

### Which DAM pricing metric creates the most lock-in?

Stored bytes and asset count create visible growth, but metadata semantics and integration URLs usually make an exit hardest. Those dependencies grow quietly and cannot be solved by downloading the masters alone.

### Should old digital assets be deleted to control cost?

Only under an explicit retention policy that accounts for rights evidence, legal holds, provenance, and historical versions. Recent access is useful input, but it is not a sufficient deletion rule.

### What should a DAM meter instead?

Meter inspectable consumption: current bytes by storage class, database and compute load, network transfer, restores, and other variable-cost operations. When the bill follows work the system actually performs, library growth remains an operational fact instead of becoming a penalty for trusting the product.
