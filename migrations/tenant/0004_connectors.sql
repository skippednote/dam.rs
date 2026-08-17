-- Connector registrations, remote entity mapping, and the webhook outbox.
-- See ARCHITECTURE §11. The Drupal connector is the first consumer.
--
-- The design premise is REFERENCE, NOT COPY: a connected CMS stores an asset id
-- and renders signed transform URLs, it never downloads and stores the bytes.
-- That is what makes rights withdrawal and expiry actually take effect
-- downstream, and it is what makes `connector_asset_refs` a real usage index
-- instead of a guess.


-- ─── connector installs ─────────────────────────────────────────────────────
-- One row per connected site or system. Credentials live in
-- dam_global.api_keys (referenced by id, no FK — see 0002 for why tenant schemas
-- carry no cross-schema FKs); this table holds only configuration.
--
-- `asset_group_ids` is the security boundary that matters. A public Drupal site
-- gets a service account scoped to released, approved groups only, so a
-- misconfigured view cannot surface an unapproved asset. Empty means all groups,
-- which is why `allow_all_groups` is explicit rather than inferred from
-- emptiness.

CREATE TABLE connectors (
    id                  uuid PRIMARY KEY,
    kind                text NOT NULL CHECK (kind IN (
                            'drupal', 'wordpress', 'adobe_cc', 'figma',
                            'hubspot', 'salesforce', 'generic')),
    label               text NOT NULL,
    -- Canonical origin of the remote system. Also the CORS allowlist entry for
    -- the asset browser iframe and the audience claim on issued tokens.
    site_url            text NOT NULL,
    remote_version      text,                -- e.g. "drupal 11.1 / damrs_dam 1.2.0"

    api_key_id          uuid,                -- dam_global.api_keys.id, no FK
    -- Shared secret for locally-signed transform URLs. The remote signs render
    -- URLs itself with this, so page rendering never blocks on a damrs API call
    -- (§11.3). Rotatable: `previous_signing_secret` stays valid until
    -- `secret_rotated_at` + grace so a rotation is not a site outage.
    signing_secret      text NOT NULL,
    previous_signing_secret text,
    secret_rotated_at   timestamptz,

    asset_group_ids     uuid[] NOT NULL DEFAULT '{}',
    allow_all_groups    boolean NOT NULL DEFAULT false,
    allow_original      boolean NOT NULL DEFAULT false,   -- may it serve masters?
    -- A connector must never trigger a Glacier restore from a page render. When
    -- false, a cold original resolves to the proxy instead of a 202.
    allow_restore       boolean NOT NULL DEFAULT false,

    config              jsonb NOT NULL DEFAULT '{}'::jsonb,   -- field + image-style maps
    status              text NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'paused', 'error', 'revoked')),
    last_seen_at        timestamptz,
    last_error          text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX connectors_kind_idx ON connectors (kind, status);
CREATE UNIQUE INDEX connectors_site_idx ON connectors (kind, site_url);


-- ─── remote entity references ───────────────────────────────────────────────
-- The usage index. "Which Drupal sites, and which nodes, use this asset?" — a
-- question that is unanswerable if the CMS copies files, and which drives three
-- things: Widen-style asset usage reporting, impact analysis before an expiry or
-- takedown, and a strong pin-hot signal for the lifecycle engine (an asset live
-- on a production site is a terrible tiering candidate).
--
-- `synced_version_no` versus assets.version_no is the drift detector: a new
-- version in damrs leaves every reference stale until the sync worker pushes it.

CREATE TABLE connector_asset_refs (
    connector_id        uuid NOT NULL REFERENCES connectors (id) ON DELETE CASCADE,
    asset_id            uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,

    remote_entity_type  text NOT NULL,       -- 'media' for Drupal
    remote_entity_id    text NOT NULL,
    remote_uuid         text,
    remote_url          text,                -- canonical edit/view URL

    -- Where it is actually used downstream. Populated by the connector, so it is
    -- advisory rather than authoritative, but it is what makes takedown impact
    -- reporting possible at all.
    usage_count         int NOT NULL DEFAULT 0,
    usage_sample        jsonb NOT NULL DEFAULT '[]'::jsonb,   -- [{url, title}]

    synced_version_no   int,
    synced_at           timestamptz,
    state               text NOT NULL DEFAULT 'linked'
                            CHECK (state IN ('linked', 'stale', 'expired',
                                             'unpublished', 'orphaned')),

    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (connector_id, remote_entity_type, remote_entity_id)
);

CREATE INDEX connector_asset_refs_asset_idx ON connector_asset_refs (asset_id);
CREATE INDEX connector_asset_refs_stale_idx ON connector_asset_refs (state)
    WHERE state IN ('stale', 'expired');
-- Backs "this asset is in use on N live sites" without a scan per asset.
CREATE INDEX connector_asset_refs_usage_idx ON connector_asset_refs (asset_id)
    WHERE usage_count > 0;


-- ─── webhook subscriptions ──────────────────────────────────────────────────

CREATE TABLE webhook_subscriptions (
    id                  uuid PRIMARY KEY,
    connector_id        uuid REFERENCES connectors (id) ON DELETE CASCADE,
    url                 text NOT NULL,
    secret              text NOT NULL,       -- HMAC-SHA256 signing key
    event_kinds          text[] NOT NULL DEFAULT '{}',   -- empty = all
    active              boolean NOT NULL DEFAULT true,
    -- Auto-disabled after sustained delivery failure so a dead endpoint does not
    -- accumulate an unbounded outbox. Reversible from the UI.
    disabled_reason     text,
    consecutive_failures int NOT NULL DEFAULT 0,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX webhook_subscriptions_active_idx ON webhook_subscriptions (active)
    WHERE active;


-- ─── delivery outbox ────────────────────────────────────────────────────────
-- Transactional outbox: the event row is written in the SAME transaction as the
-- change that caused it, then delivered asynchronously. Emitting webhooks from
-- application code after a commit loses events on crash, and emitting before the
-- commit announces changes that may roll back.
--
-- Ordering matters for a CMS: an `asset.version_created` delivered after
-- `asset.expired` would republish an expired asset. Delivery is therefore
-- sequential per (subscription, asset).

CREATE TABLE webhook_deliveries (
    id                  uuid PRIMARY KEY,
    subscription_id     uuid NOT NULL REFERENCES webhook_subscriptions (id) ON DELETE CASCADE,
    event_kind          text NOT NULL,
    asset_id            uuid REFERENCES assets (id) ON DELETE SET NULL,
    payload             jsonb NOT NULL,

    state               text NOT NULL DEFAULT 'pending'
                            CHECK (state IN ('pending', 'delivering', 'delivered',
                                             'failed', 'dead')),
    attempts            int NOT NULL DEFAULT 0,
    max_attempts        int NOT NULL DEFAULT 8,
    next_attempt_at     timestamptz NOT NULL DEFAULT now(),
    response_status     int,
    last_error          text,

    created_at          timestamptz NOT NULL DEFAULT now(),
    delivered_at        timestamptz
);

CREATE INDEX webhook_deliveries_pending_idx
    ON webhook_deliveries (next_attempt_at, id)
    WHERE state IN ('pending', 'failed');
-- Per-asset ordering guard: the dispatcher takes the oldest pending row per
-- (subscription, asset) and will not run a second concurrently.
CREATE INDEX webhook_deliveries_order_idx
    ON webhook_deliveries (subscription_id, asset_id, created_at)
    WHERE state IN ('pending', 'delivering', 'failed');
CREATE INDEX webhook_deliveries_dead_idx ON webhook_deliveries (created_at)
    WHERE state = 'dead';
