-- Vocabulary administration (Q.20b).
--
-- `taxonomies.ai_taggable` has existed since 0001 and nothing has ever read it. The zero-shot vocabulary query
-- selects every non-deprecated term in the tenant, so an administrator who marked a vocabulary off-limits to
-- machine tagging changed nothing, and category trees — filing structure, not a label set — were offered to the
-- model alongside the vocabularies. This migration makes the flag real.
--
-- The backfill is the whole reason this is a migration rather than a one-line code change. The column defaults
-- to `false`, so honouring it without one would hand every existing tenant an empty vocabulary and silently
-- stop AI tagging that works today. From here the flag governs, and a *new* taxonomy stays `false` until
-- somebody opts it in, which is the governed default the column was written for.
--
-- Scoped to `kind = 'vocabulary'`, and the first version was not. Setting it on every row looked like the
-- conservative choice — preserve today's behaviour exactly — and on the dev tenant, whose only taxonomy is a
-- category tree, it opened a browse hierarchy to the model. That is the half of the defect the flag does not
-- close on its own, so the query now requires `kind = 'vocabulary'` as well, and this sets the flag only where
-- it means anything. A tenant that was relying on category terms being suggested loses that, deliberately:
-- inviting an LLM to file assets into somebody's hierarchy is a much larger claim than inviting it to suggest
-- a tag, and nobody ever chose it.

UPDATE taxonomies SET ai_taggable = true, updated_at = now() WHERE kind = 'vocabulary';

-- The query that reads it filters on `ai_taggable` and joins terms to taxonomies, so it needs to find the
-- taggable ones without scanning every taxonomy in the tenant.
CREATE INDEX taxonomies_ai_taggable_idx ON taxonomies (id) WHERE ai_taggable;

-- Deprecation is what retires a term from *assignment*, and it is already indexed for the picker. This is the
-- vocabulary prompt's shape: taggable taxonomy, live term, ordered by slug for byte-identical prompt prefixes.
CREATE INDEX taxonomy_terms_vocabulary_idx ON taxonomy_terms (taxonomy_id, slug)
    WHERE deprecated_at IS NULL;
