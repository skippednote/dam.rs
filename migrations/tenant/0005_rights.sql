-- Rights, licensing, and releases. Closes GAPS.md G4.
--
-- Replaces the four-column model in 0001 (release_at / expires_at / legal_hold /
-- requires_eula), which could express "this asset expires on Friday" but not
-- "licensed for EU web and social only, 500k impressions, until the model release
-- lapses in March, and never for AI training."
--
-- The design premise: rights are enforced AT THE POINT OF DISTRIBUTION, not
-- recorded in a spreadsheet and hoped for. That is the failure mode of every
-- legacy system — stock licences, model releases, and territorial restrictions
-- living in separate tracking documents that nothing checks at download time.
-- damrs funnels every download, render, and connector fetch through one signed-URL
-- chokepoint, so enforcement is a natural property of the delivery path rather
-- than a bolt-on. `rights_evaluations` is that chokepoint's cache.


-- ─── licences ───────────────────────────────────────────────────────────────
-- One row per licence or contract, not per asset: a stock subscription or a
-- photographer agreement typically covers hundreds of assets, and modelling it
-- per-asset makes a renewal an N-row update that will drift.
--
-- The three `ai_*` columns are newly load-bearing and interact directly with
-- damrs's OWN pipeline: `ai_processing_allowed = false` must exclude the asset
-- from embedding and LLM enrichment, not merely from downstream generative use.
-- Without that gate, damrs would send a restricted asset to a vision model as a
-- matter of routine.

CREATE TABLE licenses (
    id                      uuid PRIMARY KEY,
    name                    text NOT NULL,
    license_type            text NOT NULL CHECK (license_type IN (
                                'rights_managed', 'royalty_free', 'editorial_only',
                                'internal_only', 'public_domain', 'cc_by', 'cc_by_sa',
                                'cc_by_nc', 'cc0', 'custom')),
    licensor                text,
    licensor_contact        text,
    contract_ref            text,
    -- The contract PDF itself, held as an asset so it is versioned, searchable,
    -- and subject to the same retention rules as everything else.
    document_asset_id       uuid REFERENCES assets (id) ON DELETE SET NULL,

    acquired_at             timestamptz,
    starts_at               timestamptz,
    ends_at                 timestamptz,
    perpetual               boolean NOT NULL DEFAULT false,
    exclusive               boolean NOT NULL DEFAULT false,
    -- Renewal is the operational half of expiry: an alert 60 days out is the only
    -- thing that turns `ends_at` into a business process (see paths in 0008).
    auto_renews             boolean NOT NULL DEFAULT false,
    renewal_notice_days     int NOT NULL DEFAULT 60,

    cost_cents              bigint,
    currency                text,

    -- AI usage restrictions. Default DENY on training and generation: an unknown
    -- licence must not be assumed permissive, because the cost of guessing wrong
    -- is a rights claim rather than a missing feature.
    ai_training_allowed     boolean NOT NULL DEFAULT false,
    ai_generation_allowed   boolean NOT NULL DEFAULT false,
    -- Enrichment (embed / tag / describe) defaults to allowed because it is
    -- internal cataloguing rather than redistribution, but it is explicitly
    -- switchable per licence for licensors who forbid any machine processing.
    ai_processing_allowed   boolean NOT NULL DEFAULT true,

    requires_credit         boolean NOT NULL DEFAULT false,
    credit_line             text,
    notes                   text,

    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT licenses_perpetual_no_end CHECK (NOT perpetual OR ends_at IS NULL)
);

CREATE INDEX licenses_expiry_idx ON licenses (ends_at)
    WHERE ends_at IS NOT NULL AND NOT perpetual;
CREATE INDEX licenses_type_idx ON licenses (license_type);


-- ─── licence scopes ─────────────────────────────────────────────────────────
-- A licence grants one or more scopes: this territory set, these channels, this
-- window, up to this volume. Multiple scopes per licence is the normal case —
-- "worldwide web in perpetuity, plus EU print for 12 months" is two rows.
--
-- Territories are ISO 3166-1 alpha-2, or the literal 'WORLD'. Storing exclusions
-- separately matters because real contracts are written that way ("worldwide
-- except China"), and expanding that to an inclusion list at ingest loses the
-- author's intent the moment the country list changes.

CREATE TABLE license_scopes (
    id                  uuid PRIMARY KEY,
    license_id          uuid NOT NULL REFERENCES licenses (id) ON DELETE CASCADE,
    label               text,

    territories         text[] NOT NULL DEFAULT '{WORLD}',
    excluded_territories text[] NOT NULL DEFAULT '{}',
    channels            text[] NOT NULL DEFAULT '{}',   -- empty = all channels
    excluded_channels   text[] NOT NULL DEFAULT '{}',

    starts_at           timestamptz,
    ends_at             timestamptz,

    -- Usage caps. NULL means uncapped; 0 is a valid cap meaning "none permitted",
    -- which is why these are nullable rather than defaulting to 0.
    max_impressions     bigint,
    max_print_run       bigint,
    max_audience        bigint,
    max_downloads       bigint,

    -- Derivative-work permissions, distinct from distribution channel.
    allow_modification  boolean NOT NULL DEFAULT true,
    allow_crop          boolean NOT NULL DEFAULT true,
    allow_composite     boolean NOT NULL DEFAULT true,

    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX license_scopes_license_idx ON license_scopes (license_id);
CREATE INDEX license_scopes_window_idx ON license_scopes (ends_at)
    WHERE ends_at IS NOT NULL;
CREATE INDEX license_scopes_channels_idx ON license_scopes USING gin (channels);
CREATE INDEX license_scopes_territories_idx ON license_scopes USING gin (territories);


-- ─── asset <-> licence ──────────────────────────────────────────────────────
-- Many-to-many because an asset can carry a stock licence AND a separate music
-- sync licence AND an internal brand approval. The effective rights are the
-- INTERSECTION of all attached licences, not the union — the most restrictive
-- term wins, which is the only safe default.

CREATE TABLE asset_licenses (
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    license_id      uuid NOT NULL REFERENCES licenses (id) ON DELETE CASCADE,
    is_primary      boolean NOT NULL DEFAULT false,
    applied_by      uuid,
    applied_at      timestamptz NOT NULL DEFAULT now(),
    notes           text,

    PRIMARY KEY (asset_id, license_id)
);

CREATE INDEX asset_licenses_license_idx ON asset_licenses (license_id);
CREATE UNIQUE INDEX asset_licenses_primary_idx ON asset_licenses (asset_id)
    WHERE is_primary;


-- ─── releases ───────────────────────────────────────────────────────────────
-- Model, property, and talent releases. Separate from licences because they
-- expire independently and have their own subject: a photo can have a valid stock
-- licence and a lapsed model release, which makes it unusable for advertising but
-- fine for editorial.
--
-- `person_id` links to the face-clustering `people` table when face identification
-- is enabled (0007 gates that), which is what allows "show me every asset whose
-- model release has lapsed" to be a query rather than an audit.

CREATE TABLE releases (
    id                  uuid PRIMARY KEY,
    kind                text NOT NULL CHECK (kind IN ('model', 'property',
                                                      'talent', 'minor_guardian')),
    subject_name        text,
    person_id           uuid REFERENCES people (id) ON DELETE SET NULL,
    document_asset_id   uuid REFERENCES assets (id) ON DELETE SET NULL,

    signed_at           timestamptz,
    starts_at           timestamptz,
    expires_at          timestamptz,
    territories         text[] NOT NULL DEFAULT '{WORLD}',
    channels            text[] NOT NULL DEFAULT '{}',

    -- A minor's release requires guardian consent and is worth its own flag
    -- rather than a note, because it changes the review path.
    subject_is_minor    boolean NOT NULL DEFAULT false,
    guardian_consent    boolean NOT NULL DEFAULT false,

    status              text NOT NULL DEFAULT 'valid'
                            CHECK (status IN ('valid', 'expired', 'missing',
                                              'disputed', 'withdrawn')),
    withdrawn_at        timestamptz,
    notes               text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT releases_minor_needs_guardian CHECK (
        NOT subject_is_minor OR guardian_consent OR status <> 'valid')
);

CREATE INDEX releases_person_idx ON releases (person_id);
CREATE INDEX releases_expiry_idx ON releases (expires_at)
    WHERE expires_at IS NOT NULL AND status = 'valid';
CREATE INDEX releases_status_idx ON releases (status) WHERE status <> 'valid';

CREATE TABLE asset_releases (
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    release_id      uuid NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    created_at      timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (asset_id, release_id)
);

CREATE INDEX asset_releases_release_idx ON asset_releases (release_id);


-- ─── usage against caps ─────────────────────────────────────────────────────
-- Append-only consumption ledger. Without it, `max_impressions` is decoration.
-- Populated from three sources: connector usage reports (0004), download events
-- (0001 events), and manual entry for offline channels like print runs.
--
-- Deliberately not a running counter on license_scopes: a counter cannot be
-- audited, cannot be attributed to a channel, and cannot be corrected without
-- losing history.

CREATE TABLE rights_usage (
    id                  uuid PRIMARY KEY,
    asset_id            uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    license_scope_id    uuid REFERENCES license_scopes (id) ON DELETE SET NULL,
    channel             text,
    territory           text,
    impressions         bigint NOT NULL DEFAULT 0,
    print_run           bigint NOT NULL DEFAULT 0,
    downloads           bigint NOT NULL DEFAULT 0,
    -- Where it was used, when the connector can tell us.
    connector_id        uuid REFERENCES connectors (id) ON DELETE SET NULL,
    reference_url       text,
    source              text NOT NULL DEFAULT 'connector'
                            CHECK (source IN ('connector', 'download', 'manual', 'import')),
    period_start        date,
    period_end          date,
    recorded_by         uuid,
    recorded_at         timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX rights_usage_asset_idx ON rights_usage (asset_id, recorded_at DESC);
CREATE INDEX rights_usage_scope_idx ON rights_usage (license_scope_id);
CREATE INDEX rights_usage_channel_idx ON rights_usage (channel, territory);


-- ─── evaluation cache ───────────────────────────────────────────────────────
-- The distribution chokepoint. Computing effective rights means intersecting
-- every attached licence's scopes, every attached release, the asset's own
-- release/expiry window, and consumed usage against caps — too much to do inside
-- a signed-URL request, and far too much to do per row in a search result set.
--
-- So it is materialised per (asset, channel, territory) and invalidated by the
-- worker when any input changes. `expires_at` is the earliest moment the verdict
-- could change on its own (a licence window closing, a release lapsing), which
-- makes time-based invalidation exact rather than a polling guess.
--
-- 'expiring' is a distinct verdict from 'allowed' because a 30-day warning is
-- what actually prevents a lapse; by the time it is 'denied' someone has already
-- had to pull an asset off a live site.

CREATE TABLE rights_evaluations (
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    channel         text NOT NULL,
    territory       text NOT NULL,
    verdict         text NOT NULL CHECK (verdict IN ('allowed', 'expiring',
                                                     'denied', 'unknown')),
    -- Machine-readable so the UI and the API can explain a denial rather than
    -- just refusing: [{code: 'release_expired', release_id: ...}, ...]
    reasons         jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Caps: what's left, so a UI can warn before the last impression is spent.
    impressions_remaining bigint,
    computed_at     timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz,

    PRIMARY KEY (asset_id, channel, territory)
);

CREATE INDEX rights_evaluations_verdict_idx ON rights_evaluations (verdict)
    WHERE verdict <> 'allowed';
CREATE INDEX rights_evaluations_stale_idx ON rights_evaluations (expires_at)
    WHERE expires_at IS NOT NULL;


-- ─── denormalised state on assets ───────────────────────────────────────────
-- Search and list endpoints need "is this usable" without a five-table join per
-- row. Worker-maintained from rights_evaluations; treated as a cache, never as
-- the source of truth.
--
-- `ai_processing_allowed` is the gate the enrichment DAG reads before dispatching
-- an asset to any model. Defaulting it to true matches licenses.ai_processing_allowed,
-- but the DAG must treat NULL as deny — an asset whose rights have not yet been
-- evaluated is not an asset to send to a third-party API.

ALTER TABLE assets
    ADD COLUMN rights_state text NOT NULL DEFAULT 'unknown'
        CHECK (rights_state IN ('allowed', 'expiring', 'denied', 'unknown')),
    ADD COLUMN rights_evaluated_at timestamptz,
    ADD COLUMN ai_processing_allowed boolean,
    ADD COLUMN earliest_rights_expiry timestamptz;

CREATE INDEX assets_rights_state_idx ON assets (rights_state)
    WHERE rights_state <> 'allowed' AND deleted_at IS NULL;
CREATE INDEX assets_rights_expiry_idx ON assets (earliest_rights_expiry)
    WHERE earliest_rights_expiry IS NOT NULL AND deleted_at IS NULL;
-- The enrichment DAG's work queue: never dispatch an asset whose rights forbid
-- processing, and never dispatch one whose rights are simply unknown.
CREATE INDEX assets_ai_gate_idx ON assets (enrichment_state)
    WHERE ai_processing_allowed AND deleted_at IS NULL;
