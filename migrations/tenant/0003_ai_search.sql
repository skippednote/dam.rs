-- AI enrichment and search substrate. See ARCHITECTURE §7, §8.
--
-- Everything in this file is derived from the MASTER PROXY, never from the
-- original master. That is what lets a fully archived library be tagged,
-- embedded, searched, and re-processed with zero restores (§2), and it is why a
-- tagging-model upgrade is a reindex rather than a library thaw.
--
-- Everything here is also rebuildable (D4). Tantivy holds no state this schema
-- cannot regenerate, and `damctl reindex` is always safe.


-- ─── model registry ─────────────────────────────────────────────────────────
-- Every AI-written value names the model and version that produced it. Without
-- this table, "re-run everything the old tagger touched" is not expressible, and
-- an enrichment regression cannot be scoped or rolled back.

CREATE TABLE ai_models (
    id              uuid PRIMARY KEY,
    key             text NOT NULL,           -- 'siglip-so400m', 'claude-opus-5', ...
    version         text NOT NULL,
    kind            text NOT NULL CHECK (kind IN (
                        'image_embed', 'text_embed', 'ocr', 'asr', 'face_detect',
                        'face_embed', 'saliency', 'tag_probe', 'llm')),
    runtime         text NOT NULL CHECK (runtime IN ('onnx', 'candle', 'api', 'native')),
    dim             int,                     -- embedding models only
    -- Frozen at registration so a served model never silently changes shape.
    config          jsonb NOT NULL DEFAULT '{}'::jsonb,
    active          boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX ai_models_key_version_idx ON ai_models (key, version);
CREATE INDEX ai_models_active_idx ON ai_models (kind) WHERE active;


-- ─── extracted text ─────────────────────────────────────────────────────────
-- PDF text layer, OCR, office extraction, ASR transcript. Small, hot forever,
-- and the reason a Deep Archive scan is still full-text searchable.
--
-- Stored per (asset, source) so a re-run of OCR does not clobber the PDF text
-- layer, and so the UI can say where a snippet came from.

CREATE TABLE asset_text (
    id              uuid PRIMARY KEY,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    source          text NOT NULL CHECK (source IN ('pdf_layer', 'ocr', 'office',
                                                    'asr', 'metadata', 'manual')),
    model_id        uuid REFERENCES ai_models (id) ON DELETE SET NULL,
    locale          text,
    content         text NOT NULL,
    -- Page / timecode anchors so a search hit can deep-link into a 300-page PDF
    -- or seek a video to the second the phrase was spoken.
    segments        jsonb NOT NULL DEFAULT '[]'::jsonb,
    confidence      real,
    char_count      int NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX asset_text_source_idx ON asset_text (asset_id, source, coalesce(locale, ''));
CREATE INDEX asset_text_asset_idx ON asset_text (asset_id);

-- Postgres FTS here is a fallback and a correctness reference; Tantivy serves
-- production queries. Keeping both means a Tantivy rebuild can be validated
-- against a second implementation rather than against itself.
CREATE INDEX asset_text_fts_idx ON asset_text
    USING gin (to_tsvector('simple', content));


-- ─── embeddings ─────────────────────────────────────────────────────────────
-- pgvector requires a fixed dimension per index, so one polymorphic embedding
-- column is not possible. Vision and text therefore get separate tables at their
-- models' native widths:
--
--   image  1152  SigLIP 2 so400m
--   text   1024  multilingual-e5-large
--
-- The split is not just a dimension detail — it is what makes the 50+ language
-- metadata claim honest (GAPS.md G16). SigLIP's text tower is English-centric, so
-- using it for both sides would mean non-English queries silently retrieve worse
-- than English ones, which is the kind of regression nobody notices until a
-- customer in Germany reports that search "feels broken."
--
-- Cross-modal text->image search therefore goes through SigLIP's own text tower
-- (stored here as kind='image_query'), while text->text search over OCR,
-- transcripts, and metadata uses the multilingual encoder. Two retrieval paths,
-- fused with the lexical one.
--
-- Adding a model of a different width is a migration, deliberately: it forces the
-- reindex and backfill decision to be explicit rather than discovered in prod.
-- fp32 rather than halfvec — quantisation is a tuning step to take with
-- measurements, not up front.

CREATE TABLE asset_image_embeddings (
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    model_id        uuid NOT NULL REFERENCES ai_models (id) ON DELETE CASCADE,
    kind            text NOT NULL DEFAULT 'image'
                        CHECK (kind IN ('image', 'shot', 'page')),
    -- Shot index for video, page index for documents; 0 for stills.
    seq             int NOT NULL DEFAULT 0,
    start_ms        bigint,
    end_ms          bigint,
    embedding       extensions.vector(1152) NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (asset_id, model_id, kind, seq)
);

-- Cosine: SigLIP vectors are normalised, and cosine is what the zero-shot label
-- scoring in §8.2 uses. Build with maintenance_work_mem raised well above the
-- default or a 1M-row HNSW build takes hours longer than it needs to.
CREATE INDEX asset_image_embeddings_hnsw_idx ON asset_image_embeddings
    USING hnsw (embedding extensions.vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

CREATE INDEX asset_image_embeddings_model_idx ON asset_image_embeddings (model_id, kind);

CREATE TABLE asset_text_embeddings (
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    model_id        uuid NOT NULL REFERENCES ai_models (id) ON DELETE CASCADE,
    -- Which text this vector came from, so a transcript hit and a metadata hit
    -- can be ranked and explained differently.
    kind            text NOT NULL CHECK (kind IN ('metadata', 'ocr', 'transcript',
                                                  'description', 'chunk')),
    seq             int NOT NULL DEFAULT 0,
    locale          text,
    embedding       extensions.vector(1024) NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (asset_id, model_id, kind, seq)
);

CREATE INDEX asset_text_embeddings_hnsw_idx ON asset_text_embeddings
    USING hnsw (embedding extensions.vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

CREATE INDEX asset_text_embeddings_locale_idx ON asset_text_embeddings (locale, kind);


-- ─── vocabulary term embeddings ─────────────────────────────────────────────
-- Pre-computed embeddings of every taxonomy term label and synonym, in SigLIP's
-- text-tower space so they are directly comparable to image embeddings.
-- Zero-shot tagging is then one cosine query per asset against this table, which
-- is what makes tagging an entire library effectively free (§8.2, generator 1).
--
-- `labels_i18n` on taxonomy_terms means a term can carry one row per locale here,
-- so a German-labelled vocabulary scores German assets without a translation hop.

CREATE TABLE term_embeddings (
    term_id         uuid NOT NULL REFERENCES taxonomy_terms (id) ON DELETE CASCADE,
    model_id        uuid NOT NULL REFERENCES ai_models (id) ON DELETE CASCADE,
    variant         text NOT NULL DEFAULT 'label',   -- 'label' | 'synonym:<n>' | 'prompt'
    -- '' rather than NULL for "unspecified": a nullable column cannot participate
    -- in a primary key usefully, and an expression is not allowed in one.
    locale          text NOT NULL DEFAULT '',
    embedding       extensions.vector(1152) NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (term_id, model_id, variant, locale)
);

CREATE INDEX term_embeddings_hnsw_idx ON term_embeddings
    USING hnsw (embedding extensions.vector_cosine_ops);


-- ─── tags ───────────────────────────────────────────────────────────────────
-- Tags resolve to taxonomy_terms or they do not land. Free-text AI tags are the
-- standard DAM data-quality failure: they are unfacetable, unmergeable, and
-- accumulate synonyms until search stops working.
--
-- `state` is the review gate. Auto-applying unreviewed AI tags to a governed
-- library is how you lose an enterprise customer, so `suggested` is the default
-- for anything below a term's threshold and `confirmed` requires either a human
-- or a term whose measured precision earns auto-application.

CREATE TABLE asset_tags (
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    term_id         uuid NOT NULL REFERENCES taxonomy_terms (id) ON DELETE CASCADE,

    state           text NOT NULL DEFAULT 'suggested'
                        CHECK (state IN ('suggested', 'confirmed', 'rejected')),
    source          text NOT NULL CHECK (source IN ('human', 'zero_shot', 'probe',
                                                    'llm', 'import', 'rule')),
    model_id        uuid REFERENCES ai_models (id) ON DELETE SET NULL,
    confidence      real,
    -- How many of the three generators independently proposed this tag. A tag
    -- all three agree on is a different thing from one the LLM alone invented,
    -- and the review queue sorts on it.
    generator_votes smallint NOT NULL DEFAULT 1,

    reviewed_by     uuid,                    -- dam_global.identities.id
    reviewed_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (asset_id, term_id)
);

CREATE INDEX asset_tags_term_idx ON asset_tags (term_id)
    WHERE state = 'confirmed';
CREATE INDEX asset_tags_review_idx ON asset_tags (confidence DESC)
    WHERE state = 'suggested';
CREATE INDEX asset_tags_source_idx ON asset_tags (source, state);


-- ─── tagging feedback loop ──────────────────────────────────────────────────
-- Every human accept/reject lands here. Nightly: retrain the per-tenant linear
-- probe on frozen embeddings, recompute per-term precision, and auto-tune
-- taxonomy_terms.ai_threshold. Terms below the precision floor demote to
-- suggest-only.
--
-- This is append-only on purpose. It is the training set, so an edit history
-- that loses the rejections loses the signal that matters most.

CREATE TABLE tag_feedback (
    id              uuid PRIMARY KEY,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    term_id         uuid NOT NULL REFERENCES taxonomy_terms (id) ON DELETE CASCADE,
    verdict         text NOT NULL CHECK (verdict IN ('accept', 'reject', 'add')),
    proposed_by     text CHECK (proposed_by IN ('zero_shot', 'probe', 'llm')),
    model_id        uuid REFERENCES ai_models (id) ON DELETE SET NULL,
    confidence      real,                    -- what the model claimed
    actor_id        uuid,                    -- dam_global.identities.id
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX tag_feedback_term_idx ON tag_feedback (term_id, created_at DESC);
CREATE INDEX tag_feedback_training_idx ON tag_feedback (created_at)
    WHERE proposed_by IS NOT NULL;


-- ─── people (face clustering) ───────────────────────────────────────────────
-- Faces are detected and embedded per asset, then clustered into `people`.
-- Clusters are unnamed until a human names one, at which point every member
-- inherits the label — which is the whole UX win over per-asset face tagging.
--
-- Face vectors are biometric data under GDPR/BIPA. `people.consent_ref` points
-- at the model release or consent record; a person without one can be flagged
-- for review or excluded from search entirely by policy.

CREATE TABLE people (
    id              uuid PRIMARY KEY,
    label           text,                    -- null = unnamed cluster
    consent_ref     text,
    is_public_figure boolean NOT NULL DEFAULT false,
    face_count      int NOT NULL DEFAULT 0,
    centroid        extensions.vector(512),  -- ArcFace is 512-dim
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX people_named_idx ON people (label) WHERE label IS NOT NULL;
CREATE INDEX people_centroid_idx ON people
    USING hnsw (centroid extensions.vector_cosine_ops);

CREATE TABLE asset_faces (
    id              uuid PRIMARY KEY,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    person_id       uuid REFERENCES people (id) ON DELETE SET NULL,
    model_id        uuid REFERENCES ai_models (id) ON DELETE SET NULL,
    -- Normalised 0..1 so the box survives every derivative size.
    bbox            real[4] NOT NULL,
    embedding       extensions.vector(512) NOT NULL,
    detect_score    real,
    cluster_score   real,
    -- A human moving a face between people must not be undone by the next
    -- clustering pass.
    pinned          boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX asset_faces_asset_idx ON asset_faces (asset_id);
CREATE INDEX asset_faces_person_idx ON asset_faces (person_id);
CREATE INDEX asset_faces_hnsw_idx ON asset_faces
    USING hnsw (embedding extensions.vector_cosine_ops);


-- ─── colour ─────────────────────────────────────────────────────────────────
-- Dominant colours from k-means in LAB (not RGB — Euclidean distance in RGB does
-- not match perceived similarity, so RGB clustering produces facets that look
-- wrong to a designer). Backs hex search and the colour facet.

CREATE TABLE asset_colors (
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    rank            smallint NOT NULL,
    hex             text NOT NULL,
    lab             real[3] NOT NULL,
    coverage        real NOT NULL,           -- fraction of pixels
    -- Snapped to a fixed palette so facet counts group meaningfully instead of
    -- producing one bucket per distinct hex.
    palette_bucket  text NOT NULL,

    PRIMARY KEY (asset_id, rank)
);

CREATE INDEX asset_colors_bucket_idx ON asset_colors (palette_bucket);
CREATE INDEX asset_colors_hex_idx ON asset_colors (hex);


-- ─── duplicates ─────────────────────────────────────────────────────────────
-- Exact duplicates are free — identical BLAKE3 means one object, caught at
-- ingest. This table is for NEAR duplicates: pHash Hamming distance plus
-- embedding cosine, surfaced as a review queue rather than auto-merged.
-- Auto-merging a crop that is actually a different licensed deliverable is a
-- rights problem, so a human decides.

CREATE TABLE asset_phashes (
    asset_id        uuid PRIMARY KEY REFERENCES assets (id) ON DELETE CASCADE,
    phash           bigint NOT NULL,
    dhash           bigint NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX asset_phashes_phash_idx ON asset_phashes (phash);

CREATE TABLE duplicate_candidates (
    id              uuid PRIMARY KEY,
    asset_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    other_id        uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    hamming         smallint,
    cosine          real,
    relation        text CHECK (relation IN ('near_identical', 'crop', 'recolor',
                                             'rescale', 'variant')),
    state           text NOT NULL DEFAULT 'open'
                        CHECK (state IN ('open', 'confirmed', 'dismissed', 'merged')),
    resolved_by     uuid,
    resolved_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT duplicate_pair_ordered CHECK (asset_id < other_id)
);

CREATE UNIQUE INDEX duplicate_candidates_pair_idx ON duplicate_candidates (asset_id, other_id);
CREATE INDEX duplicate_candidates_open_idx ON duplicate_candidates (state, cosine DESC)
    WHERE state = 'open';


-- ─── enrichment runs ────────────────────────────────────────────────────────
-- One row per DAG execution per asset. Makes enrichment auditable, resumable
-- from the failed stage rather than the start, and cost-attributable — the token
-- counters here are what roll up into dam_global.tenant_usage_daily.
--
-- `used_original` should be false on every row. If it starts appearing, some
-- stage is reading the master instead of the proxy, and the cold-storage design
-- is quietly broken — a restore storm during the next model upgrade is the
-- symptom. Worth an alert, not just a column.

CREATE TABLE enrichment_runs (
    id                  uuid PRIMARY KEY,
    asset_id            uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    pipeline            text NOT NULL,
    pipeline_version    int NOT NULL DEFAULT 1,

    state               text NOT NULL DEFAULT 'running'
                            CHECK (state IN ('running', 'succeeded', 'partial',
                                             'failed', 'skipped')),
    stages              jsonb NOT NULL DEFAULT '{}'::jsonb,   -- {stage: {state, ms, error}}
    failed_stage        text,

    used_original       boolean NOT NULL DEFAULT false,
    -- Batch API results arrive keyed by custom_id, so the id must be persisted
    -- before the batch is submitted.
    llm_batch_id        text,
    llm_custom_id       text,
    input_tokens        bigint NOT NULL DEFAULT 0,
    output_tokens       bigint NOT NULL DEFAULT 0,
    cached_tokens       bigint NOT NULL DEFAULT 0,
    est_cost_cents      numeric(12, 4) NOT NULL DEFAULT 0,

    started_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz,
    duration_ms         int
);

CREATE INDEX enrichment_runs_asset_idx ON enrichment_runs (asset_id, started_at DESC);
CREATE INDEX enrichment_runs_state_idx ON enrichment_runs (state, started_at)
    WHERE state IN ('running', 'failed', 'partial');
CREATE INDEX enrichment_runs_batch_idx ON enrichment_runs (llm_batch_id)
    WHERE llm_batch_id IS NOT NULL;
CREATE INDEX enrichment_runs_leak_idx ON enrichment_runs (started_at)
    WHERE used_original;
