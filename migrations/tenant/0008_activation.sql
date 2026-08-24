-- Notifications, saved searches, bulk operations, and migration import.
-- Closes GAPS.md G9, G15, G18, G7.


-- ─── paths: rule-based notifications (G9) ───────────────────────────────────
-- Modelled on the comparator's "Paths": trigger on an asset reaching a state, refine by
-- asset group / vocabulary / category, template an email with variables that pull
-- in asset counts and share links.
--
-- This is the feature that makes every expiry column in 0005 actually matter.
-- Without it, `licenses.ends_at`, `releases.expires_at`, and
-- `assets.earliest_rights_expiry` are columns nobody reads until after a licence
-- has lapsed and an image is already live on a customer's site.

CREATE TABLE paths (
    id                  uuid PRIMARY KEY,
    name                text NOT NULL,
    enabled             boolean NOT NULL DEFAULT true,

    trigger_kind        text NOT NULL CHECK (trigger_kind IN (
                            'asset_added_to_group', 'asset_uploaded',
                            'metadata_changed', 'version_created',
                            'review_ready', 'review_completed',
                            -- Time-based triggers: the scheduler evaluates these
                            -- daily rather than reacting to an event.
                            'asset_expiring', 'license_expiring', 'release_expiring',
                            'rights_lapsed', 'consent_expiring',
                            'restore_ready', 'duplicate_found',
                            'enrichment_needs_review', 'ai_disclosure_missing')),
    -- For *_expiring triggers: how far ahead to fire. Multiple paths on the same
    -- trigger with different lead times gives the 60/30/7-day escalation pattern.
    lead_days           int,
    trigger_config      jsonb NOT NULL DEFAULT '{}'::jsonb,
    predicate           jsonb NOT NULL DEFAULT '{}'::jsonb,

    channels            text[] NOT NULL DEFAULT '{email}',
    recipients          jsonb NOT NULL DEFAULT '{}'::jsonb,   -- {emails, role_keys, owner}
    subject_template    text NOT NULL,
    body_template       text NOT NULL,

    -- Batching and throttling, because the failure mode of a notification system
    -- is 4,000 separate emails when a bulk import lands. `digest_window` collapses
    -- firings within the window into one message.
    digest_window       interval,
    throttle_per_asset  interval,

    last_fired_at       timestamptz,
    fire_count          bigint NOT NULL DEFAULT 0,
    created_by          uuid,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT paths_lead_days_when_scheduled CHECK (
        trigger_kind NOT LIKE '%_expiring' OR lead_days IS NOT NULL)
);

CREATE INDEX paths_trigger_idx ON paths (trigger_kind) WHERE enabled;
-- The daily scheduler's work list.
CREATE INDEX paths_scheduled_idx ON paths (trigger_kind, lead_days)
    WHERE enabled AND lead_days IS NOT NULL;

-- Firing ledger. Exists purely for idempotency: a daily "expiring in 30 days"
-- sweep must not re-notify the same asset every day for thirty days. `digest_key`
-- is (path, asset, bucket) so the unique index does the deduplication.
CREATE TABLE path_firings (
    id              uuid PRIMARY KEY,
    path_id         uuid NOT NULL REFERENCES paths (id) ON DELETE CASCADE,
    asset_id        uuid REFERENCES assets (id) ON DELETE CASCADE,
    digest_key      text NOT NULL,
    recipient_count int NOT NULL DEFAULT 0,
    state           text NOT NULL DEFAULT 'queued'
                        CHECK (state IN ('queued', 'sent', 'failed', 'suppressed')),
    fired_at        timestamptz NOT NULL DEFAULT now(),
    last_error      text
);

CREATE UNIQUE INDEX path_firings_dedupe_idx ON path_firings (path_id, digest_key);
CREATE INDEX path_firings_asset_idx ON path_firings (asset_id, fired_at DESC);


-- ─── saved searches and smart collections (G15) ─────────────────────────────
-- `asset_groups.predicate` already provided the mechanism; nothing exposed it to
-- users. A smart collection is a saved search that renders as a collection, and
-- attaching a path to one gives "tell me when something new matches this" — which
-- is how a regional marketing lead tracks newly approved assets without asking.

CREATE TABLE saved_searches (
    id                  uuid PRIMARY KEY,
    owner_id            uuid,
    name                text NOT NULL,
    query               jsonb NOT NULL,      -- the query IR, not a query string
    -- Renders in the collections UI rather than the search history dropdown.
    is_smart_collection boolean NOT NULL DEFAULT false,
    shared              boolean NOT NULL DEFAULT false,
    shared_with_roles   text[] NOT NULL DEFAULT '{}',
    -- Optional subscription: fire this path when new results appear.
    notify_path_id      uuid REFERENCES paths (id) ON DELETE SET NULL,
    -- Cached count for the sidebar; recomputed by the worker, never trusted for
    -- access decisions.
    result_count        bigint,
    counted_at          timestamptz,
    last_used_at        timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX saved_searches_owner_idx ON saved_searches (owner_id, last_used_at DESC);
CREATE INDEX saved_searches_smart_idx ON saved_searches (is_smart_collection)
    WHERE is_smart_collection;


-- ─── search telemetry (G8, storage half) ────────────────────────────────────
-- The relevance feedback loop. Zero-result queries are the highest-signal product
-- input a DAM gets: they name the vocabulary gap between what users call things
-- and what the taxonomy calls them. Clicked-rank feeds nDCG measurement.
--
-- Sampled and short-retention by design — this is telemetry, not audit.

CREATE TABLE search_queries (
    id                  uuid PRIMARY KEY,
    at                  timestamptz NOT NULL DEFAULT now(),
    actor_id            uuid,
    raw_query           text,
    parsed_query        jsonb,
    -- Which retrieval paths contributed, for fusion tuning.
    lexical_hits        int,
    vector_hits         int,
    fused_hits          int,
    facets_applied      jsonb NOT NULL DEFAULT '{}'::jsonb,
    latency_ms          int,
    -- NULL = no click. Rank of the first clicked result, for MRR.
    first_click_rank    int,
    clicked_asset_ids   uuid[] NOT NULL DEFAULT '{}',
    downloaded          boolean NOT NULL DEFAULT false
);

CREATE INDEX search_queries_at_idx ON search_queries (at DESC);
-- The zero-result report.
CREATE INDEX search_queries_zero_idx ON search_queries (at DESC)
    WHERE fused_hits = 0;

-- The golden set for `damctl eval`. Hand-labelled query/asset relevance, so a
-- fusion-weight or embedding-model change can be measured instead of guessed at.
CREATE TABLE relevance_judgements (
    id              uuid PRIMARY KEY,
    query_text      text NOT NULL,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    -- Graded, not binary: nDCG needs gradations to distinguish "perfect hit" from
    -- "plausibly related", which is exactly the distinction a DAM lives on.
    grade           smallint NOT NULL CHECK (grade BETWEEN 0 AND 3),
    judged_by       uuid,
    judged_at       timestamptz NOT NULL DEFAULT now(),
    notes           text,

    UNIQUE (query_text, asset_id)
);

CREATE INDEX relevance_judgements_query_idx ON relevance_judgements (query_text);


-- ─── bulk operations (G18) ──────────────────────────────────────────────────
-- Bulk edit, bulk tag, bulk download-as-zip, bulk rights assignment. Present in
-- every competitor and impossible to bolt on later, because partial failure is the
-- hard part: an operation over 40,000 assets that fails at 31,000 must be
-- resumable and must report exactly which rows did not apply.

CREATE TABLE bulk_operations (
    id              uuid PRIMARY KEY,
    kind            text NOT NULL CHECK (kind IN (
                        'metadata_set', 'metadata_clear', 'tag_add', 'tag_remove',
                        'group_add', 'group_remove', 'collection_add',
                        'license_assign', 'delete', 'restore', 'download_zip',
                        'reprocess', 're_enrich', 'tier', 'export')),
    actor_id        uuid,
    -- Either an explicit id list or a predicate. A predicate is snapshotted to a
    -- materialised id list at start, so a long-running operation applies to the
    -- set the user saw rather than a set that shifts under it.
    predicate       jsonb,
    target_count    bigint NOT NULL DEFAULT 0,
    params          jsonb NOT NULL DEFAULT '{}'::jsonb,

    state           text NOT NULL DEFAULT 'queued'
                        CHECK (state IN ('queued', 'running', 'paused', 'completed',
                                         'partial', 'failed', 'cancelled')),
    done_count      bigint NOT NULL DEFAULT 0,
    failed_count    bigint NOT NULL DEFAULT 0,
    -- Cursor for resumption after a worker restart.
    resume_after    uuid,
    -- Sample of failures for the UI; the full list lives in bulk_operation_items.
    error_sample    jsonb NOT NULL DEFAULT '[]'::jsonb,
    result          jsonb NOT NULL DEFAULT '{}'::jsonb,   -- e.g. {zip_object_key}

    started_at      timestamptz,
    finished_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX bulk_operations_actor_idx ON bulk_operations (actor_id, created_at DESC);
CREATE INDEX bulk_operations_active_idx ON bulk_operations (state)
    WHERE state IN ('queued', 'running', 'paused');

CREATE TABLE bulk_operation_items (
    operation_id    uuid NOT NULL REFERENCES bulk_operations (id) ON DELETE CASCADE,
    asset_id        uuid NOT NULL,
    state           text NOT NULL DEFAULT 'pending'
                        CHECK (state IN ('pending', 'done', 'skipped', 'failed')),
    reason          text,

    PRIMARY KEY (operation_id, asset_id)
);

CREATE INDEX bulk_operation_items_pending_idx ON bulk_operation_items (operation_id)
    WHERE state = 'pending';
CREATE INDEX bulk_operation_items_failed_idx ON bulk_operation_items (operation_id)
    WHERE state = 'failed';


-- ─── migration import (G7) ──────────────────────────────────────────────────
-- Nobody buys a DAM greenfield; every real deal is a migration from an incumbent
-- product or a file share. The consistent finding across migration
-- postmortems is that METADATA CLEANUP is the most underestimated cost, and that
-- vendor API extraction is the right path for cross-DAM moves.
--
-- So the import subsystem is built around a crosswalk that can be reviewed and
-- corrected before any bytes move, and phased transfer with rollback rather than
-- a single cutover.

CREATE TABLE import_jobs (
    id                  uuid PRIMARY KEY,
    source              text NOT NULL CHECK (source IN (
                            'widen', 'bynder', 'brandfolder', 'aprimo', 'canto',
                            'sharepoint', 'gdrive', 's3_bucket', 'filesystem', 'csv')),
    label               text NOT NULL,
    config              jsonb NOT NULL DEFAULT '{}'::jsonb,   -- endpoint, credential ref

    -- The crosswalk: source field -> damrs field_def, with transformation rules
    -- and the edge cases found during discovery. Editable between phases, which is
    -- the whole point — discovery reveals what the mapping should be.
    crosswalk           jsonb NOT NULL DEFAULT '{}'::jsonb,
    taxonomy_mapping    jsonb NOT NULL DEFAULT '{}'::jsonb,
    unmapped_fields     jsonb NOT NULL DEFAULT '[]'::jsonb,

    phase               text NOT NULL DEFAULT 'discover'
                            CHECK (phase IN ('discover', 'crosswalk_review',
                                             'dry_run', 'transfer', 'verify',
                                             'complete', 'rolled_back', 'failed')),
    -- Phased by design: a 400k-asset library moves in batches with QA gates, not
    -- in one run that either works or doesn't.
    batch_size          int NOT NULL DEFAULT 1000,
    current_batch       int NOT NULL DEFAULT 0,

    discovered_count    bigint NOT NULL DEFAULT 0,
    migrated_count      bigint NOT NULL DEFAULT 0,
    skipped_count       bigint NOT NULL DEFAULT 0,
    failed_count        bigint NOT NULL DEFAULT 0,
    -- Dry-run output: what WOULD happen, including per-field mapping coverage and
    -- the assets whose metadata would land incomplete. This is the artifact the
    -- customer signs off on.
    report              jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Everything created under this token can be removed in one operation.
    rollback_token      uuid NOT NULL,

    started_at          timestamptz,
    finished_at         timestamptz,
    created_by          uuid,
    created_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX import_jobs_phase_idx ON import_jobs (phase)
    WHERE phase NOT IN ('complete', 'rolled_back');
CREATE UNIQUE INDEX import_jobs_rollback_idx ON import_jobs (rollback_token);

-- Per-asset mapping. Doubles as the idempotency key for a resumed run and the
-- rollback manifest. `source_id` is retained permanently: two years later,
-- "which source asset did this come from" is a question that gets asked.
CREATE TABLE import_records (
    import_job_id       uuid NOT NULL REFERENCES import_jobs (id) ON DELETE CASCADE,
    source_id           text NOT NULL,
    asset_id            uuid REFERENCES assets (id) ON DELETE SET NULL,
    source_checksum     text,
    state               text NOT NULL DEFAULT 'pending'
                            CHECK (state IN ('pending', 'migrated', 'skipped',
                                             'failed', 'rolled_back')),
    -- Non-fatal mapping losses: an unmapped field, a taxonomy term that had no
    -- equivalent, a rights value that could not be parsed. Aggregated into the
    -- dry-run report so the crosswalk can be fixed before the real run.
    warnings            jsonb NOT NULL DEFAULT '[]'::jsonb,
    error               text,
    migrated_at         timestamptz,

    PRIMARY KEY (import_job_id, source_id)
);

CREATE INDEX import_records_asset_idx ON import_records (asset_id);
CREATE INDEX import_records_pending_idx ON import_records (import_job_id)
    WHERE state = 'pending';
CREATE INDEX import_records_failed_idx ON import_records (import_job_id)
    WHERE state = 'failed';
CREATE INDEX import_records_warnings_idx ON import_records (import_job_id)
    WHERE warnings <> '[]'::jsonb;
