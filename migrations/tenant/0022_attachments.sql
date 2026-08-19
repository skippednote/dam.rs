-- Attached documents: the paperwork that goes with an asset (Q.9).
--
-- A model release, a licence, a contract. Files *about* an asset rather than assets in their own right — nobody
-- searches for a release form, rates one, or wants a thumbnail of one.
--
-- ## Why a column on `assets` rather than a table of its own
--
-- The alternative was an `asset_attachments` table with its own storage object, which means either a second ingest
-- path — a second place to sniff types, probe headers, place objects and record placements, diverging from the
-- first at whichever step somebody forgets — or a way to promote an already-ingested object into it, which is the
-- same thing with extra steps.
--
-- So an attachment *is* an ingested row, marked as belonging to another. It gets the ordinary upload path for free,
-- and the cost is one rule: a row with `attached_to` set is not part of the library.
--
-- ## And that rule shares a clause with versions
--
-- `assets.is_current` had the identical requirement — superseded versions are not part of the library either — and
-- was being filtered nowhere until Q.8. Rather than add a second clause that can be forgotten in a different set
-- of places, both conditions live in one fragment (`dam_db::versions::LIBRARY_ROWS`) applied at the four places
-- that describe the library. One rule to miss instead of two.
ALTER TABLE assets
    ADD COLUMN attached_to uuid REFERENCES assets (id) ON DELETE CASCADE,
    -- What kind of paperwork. Constrained because a UI groups by it and an open string would give every tenant
    -- their own spelling of "release".
    ADD COLUMN attachment_kind text
        CHECK (attachment_kind IN ('release', 'licence', 'contract', 'permit', 'other')),
    -- Both or neither: a row claiming to be a release while attached to nothing, or attached to something without
    -- saying what it is, is a row no screen can render honestly.
    ADD CONSTRAINT assets_attachment_complete CHECK (
        (attached_to IS NULL AND attachment_kind IS NULL)
        OR (attached_to IS NOT NULL AND attachment_kind IS NOT NULL)
    ),
    -- Paperwork about paperwork is not a thing anybody asked for, and allowing it would make the exclusion rule
    -- recursive: "not in the library" would have to walk a chain rather than check a column.
    ADD CONSTRAINT assets_attachment_not_self CHECK (attached_to IS NULL OR attached_to <> id);

-- The dominant read: everything attached to one asset.
CREATE INDEX assets_attached_idx ON assets (attached_to) WHERE attached_to IS NOT NULL;

COMMENT ON COLUMN assets.attached_to IS
    'When set, this row is paperwork for another asset and is NOT part of the library: it must be excluded from '
    'browse, search, facet counts and the dashboard total, which is what dam_db::versions::LIBRARY_ROWS does. It '
    'is still an ordinary asset row in every other respect, which is how it got the normal ingest path.';
