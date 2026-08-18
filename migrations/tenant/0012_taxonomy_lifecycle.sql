-- Taxonomy deprecation and supersession (2.2).
--
-- A taxonomy is not a fixed thing. Customers rename, reparent, and discover that
-- two of their terms were always the same term. What they must never do is change
-- what an existing asset means, and the naive implementations of all three
-- operations do exactly that:
--
--   * deleting a term takes its tags with it (ON DELETE CASCADE on asset_tags),
--     so "merging duplicates" silently untags assets;
--   * reusing a term's id for a different concept rewrites history;
--   * hard-deleting breaks every stored reference held outside this database —
--     a saved search, a Drupal field, an API client's cached id.
--
-- So terms are deprecated, never deleted, and a merge records where the meaning
-- went rather than discarding the term that carried it.
--
-- `deprecated_at` retires a term from *new* assignment. It stays fully
-- resolvable: the row keeps its id, path and label, existing asset_tags keep
-- pointing at it, and a search for it still works. That is the whole point —
-- "outdoor" tagged in 2019 still means outdoor after the vocabulary is
-- reorganised in 2026.
--
-- `superseded_by` is where a merge sent the meaning. It exists so an external
-- reference to the old id keeps working: resolution follows the chain to the
-- surviving term. Without it, a merge is indistinguishable from a deletion to
-- everything outside this schema.

ALTER TABLE taxonomy_terms
    ADD COLUMN deprecated_at  timestamptz,
    -- ON DELETE SET NULL, not CASCADE. If the surviving term is later deleted,
    -- the deprecated term must remain — it still carries the meaning of every
    -- asset tagged with it. CASCADE here would delete those assets' tags as a
    -- second-order effect of an unrelated deletion, which is the failure this
    -- whole migration exists to prevent.
    ADD COLUMN superseded_by  uuid REFERENCES taxonomy_terms (id) ON DELETE SET NULL,
    -- A live term cannot point somewhere else; that would mean two active terms
    -- for one concept, which is the state a merge is supposed to end.
    ADD CONSTRAINT taxonomy_terms_superseded_is_deprecated CHECK (
        superseded_by IS NULL OR deprecated_at IS NOT NULL),
    -- A one-element cycle. Longer cycles cannot be expressed in a CHECK and are
    -- refused in `dam_db::taxonomy`, which walks the chain.
    ADD CONSTRAINT taxonomy_terms_no_self_supersede CHECK (
        superseded_by IS NULL OR superseded_by <> id);

-- The set a picker offers and a new tag may use. Partial, because the live terms
-- are the overwhelming majority and the deprecated ones are read by id.
CREATE INDEX taxonomy_terms_live_idx ON taxonomy_terms (taxonomy_id, path)
    WHERE deprecated_at IS NULL;

-- Walking a supersession chain is a per-request operation on the delivery path
-- once saved searches carry term ids, so the hop is indexed.
CREATE INDEX taxonomy_terms_superseded_idx ON taxonomy_terms (superseded_by)
    WHERE superseded_by IS NOT NULL;

COMMENT ON COLUMN taxonomy_terms.deprecated_at IS
    'Retired from new assignment. Still resolvable, and existing asset_tags still '
    'point here — a term tagged years ago keeps its meaning.';

COMMENT ON COLUMN taxonomy_terms.superseded_by IS
    'Where a merge sent this term''s meaning. Resolution follows the chain so an '
    'external reference to the old id keeps working.';
