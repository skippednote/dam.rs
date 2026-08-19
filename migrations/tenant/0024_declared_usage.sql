-- Intended use: whether a person answered, or a client defaulted (Q.12).
--
-- `rights_usage` has been an append-only consumption ledger since 0005, and its own comment names the three
-- things meant to fill it: "connector usage reports (0004), download events (0001 events), and manual entry for
-- offline channels like print runs". The download half was never written, which meant `max_downloads` was
-- decoration in exactly the way that comment warned `max_impressions` would be.
--
-- Writing it raises one question the table could not answer, and this column is that answer.
--
-- ## Why "somebody said so" needs to be distinguishable from "nobody asked"
--
-- The point of capturing intended use is that a person states what they are going to do with an asset, and the
-- statement is auditable. A row recording `channel = 'internal'` because a client sent nothing and the API
-- defaulted looks identical to a row recording `channel = 'internal'` because somebody chose it — and an audit
-- that cannot tell those apart is not an audit. It would let "we asked everybody" be claimed on the strength of
-- rows nobody ever saw.
--
-- So: true when the request named the channel and territory, false when the API supplied them. The API decides
-- that by whether the fields were present, not by a flag the client sets — a client that could assert
-- "declared" without a person having answered would defeat the distinction it exists for.
ALTER TABLE rights_usage
    ADD COLUMN declared boolean NOT NULL DEFAULT false,
    -- Only a download can carry a declaration. A connector report, a manual print-run entry and an import are
    -- all records of something that already happened elsewhere, with no person at a dialog — so `declared` on
    -- one of those would be a claim about an event nobody witnessed. Constrained rather than documented,
    -- because a column whose meaning depends on another column's value is one a future writer gets wrong.
    ADD CONSTRAINT rights_usage_declared_is_a_download CHECK (
        NOT declared OR source = 'download'
    );

COMMENT ON COLUMN rights_usage.declared IS
    'For source = ''download'': true when the request named the channel and territory, false when the API '
    'defaulted them. An audit that cannot tell a stated intention from a defaulted one is not an audit.';
