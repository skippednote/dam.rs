-- ─── refine-search configuration (Q.19) ─────────────────────────────────────
--
-- Which filters the rail offers, in which order. Until now the rail was every
-- facetable field ordered by `field_defs.display_order`, then every vocabulary by
-- label, then the four built-ins in a fixed order — a reasonable default and not
-- something a tenant could change. A library with thirty facetable fields has a rail
-- nobody scrolls to the bottom of, and the fields that matter are wherever the schema
-- happened to put them.
--
-- One row per *entry*, and an entry is not a field: it is anything the rail can show,
-- named by kind so a taxonomy and a metadata field with the same name cannot collide.
--   field:brand          a facetable metadata field
--   taxonomy:<uuid>      a vocabulary
--   builtin:status       one of the four every library has (Q.15)
--
-- Absent rows mean "the default", which is why this table starts empty and stays that
-- way for a tenant that never configures anything. A configured tenant's rail is
-- exactly the enabled rows in `position` order — including the built-ins, which is
-- what makes "we do not use ratings" expressible without asking us.
CREATE TABLE search_facets (
    entry           text PRIMARY KEY,
    -- Gaps are allowed and expected: the API writes 10, 20, 30 so a later insertion
    -- between two entries does not have to renumber the rest.
    position        integer NOT NULL,
    is_enabled      boolean NOT NULL DEFAULT true,
    updated_at      timestamptz NOT NULL DEFAULT now(),

    -- The shape is enforced here rather than in a handler, because a row that names
    -- neither a field, a taxonomy nor a built-in is a rail entry nothing can render —
    -- and it would fail silently, as an absence.
    CONSTRAINT search_facets_entry_shape CHECK (
        entry ~ '^field:[a-z][a-z0-9_]*$'
        OR entry ~ '^taxonomy:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
        OR entry IN ('builtin:status', 'builtin:orientation', 'builtin:stars', 'builtin:has')
    )
);

CREATE INDEX search_facets_position_idx ON search_facets (position, entry);
