-- Enterprise control plane: feature gating, SCIM, quotas, keys, residency, DR.
-- Closes GAPS.md G10, G19, G20, G11 and the flag half of G3.
--
-- search_path is `dam_global, extensions, public`.


-- ─── feature flags (G3 gate) ────────────────────────────────────────────────
-- Per-tenant capability switches. This is where face identification is held OFF
-- BY DEFAULT: the flag cannot be enabled unless an approved DPIA exists in the
-- tenant's schema, and enabling it records who did so and on what basis.
--
-- `blocked_jurisdictions` is the sharper commercial control. Illinois BIPA and
-- Texas CUBI carry private rights of action and statutory damages, which makes US
-- state law a more immediate risk than GDPR for a US-hosted tenant — a per-tenant
-- regional kill switch is cheaper than a lawsuit.

CREATE TABLE feature_flags (
    tenant_id               uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    key                     text NOT NULL,
    enabled                 boolean NOT NULL DEFAULT false,
    config                  jsonb NOT NULL DEFAULT '{}'::jsonb,

    -- Gate metadata. `requires_dpia` is a property of the FEATURE, seeded at
    -- provisioning; `dpia_ref` points at the tenant-schema dpia_records row.
    requires_dpia           boolean NOT NULL DEFAULT false,
    dpia_ref                uuid,
    legal_basis             text,
    blocked_jurisdictions   text[] NOT NULL DEFAULT '{}',

    enabled_by              uuid REFERENCES identities (id) ON DELETE SET NULL,
    enabled_at              timestamptz,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, key),

    -- A DPIA-requiring feature cannot be switched on without a reference and a
    -- recorded legal basis. Enforced here so a support engineer with database
    -- access cannot flip it as a favour.
    CONSTRAINT feature_flags_dpia_gate CHECK (
        NOT enabled OR NOT requires_dpia
        OR (dpia_ref IS NOT NULL AND legal_basis IS NOT NULL AND enabled_by IS NOT NULL))
);

CREATE INDEX feature_flags_enabled_idx ON feature_flags (key) WHERE enabled;


-- ─── SCIM provisioning (G10) ────────────────────────────────────────────────
-- SCIM 2.0 is an RFP pass/fail item, and the deprovisioning half is the part that
-- matters: SSO alone leaves orphaned accounts when someone leaves, which is
-- exactly what a security questionnaire asks about.

CREATE TABLE scim_clients (
    id                  uuid PRIMARY KEY,
    tenant_id           uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    label               text NOT NULL,
    token_hash          text NOT NULL,
    -- Which SCIM resources this client may manage.
    scopes              text[] NOT NULL DEFAULT '{Users,Groups}',
    last_sync_at        timestamptz,
    last_sync_status    text,
    revoked_at          timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX scim_clients_token_idx ON scim_clients (token_hash);
CREATE INDEX scim_clients_tenant_idx ON scim_clients (tenant_id)
    WHERE revoked_at IS NULL;

-- SCIM-managed identities must not be editable in the damrs UI, or the IdP will
-- overwrite local edits on next sync and the customer will report it as data loss.
ALTER TABLE identities
    ADD COLUMN scim_external_id text,
    ADD COLUMN scim_managed boolean NOT NULL DEFAULT false,
    ADD COLUMN deprovisioned_at timestamptz;

CREATE UNIQUE INDEX identities_scim_idx ON identities (scim_external_id)
    WHERE scim_external_id IS NOT NULL;


-- ─── quotas and spend caps (G19, G20) ───────────────────────────────────────
-- `tenant_usage_daily` in 0001 collects the data; nothing turned it into
-- enforcement. AI spend is the larger and more variable cost than storage — a
-- single mis-triggered re-enrichment of a 1M-asset library is a five-figure event
-- — yet restore budgets were designed (§6.5) and AI budgets were not.
--
-- `enforcement` distinguishes soft (alert, keep serving) from hard (refuse new
-- work). Hard caps on ingest are dangerous, hard caps on AI enrichment are
-- prudent, which is why it is per-quota rather than per-tenant.

CREATE TABLE tenant_quotas (
    tenant_id           uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    quota_key           text NOT NULL CHECK (quota_key IN (
                            'storage_bytes', 'asset_count', 'egress_bytes_month',
                            'ai_spend_cents_month', 'restore_spend_cents_month',
                            'api_requests_minute', 'seats')),
    limit_value         bigint NOT NULL,
    -- Fire a warning path at this fraction; 0.8 gives the customer time to react
    -- rather than discovering the cap by hitting it.
    warn_at_fraction    real NOT NULL DEFAULT 0.8,
    enforcement         text NOT NULL DEFAULT 'soft'
                            CHECK (enforcement IN ('soft', 'hard')),
    -- Overage pricing where the plan allows it, instead of a hard stop.
    overage_cents_per_unit numeric(12, 6),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, quota_key)
);

-- Running counters, reset per period. Separate from tenant_usage_daily because
-- enforcement needs a current value readable in one indexed lookup on the request
-- path, not a sum over a date range.
CREATE TABLE tenant_spend (
    tenant_id           uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    quota_key           text NOT NULL,
    period_start        date NOT NULL,
    used_value          bigint NOT NULL DEFAULT 0,
    warned_at           timestamptz,
    exceeded_at         timestamptz,
    updated_at          timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, quota_key, period_start)
);

CREATE INDEX tenant_spend_exceeded_idx ON tenant_spend (exceeded_at)
    WHERE exceeded_at IS NOT NULL;


-- ─── customer-managed keys (G10) ────────────────────────────────────────────
-- BYOK is the procurement item with real architectural consequences: per-tenant
-- keys touch S3 SSE-KMS configuration, the derivative cache, and the C2PA signing
-- identity from 0006. Schema-per-tenant (D2) makes the scoping tractable, which is
-- a point in that decision's favour.
--
-- `purpose` separation matters: a customer revoking their blob key should make
-- their content unreadable without also invalidating the provenance signatures on
-- content already distributed.

CREATE TABLE encryption_keys (
    id                  uuid PRIMARY KEY,
    tenant_id           uuid REFERENCES tenants (id) ON DELETE CASCADE,
    purpose             text NOT NULL CHECK (purpose IN (
                            'blob', 'c2pa_signing', 'field', 'backup')),
    provider            text NOT NULL DEFAULT 'aws_kms'
                            CHECK (provider IN ('aws_kms', 'gcp_kms', 'azure_kv',
                                                'vault', 'local')),
    key_ref             text NOT NULL,       -- ARN or URI; never key material
    customer_managed    boolean NOT NULL DEFAULT false,
    state               text NOT NULL DEFAULT 'active'
                            CHECK (state IN ('active', 'rotating', 'retired', 'revoked')),
    -- Retired keys must be retained as long as anything they encrypted exists, so
    -- retirement is not deletion.
    activated_at        timestamptz NOT NULL DEFAULT now(),
    rotated_at          timestamptz,
    retired_at          timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX encryption_keys_active_idx ON encryption_keys (tenant_id, purpose)
    WHERE state = 'active';
CREATE INDEX encryption_keys_tenant_idx ON encryption_keys (tenant_id);


-- ─── data residency (G10) ───────────────────────────────────────────────────
-- A residency commitment is a contractual term, so it needs to be enforceable
-- rather than aspirational: pool selection, model inference geography, and backup
-- destination all have to honour it. Storing `allowed_inference_geos` here lets
-- the AI layer refuse to send an EU tenant's asset to a US endpoint.

ALTER TABLE tenants
    ADD COLUMN residency_region text,
    ADD COLUMN allowed_pool_regions text[] NOT NULL DEFAULT '{}',
    ADD COLUMN allowed_inference_geos text[] NOT NULL DEFAULT '{}',
    -- Jurisdictions this tenant is assessed against, for AI Act marking (0006)
    -- and biometric law (feature_flags.blocked_jurisdictions).
    ADD COLUMN jurisdictions text[] NOT NULL DEFAULT '{}',
    ADD COLUMN plan text,
    ADD COLUMN is_sandbox boolean NOT NULL DEFAULT false,
    -- Sandbox tenants mirror a production tenant's schema config for testing
    -- workflow and schema changes — an expected enterprise capability.
    ADD COLUMN sandbox_of uuid REFERENCES tenants (id) ON DELETE SET NULL;

CREATE INDEX tenants_sandbox_idx ON tenants (sandbox_of) WHERE sandbox_of IS NOT NULL;


-- ─── disaster recovery bookkeeping (G11) ────────────────────────────────────
-- D4 claims the search index is "rebuildable from Postgres", which is only true if
-- Postgres itself is recoverable — and the Tantivy rebuild time is what actually
-- sits on the RTO. Measuring it per tenant turns a stated RTO into a defensible
-- one.
--
-- `last_verified_restore_at` is the column that matters. An untested backup is not
-- a backup, and the gap between "we take backups" and "we have restored one" is
-- where most DR plans fail.

CREATE TABLE dr_state (
    tenant_id                   uuid PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
    rpo_seconds                 int NOT NULL DEFAULT 300,
    rto_seconds                 int NOT NULL DEFAULT 3600,

    last_backup_at              timestamptz,
    last_wal_archive_at         timestamptz,
    -- Set only by an actual restore drill, never by a successful backup.
    last_verified_restore_at    timestamptz,
    verified_restore_duration_s int,

    -- Measured, not estimated. Feeds the RTO directly.
    index_doc_count             bigint,
    index_rebuild_seconds       int,
    index_snapshot_at           timestamptz,
    index_snapshot_key          text,        -- S3 key of the Tantivy dir snapshot

    replica_regions             text[] NOT NULL DEFAULT '{}',
    notes                       text,
    updated_at                  timestamptz NOT NULL DEFAULT now()
);

-- Tenants whose restore has never been verified, or not verified recently. This is
-- the DR report, and it should be short.
CREATE INDEX dr_state_unverified_idx ON dr_state (last_verified_restore_at NULLS FIRST);


-- ─── support access log (G10) ───────────────────────────────────────────────
-- Break-glass access to customer data, separate from the per-tenant audit_log
-- because the customer must be able to see it even if their own tenant is the
-- thing being investigated. Time-boxed and reason-required.

CREATE TABLE support_access (
    id                  uuid PRIMARY KEY,
    tenant_id           uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    identity_id         uuid NOT NULL REFERENCES identities (id) ON DELETE RESTRICT,
    reason              text NOT NULL,
    ticket_ref          text,
    -- Customer-approved access is a different thing from unilateral break-glass,
    -- and enterprise contracts often permit only the former.
    customer_approved   boolean NOT NULL DEFAULT false,
    approved_by         uuid REFERENCES identities (id) ON DELETE SET NULL,
    granted_at          timestamptz NOT NULL DEFAULT now(),
    expires_at          timestamptz NOT NULL,
    revoked_at          timestamptz,
    actions_taken       jsonb NOT NULL DEFAULT '[]'::jsonb
);

CREATE INDEX support_access_tenant_idx ON support_access (tenant_id, granted_at DESC);
CREATE INDEX support_access_active_idx ON support_access (expires_at)
    WHERE revoked_at IS NULL;
