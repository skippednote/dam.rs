# Acquia DAM: what it does, and what damrs owes it

Surveyed 2026-08-19 against a live Acquia DAM tenant (`axelerant.acquiadam.com`, the "Axel Paints" demo
content, 136 assets, 27 products). Read-only: every page below was viewed, no setting was changed and
nothing was created, sent or deleted in that tenant.

This document exists because "reach parity with Acquia" is not actionable until the feature set is written
down. It is the inventory, then the gap against damrs, then the order to build in. The order matters more
than the list: several of these are already half-built in damrs's schema, and several belong to milestones
ARCHITECTURE §13 already names — Acquia's catalogue turns out to be mostly a *concretisation* of M4–M6 and
Pre-GA rather than a set of surprises.

## The shape of the product

Acquia DAM is six applications behind one app switcher, not one application with sections:

| Application | What it is | damrs equivalent |
|---|---|---|
| **Assets** | The DAM proper: search, asset detail, upload, collections, categories, activity | `web/src/routes/assets`, `dam-api`, `dam-search` |
| **Entries** | A PIM: products with SKUs, types, variants, channel distribution | nothing |
| **Insights** | Analytics: custom dashboards + seven report types, exportable | nothing (M6 names "analytics") |
| **Portals** | Branded distribution sites — Standard, Brand, Video, Channel | the share portal, one page of it |
| **Workflow** | Requests → projects → deliverables, with approvals and priorities | nothing (M6 names "workflow/proofing") |
| **Admin** | Nine setting groups; the configuration surface is the feature surface | `/schema`, `/settings`, scattered |

That the product is *six applications* is itself the finding. damrs is currently one application, and several
Acquia features are only coherent as a separate surface with its own navigation (Insights, Workflow, Entries).

## The feature catalogue

Taken from Admin → Features, which is Acquia's own enumeration, grouped as they group it. Marked **[AI]**
where Acquia labels it an AI feature, **[$]** where it is a paid add-on.

### Analyze
- Asset insights (asset-level usage data on the detail page)
- Collect intended use — on external collections, on order pickup, and within the site (three separate
  switches for the same question: *what will you use this for*, asked before download)
- Export metadata with an order (CSV/TXT of every asset's metadata)
- Google Analytics linkage

### Collaborate
- Asset approval
- Favourite assets (and favourites rank first in search)
- Public and private comments, routed to chosen recipients, with statuses that mean approval
- Rate assets 1–5, average shown in search results and on the detail page

### Integrate
- REST API **[$]**
- Webhooks (real-time change notification for downstream sync)
- Hootsuite **[$]**

### Manage assets
- Activity feed on the dashboard (uploads, shares, comments)
- AI Copilot — conversational access to the DAM **[AI]**
- AI document summaries **[AI]**, AI facial recognition **[AI]**, AI tags with backfill **[AI]**,
  AI video transcripts **[AI]**
- Asset versioning (retain, add, access and download previous versions)
- Attached documents (rights or release paperwork attached to an asset)
- Closed captioning for video
- Cropping; cropping with multiple asset views (a crop becomes the default view used in search results,
  share links and embeds); **smart crop** — focal-point detection **[AI]**
- Duplicate detection by look, with a backfill over existing assets
- Embed metadata into XMP on download; embed star ratings into XMP
- FTP batch upload
- Mandatory required fields in the uploader
- Primary/secondary metadata sections during upload
- Dependent metadata fields in Refine Search
- Tags

### Manage users
- SAML integration **[$]**, simple one-way SSO **[$]**
- Global new search experience switch

### Search
- Advanced search (date ranges, formats, metadata values; saveable; results exportable)
- Document text search (full text of PDF and Word)
- Multiple asset search (a pasted list of filenames or metadata values)
- Natural language search — semantic, triggers a reindex **[AI]**
- Predictive search (as-you-type matches, only from released, unexpired, complete assets)
- Search suggestions ("did you mean?")
- Substring search (match inside words)

### Secure
- Archive to Glacier, retaining search and ordering **[$]**
- Digimarc watermarking with use-on-the-web reports **[$]**
- Hide expired order information

### Share
- Collections, shareable
- Export search results to CSV (comma and semicolon variants, separately switchable)
- Portals **[$]**
- Share with multiple recipients

### Applications
- Entries **[$]**, Mobile **[$]**, Syndicate **[$]**, Templates (web-to-print) **[$]**,
  Video Creator **[AI][$]**, Workflow **[$]**

## The admin surface

Nine groups. Each is a feature in its own right, and several name concepts damrs has no word for.

| Group | Pages |
|---|---|
| Global | Admin dashboard, API setup, Dashboard messages, Features, Site branding, Uptime report |
| Permissions | **Asset groups**, **Roles** |
| Users | Default user settings, Deleted users, **Registration codes**, **Registration page**, User administration |
| Search | **Asset categories**, **Auto-import mappings**, Deleted assets, **Metadata types**, **Refine search** |
| Upload | **Attached documents**, Auto-import mappings, **Upload profiles** |
| Order | **Asset conversions**, **Asset deliveries**, **Asset orders**, **Embed metadata**, **Intended use**, Share links |
| — | Path management, Tags management, AI Tags management |

Three of these are structural and worth spelling out:

**Metadata types.** Fields are grouped into *types* bound to a kind of asset — the tenant surveyed has
Archives (1 field), Document (0), Image (12), Video (0), plus two custom types. An asset's detail page names
its type ("Metadata type: Image"). damrs has one flat `field_defs` per tenant, so every field applies to
every asset: a video carries the print-resolution fields and an archive carries the alt text.

**Asset conversions.** Named, user-selectable download formats, per media class, each with a description
written for the person choosing and **its own role permissions**: "JPG (Small) — Email/Web/PPT, 72 dpi, RGB,
800px", "TIFF — High Res for print", "Audio MP3 160kbps". damrs has three internal derivative profiles for
its own UI and no concept of a download format a user picks.

**Orders.** Downloading in bulk is an *order*: it can require approval, it produces a pickup page, the
pickup page can carry a metadata export, it can be shared with multiple recipients, it expires, and expired
orders can hide their contents. The admin dashboard tracks "Unapproved Orders" and "Assets Never Ordered".
damrs has share links, which are one recipient, one asset, no approval and no pickup.

## The admin worklists

The admin dashboard is a set of queries over the library, each a link to the assets that fail it. They are
cheap for damrs to add and they are the difference between a library that is governed and one that merely
has governance features:

Users: Expired/expiring users · Pending registration requests · Unapproved orders · Users without roles.
Assets: Never ordered (112) · Empty required metadata (107) · Without categories (61) · Conflicted uploads
(5) · Expired (17) · Lost (0) · Pending delete (32) · Unreleased/pending admin approval (1).

## Search, as the user meets it

The refine rail, in order: Categories with counts · Search Within (keyword inside the current result set) ·
Document Search (a checkbox: search document text) · File Types with counts · Metadata, then one facet per
facetable field (Application Areas, Asset Type, Available PAN India, Channel, Image Type, Market/Region,
Price, Region, Shipping Cost, Usage Rights, Year) · **Asset Status** · **Orientation** · **Average Rating** ·
**Has Attached Document**.

Each result card carries: a star rating, Download, Share, its **flags** (asset-group membership, shown as a
labelled flag), and Watch · Edit · Delete. Above the grid: Select All, the count, sort, view mode, Export.

The four bolded facets are built-in rather than metadata-derived, and damrs has none of them. Orientation is
free — it is a function of the dimensions already stored.

## Asset detail

Tabs: **Details · History · File info · Attachments · Versions**. Actions: Select · Share · Download ·
Insights · More (Add version · Upload alternate preview · Delete). The Details tab is Categories, Tags,
AI Tags (with a "Generate AI tags" button), then Metadata with a "Show empty fields" toggle and required
fields marked.

damrs's detail panel has metadata, rights, provenance, technical and now sharing. It has no history, no
attachments, no versions tab, no per-asset insights, and no way to add a version.

## Gap analysis

### Already in damrs
Multi-tenancy, API keys, roles and permissions, asset groups, the access predicate, resumable upload,
derivatives, field definitions and validation with schema administration, taxonomies, Tantivy search with
facets and a shorthand query language, saved searches, a relevance eval harness, licences and rights
evaluation at the delivery chokepoint, consent and DPIA and erasure records, signed delivery URLs, share
links with a portal, collections, storage tiers with restore requests, retention and lifecycle policies,
C2PA-style provenance, bulk operations, an audit log, an event table, a job queue and worker.

### Tables exist, behaviour does not
`asset_tags`, `tag_feedback`, `ai_models`, `ai_disclosures`, `asset_image_embeddings`,
`asset_text_embeddings`, `asset_text`, `asset_faces`, `asset_phashes`, `duplicate_candidates`,
`term_embeddings`, `asset_colors`, `enrichment_runs`, `webhook_subscriptions`, `webhook_deliveries`,
`import_jobs`, `import_records`, `connectors`, `connector_asset_refs`. M4/M5/Pre-GA are largely "write the
behaviour over the schema that is already there".

### Missing outright
1. Metadata types bound to asset kinds
2. Asset conversions — named download formats with per-role permissions
3. Orders: approval, pickup page, metadata export, multiple recipients, expiry
4. Intended-use capture before download, and its reporting
5. Ratings (and rating as a facet, and rating in XMP)
6. Comments — public/private, routed, with approval statuses
7. Favourites, and favourites ranking first
8. Watch/subscribe plus the notification that makes it mean something
9. Versions: add, list, download an earlier one
10. Attached documents
11. Cropping, multiple asset views, smart crop
12. Closed captions
13. Alternate preview upload
14. Portals as a product (Standard/Brand/Video/Channel)
15. Workflow: workgroups, workflow definitions, intake forms, email templates, requests/projects/deliverables, priorities
16. Entries: products, SKUs, variants, attributes, channels
17. Insights: dashboards and the seven report types (downloads, asset views, storage, user count, searches, logins, uploads)
18. Hierarchical asset categories, distinct from taxonomies
19. Upload profiles
20. Auto-import mappings (embedded metadata → fields)
21. Embedding metadata into XMP on download
22. Advanced search, multiple-asset search, search-within, substring, predictive, did-you-mean, document text, semantic
23. Export search results to CSV
24. Refine-search configuration, including dependent fields
25. Duplicate detection behaviour
26. The AI set: tags, faces, summaries, transcripts, smart crop, conversational access
27. SSO/SAML, registration codes, a registration page, user administration
28. Site branding, dashboard messages, curated dashboard sections, spotlight searches
29. FTP upload
30. Webhook delivery
31. The admin worklists
32. Tag and AI-tag vocabulary administration
33. The built-in facets: asset status, orientation, average rating, has-attachment
34. An activity feed
35. Contacts / address book
36. Multi-language UI

### Deliberately not building
Hootsuite, Mobile, Templates (web-to-print), Video Creator, Syndicate, Digimarc, Google Analytics linkage.
These are third-party products or separate applications rather than DAM features; a DAM reaches them through
the API and webhooks, which are on the list. Uptime reporting belongs to whoever operates the deployment.

## Build order

Grouped so each numbered item is one full-stack slice: schema, API, UI, tests, mutation-tested, driven
against the real stack. The order is chosen so that foundations land before the things that need them, and so
that cheap high-value features are not stuck behind large ones.

**Foundations for everything metadata**
1. Metadata types bound to asset kinds — extends the schema administration just built
2. Hierarchical asset categories, plus the "without categories" worklist
3. Upload profiles (defaults, required-field enforcement, per-profile AI switches)
4. Auto-import mappings from embedded metadata — **done** (Q.4)

**Engagement, small and independent**
5. Ratings, favourites, watch — three features, one slice, one new facet each
6. Comments: public/private, routed, approval statuses (this is M6's "annotations")
7. Activity feed, and the dashboard sections and spotlight searches that make a landing page

**The asset's own history**
8. Versions: add, list, download an earlier one, and the detail tab
9. Attached documents, and the has-attachment facet
10. History tab over the existing audit log — done. Whole version group, access-filtered, one renderer shared with
    the dashboard feed. The alternate preview upload that shared this slice is deferred to ingest: it is a second
    rendition for an asset whose own bytes preview badly, which has nothing to do with a history

**Distribution**
11. Asset conversions: named formats, per-role permissions, and the download dialog that offers them — done for
    images, including the authenticated download the DAM had never had. Video conversions are open: they need a
    parameterised ffmpeg recipe. An administration screen for the format list is open too; the API is complete
12. Intended-use capture before download, and the record that makes it auditable — done, and it turned
    `license_scopes.max_downloads` from a decorative number into a cap that refuses. A declared vocabulary of a
    tenant's own (rather than the one derived from its licences) is open
13. Orders: approval, pickup, metadata export, multiple recipients, expiry
14. Portals: the four types, branded, over the share-portal foundation

**Search, the whole set**
15. Built-in facets: status, orientation, average rating, has-attachment
16. Search-within, substring, advanced search, multiple-asset search
17. Predictive search, did-you-mean
18. Export search results to CSV
19. Refine-search configuration and dependent fields

**Then the milestones ARCHITECTURE already names, which absorb most of what is left**
20. M4: duplicate detection (phash), document text extraction, embeddings and semantic search, faces, transcripts
21. M5: Claude enrichment, the MCP server (this is "AI Copilot"), AI Act marking G2, budget caps G20
22. M6: workflow — workgroups, definitions, forms, emails, requests/projects/deliverables; Insights
23. M3d: the Drupal 11 connector
24. Pre-GA: migration and import G7 (including FTP), SCIM + BYOK + the audit chain G10 (including SAML/SSO
    and user administration), backup and DR G11, metering G19 (the storage and usage reports), quotas
25. Entries: the PIM application
26. Cropping, multiple asset views, smart crop, closed captions
27. Site branding, webhook delivery, the admin worklists, tag vocabulary administration

Items 5, 15 and 27's worklists are the cheapest real value in the list; items 14, 22 and 25 are the largest.
