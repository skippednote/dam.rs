-- ─── publication: the per-asset act a public page can rest on (Q.14, NEEDS-REVIEW) ──
--
-- A portal is visible to people with no account, and its set can come from a
-- collection, a saved search or a media class. A collection is safe because somebody
-- with Manage put each asset in it, and that act *is* the publication decision. A live
-- query is not: a portal backed by `brand:acme` would publish every future asset that
-- happens to match, so nobody decides and a rule does.
--
-- This column is the decision, made once per asset by a person. A live-query portal
-- shows only assets that carry it, so the query *narrows* an explicitly published set
-- rather than defining one. The alternatives were a rights floor (publishes anything
-- with a broad licence, hides cleared assets whose evaluation is stale) and a warning
-- on the portal screen (makes an irreversible disclosure depend on somebody reading).
--
-- Nullable rather than a boolean, because "when" is the audit question a public
-- appearance raises. Who published it is in `bulk_operations` — publication is a bulk
-- kind, so the actor, the selection and the per-item outcomes are already recorded there.
ALTER TABLE assets ADD COLUMN published_at timestamptz;

-- Partial: the interesting set is the published one, and on a library where most assets
-- are internal that is a small fraction of the table.
CREATE INDEX assets_published_idx ON assets (published_at DESC)
    WHERE published_at IS NOT NULL AND deleted_at IS NULL;

-- `publish` and `unpublish` join the bulk vocabulary. Publication is a decision over a
-- selection — the same shape as a metadata edit — and routing it through the bulk
-- machinery is what gives it an actor, a target count and a per-item outcome without a
-- second audit mechanism.
ALTER TABLE bulk_operations DROP CONSTRAINT bulk_operations_kind_check;
ALTER TABLE bulk_operations ADD CONSTRAINT bulk_operations_kind_check CHECK (kind IN (
    'metadata_set', 'metadata_clear', 'tag_add', 'tag_remove',
    'group_add', 'group_remove', 'collection_add',
    'license_assign', 'delete', 'restore', 'download_zip',
    'reprocess', 're_enrich', 'tier', 'export',
    'publish', 'unpublish'));
