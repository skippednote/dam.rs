-- Fix a dangerous comment on an access-control column.
--
-- 0001 said, above `roles.asset_group_ids`:
--
--     -- Empty array = all groups. Explicit rather than null so the "no access"
--     -- and "all access" cannot be confused.
--
-- which contradicts itself and the column beneath it. `all_asset_groups` exists
-- precisely to express "all access", so an empty `asset_group_ids` must mean "no
-- groups" — otherwise the boolean is dead code and, far worse, a role created with
-- the column defaults (`'{}'`, `false`) would grant every group in the tenant.
-- That is the most dangerous default available and the exact opposite of what the
-- second half of the same sentence was reaching for.
--
-- The comment is corrected here rather than edited in place because sqlx checksums
-- migration files: rewriting 0001 would make every already-migrated database
-- refuse to migrate. `COMMENT ON COLUMN` is also the better home — it travels with
-- the schema, so `\d+ roles` shows it and nobody has to find the migration.
--
-- The behaviour was already correct in `dam_core::policy` and is asserted by
-- `a_role_with_the_column_defaults_grants_nothing`.

COMMENT ON COLUMN roles.asset_group_ids IS
    'Groups this role is scoped to. EMPTY MEANS NO GROUPS — use all_asset_groups '
    'for unrestricted scope. A role with the defaults ({}, false) therefore grants '
    'nothing, which is the correct direction for a default.';

COMMENT ON COLUMN roles.all_asset_groups IS
    'Unrestricted group scope. Bypasses group scoping and release windows, but NOT '
    'expiry, legal hold, or rights_state in (denied, unknown): those are legal facts '
    'about an asset rather than permissions anyone holds. See DECISIONS.md, ABAC 5.';

COMMENT ON COLUMN roles.permissions IS
    'Verbs, e.g. asset:read, asset:download, asset:manage. Empty means none. Roles '
    'combine as a UNION across every role a caller holds (DECISIONS.md, ABAC 1).';

COMMENT ON COLUMN roles.requires_eula IS
    'Gates download and derivative delivery only, never visibility: browsing is what '
    'tells someone the EULA is worth accepting (DECISIONS.md, ABAC 3).';
