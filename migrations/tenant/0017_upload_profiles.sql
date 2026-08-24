-- Upload profiles: what an upload arrives already knowing (Q.3).
--
-- The comparator ties three things to the profile an upload was made under: metadata defaults, whether the uploader
-- forces required fields to be filled before proceeding, and whether AI tagging applies (its tag backfill
-- explicitly skips assets "that have AI Tags turned off on the associated Upload Profile"). One profile per
-- kind of intake — a photographer's drop, a partner's bulk delivery, a marketing upload — so the answer to
-- "who sent this and what should already be true of it" is a row rather than a convention.
--
-- Why a profile rather than more columns on the session: the same three answers are needed by the uploader
-- (before any bytes move), by finalise (when the asset row is written) and by enrichment (later, in a
-- worker). A profile is the one place all three can read, and it is what makes the intake reproducible —
-- re-running an import under the same profile gets the same defaults.

CREATE TABLE upload_profiles (
    id                  uuid PRIMARY KEY,
    key                 text NOT NULL,
    label               text NOT NULL,

    -- The form uploads under this profile get, overriding the media-class guess ingest would otherwise make.
    -- Null means "let the mime decide", which is the behaviour from before profiles existed.
    --
    -- ON DELETE SET NULL rather than RESTRICT: removing a metadata type is a schema decision and must not be
    -- blocked by a profile referencing it. The profile falls back to the guess, visibly.
    metadata_type_id    uuid REFERENCES metadata_types(id) ON DELETE SET NULL,

    -- Field values applied to every asset arriving under this profile.
    --
    -- Validated against the schema at *save* time and again at apply time. Twice on purpose: a definition can
    -- change between the two, and a default that has quietly become invalid must fail where somebody can see
    -- it rather than silently not apply.
    defaults            jsonb NOT NULL DEFAULT '{}',

    -- Whether the uploader makes a person fill required fields before proceeding.
    --
    -- A client-facing rule, and deliberately not enforced at finalise: bytes are already in staging by then,
    -- and refusing there would strand an upload over metadata a person could have supplied. The library's
    -- answer to "which assets are incomplete" is the worklist query instead, which is where an incomplete
    -- asset can actually be fixed.
    require_complete    boolean NOT NULL DEFAULT false,

    -- Whether AI tagging runs for assets from this intake. Some deliveries arrive already described, and
    -- some (a partner's, a legal set) must not be machine-tagged at all.
    ai_tags_enabled     boolean NOT NULL DEFAULT true,

    -- The fallback when an upload names no profile. At most one, enforced below.
    is_default          boolean NOT NULL DEFAULT false,

    display_order       integer NOT NULL DEFAULT 0,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX upload_profiles_key_idx ON upload_profiles (key);

-- One default, for the same reason `metadata_types` has one: "the fallback" is singular, and two rows
-- claiming it would make an upload's treatment depend on row order.
CREATE UNIQUE INDEX upload_profiles_one_default_idx ON upload_profiles ((is_default)) WHERE is_default;

-- Which profile an upload was made under.
--
-- On the session rather than only on the asset, because the uploader needs the answer before an asset exists —
-- that is what `require_complete` is for — and because a session that never finalises should still record what
-- it was trying to be.
ALTER TABLE upload_sessions
    ADD COLUMN upload_profile_id uuid REFERENCES upload_profiles(id) ON DELETE SET NULL;

-- `assets.upload_profile_id` already existed: 0001 reserved the column with no table behind it and no
-- constraint, anticipating this. So this adds the reference rather than the column — which is the part that was
-- missing, and the part that keeps a profile id pointing at something real.
--
-- The column is on the asset as well as the session because the answer has to survive the session's cleanup:
-- enrichment runs long after the reaper has taken the session row, and "was this allowed to be machine-tagged"
-- must still be answerable then.
ALTER TABLE assets
    ADD CONSTRAINT assets_upload_profile_fkey
        FOREIGN KEY (upload_profile_id) REFERENCES upload_profiles(id) ON DELETE SET NULL;

CREATE INDEX assets_upload_profile_idx ON assets (upload_profile_id)
    WHERE upload_profile_id IS NOT NULL;
