-- `archive` and `unarchive` join the bulk vocabulary.
--
-- The constraint already held `tier` and `restore` — the *storage* operations, cold and back — and had no verb
-- for the curation status at all, even though `assets.status` has accepted `'archived'` since 0001 and
-- `status:archived` has been a live search selector since Q.15. So the state was reachable by query and not by
-- any action: a library could be searched for archived assets and could never archive one.
--
-- Two different things, kept apart deliberately, and conflating them would be wrong in both directions:
--
--   * `status = 'archived'` is curation. Out of circulation, off the default grid, still instantly fetchable.
--   * `storage_class = 'GLACIER'` is cost. Cheap and slow, and says nothing about whether anybody wants it.
--
-- A library archives what it has finished with and tiers what nobody reads, and those are frequently
-- different sets: last season's campaign is archived and still opened weekly, while a master nobody has
-- touched in two years is live and cold. Keeping them separate is also what lets them compose — a lifecycle
-- policy scoped to archived assets is the first rule anybody writes.

ALTER TABLE bulk_operations DROP CONSTRAINT bulk_operations_kind_check;
ALTER TABLE bulk_operations ADD CONSTRAINT bulk_operations_kind_check CHECK (kind IN (
    'metadata_set', 'metadata_clear', 'tag_add', 'tag_remove',
    'group_add', 'group_remove', 'collection_add',
    'license_assign', 'delete', 'restore', 'download_zip',
    'reprocess', 're_enrich', 'tier', 'export',
    'publish', 'unpublish', 'archive', 'unarchive'));
