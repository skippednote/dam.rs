-- Site branding: the tenant's own name, logo and accent (Q.20d).
--
-- Two things were true before this. The application called itself "damrs" in the nav of every tenant's
-- library, which is a vendor's name where a customer's should be. And every portal carried its own accent with
-- a hard-coded default (`#2563eb`), so a tenant with six press kits set the same colour six times and a
-- seventh portal silently reverted to ours.
--
-- ## A singleton in the tenant schema, not `tenants.settings`
--
-- `dam_global.tenants.settings jsonb` exists and nothing has ever read it, and it was tempting. Two reasons
-- against. Branding is tenant *data* — a logo is an asset in the tenant's own schema, and a foreign key across
-- schemas is exactly what 0002 forbids — so half of it could not live there anyway. And a jsonb blob has no
-- CHECK constraint, which for a colour that gets interpolated into CSS is the difference between a validated
-- value and an injection point.
--
-- Same shape as `enrichment_settings`: one row, locked by a boolean primary key, created at migration time so
-- every reader can assume it exists and a settings screen has something to render.
--
-- ## The logo is an asset, following the portal precedent
--
-- 0030 said it best: a logo is an asset, it is already governed, and a second upload path for it would be a
-- second thing to back up and a second place for an unlicensed image to appear. `ON DELETE SET NULL` rather
-- than cascade, because deleting the logo asset should cost a tenant their logo, not their branding row.

CREATE TABLE site_branding (
    -- The singleton lock. `true` is the only permitted value, so this table holds one row.
    id                  boolean PRIMARY KEY DEFAULT true CHECK (id),

    -- What the library calls itself. Empty means "use the tenant's display name", which is the sensible
    -- fallback and the reason this is not NOT NULL with a placeholder: a tenant that has never opened this
    -- screen should see their own name, not an empty header.
    site_name           text NOT NULL DEFAULT ''
                            CHECK (length(site_name) <= 64),

    logo_asset_id       uuid REFERENCES assets (id) ON DELETE SET NULL,

    -- The same shape and the same default as `portals.accent`, so a portal created before a tenant sets this
    -- and one created after do not look different for no reason. Lowercase six-digit hex only: this value is
    -- interpolated into a stylesheet, and the CHECK is what makes that safe rather than a sanitiser somebody
    -- has to remember to call.
    accent              text NOT NULL DEFAULT '#2563eb'
                            CHECK (accent ~ '^#[0-9a-f]{6}$'),

    -- Shown in a portal's footer, where an external recipient has no account and nobody to ask. Optional,
    -- because a tenant that does not want to publish an address should not have to.
    support_email       text
                            CHECK (support_email IS NULL OR support_email ~ '^[^@[:space:]]+@[^@[:space:]]+$'),

    updated_at          timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE site_branding IS
    'One row. What this tenant''s library calls itself and what it looks like, and the default a new portal '
    'inherits — so a tenant sets their colour once rather than once per press kit.';

COMMENT ON COLUMN site_branding.site_name IS
    'Empty means fall back to the tenant''s display name. A tenant that has never opened the branding screen '
    'should see their own name rather than a placeholder or ours.';

COMMENT ON COLUMN site_branding.accent IS
    'Interpolated into a stylesheet, so the CHECK is the sanitiser. Same default as portals.accent, so a '
    'portal created before this is set and one created after do not differ for no reason.';

-- The row exists from the start, so every reader can assume it and a screen has something to read.
INSERT INTO site_branding (id) VALUES (true) ON CONFLICT (id) DO NOTHING;
