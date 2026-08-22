-- Resumable upload sessions. See ARCHITECTURE §18.3 and dam_store::resumable.
--
-- `dam_store::resumable` keeps no map of live uploads: every byte of state it
-- needs is a value the caller persists, and this table is that value. That is
-- what lets any node serve any TUS PATCH and lets a restart lose nothing.
--
-- The constraints below are not defence in depth around the Rust. A session is
-- written by one request and read by another, possibly on another process, and an
-- inconsistent row silently assembles the WRONG BYTES under a content-addressed
-- key — an object that then looks canonical. A failed upload is recoverable; a
-- plausible-looking wrong one is not.

CREATE TABLE upload_sessions (
    id                  uuid PRIMARY KEY,

    -- The tenant uuid, because it is the object-key PREFIX
    -- (`<tenant>/staging/<upload_id>`) and the reaper has to rebuild that key from
    -- the row alone. Not an access-control boundary — the schema is that (D2) —
    -- and the same reason object_placements stores whole keys rather than
    -- reconstructing them. Deriving it from the control plane instead would make
    -- cleanup depend on a tenants row still existing, which is precisely when
    -- cleanup matters most.
    tenant_id           uuid NOT NULL,

    -- Ours, never the client's: it becomes part of an object key
    -- (`<tenant>/staging/<upload_id>`). The pattern mirrors Key::staging, and is
    -- repeated here because a row can also arrive from a bulk import or a psql
    -- prompt, neither of which goes through the Rust validator.
    upload_id           text NOT NULL
                            CHECK (upload_id ~ '^[A-Za-z0-9_-]{1,64}$'),

    status              text NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active', 'completed',
                                              'terminated', 'expired')),

    -- Bytes accepted so far: the authoritative answer to a TUS HEAD.
    offset_bytes        bigint NOT NULL DEFAULT 0 CHECK (offset_bytes >= 0),
    -- NULL is meaningful: TUS Upload-Defer-Length means the client genuinely does
    -- not know the total yet. Defaulting to 0 would make "unknown" and "empty"
    -- the same value.
    declared_length     bigint CHECK (declared_length IS NULL OR declared_length >= 0),

    -- The backend's multipart upload id, opened lazily — a small upload never
    -- needs one.
    backend_upload_id   text,
    part_count          int NOT NULL DEFAULT 0 CHECK (part_count >= 0),
    -- The completion list: [{"number": 1, "etag": "\"...\""}, ...] in the order S3
    -- must concatenate them. Order is load-bearing — S3 assembles by this list, so
    -- a re-ordered array produces a scrambled object that still completes.
    parts               jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- Bytes parked in the tail object, always below the 5 MiB part minimum.
    tail_bytes          bigint NOT NULL DEFAULT 0 CHECK (tail_bytes >= 0),

    -- What the client told us, recorded for the audit trail and never trusted.
    -- The stored mime is sniffed at finalisation (dam_media::ingest).
    declared_filename   text,
    declared_mime       text,

    -- Set when the session completes. The asset may be absent even then: a
    -- duplicate upload resolves to content that already exists.
    asset_id            uuid REFERENCES assets (id) ON DELETE SET NULL,
    completed_at        timestamptz,

    -- dam_global.identities.id. No FK, for the same reason object_placements
    -- carries none to storage_pools: an FK from every tenant schema would make
    -- retiring an identity an O(tenants) lock.
    created_by          uuid,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    -- NOT NULL with a default: an upload without an expiry leaves its parts
    -- billable indefinitely, which is the failure mode TUS's expiration
    -- extension exists to prevent.
    expires_at          timestamptz NOT NULL DEFAULT now() + interval '24 hours',

    -- An upload cannot have accepted more than the client said it would send.
    CONSTRAINT upload_offset_within_declared CHECK (
        declared_length IS NULL OR offset_bytes <= declared_length),

    -- A tail at or above the part minimum should have been flushed as a part.
    -- 5242880 is S3's minimum part size; a row saying otherwise means the engine
    -- failed to flush, and completing from it produces an upload S3 rejects at
    -- the last step.
    CONSTRAINT upload_tail_below_part_minimum CHECK (tail_bytes < 5242880),

    -- Parts exist only inside a multipart upload. Parts with no backend id can be
    -- neither completed nor aborted, so they would be billed until a bucket
    -- lifecycle rule expired them.
    CONSTRAINT upload_parts_need_a_backend_upload CHECK (
        part_count = 0 OR backend_upload_id IS NOT NULL),

    -- A completed session must record its completion, or it reads as done while
    -- pointing at nothing the caller can promote.
    CONSTRAINT upload_completed_is_recorded CHECK (
        status <> 'completed' OR completed_at IS NOT NULL),

    -- The counter and the list are written together, so a disagreement means a
    -- hand-edited row or a partial write. Completing from it would omit or repeat
    -- a part — which S3 accepts, producing a corrupt object under a
    -- content-addressed key.
    CONSTRAINT upload_part_count_matches_list CHECK (
        part_count = jsonb_array_length(parts)),

    CONSTRAINT upload_parts_is_an_array CHECK (jsonb_typeof(parts) = 'array')
);

CREATE UNIQUE INDEX upload_sessions_upload_id_idx ON upload_sessions (upload_id);

-- The reaper's query: expired sessions still holding storage. Partial, because
-- the vast majority of rows are completed and of no interest to it — and this
-- runs on every deployment forever, so a sequential scan over every upload ever
-- made is fine for a year and then is not.
CREATE INDEX upload_sessions_reap_idx ON upload_sessions (expires_at)
    WHERE status = 'active';

CREATE INDEX upload_sessions_asset_idx ON upload_sessions (asset_id)
    WHERE asset_id IS NOT NULL;
