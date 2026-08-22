-- Natural-language search is its own switch (M5d).
--
-- 0028 gave the tenant one boolean for hosted-model work, and that was right while there was one such feature.
-- There are now two, and they are not the same decision:
--
--   * `is_enabled` describes the *library* — every upload gets a description, at a cost per asset.
--   * `natural_language_search` describes *asking* — a call per question, from whoever is searching.
--
-- A tenant may reasonably want either without the other. Worse, conflating them means adding a key so the
-- library can be described silently turns every reader's search box into a paid endpoint. So: a second column,
-- also defaulting to false, and the spend cap (G20) is what bounds it once it is on.
--
-- Why a reader may spend at all: a search is what a reader does, and gating this behind Manage would put the
-- feature exactly where it is not needed. The cap is the control, which is what a cap is for.
ALTER TABLE enrichment_settings
    ADD COLUMN natural_language_search boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN enrichment_settings.natural_language_search IS
    'Whether a question typed into the search box may be turned into a query by a hosted model. Off by '
    'default: it is a paid call per question, made by whoever is searching, and bounded by the AI spend cap.';
