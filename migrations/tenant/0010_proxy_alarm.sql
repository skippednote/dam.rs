-- The §2 alarm, made triageable. See ARCHITECTURE §2 and dam_media::proxy.
--
-- `enrichment_runs.used_original` already exists and defaults false. The comment
-- on it in 0003 says it is "worth an alert, not just a column" — this is that,
-- plus the one thing an alert needs to be actionable: a reason.
--
-- Why it matters. Nothing breaks on the day a stage starts reading originals. The
-- bill arrives at the next model upgrade, as a restore storm across the whole
-- archive, and by then nobody remembers which stage changed. A boolean says the
-- design is broken; a boolean with a reason says where.

ALTER TABLE enrichment_runs
    ADD COLUMN original_read_reason text;

-- A flag with no reason gets muted, and a muted alarm is worse than none: it
-- reads as "we are watching this" while nobody is.
ALTER TABLE enrichment_runs
    ADD CONSTRAINT enrichment_original_read_is_explained CHECK (
        NOT used_original OR original_read_reason IS NOT NULL);

-- The alert's query, as an object rather than a snippet pasted into a dashboard.
-- A view can be granted, tested, and found by someone who does not already know
-- it exists — which is the difference between an invariant and a folk memory.
--
-- Some original reads are legitimate: C2PA verification at ingest attests to the
-- master's own bytes while it is still hot. Those are still listed, because
-- "legitimate" is a judgement about the reason and the view's job is to surface
-- every instance for that judgement to be made.
CREATE VIEW enrichment_original_reads AS
SELECT
    r.id,
    r.asset_id,
    r.pipeline,
    r.state,
    r.original_read_reason,
    r.started_at,
    a.filename,
    a.bytes AS asset_bytes
FROM enrichment_runs r
JOIN assets a ON a.id = r.asset_id
WHERE r.used_original;

-- Partial, because the whole point is that matching rows are rare: a full index
-- on a boolean that is false for every row in the table would be dead weight.
CREATE INDEX enrichment_runs_used_original_idx ON enrichment_runs (started_at DESC)
    WHERE used_original;
