-- Per-tier retrieval prices, so a restore estimate can show the spread it exists to show.
--
-- `storage_pools` had one `cost_per_gb_retrieval`, and §6.5's whole argument for showing an estimate before
-- somebody confirms is that "Expedited against Bulk is roughly 10× on price and 100× on latency". One column
-- cannot express that: every tier quoted the same number, so the screen asking a user to choose between them
-- would have shown the same price three times.
--
-- Three ways to get this wrong, and why this is none of them:
--
--   * Deriving Expedited and Bulk from the Standard price by AWS's published ratios. The ratios are real, but
--     they are AWS's, and the number would be a hardcoded assumption inside a figure somebody approves a
--     spend against. On another provider it would be wrong and nothing would say so.
--   * Leaving one column and quoting it for all three. Hides the tradeoff the screen is for.
--   * Requiring all three. A deployment that has only ever recorded one price would start refusing to
--     estimate, which is a regression in exchange for nothing.
--
-- So: two new columns, nullable, and a null falls back to `cost_per_gb_retrieval` in the reader. A pool that
-- records nothing estimates zero — which is what the reader already treats as "this deployment does not
-- know", and is honest rather than invented.
--
-- Deliberately not seeded with AWS list prices. The dev stack is SeaweedFS, where retrieval is free, and
-- seeding AWS numbers there would make every local estimate a fiction. Prices are an operator's fact about
-- their own account.

ALTER TABLE dam_global.storage_pools
    ADD COLUMN cost_per_gb_retrieval_expedited numeric(12, 8),
    ADD COLUMN cost_per_gb_retrieval_bulk      numeric(12, 8);

COMMENT ON COLUMN dam_global.storage_pools.cost_per_gb_retrieval IS
    'Standard-tier retrieval price per GB. The fallback for the other two tiers when they are not recorded.';
COMMENT ON COLUMN dam_global.storage_pools.cost_per_gb_retrieval_expedited IS
    'Expedited retrieval price per GB. Null falls back to cost_per_gb_retrieval.';
COMMENT ON COLUMN dam_global.storage_pools.cost_per_gb_retrieval_bulk IS
    'Bulk retrieval price per GB. Null falls back to cost_per_gb_retrieval.';
