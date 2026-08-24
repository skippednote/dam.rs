-- ─── the SCIM link belongs to the membership, not the person (G10·2b) ───────
-- `0005_scim_client_scope.sql`, added minutes before this one, scoped
-- `scim_external_id` to the client that issued it. That fixed the collision
-- between two customers' providers using the same id. It did not fix the thing
-- underneath, which is that the *link itself* is per-tenant and was sitting on
-- a global table.
--
-- `identities` has one row per person across the whole deployment — that is
-- deliberate, and `members::add` relies on it so somebody working with two
-- customers of one deployment has one account. But `scim_client_id`,
-- `scim_external_id` and `scim_managed` are single-valued on that row, so:
--
--   * Two tenants' providers provisioning the same person overwrite each
--     other's link. The second write silently takes ownership, and the first
--     tenant's provider then fails its own `ours()` check — its sync breaks
--     because a different customer provisioned the same consultant.
--
--   * `scim_managed` made a person uneditable *everywhere*. Provisioned by
--     tenant A's provider, they could no longer have their roles changed in
--     tenant B, where no provider manages them at all and an administrator is
--     the only authority.
--
-- Both are the same mistake as the index 0005 fixed, one level up: per-tenant
-- state on a global row. So the three columns move to `tenant_members`, which
-- is keyed exactly right — one row per (tenant, person) — and the ownership
-- question becomes answerable per customer.
--
-- `status` and `deprovisioned_at` stay on `identities`, and that is not an
-- inconsistency: disabling an account disables it everywhere, which is what
-- `auth::authenticate` reads and what a deprovisioned person should experience.
-- Who *provisions* them is per-customer; whether they exist is not.

ALTER TABLE tenant_members
    ADD COLUMN scim_client_id uuid REFERENCES scim_clients (id) ON DELETE SET NULL,
    ADD COLUMN scim_external_id text,
    ADD COLUMN scim_managed boolean NOT NULL DEFAULT false;

-- Carries over whatever the old columns hold. In practice nothing: 0005 landed
-- in the same session as the first writer. Written anyway, because a migration
-- that assumes its predecessor was never used is a migration that loses data
-- the one time it was.
UPDATE tenant_members m
SET scim_client_id = i.scim_client_id,
    scim_external_id = i.scim_external_id,
    scim_managed = i.scim_managed
FROM identities i
WHERE i.id = m.identity_id
  AND (i.scim_external_id IS NOT NULL OR i.scim_managed);

DROP INDEX identities_scim_idx;
DROP INDEX identities_scim_client_idx;

ALTER TABLE identities
    DROP COLUMN scim_client_id,
    DROP COLUMN scim_external_id,
    DROP COLUMN scim_managed;

-- Unique per client, as 0005 established, and now per membership as well —
-- which is what makes two customers provisioning the same person work.
CREATE UNIQUE INDEX tenant_members_scim_idx
    ON tenant_members (scim_client_id, scim_external_id)
    WHERE scim_external_id IS NOT NULL;

-- Lets a deprovisioning sweep find one client's people without a scan.
CREATE INDEX tenant_members_scim_client_idx ON tenant_members (scim_client_id)
    WHERE scim_client_id IS NOT NULL;
