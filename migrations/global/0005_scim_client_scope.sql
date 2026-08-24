-- ─── SCIM external ids belong to the client that issued them (G10·2b) ───────
-- `0002_enterprise.sql` put `scim_external_id` on `identities` with a unique
-- index across the whole table:
--
--   CREATE UNIQUE INDEX identities_scim_idx ON identities (scim_external_id)
--       WHERE scim_external_id IS NOT NULL;
--
-- `identities` is global — one row per person across the fleet — so that index
-- is unique across every customer's identity provider at once. Two of them
-- number their users independently, and Okta's default `externalId` is an
-- opaque per-org id, so the second tenant to provision a user whose id happens
-- to collide gets a constraint violation. The failure lands on the customer who
-- did nothing wrong, in a sync they do not control, and the message names a row
-- they cannot see.
--
-- So the id is scoped to the client that supplied it. The client already
-- carries the tenant, which makes this per-tenant without adding a tenant
-- column to a table that deliberately has none: a person is one identity across
-- the deployment, and only their *provisioning* is per-customer.

ALTER TABLE identities
    ADD COLUMN scim_client_id uuid REFERENCES scim_clients (id) ON DELETE SET NULL;

-- ON DELETE SET NULL rather than CASCADE. Revoking a SCIM client must not
-- delete the people it provisioned — they still have memberships, they may
-- still work here, and the customer may be moving between providers. What is
-- lost is the link, which is what "no longer provisioned by anybody" means.

DROP INDEX identities_scim_idx;

CREATE UNIQUE INDEX identities_scim_idx
    ON identities (scim_client_id, scim_external_id)
    WHERE scim_external_id IS NOT NULL;

-- Still partial on `scim_external_id`, so the many identities that no provider
-- manages do not collide on a shared NULL. A row with an external id and no
-- client cannot exist in practice — the writer sets both — and the index would
-- treat several such rows as distinct anyway, which is the honest behaviour for
-- a state nothing produces.

-- Lets the deprovisioning sweep find a client's people without a scan.
CREATE INDEX identities_scim_client_idx ON identities (scim_client_id)
    WHERE scim_client_id IS NOT NULL;
