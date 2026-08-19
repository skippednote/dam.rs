-- Orders: a request for assets, and somebody else's authorisation to hand them over (Q.13).
--
-- ## Why this needs no new access-control concept
--
-- The obvious design delegates: approving an order grants the requester a download right they did not have, on
-- those assets, until an expiry. That is a fourth kind of grant — neither a role nor a share — and ARCHITECTURE
-- does not settle it, so it is not what this is.
--
-- Instead an order is a *workflow around a share link*. The requester asks; an approver decides; approval leads
-- to a share (3.4's machinery: a token, an optional passcode, an expiry, a download cap, revocation, and rights
-- re-evaluated at every delivery). Who may take bytes is answered exactly where it was already answered, and the
-- order adds the thing that was missing: a recorded request, a named approver, and a reason.
--
-- The alternative reading is written up in NEEDS-REVIEW.md rather than guessed at.
--
-- ## What an order is for
--
-- Somebody who may *see* assets but not take them — an agency contact, a regional team, anybody a role
-- deliberately restricts — needs a way to ask. Today they email. An order makes the ask, the reason, the decision
-- and the delivery one auditable object.
--
-- It also carries the two answers the rest of the system now wants: the intended use (Q.12), so the pickup's
-- downloads land in `rights_usage` as a declared use rather than a default; and the format (Q.11), so an approver
-- is agreeing to hand over a 2048px JPEG rather than a 40 MB master.

-- A human-quotable reference, per tenant. People talk about orders on the phone, and a uuid is not something
-- anybody reads aloud. The sequence lives in the tenant schema, so two tenants both have an ORD-000001.
CREATE SEQUENCE orders_reference_seq;

CREATE TABLE orders (
    id                  uuid PRIMARY KEY,
    reference           text NOT NULL UNIQUE
                            DEFAULT ('ORD-' || lpad(nextval('orders_reference_seq')::text, 6, '0')),

    -- No foreign key: identities live in the control plane (D2), the same reason `asset_comments.author_id`
    -- has none. An order outlives the person who asked for it, which is the point of an audit trail.
    requested_by        uuid NOT NULL,
    -- The sentence an approver reads. Required, because "why do you want these" is the entire question an
    -- approver is answering, and an order with no reason forces them to guess or to go and ask.
    purpose             text NOT NULL CHECK (length(btrim(purpose)) BETWEEN 1 AND 2000),

    -- The intended use (Q.12), carried into the ledger when the pickup is collected. Nullable because a
    -- requester may not know, and a forced answer is a worse record than an absent one.
    channel             text,
    territory           text,
    -- Which format (Q.11). Null means the original.
    conversion_key      text,
    -- Whether the pickup includes a metadata export of the assets in it.
    include_metadata    boolean NOT NULL DEFAULT false,

    -- Who the delivery is addressed to. Plural because an order is usually for a team, and telling the requester
    -- to forward the link defeats both the expiry and the download cap.
    recipients          text[] NOT NULL DEFAULT '{}',

    state               text NOT NULL DEFAULT 'submitted'
                            CHECK (state IN ('submitted', 'approved', 'rejected',
                                             'ready', 'collected', 'cancelled')),
    -- `expired` is deliberately *not* a state. An expiry is a timestamp passing, not an event anybody performs,
    -- and a stored state would need a sweeper to keep it true — which is a second source of truth that is wrong
    -- between sweeps. Reads derive it from `expires_at`.

    decided_by          uuid,
    decided_at          timestamptz,
    -- Why it was refused, or a condition on an approval. Optional: "no" with no explanation is bad manners but
    -- it is not a data error, and forcing a sentence produces "n/a".
    decision_note       text,

    -- When the pickup stops working. Set at approval, from the decision rather than from the request: an order
    -- approved three weeks after it was asked for should give its recipients the full window.
    expires_at          timestamptz,
    -- The share the pickup goes through, once fulfilment has made one. Null until then, which is exactly the
    -- difference between `approved` and `ready`.
    share_link_id       uuid REFERENCES share_links (id) ON DELETE SET NULL,

    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),

    -- A decision has a decider and a moment, or it has neither. A row saying "rejected" with nobody attached is
    -- an audit trail that has lost the only thing it was keeping.
    CONSTRAINT orders_decision_is_complete CHECK (
        (state IN ('submitted', 'cancelled') AND decided_by IS NULL AND decided_at IS NULL)
        OR (state NOT IN ('submitted', 'cancelled') AND decided_by IS NOT NULL AND decided_at IS NOT NULL)
    ),
    -- A pickup that is ready has something to pick up from, and one that is not, does not. This is what stops
    -- `ready` from being set by anything other than fulfilment.
    CONSTRAINT orders_ready_has_a_share CHECK (
        (state IN ('ready', 'collected')) = (share_link_id IS NOT NULL)
    )
);

-- The two dominant reads: "my orders" and "orders waiting for me".
CREATE INDEX orders_requester_idx ON orders (requested_by, created_at DESC);
CREATE INDEX orders_queue_idx ON orders (created_at) WHERE state = 'submitted';

-- The assets asked for, snapshotted at submission.
--
-- Snapshotted rather than a saved search, because an order is an agreement about a *specific* set: an approver
-- who agreed to forty photographs must not find they agreed to four hundred because somebody widened a query.
CREATE TABLE order_items (
    order_id            uuid NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    -- Cascade on delete: an order for an asset that has since been deleted should shrink rather than dangle. The
    -- order itself survives — what it was for is still a fact — and a pickup of nothing is refused at fulfilment.
    asset_id            uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    -- The filename as asked for, so an order still reads sensibly after a rename or a deletion.
    filename            text NOT NULL,
    PRIMARY KEY (order_id, asset_id)
);

CREATE INDEX order_items_asset_idx ON order_items (asset_id);

COMMENT ON TABLE orders IS
    'A request for assets and somebody else''s authorisation to hand them over. Fulfilment creates a share '
    'link, so no new kind of grant exists: who may take bytes is still answered by the share machinery and the '
    'rights evaluation at delivery.';
COMMENT ON COLUMN orders.state IS
    'submitted -> approved|rejected; approved -> ready (fulfilment made the share) -> collected. `expired` is '
    'not a state: it is derived from expires_at, because a stored one needs a sweeper to stay true.';
