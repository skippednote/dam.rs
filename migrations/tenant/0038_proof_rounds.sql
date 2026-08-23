-- Proofing rounds: a named review over a set of assets, with a verdict per reviewer (M6b).
--
-- ## What was already here, and what was missing
--
-- `asset_comments.status` has carried `approved` and `changes_requested` since 0020, and that migration says
-- nothing enforces them — deliberately, because a status that gated publishing would be a rights decision
-- rather than a collaboration artefact. What was missing is the *round*: the thing that asks a named group of
-- people to look at a specific set of assets by a specific date. A comment is one person's opinion about one
-- asset; a round is an agreement about a batch.
--
-- ## The asset set is snapshotted, for 0025's reason word for word
--
-- "An approver who agreed to forty photographs must not find they agreed to four hundred because somebody
-- widened a query." That argument is about orders and applies here unchanged — arguably more strongly, since an
-- order is fulfilled once and an approval is cited later. So `proof_round_assets` is a list, never a saved
-- search.
--
-- ## The round's state is derived, not stored
--
-- A round is `changes_requested` if any reviewer said so, `approved` if every reviewer approved, and open
-- otherwise. Storing that alongside the verdicts would be two sources of truth for one fact, and the one that
-- drifts is always the copy. Only the two things that *cannot* be derived are columns: `cancelled_at`, which is
-- somebody's decision rather than a consequence, and `closed_at`, which is a moment rather than a state.
--
-- ## A second round is a new row
--
-- A round that came back with changes leads to round 2, pointing at round 1 — never to reopening round 1.
-- "Who approved what, and when" has to stay answerable, and mutating a closed round erases exactly that. It is
-- the same reason a version is a new asset row rather than an edit.
--
-- ## It gates nothing
--
-- Like the comment status it builds on. A round records that people agreed; whether an unapproved asset may be
-- published is a rights question, and answering it here would put a collaboration table in the delivery path.

CREATE TABLE proof_rounds (
    id                  uuid PRIMARY KEY,

    -- What this review is called, in the words of whoever asked for it. Bounded in the column because it comes
    -- from a text box.
    title               text NOT NULL CHECK (char_length(btrim(title)) BETWEEN 1 AND 200),
    -- What the reviewers are being asked. Optional: sometimes the title and the assets are the whole brief.
    brief               text NOT NULL DEFAULT '' CHECK (char_length(brief) <= 4000),

    -- Round 1, 2, 3 of a sequence. Denormalised from the `supersedes` chain so a screen can say "round 3"
    -- without walking it, and so two rounds of one review sort correctly without a recursive query.
    number              int NOT NULL DEFAULT 1 CHECK (number >= 1),
    -- The round this one follows. NULL for a first round.
    supersedes          uuid REFERENCES proof_rounds (id) ON DELETE SET NULL,

    -- When the reviewers were asked to be done. Advisory: nothing expires a round, because a review that
    -- vanished at midnight would lose the verdicts already given.
    due_at              timestamptz,

    requested_by        uuid,                    -- dam_global.identities.id, no FK (see 0002)
    created_at          timestamptz NOT NULL DEFAULT now(),

    -- Set when every reviewer has decided, or when one asked for changes. A moment, not a state — the state is
    -- derived from the verdicts.
    closed_at           timestamptz,
    -- Withdrawn by whoever asked for it. Not derivable from any verdict, so it is a column.
    cancelled_at        timestamptz,
    cancelled_by        uuid,

    -- A cancelled round is closed; a closed round need not be cancelled. Stated as a constraint so the two
    -- timestamps cannot disagree about whether the round is over.
    CONSTRAINT proof_rounds_cancelled_is_closed CHECK (
        cancelled_at IS NULL OR closed_at IS NOT NULL),
    -- A round cannot follow itself, which is the one cycle a single row can create.
    CONSTRAINT proof_rounds_not_self_superseding CHECK (supersedes IS DISTINCT FROM id)
);

CREATE INDEX proof_rounds_open_idx ON proof_rounds (created_at DESC) WHERE closed_at IS NULL;
CREATE INDEX proof_rounds_due_idx ON proof_rounds (due_at)
    WHERE due_at IS NOT NULL AND closed_at IS NULL;
CREATE INDEX proof_rounds_chain_idx ON proof_rounds (supersedes) WHERE supersedes IS NOT NULL;


-- The assets under review, snapshotted. See the note above on why this is a list.
CREATE TABLE proof_round_assets (
    round_id            uuid NOT NULL REFERENCES proof_rounds (id) ON DELETE CASCADE,
    -- Cascade, following 0025: a round over an asset since deleted should shrink rather than dangle. What the
    -- round was for is still a fact, and a round whose assets have all gone is visibly empty rather than broken.
    asset_id            uuid NOT NULL REFERENCES assets (id) ON DELETE CASCADE,
    -- The order they were put in, so a reviewer sees them as the requester arranged them.
    position            int NOT NULL DEFAULT 0,

    PRIMARY KEY (round_id, asset_id)
);

CREATE INDEX proof_round_assets_asset_idx ON proof_round_assets (asset_id);


-- Who was asked, and what they said.
--
-- A verdict is per (round, reviewer) rather than per asset. A round asks "have you reviewed this batch", and the
-- per-asset opinions are the comments and annotations — which already exist and already carry a status. Making
-- this per-asset would duplicate that at a hundred times the row count, and leave two places to look for one
-- person's view of one picture.
CREATE TABLE proof_round_reviewers (
    round_id            uuid NOT NULL REFERENCES proof_rounds (id) ON DELETE CASCADE,
    identity_id         uuid NOT NULL,           -- dam_global.identities.id, no FK

    -- `pending` until they decide. Not nullable: "asked and has not answered" is a state worth naming, and a
    -- null would make every query treat it as missing data instead.
    verdict             text NOT NULL DEFAULT 'pending'
                            CHECK (verdict IN ('pending', 'approved', 'changes_requested')),
    -- Why, in their words. The specifics belong in comments on the assets; this is the covering note.
    note                text NOT NULL DEFAULT '' CHECK (char_length(note) <= 4000),

    decided_at          timestamptz,
    added_at            timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (round_id, identity_id),

    -- A decision has a moment, and a pending reviewer has not decided. Both directions, because either alone
    -- would let the pair contradict itself.
    CONSTRAINT proof_round_reviewers_decided_together CHECK (
        (verdict = 'pending') = (decided_at IS NULL))
);

-- "What is waiting for me" — the read a reviewer's own dashboard makes, and the only one that starts from the
-- person rather than the round.
CREATE INDEX proof_round_reviewers_waiting_idx
    ON proof_round_reviewers (identity_id, round_id) WHERE verdict = 'pending';

COMMENT ON TABLE proof_rounds IS
    'A named review over a snapshotted set of assets. Records that people agreed; gates nothing — whether an '
    'unapproved asset may be published is a rights question, not a collaboration one.';

COMMENT ON COLUMN proof_rounds.closed_at IS
    'A moment, not a state. The state is derived: changes_requested if any reviewer said so, approved if all '
    'did, open otherwise — storing it beside the verdicts would be two sources of truth for one fact.';

COMMENT ON COLUMN proof_rounds.supersedes IS
    'The round this one follows. A review that came back with changes leads to a new round rather than a '
    'reopened one, because "who approved what, and when" has to stay answerable.';
