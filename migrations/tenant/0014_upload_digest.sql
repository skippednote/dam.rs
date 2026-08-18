-- The digest of a promoted upload, so finalisation is resumable at its most dangerous point.
--
-- Finalisation promotes the staged object to its content-addressed key and *then* records the asset. Those
-- cannot be one transaction — one is an object store and the other is Postgres — so a failure between them
-- leaves the bytes promoted, the staging object gone, and no asset row. The retry then failed at the first
-- step with "object not found", permanently, on an upload whose bytes were safely stored the whole time.
--
-- Found by running the real pipeline: the first attempt failed on a missing storage pool *after* promoting,
-- and the retry could no longer see staging.
--
-- Recording the digest as soon as the promotion succeeds closes it. A re-run reads this column, skips the
-- promotion, and records the asset against bytes that are already there.
ALTER TABLE upload_sessions
    ADD COLUMN content_hash text
        CHECK (content_hash IS NULL OR content_hash ~ '^[0-9a-f]{64}$');

COMMENT ON COLUMN upload_sessions.content_hash IS
    'BLAKE3 of the promoted object, set when the promotion succeeds and before the asset row exists. Its '
    'presence means the bytes are at their content-addressed key, whatever state the rest of finalisation '
    'reached.';
