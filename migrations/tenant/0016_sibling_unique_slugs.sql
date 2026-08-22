-- Category trees need the same slug in different branches (Q.2).
--
-- `taxonomy_terms` carried two unique indexes:
--
--   taxonomy_terms_path_idx (taxonomy_id, path)   -- no two siblings share a slug
--   taxonomy_terms_slug_idx (taxonomy_id, slug)   -- no two terms anywhere share a slug
--
-- The second forbids an ordinary category tree. "Yellow" under both Exterior and Interior, "Overview" under
-- every product line, "2024" under every campaign — these are the shape a filing hierarchy takes, and the
-- slug index made them a constraint violation.
--
-- It is also redundant for the property that actually matters. A path *is* the parent's path plus this term's
-- slug, so uniqueness on `(taxonomy_id, path)` already says "no two siblings share a slug" — which is the rule
-- a tree needs. The slug index added only the stronger, unwanted claim.
--
-- Nothing relied on the stronger form: no query in the workspace resolves a term by slug alone. `slug` is
-- selected alongside `path` for display, and `taxonomy::move_term` uses it to rebuild paths on re-parenting —
-- where a genuine collision still surfaces, from the path index, which is the right place for it.
--
-- Relaxing a uniqueness constraint is one-way in practice: re-adding this index later would fail on any tree
-- that has since used a slug twice. That is accepted deliberately — the alternative is a DAM that cannot
-- express a category tree.

DROP INDEX IF EXISTS taxonomy_terms_slug_idx;

-- Kept as a non-unique index: `slug` is still worth looking up (a picker filtering as somebody types), and
-- dropping the index outright would turn that into a sequential scan over every term in the tenant.
CREATE INDEX taxonomy_terms_slug_lookup_idx ON taxonomy_terms (taxonomy_id, slug);
