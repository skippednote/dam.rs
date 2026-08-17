-- Storage placements, lifecycle, and restore. See ARCHITECTURE §6.
--
-- The invariant this schema exists to enforce: ONLY the original master tiers to
-- cold storage. The search substrate — metadata, extracted text, embeddings,
-- thumbnails, and the master proxy — stays hot forever, so a fully archived
-- library is still fully searchable, previewable, and re-processable by AI
-- (§2). `object_placements.pinned` is the mechanical expression of that.


-- ─── placements ─────────────────────────────────────────────────────────────
-- Keyed on (object_key, pool_id) rather than asset_id: one asset has many
-- objects (original, proxy, N derivatives), and one object can live in several
-- pools at once. That many-to-many is what buys multi-cloud replication, the
-- verification scrub, and per-pool cost attribution.
--
-- `pool_id` references dam_global.storage_pools. This is the ONE deliberate
-- cross-schema reference in the tenant schema, and it carries no FK: pools are
-- infrastructure with a handful of rows, tenant schemas number in the thousands,
-- and an FK from every tenant would make retiring a pool an O(tenants) lock.
-- Integrity is enforced in the application; an unknown pool_id is a hard error
-- at resolve time, not a silent fallback.
--
-- A restore does NOT change storage_class. Glacier and Deep Archive objects stay
-- in their class while a temporary copy is made available, which is why
-- restore_state and restore_expires_at are separate columns rather than
-- additional storage_class values. Conflating them is the classic bug: the
-- object reads as "available" forever and the download 403s the day the
-- temporary copy expires.

CREATE TABLE object_placements (
    object_key          text NOT NULL,
    pool_id             uuid NOT NULL,       -- dam_global.storage_pools.id, no FK
    asset_id            uuid REFERENCES assets (id) ON DELETE CASCADE,
    derivative_id       uuid REFERENCES derivatives (id) ON DELETE CASCADE,

    size_bytes          bigint NOT NULL,
    checksum_algo       text NOT NULL DEFAULT 'blake3'
                            CHECK (checksum_algo IN ('blake3', 'sha256', 'crc32c')),
    checksum            text NOT NULL,
    -- S3 additional checksums let the scrub verify integrity from HeadObject
    -- alone, without paying egress to download the bytes.
    remote_checksum     text,
    etag                text,

    storage_class       text NOT NULL DEFAULT 'STANDARD',
    -- Billing trap: IA 30d, GLACIER_IR 90d, GLACIER 90d, DEEP_ARCHIVE 180d.
    -- Tier an object then delete it three days later and the full minimum is
    -- still charged. The lifecycle engine checks this before ANY transition,
    -- both for the first hop and for re-tiering.
    min_duration_until  timestamptz,

    state               text NOT NULL DEFAULT 'present'
                            CHECK (state IN ('uploading', 'present', 'transitioning',
                                             'missing', 'corrupt', 'deleting')),

    restore_state       text NOT NULL DEFAULT 'none'
                            CHECK (restore_state IN ('none', 'requested',
                                                     'ongoing', 'available', 'expired')),
    restore_expires_at  timestamptz,

    -- Blocks the lifecycle engine unconditionally. Set on proxies, thumbnails,
    -- previews, legal-hold assets, and anything in a pin_hot collection.
    pinned              boolean NOT NULL DEFAULT false,
    pin_reason          text,

    placed_at           timestamptz NOT NULL DEFAULT now(),
    last_accessed_at    timestamptz,
    last_verified_at    timestamptz,
    verify_failures     int NOT NULL DEFAULT 0,

    PRIMARY KEY (object_key, pool_id),

    -- An object belongs to an asset (original) or a derivative, never both.
    CONSTRAINT placements_owner CHECK (
        (asset_id IS NOT NULL AND derivative_id IS NULL)
        OR (asset_id IS NULL AND derivative_id IS NOT NULL)),

    -- If a restore is live, it must have an expiry — otherwise the temporary
    -- copy is unreclaimable state and cache invalidation has nothing to key on.
    CONSTRAINT placements_restore_expiry CHECK (
        restore_state <> 'available' OR restore_expires_at IS NOT NULL)
);

CREATE INDEX placements_asset_idx ON object_placements (asset_id)
    WHERE asset_id IS NOT NULL;
CREATE INDEX placements_derivative_idx ON object_placements (derivative_id)
    WHERE derivative_id IS NOT NULL;
CREATE INDEX placements_pool_idx ON object_placements (pool_id, storage_class);

-- Lifecycle candidate scan: untouched, unpinned, past its minimum duration.
CREATE INDEX placements_tier_candidates_idx
    ON object_placements (storage_class, last_accessed_at NULLS FIRST)
    WHERE NOT pinned AND state = 'present';

-- Restore expiry sweep — drives cache invalidation and re-restore prompts.
CREATE INDEX placements_restore_expiry_idx ON object_placements (restore_expires_at)
    WHERE restore_state IN ('ongoing', 'available');

-- Scrub queue: oldest-verified first, and anything already suspect.
CREATE INDEX placements_verify_idx ON object_placements (last_verified_at NULLS FIRST)
    WHERE state = 'present';
CREATE INDEX placements_suspect_idx ON object_placements (verify_failures)
    WHERE verify_failures > 0;


-- ─── lifecycle policies ─────────────────────────────────────────────────────
-- damrs drives transitions itself rather than delegating to S3 lifecycle rules,
-- for two reasons: cross-provider tiering (hot S3 to B2 or tape) cannot be
-- expressed as an S3 lifecycle rule at all, and self-driven transitions keep
-- object_placements authoritative instead of eventually-consistent. A nightly
-- scrub reconciles against ListObjectsV2 / S3 Inventory.
--
-- `predicate` is the same query IR the search layer compiles, so a rule can say
-- "no download in 180 d AND not in any collection AND not referenced by a live
-- portal AND not tagged legal-hold" without a bespoke rule language.

CREATE TABLE lifecycle_policies (
    id                  uuid PRIMARY KEY,
    name                text NOT NULL,
    priority            int NOT NULL DEFAULT 100,   -- lowest wins, first match applies
    enabled             boolean NOT NULL DEFAULT true,

    -- What it applies to.
    applies_to          text NOT NULL DEFAULT 'original'
                            CHECK (applies_to IN ('original', 'derivative', 'both')),
    derivative_roles    text[] NOT NULL DEFAULT '{}',   -- empty = any role
    only_superseded     boolean NOT NULL DEFAULT false, -- non-current versions only
    predicate           jsonb NOT NULL DEFAULT '{}'::jsonb,

    -- When.
    min_age_days        int NOT NULL DEFAULT 0,
    idle_days           int,                 -- no access in N days
    from_storage_class  text,                -- null = any

    -- What to do.
    action              text NOT NULL DEFAULT 'transition'
                            CHECK (action IN ('transition', 'evict', 'replicate')),
    target_pool_id      uuid,                -- dam_global.storage_pools.id, no FK
    target_class        text,

    -- Safety rails. A policy that would move more than max_objects_per_run in
    -- one pass halts and alerts instead: a mis-scoped predicate that archives a
    -- whole library is recoverable, but only slowly and only at Bulk pricing.
    max_objects_per_run int NOT NULL DEFAULT 10000,
    dry_run             boolean NOT NULL DEFAULT true,

    last_run_at         timestamptz,
    last_run_moved      int,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT lifecycle_target_present CHECK (
        action <> 'transition' OR (target_pool_id IS NOT NULL AND target_class IS NOT NULL))
);

CREATE INDEX lifecycle_policies_order_idx ON lifecycle_policies (priority)
    WHERE enabled;


-- ─── restore requests ───────────────────────────────────────────────────────
-- Glacier: Expedited 1-5 min / Standard 3-5 h / Bulk 5-12 h.
-- Deep Archive: NO Expedited tier — Standard ~12 h / Bulk ~48 h.
-- The tier CHECK cannot encode that (it depends on the pool's class), so it is
-- validated in Rust against the resolved pool before RestoreObject is issued.
--
-- Cost guardrails matter here because Expedited vs Bulk is roughly a 10x spread.
-- `est_cost_cents` is computed from the pool's retrieval price and shown to the
-- user before they confirm; requests above the tenant threshold sit in
-- 'awaiting_approval' until an admin releases them.

CREATE TABLE restore_requests (
    id                  uuid PRIMARY KEY,
    object_key          text NOT NULL,
    pool_id             uuid NOT NULL,       -- dam_global.storage_pools.id, no FK
    asset_id            uuid REFERENCES assets (id) ON DELETE CASCADE,

    tier                text NOT NULL DEFAULT 'standard'
                            CHECK (tier IN ('expedited', 'standard', 'bulk')),
    -- How long the temporary copy stays available once restored.
    keep_warm_days      int NOT NULL DEFAULT 7,

    state               text NOT NULL DEFAULT 'queued'
                            CHECK (state IN ('queued', 'awaiting_approval', 'requested',
                                             'ongoing', 'available', 'expired',
                                             'failed', 'cancelled')),

    requested_by        uuid,                -- dam_global.identities.id
    requested_at        timestamptz NOT NULL DEFAULT now(),
    approved_by         uuid,
    approved_at         timestamptz,
    -- Shown to the user as an ETA, derived from the pool's latency_class + tier.
    eta_at              timestamptz,
    available_at        timestamptz,
    expires_at          timestamptz,

    est_cost_cents      bigint NOT NULL DEFAULT 0,
    bytes               bigint NOT NULL DEFAULT 0,

    -- Sibling requests are batched: one collection restore becomes one bulk job,
    -- not 400 expedited ones. The first request in a batch owns the S3 call.
    batch_id            uuid,
    notify              jsonb NOT NULL DEFAULT '{}'::jsonb,   -- {email, webhook, in_app}
    last_error          text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- Coalesce duplicate requests for the same object rather than paying twice.
CREATE UNIQUE INDEX restore_requests_inflight_idx
    ON restore_requests (object_key, pool_id)
    WHERE state IN ('queued', 'awaiting_approval', 'requested', 'ongoing');

CREATE INDEX restore_requests_state_idx ON restore_requests (state, requested_at);
CREATE INDEX restore_requests_asset_idx ON restore_requests (asset_id);
CREATE INDEX restore_requests_batch_idx ON restore_requests (batch_id)
    WHERE batch_id IS NOT NULL;
CREATE INDEX restore_requests_poll_idx ON restore_requests (eta_at)
    WHERE state IN ('requested', 'ongoing');
