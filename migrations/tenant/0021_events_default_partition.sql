-- A default partition for `events`, because nothing was rolling them forward (Q.7).
--
-- 0001 creates `events` partitioned by month with a single seed partition for January 2026, and a comment saying
-- "damctl rolls these forward". No such command was ever written. Nothing has written an event yet, so the gap has
-- been invisible — but the first write outside January 2026 would fail with "no partition of relation events found
-- for row", and it would fail for every event from then on.
--
-- The fix that cannot lose a write is a DEFAULT partition: any row whose timestamp falls outside every declared
-- range lands there, and reads through the parent see it like any other.
--
-- ## Why not monthly partitions up front
--
-- Two dozen `CREATE TABLE ... PARTITION OF` statements would cover a couple of years and then have the same
-- problem again, one silent cliff further out. A default has no cliff.
--
-- The cost is real and worth stating: attaching a *new* monthly partition later requires Postgres to scan the
-- default for rows that would belong to it, holding an ACCESS EXCLUSIVE lock while it does. So monthly partitions
-- are a performance measure for a large tenant, taken deliberately by a maintenance job that drains the default
-- first — not a correctness requirement, which is what they were being relied on as.
CREATE TABLE events_default PARTITION OF events DEFAULT;

COMMENT ON TABLE events_default IS
    'Catch-all partition. Rows land here when no monthly partition covers their timestamp, which is the normal '
    'case until a maintenance job starts creating months. Never drop it: without a default, a write outside every '
    'declared range fails rather than being stored.';
