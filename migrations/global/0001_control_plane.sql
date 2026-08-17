-- Global control plane. Applied ONCE to the `dam_global` schema.
--
-- Bootstrap ordering matters. `damctl migrate` must create the `dam_global` and
-- `extensions` schemas and install the extensions BEFORE invoking the sqlx
-- migrator, because Postgres silently ignores nonexistent schemas in a
-- search_path — if `dam_global` did not exist yet, sqlx would create its
-- `_sqlx_migrations` ledger in whichever schema came next (`extensions` or
-- `public`) and every subsequent run would think the database was unmigrated.
--
-- Bootstrap, run imperatively by damctl before this file:
--
--   CREATE SCHEMA IF NOT EXISTS dam_global;
--   CREATE SCHEMA IF NOT EXISTS extensions;
--   CREATE EXTENSION IF NOT EXISTS vector   SCHEMA extensions;
--   CREATE EXTENSION IF NOT EXISTS ltree    SCHEMA extensions;
--   CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA extensions;
--
-- Extensions are database-scoped, not schema-scoped, so they are installed once
-- here and referenced schema-qualified from every tenant schema.
--
-- This connection's search_path is `dam_global, extensions, public`.
-- Guards below are redundant with the bootstrap but harmless.

CREATE SCHEMA IF NOT EXISTS dam_global;
CREATE SCHEMA IF NOT EXISTS extensions;


-- ─── tenants ────────────────────────────────────────────────────────────────
-- One row per tenant; `schema_name` is the authoritative pointer to its schema.
-- `slug` is validated in Rust against ^[a-z][a-z0-9_]{1,38}$ and the schema name
-- is always emitted through quote_ident. It is never built from raw input.
--
-- `db_target` is null today (single cluster). It exists so that crossing the
-- ~1-2k-schemas-per-cluster ceiling is an additive change: point a tenant at a
-- different cluster instead of rewriting the data layer.

CREATE TABLE tenants (
    id              uuid PRIMARY KEY,
    slug            text NOT NULL UNIQUE,
    schema_name     text NOT NULL UNIQUE,
    display_name    text NOT NULL,
    status          text NOT NULL DEFAULT 'provisioning'
                        CHECK (status IN ('provisioning', 'active', 'suspended',
                                          'migration_failed', 'deprovisioning')),
    storage_prefix  text NOT NULL,           -- "<tenant_id>/" within the pool bucket
    db_target       text,                    -- null = default cluster
    schema_version  bigint,                  -- last applied migrations/tenant version
    settings        jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT tenants_slug_shape CHECK (slug ~ '^[a-z][a-z0-9_]{1,38}$'),
    CONSTRAINT tenants_schema_shape CHECK (schema_name ~ '^t_[a-z][a-z0-9_]{1,38}$')
);

CREATE INDEX tenants_status_idx ON tenants (status) WHERE status <> 'active';


-- ─── identities ─────────────────────────────────────────────────────────────
-- Global because SSO identity spans tenants: one human, one row, N memberships.
-- Tenant schemas store the bare uuid with NO foreign key back here — a
-- cross-schema FK from every tenant would make identity deletion O(tenants).
-- Referential integrity for `created_by`-style columns is enforced in the
-- application, and orphaned ids degrade to "unknown user" rather than an error.

CREATE TABLE identities (
    id              uuid PRIMARY KEY,
    email           text NOT NULL,
    email_lower     text NOT NULL GENERATED ALWAYS AS (lower(email)) STORED,
    display_name    text,
    idp             text NOT NULL DEFAULT 'local'
                        CHECK (idp IN ('local', 'oidc', 'saml')),
    idp_subject     text,
    password_hash   text,                    -- argon2id; null for federated
    mfa_secret      text,
    status          text NOT NULL DEFAULT 'active'
                        CHECK (status IN ('active', 'disabled', 'invited')),
    last_login_at   timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX identities_email_idx ON identities (email_lower);
CREATE UNIQUE INDEX identities_idp_subject_idx ON identities (idp, idp_subject)
    WHERE idp_subject IS NOT NULL;


-- ─── tenant membership ──────────────────────────────────────────────────────
-- Role names resolve against the tenant schema's own `roles` table, so the same
-- identity can be an admin in one tenant and a read-only contributor in another.

CREATE TABLE tenant_members (
    tenant_id       uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    identity_id     uuid NOT NULL REFERENCES identities (id) ON DELETE CASCADE,
    role_names      text[] NOT NULL DEFAULT '{}',
    is_tenant_admin boolean NOT NULL DEFAULT false,
    invited_by      uuid REFERENCES identities (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (tenant_id, identity_id)
);

CREATE INDEX tenant_members_identity_idx ON tenant_members (identity_id);


-- ─── api keys ───────────────────────────────────────────────────────────────
-- Only the hash is stored; the plaintext is shown once at creation.
-- `key_prefix` is the displayable first bytes, for identification in the UI.

CREATE TABLE api_keys (
    id              uuid PRIMARY KEY,
    tenant_id       uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    identity_id     uuid REFERENCES identities (id) ON DELETE SET NULL,
    name            text NOT NULL,
    key_prefix      text NOT NULL,
    key_hash        text NOT NULL,
    scopes          text[] NOT NULL DEFAULT '{}',
    expires_at      timestamptz,
    last_used_at    timestamptz,
    revoked_at      timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX api_keys_hash_idx ON api_keys (key_hash);
CREATE INDEX api_keys_tenant_idx ON api_keys (tenant_id) WHERE revoked_at IS NULL;


-- ─── storage pools ──────────────────────────────────────────────────────────
-- A pool is a (driver, endpoint, bucket, prefix, storage_class) tuple plus its
-- cost and latency characteristics. Global by default (`tenant_id IS NULL`);
-- a non-null tenant_id is a bring-your-own-bucket pool for one tenant.
--
-- `latency_class` — not the provider name — is what the download path and the UI
-- branch on. That is what lets Azure Archive or LTO tape slot in later without
-- special-casing.
--
-- D1: only the `s3` driver is implemented for now. The CHECK lists the intended
-- set so adding a driver does not need a migration.
--
-- `credentials_ref` is a POINTER (env var name or secret-manager path), never a
-- secret. Nothing in this table should ever be safe-to-leak-only-because-nobody-
-- looked; treat it as fully readable by any operator with database access.

CREATE TABLE storage_pools (
    id                      uuid PRIMARY KEY,
    tenant_id               uuid REFERENCES tenants (id) ON DELETE CASCADE,
    name                    text NOT NULL,
    driver                  text NOT NULL DEFAULT 's3'
                                CHECK (driver IN ('s3', 'azure', 'fs', 'tape')),

    endpoint                text,            -- null = AWS default for the region
    region                  text,
    bucket                  text NOT NULL,
    prefix                  text NOT NULL DEFAULT '',
    force_path_style        boolean NOT NULL DEFAULT false,   -- MinIO / Ceph RGW
    credentials_ref         text NOT NULL,

    storage_class           text NOT NULL DEFAULT 'STANDARD'
                                CHECK (storage_class IN (
                                    'STANDARD', 'STANDARD_IA', 'ONEZONE_IA',
                                    'INTELLIGENT_TIERING', 'GLACIER_IR',
                                    'GLACIER', 'DEEP_ARCHIVE')),
    latency_class           text NOT NULL DEFAULT 'instant'
                                CHECK (latency_class IN ('instant', 'seconds',
                                                         'minutes', 'hours', 'days')),

    immutable               boolean NOT NULL DEFAULT false,   -- S3 Object Lock enabled
    -- Billing traps the lifecycle engine must respect. See ARCHITECTURE §6.4.
    min_duration_days       int NOT NULL DEFAULT 0,           -- IA 30 / GIR 90 / GLACIER 90 / DDA 180
    min_billable_bytes      bigint NOT NULL DEFAULT 0,        -- 131072 on IA and GIR
    cost_per_gb_month       numeric(12, 8) NOT NULL DEFAULT 0,
    cost_per_gb_retrieval   numeric(12, 8) NOT NULL DEFAULT 0,
    cost_per_1k_requests    numeric(12, 8) NOT NULL DEFAULT 0,

    enabled                 boolean NOT NULL DEFAULT true,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX storage_pools_name_idx ON storage_pools (name)
    WHERE tenant_id IS NULL;
CREATE UNIQUE INDEX storage_pools_tenant_name_idx ON storage_pools (tenant_id, name)
    WHERE tenant_id IS NOT NULL;

-- Archive classes require an explicit RestoreObject before a GET succeeds.
-- Encoding that as a constraint keeps a misconfigured pool from silently
-- producing "download works locally, 403s in prod" behaviour.
ALTER TABLE storage_pools ADD CONSTRAINT storage_pools_latency_matches_class CHECK (
    (storage_class IN ('GLACIER', 'DEEP_ARCHIVE') AND latency_class IN ('minutes', 'hours', 'days'))
    OR (storage_class NOT IN ('GLACIER', 'DEEP_ARCHIVE') AND latency_class = 'instant')
);


-- ─── job queue ──────────────────────────────────────────────────────────────
-- D6: global, not per tenant. One worker polls one table with FOR UPDATE SKIP
-- LOCKED. Polling N tenant schemas does not scale, and a global table is what
-- makes per-tenant fairness (round-robin over tenant_id) expressible at all.
--
-- Leases rather than a boolean lock: a worker that dies mid-job has its rows
-- reclaimed when lease_expires_at passes, with no reaper process required.

CREATE TABLE jobs (
    id                  uuid PRIMARY KEY,
    tenant_id           uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    kind                text NOT NULL,
    payload             jsonb NOT NULL DEFAULT '{}'::jsonb,

    state               text NOT NULL DEFAULT 'queued'
                            CHECK (state IN ('queued', 'running', 'succeeded',
                                             'failed', 'cancelled', 'dead')),
    priority            smallint NOT NULL DEFAULT 100,   -- lower runs first
    run_after           timestamptz NOT NULL DEFAULT now(),

    attempts            int NOT NULL DEFAULT 0,
    max_attempts        int NOT NULL DEFAULT 5,
    locked_by           text,                            -- worker id
    lease_expires_at    timestamptz,

    -- Idempotency. Enqueueing "derive thumbnails for asset X" twice while the
    -- first is still pending should be a no-op, not two jobs.
    dedupe_key          text,

    last_error          text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz
);

-- The polling index. Order must match the ORDER BY in the dequeue query exactly
-- or SKIP LOCKED degrades into a sequential scan under contention.
CREATE INDEX jobs_dequeue_idx ON jobs (priority, run_after, id)
    WHERE state = 'queued';

CREATE INDEX jobs_lease_idx ON jobs (lease_expires_at)
    WHERE state = 'running';

CREATE INDEX jobs_tenant_kind_idx ON jobs (tenant_id, kind, state);

CREATE UNIQUE INDEX jobs_dedupe_idx ON jobs (tenant_id, kind, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND state IN ('queued', 'running');


-- ─── fleet rollups ──────────────────────────────────────────────────────────
-- D2 forbids cross-tenant joins, so fleet-wide reporting is served from rollups
-- the worker writes here. Storage/AI cost attribution and quota enforcement read
-- this table, never the tenant schemas.

CREATE TABLE tenant_usage_daily (
    tenant_id           uuid NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    day                 date NOT NULL,
    asset_count         bigint NOT NULL DEFAULT 0,
    bytes_by_pool       jsonb NOT NULL DEFAULT '{}'::jsonb,   -- {pool_name: bytes}
    downloads           bigint NOT NULL DEFAULT 0,
    restores            bigint NOT NULL DEFAULT 0,
    restore_bytes       bigint NOT NULL DEFAULT 0,
    ai_input_tokens     bigint NOT NULL DEFAULT 0,
    ai_output_tokens    bigint NOT NULL DEFAULT 0,
    ai_cached_tokens    bigint NOT NULL DEFAULT 0,
    est_cost_cents      bigint NOT NULL DEFAULT 0,

    PRIMARY KEY (tenant_id, day)
);
