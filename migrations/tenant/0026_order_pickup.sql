-- An order's pickup is a share (Q.13d).
--
-- `share_links.kind` has been `('asset', 'collection', 'search')` since 0001, and the portal has only ever
-- rendered the single-asset case — it says so, in as many words: "this link shares something this portal cannot
-- show yet". An order is a *set*, so the pickup needs both a kind and that view.
--
-- ## Why a kind rather than a synthetic collection
--
-- The alternative is to build a collection from the order's items and share that. It works, and it leaves a
-- collection nobody asked for in the tenant's list — one per order, forever, named after a request. An order
-- already *is* a named list of assets with an owner, an expiry and a reason, so pointing at it directly is the
-- smaller change: `target_id` becomes the order's id, and the pickup has no shadow object behind it.
--
-- ## What this does not change
--
-- Everything about who may take bytes. An order share is an ordinary share link: it can be revoked, it expires,
-- it can carry a passcode and a download cap, and every delivery through it re-evaluates rights. That is the
-- whole reason fulfilment creates one rather than granting the requester something new — see 0025 and
-- NEEDS-REVIEW.md.
ALTER TABLE share_links
    DROP CONSTRAINT share_links_kind_check;

ALTER TABLE share_links
    ADD CONSTRAINT share_links_kind_check
        CHECK (kind IN ('asset', 'collection', 'search', 'order'));

COMMENT ON COLUMN share_links.kind IS
    'What the token shares. `asset` and `order` are the two the portal renders: one asset, or the set an order '
    'was placed for. `collection` and `search` are reserved and answered with a refusal that says so.';
