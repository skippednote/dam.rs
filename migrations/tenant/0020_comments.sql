-- Comments on assets: public and private, routed to people, carrying a status (Q.6).
--
-- ## Visibility is a *second* filter, never the first
--
-- Every read of these composes with the caller's asset predicate. A comment on an asset you cannot see must not
-- be reachable no matter who it was addressed to, so "can I see this comment" is only ever asked about comments
-- on assets the predicate already admitted. The reverse order — find the comments addressed to me, then check the
-- assets — would disclose the existence of assets through the comments attached to them.
--
-- ## Private means the author and the people named, and nobody else
--
-- Deliberately not "and administrators". Whether a tenant admin may read a private comment is a question about
-- what the product promises its users, and it is recorded in NEEDS-REVIEW.md rather than decided here. The strict
-- rule is the one that can be relaxed later: adding a reader is additive, while telling somebody after the fact
-- that their private note was readable is not.
--
-- ## Recipients are routing, not permission
--
-- A public comment may name recipients too — "this is for you to look at" — and naming somebody does not widen
-- what they can see. On a *private* comment the same list also happens to be the visibility set, which is why the
-- two ideas share a table: they are one list used for one purpose that has a second consequence in one case.


-- ─── comments ───────────────────────────────────────────────────────────────
CREATE TABLE asset_comments (
    id              uuid PRIMARY KEY,
    asset_id        uuid        NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    author_id       uuid        NOT NULL,      -- dam_global.identities.id, no FK (see 0001)

    -- Bounded in the column, not only in Rust: a comment arrives from a text box and the bound is what keeps a
    -- paste of a log file out of the row. One character minimum, because an empty comment is not a comment.
    body            text        NOT NULL CHECK (char_length(body) BETWEEN 1 AND 10000),

    visibility      text        NOT NULL CHECK (visibility IN ('public', 'private')),

    -- The status a comment carries. `approved` and `changes_requested` are a reviewer's verdict on what the
    -- comment asked for; `resolved` is "this thread is dealt with".
    --
    -- Nothing enforces these yet, and that is deliberate rather than unfinished: a status that gated publishing
    -- would be a rights decision, and this is a collaboration artefact. It records what somebody decided.
    status          text        NOT NULL DEFAULT 'open'
                                CHECK (status IN ('open', 'resolved', 'approved', 'changes_requested')),

    -- One level of threading. A reply to a reply is refused in Rust, because arbitrary depth turns every read
    -- into a recursive query and every screen into an indentation problem, and nobody asked for it.
    parent_id       uuid        REFERENCES asset_comments (id) ON DELETE CASCADE,

    -- Who last moved the status, and when. Kept because "approved" with no name attached is an assertion nobody
    -- owns, and an approval nobody owns is worth nothing in an audit.
    status_by       uuid,                      -- dam_global.identities.id
    status_at       timestamptz,

    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    -- Set when the body was changed after posting, so a screen can say "edited" rather than silently showing
    -- different words than the person who replied to it saw.
    edited_at       timestamptz
);

-- The dominant read: every comment on one asset, oldest first within a thread.
CREATE INDEX asset_comments_asset_idx ON asset_comments (asset_id, created_at);

-- Replies to one comment.
CREATE INDEX asset_comments_thread_idx ON asset_comments (parent_id) WHERE parent_id IS NOT NULL;

-- "What have I written" and, more usefully, the author half of the private-visibility check.
CREATE INDEX asset_comments_author_idx ON asset_comments (author_id);

-- The open-threads queue, which is what makes a status worth having.
CREATE INDEX asset_comments_status_idx ON asset_comments (status, created_at)
    WHERE status IN ('open', 'changes_requested');


-- ─── routing ────────────────────────────────────────────────────────────────
-- Who a comment was addressed to. On a private comment this is also who may read it.
CREATE TABLE asset_comment_recipients (
    comment_id      uuid NOT NULL REFERENCES asset_comments (id) ON DELETE CASCADE,
    identity_id     uuid NOT NULL,             -- dam_global.identities.id, no FK
    added_at        timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (comment_id, identity_id)
);

-- "Which comments name me" — the recipient half of the visibility check, and the read a notification sender
-- will make. Identity first, because that is the direction both of those ask in.
CREATE INDEX asset_comment_recipients_identity_idx
    ON asset_comment_recipients (identity_id, comment_id);
