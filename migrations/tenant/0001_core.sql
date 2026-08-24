-- Tenant core schema. Applied to `tenant_template` and to every `t_*` schema.
--
-- This connection's search_path is `<tenant_schema>, extensions, public`, set at
-- CONNECT time (not SET LOCAL — the sqlx migrator manages its own transactions).
-- sqlx puts its `_sqlx_migrations` ledger in the first schema on the path, which
-- gives each tenant an independent migration track for free.
--
-- Extension types are schema-qualified (`extensions.ltree`) rather than relying
-- on search_path order, so these statements are correct even if an operator
-- connects with a different path.
--
-- No foreign keys point at `dam_global`. Identity columns hold a bare uuid; a
-- cross-schema FK from every tenant would make identity deletion O(tenants).


-- ─── metadata schema engine ─────────────────────────────────────────────────
-- The configurable metadata schema is the core of a DAM's value: every customer
-- wants business-specific fields. Values live in `asset_metadata.values` as
-- JSONB, validated in Rust against these definitions at write time.
--
-- Hybrid rather than pure EAV: EAV makes every read a self-join and every facet
-- a nightmare, while pure JSONB with no schema table makes validation and facet
-- discovery impossible. This keeps the definitions relational and the values
-- document-shaped.

CREATE TABLE field_defs (
    id              uuid PRIMARY KEY,
    key             text NOT NULL,           -- JSONB key in asset_metadata.values
    label           text NOT NULL,
    kind            text NOT NULL CHECK (kind IN (
                        'text', 'textarea', 'long_text', 'int', 'decimal',
                        'date', 'datetime', 'bool', 'select', 'multiselect',
                        'taxonomy_ref', 'user_ref', 'url', 'geo')),
    taxonomy_id     uuid,                    -- required when kind = taxonomy_ref
    multivalued     boolean NOT NULL DEFAULT false,
    required        boolean NOT NULL DEFAULT false,
    read_only       boolean NOT NULL DEFAULT false,   -- the comparator's "read-only data"
    searchable      boolean NOT NULL DEFAULT true,    -- include in Tantivy text
    facetable       boolean NOT NULL DEFAULT false,   -- emit a Tantivy fast field
    -- The comparator's search shorthand alias, e.g. `bra:` for brand. Deliberately optional:
    -- aliases break when a display name changes, so metadata search prefers keys.
    search_alias    text,
    validation      jsonb NOT NULL DEFAULT '{}'::jsonb,  -- min/max/pattern/enum
    default_value   jsonb,
    display_order   int NOT NULL DEFAULT 0,
    ai_writable     boolean NOT NULL DEFAULT false,   -- may enrichment write here?
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT field_defs_key_shape CHECK (key ~ '^[a-z][a-z0-9_]{0,62}$'),
    CONSTRAINT field_defs_taxonomy_present CHECK (
        kind <> 'taxonomy_ref' OR taxonomy_id IS NOT NULL)
);

CREATE UNIQUE INDEX field_defs_key_idx ON field_defs (key);
CREATE UNIQUE INDEX field_defs_alias_idx ON field_defs (search_alias)
    WHERE search_alias IS NOT NULL;


-- ─── taxonomies ─────────────────────────────────────────────────────────────
-- Backs categories, controlled vocabularies, and product attributes. ltree gives
-- ancestor/descendant queries and prefix matching without a recursive CTE per
-- request, which matters when facet counts roll up a hierarchy.

CREATE TABLE taxonomies (
    id              uuid PRIMARY KEY,
    key             text NOT NULL UNIQUE,
    label           text NOT NULL,
    kind            text NOT NULL DEFAULT 'vocabulary'
                        CHECK (kind IN ('category', 'vocabulary', 'product_attribute')),
    -- Vocabulary terms are the label set that zero-shot tagging scores against
    -- (ARCHITECTURE §8.2). A closed vocabulary is what keeps AI tags governable.
    ai_taggable     boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE taxonomy_terms (
    id              uuid PRIMARY KEY,
    taxonomy_id     uuid NOT NULL REFERENCES taxonomies (id) ON DELETE CASCADE,
    parent_id       uuid REFERENCES taxonomy_terms (id) ON DELETE CASCADE,
    path            extensions.ltree NOT NULL,
    slug            text NOT NULL,
    label           text NOT NULL,
    -- Synonyms widen zero-shot matching and let imports resolve messy inputs.
    synonyms        text[] NOT NULL DEFAULT '{}',
    labels_i18n     jsonb NOT NULL DEFAULT '{}'::jsonb,   -- {locale: label}
    -- Auto-tuned by the tagging feedback loop; below the floor a term is
    -- demoted to suggest-only rather than auto-applied.
    ai_threshold    real NOT NULL DEFAULT 0.35,
    ai_precision    real,                    -- measured from tag_feedback
    asset_count     bigint NOT NULL DEFAULT 0,   -- denormalised, worker-maintained
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX taxonomy_terms_path_idx ON taxonomy_terms (taxonomy_id, path);
CREATE UNIQUE INDEX taxonomy_terms_slug_idx ON taxonomy_terms (taxonomy_id, slug);
CREATE INDEX taxonomy_terms_gist_idx ON taxonomy_terms USING gist (path);
CREATE INDEX taxonomy_terms_parent_idx ON taxonomy_terms (parent_id);


-- ─── assets ─────────────────────────────────────────────────────────────────
-- `content_hash` is BLAKE3 of the original bytes and is what the S3 key is
-- derived from, so re-uploading identical bytes costs nothing.
--
-- Versioning uses a group id rather than a self-referential chain: "give me the
-- current version" is then an index lookup, not a walk.

CREATE TABLE assets (
    id                  uuid PRIMARY KEY,
    content_hash        text NOT NULL,           -- BLAKE3, lowercase hex
    filename            text NOT NULL,
    ext                 text,
    mime                text NOT NULL,           -- sniffed, never client-supplied
    bytes               bigint NOT NULL,

    -- Probed technical facts. Nullable because they are type-dependent.
    width               int,
    height              int,
    duration_ms         bigint,
    page_count          int,
    orientation         smallint,
    color_space         text,
    has_alpha           boolean,

    status              text NOT NULL DEFAULT 'active'
                            CHECK (status IN ('uploading', 'processing', 'active',
                                              'archived', 'deleted')),
    -- Independent of `status`: an asset can be active but not yet released, or
    -- active and expired. Both are enforced in the ABAC predicate.
    release_at          timestamptz,
    expires_at          timestamptz,
    legal_hold          boolean NOT NULL DEFAULT false,   -- blocks tiering AND deletion

    version_group_id    uuid NOT NULL,
    version_no          int NOT NULL DEFAULT 1,
    is_current          boolean NOT NULL DEFAULT true,
    replaces_id         uuid REFERENCES assets (id) ON DELETE SET NULL,

    uploaded_by         uuid,                    -- dam_global.identities.id, no FK
    upload_profile_id   uuid,
    source              text NOT NULL DEFAULT 'api'
                            CHECK (source IN ('api', 'ui', 'import', 'connector')),

    -- Set by the enrichment DAG so the UI can show "awaiting review" without a
    -- join, and so the review queue is an index scan.
    enrichment_state    text NOT NULL DEFAULT 'pending'
                            CHECK (enrichment_state IN ('pending', 'running',
                                                        'needs_review', 'done', 'failed')),

    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    deleted_at          timestamptz,

    CONSTRAINT assets_version_positive CHECK (version_no >= 1)
);

CREATE UNIQUE INDEX assets_version_idx ON assets (version_group_id, version_no);
CREATE UNIQUE INDEX assets_current_idx ON assets (version_group_id)
    WHERE is_current AND deleted_at IS NULL;
CREATE INDEX assets_hash_idx ON assets (content_hash);
CREATE INDEX assets_status_idx ON assets (status) WHERE deleted_at IS NULL;
CREATE INDEX assets_enrichment_idx ON assets (enrichment_state)
    WHERE enrichment_state IN ('pending', 'needs_review');
CREATE INDEX assets_expiry_idx ON assets (expires_at)
    WHERE expires_at IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX assets_created_idx ON assets (created_at DESC);


-- ─── asset metadata ─────────────────────────────────────────────────────────
-- One row per asset. `values` holds the customer-defined fields; `technical`
-- holds raw EXIF/XMP/IPTC/ID3 so nothing extracted at ingest is ever lost, even
-- if no field is mapped to it yet.
--
-- Provenance is per-field, not per-row: `provenance->'<key>'` carries
-- {source, model, model_version, confidence, at, reviewed_by}. That is what
-- makes every AI write attributable and revertible (ARCHITECTURE §8.2).
--
-- Schema-per-tenant (D2) has a real payoff here: a tenant with a hot field can
-- get a promoted generated column without affecting anyone else, e.g.
--   ALTER TABLE asset_metadata
--     ADD COLUMN campaign text GENERATED ALWAYS AS (values->>'campaign') STORED;
--   CREATE INDEX ON asset_metadata (campaign);

CREATE TABLE asset_metadata (
    asset_id        uuid PRIMARY KEY REFERENCES assets (id) ON DELETE CASCADE,
    values          jsonb NOT NULL DEFAULT '{}'::jsonb,
    technical       jsonb NOT NULL DEFAULT '{}'::jsonb,
    provenance      jsonb NOT NULL DEFAULT '{}'::jsonb,
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX asset_metadata_values_idx ON asset_metadata USING gin (values jsonb_path_ops);


-- ─── asset groups (the ABAC unit) ───────────────────────────────────────────
-- Groups are the permission slice. Membership is either explicit or predicate-
-- driven; the predicate is the same query IR the search layer compiles, so a
-- group is literally a saved search.
--
-- Group ids are indexed as a Tantivy fast field and injected into every query.
-- ACL is a query-time filter, never a post-filter — post-filtering leaks
-- existence through pagination counts.

CREATE TABLE asset_groups (
    id              uuid PRIMARY KEY,
    key             text NOT NULL UNIQUE,
    label           text NOT NULL,
    predicate       jsonb,                   -- null = explicit membership only
    is_default      boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE asset_group_members (
    group_id        uuid NOT NULL REFERENCES asset_groups (id) ON DELETE CASCADE,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    added_at        timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (group_id, asset_id)
);

CREATE INDEX asset_group_members_asset_idx ON asset_group_members (asset_id);


-- ─── roles ──────────────────────────────────────────────────────────────────
-- RBAC (what verbs) x ABAC (over which groups). Compiled to one predicate that
-- SQL, Tantivy, and MCP all consume — divergence between the three is a leak.

CREATE TABLE roles (
    id                  uuid PRIMARY KEY,
    key                 text NOT NULL UNIQUE,
    label               text NOT NULL,
    permissions         text[] NOT NULL DEFAULT '{}',
    -- Empty array = all groups. Explicit rather than null so the "no access"
    -- and "all access" cases cannot be confused.
    asset_group_ids     uuid[] NOT NULL DEFAULT '{}',
    all_asset_groups    boolean NOT NULL DEFAULT false,
    -- The comparator's "time-based access".
    valid_from          timestamptz,
    valid_until         timestamptz,
    requires_eula       boolean NOT NULL DEFAULT false,
    is_builtin          boolean NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);


-- ─── collections ────────────────────────────────────────────────────────────

CREATE TABLE collections (
    id              uuid PRIMARY KEY,
    key             text NOT NULL UNIQUE,
    label           text NOT NULL,
    description     text,
    owner_id        uuid,                    -- dam_global.identities.id
    visibility      text NOT NULL DEFAULT 'private'
                        CHECK (visibility IN ('private', 'shared', 'public')),
    -- Collection membership is a strong signal against tiering (§6.4) and a
    -- trigger for predictive pre-warm (§8.4).
    pin_hot         boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE collection_items (
    collection_id   uuid NOT NULL REFERENCES collections (id) ON DELETE CASCADE,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    position        int NOT NULL DEFAULT 0,
    added_by        uuid,
    added_at        timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (collection_id, asset_id)
);

CREATE INDEX collection_items_asset_idx ON collection_items (asset_id);
CREATE INDEX collection_items_order_idx ON collection_items (collection_id, position);


-- ─── derivatives ────────────────────────────────────────────────────────────
-- Content-addressed on (asset content_hash, op_hash), so two assets that
-- produce byte-identical renditions share one object.
--
-- `role` drives the lifecycle engine: `proxy`, `thumbnail`, and `preview` never
-- tier (§6.4) — the 128 KB minimum billable size on IA/GLACIER_IR makes tiering
-- a 20 KB thumbnail cost more than leaving it in Standard.

CREATE TABLE derivatives (
    id              uuid PRIMARY KEY,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    role            text NOT NULL CHECK (role IN (
                        'thumbnail', 'preview', 'proxy', 'rendition',
                        'transcript', 'subtitle', 'waveform', 'hls')),
    profile         text NOT NULL,           -- named conversion format
    op_hash         text NOT NULL,           -- hash of the normalised op string
    object_key      text NOT NULL,
    mime            text NOT NULL,
    bytes           bigint NOT NULL,
    width           int,
    height          int,
    -- Distinguishes "cheap to regenerate, safe to evict" from "expensive,
    -- keep". A 40-minute ProRes export is worth storing; a 400px JPEG is not.
    regen_cost_ms   int,
    created_at      timestamptz NOT NULL DEFAULT now(),
    last_served_at  timestamptz
);

CREATE UNIQUE INDEX derivatives_op_idx ON derivatives (asset_id, op_hash);
CREATE INDEX derivatives_role_idx ON derivatives (asset_id, role);
CREATE UNIQUE INDEX derivatives_proxy_idx ON derivatives (asset_id)
    WHERE role = 'proxy';


-- ─── share links ────────────────────────────────────────────────────────────

CREATE TABLE share_links (
    id                  uuid PRIMARY KEY,
    token               text NOT NULL UNIQUE,
    kind                text NOT NULL CHECK (kind IN ('asset', 'collection', 'search')),
    target_id           uuid,
    search_query        jsonb,
    passcode_hash       text,
    expires_at          timestamptz,
    max_downloads       int,
    download_count      int NOT NULL DEFAULT 0,
    allow_original      boolean NOT NULL DEFAULT false,
    requires_eula       boolean NOT NULL DEFAULT false,
    created_by          uuid,
    created_at          timestamptz NOT NULL DEFAULT now(),
    revoked_at          timestamptz
);

CREATE INDEX share_links_active_idx ON share_links (expires_at)
    WHERE revoked_at IS NULL;


-- ─── events ─────────────────────────────────────────────────────────────────
-- Append-only, monthly range partitions. Backs asset-level analytics, the audit
-- trail, and the access-recency signal the lifecycle engine reads. damctl
-- pre-creates partitions; a missing partition must fail loudly rather than
-- silently dropping audit rows, so there is no DEFAULT partition.

CREATE TABLE events (
    id              uuid NOT NULL,
    occurred_at     timestamptz NOT NULL DEFAULT now(),
    kind            text NOT NULL,           -- view | download | share | edit | restore | ...
    asset_id        uuid,
    actor_id        uuid,
    actor_kind      text NOT NULL DEFAULT 'user'
                        CHECK (actor_kind IN ('user', 'api_key', 'share_link',
                                              'system', 'connector')),
    context         jsonb NOT NULL DEFAULT '{}'::jsonb,
    bytes           bigint,
    ip_hash         text,                    -- hashed, not raw: GDPR
    user_agent      text,

    PRIMARY KEY (id, occurred_at)
) PARTITION BY RANGE (occurred_at);

CREATE INDEX events_asset_idx ON events (asset_id, occurred_at DESC);
CREATE INDEX events_kind_idx ON events (kind, occurred_at DESC);

-- Seed partition; damctl rolls these forward.
CREATE TABLE events_2026_01 PARTITION OF events
    FOR VALUES FROM ('2026-01-01') TO ('2026-02-01');
