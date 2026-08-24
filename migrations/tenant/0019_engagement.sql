-- Ratings, favourites and watches (Q.5).
--
-- Three tables rather than one `asset_engagement` with nullable columns. They look alike — every one is
-- (asset, person) — but they answer different questions and are read on different paths: a rating is a *value*
-- that aggregates across people, a favourite is a private list belonging to one person, and a watch is a standing
-- request to be told about changes. Folding them together would mean a row that is a favourite carrying a null
-- `stars`, and every query filtering on which columns happen to be set.
--
-- No FK to `dam_global.identities`, per the note in 0001: a cross-schema FK from every tenant makes identity
-- deletion O(tenants). Rows for a departed identity are cleaned up by the same sweep that handles the rest.
--
-- ## Clearing is deleting
--
-- There is no "0 stars" and no `favourite = false`. Un-rating removes the row, which keeps "has no opinion" and
-- "thinks it is bad" from sharing a representation — an average over a table where 0 means *absent* is wrong in a
-- way nobody notices until the numbers are on a screen.


-- ─── ratings ────────────────────────────────────────────────────────────────
-- One rating per person per asset, so the aggregate is an average of opinions rather than of clicks.
--
-- `smallint` with a CHECK rather than an enum: the range is arithmetic (it is averaged, filtered as `>= 4`, and
-- the comparator writes it into XMP as a number), and an enum would need casting at every one of those.
CREATE TABLE asset_ratings (
    asset_id    uuid        NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    identity_id uuid        NOT NULL,       -- dam_global.identities.id, no FK
    stars       smallint    NOT NULL CHECK (stars BETWEEN 1 AND 5),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (asset_id, identity_id)
);

-- The aggregate read: every rating for one asset, or for a page of them.
CREATE INDEX asset_ratings_asset_idx ON asset_ratings (asset_id, stars);

-- "What have I rated" — the other direction, and the one a person's own screen asks for.
CREATE INDEX asset_ratings_identity_idx ON asset_ratings (identity_id, asset_id);


-- ─── favourites ─────────────────────────────────────────────────────────────
-- Identity first in the key, deliberately: the dominant read is "my favourites, newest first", and that is an
-- index-only scan under this order. The asset-first order would make every such read a full scan of the table.
CREATE TABLE asset_favourites (
    identity_id uuid        NOT NULL,       -- dam_global.identities.id, no FK
    asset_id    uuid        NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (identity_id, asset_id)
);

-- Newest first within one person's list, without sorting it.
CREATE INDEX asset_favourites_recent_idx ON asset_favourites (identity_id, created_at DESC);

-- And the reverse: how many people favourited this asset. Separate from the primary key because the key's
-- leading column is the identity, which cannot serve a lookup by asset.
CREATE INDEX asset_favourites_asset_idx ON asset_favourites (asset_id);


-- ─── watches ────────────────────────────────────────────────────────────────
-- A standing request to be told when an asset changes. The notification side does not exist yet (M6), so this
-- records intent only — and the intent is worth recording now because it is what makes a notification system
-- worth building rather than something to retrofit onto an empty table.
--
-- Access is *not* frozen here. A watch created while somebody could see an asset must not deliver anything after
-- they lose access, so whatever eventually sends notifications re-checks the predicate at send time. Storing a
-- copy of the grant would be storing a stale answer to the only question that matters.
CREATE TABLE asset_watches (
    identity_id uuid        NOT NULL,       -- dam_global.identities.id, no FK
    asset_id    uuid        NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (identity_id, asset_id)
);

-- Who is watching this asset — the read the notification sender will make, once there is one.
CREATE INDEX asset_watches_asset_idx ON asset_watches (asset_id);
