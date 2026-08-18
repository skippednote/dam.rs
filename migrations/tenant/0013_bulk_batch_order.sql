-- Batch selection for bulk operations is by item state, in asset id order.
--
-- 0008 gave `bulk_operation_items` a partial index on `(operation_id) WHERE state = 'pending'`, which bounds the
-- scan to the items still outstanding but leaves the `ORDER BY asset_id` that batching needs to a sort. Adding the
-- id to the index makes the order come from the index itself, so claiming a batch out of a 40,000-item operation
-- reads only the rows it returns.
--
-- This replaces the `resume_after` cursor as the mechanism for resumption. The cursor was a high-water mark of the
-- greatest asset id recorded, and a worker that fans a batch out concurrently records in completion order rather
-- than id order — recording the highest id first stepped the cursor past every lower pending item, which then
-- could never be served again. `done + failed = target` would never hold and the operation could not finish.
-- Item state cannot skip a row it has not seen an outcome for. `resume_after` is still written, as the progress
-- marker an operator reads, but it no longer decides what is served.

DROP INDEX bulk_operation_items_pending_idx;
CREATE INDEX bulk_operation_items_pending_idx
    ON bulk_operation_items (operation_id, asset_id)
    WHERE state = 'pending';
