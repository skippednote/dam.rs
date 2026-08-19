-- Metadata types: which fields apply to which kind of asset (Q.1).
--
-- `field_defs` is the tenant's field *vocabulary* and stays exactly that. A key is still unique per tenant,
-- still the JSONB member name every stored value sits under, and still has exactly one kind — that invariant
-- is what the schema-admin refusals in `dam_db::fields` exist to protect, and splitting it per type would
-- reintroduce the mismatch they prevent. What a metadata type varies is *which* of those fields apply to an
-- asset, and in what order.
--
-- Why that matters: a tenant with one flat field set makes every field apply to every asset, so a video
-- carries the print-resolution fields, an archive carries alt text, and "required" is unusable — a field
-- required for photographs cannot be required at all if a ZIP has to satisfy it too. Acquia's answer is a
-- type per kind of asset (Image, Video, Document, Archives, plus custom ones), and an asset names its type.
--
-- Deliberately created empty. A tenant with no types behaves exactly as it did before this migration: every
-- field applies to every asset. Seeding built-in types here would silently narrow every existing asset's
-- field list to whichever type it matched, which is a data-visibility change disguised as a migration.

CREATE TABLE metadata_types (
    id              uuid PRIMARY KEY,
    key             text NOT NULL,
    label           text NOT NULL,

    -- The media classes this type is the natural choice for, so ingest can pick without being told:
    -- 'image', 'video', 'audio', 'document', 'archive'. Empty means "only when asked for by name".
    -- An array rather than one value because one type legitimately covers stills and vector art.
    applies_to      text[] NOT NULL DEFAULT '{}',

    -- The fallback when nothing matches an asset's media class. At most one per tenant, enforced below.
    is_default      boolean NOT NULL DEFAULT false,

    display_order   integer NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX metadata_types_key_idx ON metadata_types (key);

-- One default, not several: "the fallback" is singular, and two rows claiming it would make an asset's field
-- list depend on row order. A partial unique index says so in the schema rather than in a comment.
CREATE UNIQUE INDEX metadata_types_one_default_idx ON metadata_types ((is_default)) WHERE is_default;

-- Which fields a type includes, and their order within it.
--
-- `field_key` references the key rather than the id on purpose: the key is what the payload, the search
-- shorthand and the stored JSONB all use, so a membership row keyed by id would be the one place in the
-- system that identified a field differently. ON DELETE CASCADE because removing a definition removes it
-- from every type — its *values* survive (see `dam_db::fields::remove`), its membership does not.
CREATE TABLE metadata_type_fields (
    metadata_type_id uuid NOT NULL REFERENCES metadata_types(id) ON DELETE CASCADE,
    field_key        text NOT NULL REFERENCES field_defs(key) ON DELETE CASCADE,
    display_order    integer NOT NULL DEFAULT 0,
    PRIMARY KEY (metadata_type_id, field_key)
);

CREATE INDEX metadata_type_fields_field_idx ON metadata_type_fields (field_key);

-- The asset's type. Nullable, and null is meaningful: an asset ingested before its tenant had types, or one
-- whose type was removed. Resolution falls back to the default type, and then to every field — so a null
-- here never hides metadata that is already stored.
--
-- ON DELETE SET NULL rather than RESTRICT: removing a type is an administrative decision about the schema,
-- and it must not be blocked by however many assets happen to reference it. They fall back, visibly.
ALTER TABLE assets
    ADD COLUMN metadata_type_id uuid REFERENCES metadata_types(id) ON DELETE SET NULL;

CREATE INDEX assets_metadata_type_idx ON assets (metadata_type_id) WHERE metadata_type_id IS NOT NULL;
