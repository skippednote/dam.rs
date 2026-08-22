-- Auto-import mappings: embedded metadata into the tenant's own fields (Q.4).
--
-- A camera writes `exif.artist`; this tenant calls it `photographer`. Acquia has a screen for exactly that
-- translation, and it is configuration rather than code because the left-hand side is fixed by the file formats
-- while the right-hand side is whatever the tenant decided their schema is.
--
-- Why a table and not a column on `field_defs`: the relation is many-to-one in the useful direction. Two
-- sources can feed one field — `xmp.creator` when an editor set it, `exif.artist` when only the camera did —
-- and the *order* between them is the interesting part, because the first one present wins.

CREATE TABLE auto_import_mappings (
    id              uuid PRIMARY KEY,

    -- The embedded name, as `dam_media::embedded` reports it: `exif.artist`, `xmp.title`. Free text rather than
    -- an enum, because the source list grows with the extractor and a migration per tag would be absurd — and
    -- because a mapping naming a source this build does not produce is harmless: it simply never matches.
    source          text NOT NULL CHECK (source ~ '^[a-z][a-z0-9_]*\.[a-z][a-z0-9_]*$'),

    -- The tenant's field. ON DELETE CASCADE because a mapping into a field that no longer exists is not a
    -- mapping; it is a rule that can never fire, and keeping it would make the import screen list phantoms.
    field_key       text NOT NULL REFERENCES field_defs(key) ON DELETE CASCADE,

    -- Which source wins when several are present for one field. Lower first.
    --
    -- The reason this table exists in this shape: "prefer what the editor typed, fall back to what the camera
    -- recorded" is the ordinary requirement, and it cannot be expressed without an order.
    priority        integer NOT NULL DEFAULT 0,

    -- Whether an import may replace a value the asset already has.
    --
    -- Default false, and that default is the safe direction: re-running an import over a library somebody has
    -- since curated would otherwise overwrite their corrections with whatever the camera said. Turning it on is
    -- a deliberate "the file is the source of truth for this field".
    overwrite       boolean NOT NULL DEFAULT false,

    enabled         boolean NOT NULL DEFAULT true,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- One mapping per (source, field): the same source twice into one field is either a duplicate or a contradiction
-- about priority, and neither is worth storing.
CREATE UNIQUE INDEX auto_import_source_field_idx
    ON auto_import_mappings (source, field_key);

-- The read path is "every enabled mapping, best first", so the index matches it.
CREATE INDEX auto_import_resolution_idx
    ON auto_import_mappings (field_key, priority) WHERE enabled;
