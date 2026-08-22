-- Sub-unit spend, so a cap made of fractions is reachable (G20, M5a·4).
--
-- `tenant_spend.used_value` is a bigint of whole units, which is right for every quota 0002 anticipated: bytes,
-- assets, seats, requests. AI spend is the one that is not. A single enrichment call costs a fraction of a cent
-- — about 2.25¢ on Opus 5 at §8.3's per-asset shape, about 0.45¢ on a small model — so charging in whole cents
-- has to round, and both directions are wrong:
--
--   * down, and a million calls cost nothing, and `ai_spend_cents_month` is decoration;
--   * up, and a 0.45¢ call is billed at 1¢, overstating a cheap model by more than twofold.
--
-- So a charge arrives in micro-units — millionths of the quota's own unit — and the part below one whole unit
-- stays here until the next charge carries it over. `used_value` keeps precisely the meaning it had, which is
-- what lets one column serve every quota key.
--
-- Why not a numeric column: enforcement reads this on the path of every enrichment job, and the point of
-- `tenant_spend` (see 0002) is one indexed lookup of an integer. Integers also make the increment expressible as
-- a single statement, so two workers charging the same tenant cannot lose one between a read and a write.
ALTER TABLE tenant_spend
    ADD COLUMN spend_remainder_micro bigint NOT NULL DEFAULT 0
        CHECK (spend_remainder_micro >= 0 AND spend_remainder_micro < 1000000);

COMMENT ON COLUMN tenant_spend.spend_remainder_micro IS
    'Millionths of one unit of quota_key charged but not yet whole. Carried into the next charge so that a '
    'stream of sub-unit costs accumulates rather than rounding away. Always less than 1000000.';
