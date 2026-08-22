-- What a tenant wants a model to do, and whether it may (M5b).
--
-- Enrichment is the first feature in damrs that spends money per asset, and §8.3's table puts a naive run over a
-- million-asset library at $23k. So this exists for one reason before any other: `is_enabled` defaults to
-- **false**, and nothing enriches anything until somebody turns it on. A feature that starts enabled and bills
-- by the asset is a feature that produces an invoice before a decision.
--
-- ## A singleton, spelled as one
--
-- One row per tenant schema, enforced by the primary key rather than by application code: `id` is a boolean that
-- must be true, so a second row is a constraint violation instead of a silent second opinion about which
-- settings apply. The alternative — a key/value table — turns every read into a lookup with a default and every
-- typo into a silently ignored setting.
--
-- ## The field names are configuration
--
-- A model writes *values*, and which field they land in is the tenant's decision: a library may call it
-- `caption` rather than `description`, and one that has neither should not have rows appearing under names it
-- did not define. `field_defs.ai_writable` still governs whether the write is allowed at all — this only says
-- where to aim.
--
-- ## The prompt lives here too
--
-- `guidance` is the tenant's own words — house style, what to avoid — and it is the cacheable half of every
-- request (§8.3's ~90% discount applies to exactly this prefix). Keeping it in a column rather than in a config
-- file is what lets a tenant change its own instructions without a deploy, and what makes "why did it write
-- that" answerable.

CREATE TABLE enrichment_settings (
    -- The singleton lock. `true` is the only permitted value, so this table holds one row or none.
    id                  boolean PRIMARY KEY DEFAULT true CHECK (id),

    -- Off until somebody says otherwise. See the note above.
    is_enabled          boolean NOT NULL DEFAULT false,

    guidance            text NOT NULL DEFAULT '',
    -- Written into the instructions, so a library serving one market does not get English by accident.
    language            text NOT NULL DEFAULT 'English'
                            CHECK (length(btrim(language)) BETWEEN 2 AND 64),

    -- Overrides the credential's default model for this pipeline. §8.3: "model routing per pipeline stage is
    -- configuration, not code".
    model               text CHECK (model IS NULL OR length(btrim(model)) BETWEEN 1 AND 128),

    -- Where the two written values land. NULL means "do not write this one at all", which is how a tenant that
    -- wants tags and no prose says so.
    alt_text_field      text DEFAULT 'alt_text'
                            CHECK (alt_text_field IS NULL OR length(btrim(alt_text_field)) BETWEEN 1 AND 64),
    description_field   text DEFAULT 'description'
                            CHECK (description_field IS NULL OR length(btrim(description_field)) BETWEEN 1 AND 64),
    suggest_tags        boolean NOT NULL DEFAULT true,

    updated_at          timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE enrichment_settings IS
    'One row. What the hosted-model enrichment pipeline should do for this tenant, and whether it may run at '
    'all — `is_enabled` is false until somebody turns it on, because the pipeline bills per asset.';

-- The row exists from the start, disabled, so a settings screen has something to read and every caller can
-- assume a row rather than branching on its absence.
INSERT INTO enrichment_settings (id) VALUES (true);
