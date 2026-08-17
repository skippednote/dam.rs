-- Consent, retention, and audit. Closes GAPS.md G3, G14, and the audit half of G10.


-- ─── biometric consent (G3) ─────────────────────────────────────────────────
-- Face identification produces biometric data — a GDPR Article 9 special
-- category, where processing is PROHIBITED BY DEFAULT unless a specific legal
-- basis applies. For a DAM the realistic basis is explicit consent from each
-- individual. The AI Act adds constraints on biometric categorisation and bans
-- building facial-recognition databases by untargeted scraping.
--
-- 0003 originally put face clustering in the default enrichment path with
-- `people.consent_ref` nullable, which made the compliant case the exception.
-- This corrects that: the feature is off by default (flag in global/0002), and
-- naming a cluster requires a consent record.
--
-- Note the asymmetry that makes this shippable: face DETECTION (blur,
-- crop-to-subject, counting) is far less exposed than face IDENTIFICATION. The
-- useful half ships without the liability.

CREATE TABLE consent_records (
    id                  uuid PRIMARY KEY,
    person_id           uuid REFERENCES people (id) ON DELETE CASCADE,
    subject_name        text NOT NULL,
    subject_contact     text,

    legal_basis         text NOT NULL CHECK (legal_basis IN (
                            'explicit_consent',      -- GDPR Art. 9(2)(a)
                            'employment_contract',   -- Art. 9(2)(b), narrow
                            'public_figure',         -- editorial; jurisdiction-specific
                            'vital_interests',
                            'legal_claim')),
    -- What the subject actually agreed to. 'identify' alone permits clustering and
    -- internal search; 'publish' is required before the asset can leave the DAM.
    scope               text[] NOT NULL DEFAULT '{identify}',
    territories         text[] NOT NULL DEFAULT '{}',

    -- The signed form, held as an asset so it is versioned and retained like any
    -- other record of consent.
    document_asset_id   uuid REFERENCES assets (id) ON DELETE SET NULL,
    evidence            jsonb NOT NULL DEFAULT '{}'::jsonb,

    granted_at          timestamptz NOT NULL,
    expires_at          timestamptz,
    -- Withdrawal is unconditional under GDPR and must be honoured, so it is a
    -- first-class column with its own erasure workflow rather than a status value.
    withdrawn_at        timestamptz,
    withdrawal_note     text,

    recorded_by         uuid,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX consent_records_person_idx ON consent_records (person_id);
CREATE INDEX consent_records_expiry_idx ON consent_records (expires_at)
    WHERE expires_at IS NOT NULL AND withdrawn_at IS NULL;
-- Withdrawal queue: face vectors and cluster membership must be erased, and any
-- asset relying on this consent re-evaluated.
CREATE INDEX consent_records_withdrawn_idx ON consent_records (withdrawn_at)
    WHERE withdrawn_at IS NOT NULL;

-- A cluster may only be named once a live consent record backs it. Enforced as a
-- trigger rather than a CHECK because it spans tables, and in the application as
-- well — but having it in the database means a bulk import or a stray SQL fix
-- cannot bypass it.
CREATE FUNCTION assert_person_consent() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.label IS NOT NULL AND NOT NEW.is_public_figure THEN
        IF NOT EXISTS (
            SELECT 1 FROM consent_records c
            WHERE c.person_id = NEW.id
              AND c.withdrawn_at IS NULL
              AND (c.expires_at IS NULL OR c.expires_at > now())
              AND 'identify' = ANY (c.scope)
        ) THEN
            RAISE EXCEPTION
              'naming a face cluster requires a live consent record (person %)', NEW.id
              USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER people_consent_gate
    BEFORE INSERT OR UPDATE OF label ON people
    FOR EACH ROW EXECUTE FUNCTION assert_person_consent();


-- ─── DPIA gate (G3) ─────────────────────────────────────────────────────────
-- A Data Protection Impact Assessment is required before high-risk processing
-- begins, not after. Recording it here — and requiring it before the feature flag
-- can be enabled — turns "we should do a DPIA" into a precondition the system
-- enforces.

CREATE TABLE dpia_records (
    id                  uuid PRIMARY KEY,
    feature             text NOT NULL,       -- 'face_identify', 'ai_enrichment', ...
    status              text NOT NULL DEFAULT 'draft'
                            CHECK (status IN ('draft', 'approved', 'rejected', 'expired')),
    risk_level          text CHECK (risk_level IN ('low', 'medium', 'high', 'unacceptable')),
    mitigations         jsonb NOT NULL DEFAULT '[]'::jsonb,
    document_asset_id   uuid REFERENCES assets (id) ON DELETE SET NULL,
    assessed_by         uuid,
    assessed_at         timestamptz,
    -- DPIAs go stale as processing changes; a review date makes that visible.
    review_due_at       timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX dpia_records_feature_idx ON dpia_records (feature)
    WHERE status = 'approved';
CREATE INDEX dpia_records_review_idx ON dpia_records (review_due_at)
    WHERE status = 'approved';


-- ─── retention and trash (G14) ──────────────────────────────────────────────
-- `assets.deleted_at` existed but nothing acted on it: no restore path, no
-- retention window, no purge. Soft delete without a purge job is a GDPR problem
-- (erasure requests are not satisfied by hiding a row) and a cost problem (the
-- bytes keep billing).

CREATE TABLE retention_policies (
    id                  uuid PRIMARY KEY,
    name                text NOT NULL,
    priority            int NOT NULL DEFAULT 100,
    enabled             boolean NOT NULL DEFAULT true,

    applies_to          text NOT NULL DEFAULT 'trashed'
                            CHECK (applies_to IN ('trashed', 'expired', 'superseded',
                                                  'unused', 'all')),
    predicate           jsonb NOT NULL DEFAULT '{}'::jsonb,
    retain_days         int NOT NULL,
    action              text NOT NULL DEFAULT 'purge'
                            CHECK (action IN ('purge', 'anonymise', 'archive_tier')),
    -- Legal hold always wins. Making this explicit and defaulting it to false
    -- means a policy cannot silently delete something under litigation hold.
    overrides_legal_hold boolean NOT NULL DEFAULT false,
    -- Purge is irreversible, so it is dry-run by default like the lifecycle engine.
    dry_run             boolean NOT NULL DEFAULT true,
    last_run_at         timestamptz,
    last_run_affected   int,
    created_at          timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT retention_no_legal_hold_override CHECK (NOT overrides_legal_hold)
);

CREATE INDEX retention_policies_order_idx ON retention_policies (priority)
    WHERE enabled;

ALTER TABLE assets
    ADD COLUMN deleted_by uuid,
    ADD COLUMN delete_reason text,
    -- When the bytes actually go. NULL while a policy has not yet claimed the row.
    ADD COLUMN purge_after timestamptz,
    ADD COLUMN purged_at timestamptz;

-- The trash view the UI lists, and the purge worker's queue.
CREATE INDEX assets_trash_idx ON assets (deleted_at DESC)
    WHERE deleted_at IS NOT NULL AND purged_at IS NULL;
CREATE INDEX assets_purge_queue_idx ON assets (purge_after)
    WHERE purge_after IS NOT NULL AND purged_at IS NULL AND NOT legal_hold;


-- ─── tamper-evident audit log (G10) ─────────────────────────────────────────
-- The `events` table in 0001 is append-only by convention, which is not what an
-- enterprise security questionnaire means by "audit trail". This is a hash chain:
-- each row commits to its predecessor, so removing or editing any row breaks
-- verification from that point forward.
--
-- Deliberately separate from `events`: events are high-volume analytics
-- (partitioned, prunable, sampled if needed), audit is low-volume and permanent.
-- Mixing them means either a chain over millions of view events, or a prunable
-- audit log — both wrong.
--
-- Not partitioned, for the same reason: a chain across partition boundaries is
-- verifiable but the detach-old-partitions operation that makes partitioning
-- worthwhile would break it.

CREATE TABLE audit_log (
    seq             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at              timestamptz NOT NULL DEFAULT now(),
    actor_id        uuid,
    actor_kind      text NOT NULL DEFAULT 'user'
                        CHECK (actor_kind IN ('user', 'api_key', 'connector',
                                              'system', 'support')),
    -- Support access to customer data is itself auditable; conflating it with
    -- 'system' hides exactly what a customer most wants to see.
    action          text NOT NULL,
    target_kind     text NOT NULL,
    target_id       text,
    payload         jsonb NOT NULL DEFAULT '{}'::jsonb,

    -- sha256(seq || at || actor_id || action || target_kind || target_id ||
    --        payload::canonical_json || prev_hash)
    -- Computed in Rust so the canonicalisation is explicit and version-pinned;
    -- Postgres jsonb key ordering is stable but not a contract worth relying on.
    prev_hash       text,
    hash            text NOT NULL
);

CREATE INDEX audit_log_at_idx ON audit_log (at DESC);
CREATE INDEX audit_log_actor_idx ON audit_log (actor_id, at DESC);
CREATE INDEX audit_log_target_idx ON audit_log (target_kind, target_id, at DESC);
CREATE INDEX audit_log_action_idx ON audit_log (action, at DESC);

-- Refuse mutation at the database level. Application-level append-only is a
-- convention; a rule that rejects UPDATE and DELETE is a control an auditor can
-- be shown. Superusers can still drop the rule, which is the honest limit of
-- in-database tamper evidence — the hash chain is what detects that.
CREATE RULE audit_log_no_update AS ON UPDATE TO audit_log DO INSTEAD NOTHING;
CREATE RULE audit_log_no_delete AS ON DELETE TO audit_log DO INSTEAD NOTHING;


-- ─── erasure requests (GDPR Art. 17) ────────────────────────────────────────
-- A subject erasure request touches assets, faces, consent records, events, and
-- audit rows, each with different rules: face vectors get deleted, audit entries
-- get their payload redacted but keep their hash-chain position, and assets under
-- legal hold are exempt and must be reported as such rather than silently skipped.

CREATE TABLE erasure_requests (
    id              uuid PRIMARY KEY,
    subject_name    text NOT NULL,
    subject_contact text,
    person_id       uuid REFERENCES people (id) ON DELETE SET NULL,
    basis           text NOT NULL DEFAULT 'gdpr_art17'
                        CHECK (basis IN ('gdpr_art17', 'ccpa', 'consent_withdrawal',
                                         'bipa', 'other')),
    state           text NOT NULL DEFAULT 'received'
                        CHECK (state IN ('received', 'verifying', 'in_progress',
                                         'partially_complete', 'complete', 'refused')),
    -- What was done, and what could not be and why. A request that hit a legal
    -- hold is 'partially_complete' with a documented exemption, never 'complete'.
    scope_report    jsonb NOT NULL DEFAULT '{}'::jsonb,
    exemptions      jsonb NOT NULL DEFAULT '[]'::jsonb,
    received_at     timestamptz NOT NULL DEFAULT now(),
    -- GDPR gives one month, extendable to three. A due date makes the clock visible.
    due_at          timestamptz,
    completed_at    timestamptz,
    handled_by      uuid
);

CREATE INDEX erasure_requests_open_idx ON erasure_requests (due_at)
    WHERE state NOT IN ('complete', 'refused');
CREATE INDEX erasure_requests_person_idx ON erasure_requests (person_id);
