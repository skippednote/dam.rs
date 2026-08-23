-- The webhook outbox orders by a sequence, not by a timestamp (Q.20c).
--
-- 0004 gave `webhook_deliveries` an ordering guarantee — "delivery is sequential per (subscription, asset)" —
-- and `created_at timestamptz DEFAULT now()` to order by. Those two are incompatible, and the test that found
-- it looks like a fixture bug until you check what `now()` means: it is the *transaction* timestamp, identical
-- for every statement in the transaction. So two events enqueued together for one asset get the same
-- `created_at`, the comparison ties, and the tie-break falls to `gen_random_uuid()` — random order, on the one
-- table whose entire purpose is order.
--
-- That is not a corner case. An outbox row is written in the same transaction as the change it describes, so
-- "publish this version and expire the old one" is exactly one transaction with two events for one asset — the
-- pair from 0004's own comment about republishing an expired asset.
--
-- `clock_timestamp()` would break the tie but not the dependency: it is wall-clock, so it moves backwards on
-- an NTP correction and is not comparable across a leap. A sequence is what "the order they happened" actually
-- means here.
--
-- What this does NOT promise, stated so nobody reads more into it: a sequence is allocated at INSERT, not at
-- COMMIT, so two *concurrent* transactions can commit out of sequence order. Ordering is therefore exact
-- within a transaction and best-effort across them. That is the right trade — events for one asset come from
-- one logical change, and a cross-transaction race over one asset has no correct answer to preserve.

ALTER TABLE webhook_deliveries ADD COLUMN seq bigserial NOT NULL;

-- Backfilled in `created_at` order so any rows already queued keep the order they were written in. There are
-- none in practice — nothing has ever written to this table — but a migration that silently reordered an
-- existing queue would be the wrong thing to leave behind.
WITH ordered AS (
    SELECT id, row_number() OVER (ORDER BY created_at, id) AS n FROM webhook_deliveries
)
UPDATE webhook_deliveries d SET seq = ordered.n FROM ordered WHERE d.id = ordered.id;

SELECT setval(
    pg_get_serial_sequence('webhook_deliveries', 'seq'),
    coalesce((SELECT max(seq) FROM webhook_deliveries), 1)
);

-- The per-asset ordering guard, re-cut on the column the dispatcher now compares. Replaced rather than added:
-- an index on `created_at` would still be scanned for a comparison nothing performs.
DROP INDEX webhook_deliveries_order_idx;
CREATE INDEX webhook_deliveries_order_idx
    ON webhook_deliveries (subscription_id, asset_id, seq)
    WHERE state IN ('pending', 'delivering', 'failed');

COMMENT ON COLUMN webhook_deliveries.seq IS
    'Monotonic enqueue order. The dispatcher holds back any delivery for a (subscription, asset) that has an '
    'earlier seq still unsent, which is what keeps an asset''s event stream in order. Ordered on rather than '
    'created_at because now() is the transaction timestamp: events enqueued together tie.';
