-- Asset conversions: the named download formats a tenant offers (Q.11).
--
-- `derivatives.role` has carried `'rendition'` and `derivatives.profile` has been documented as "named
-- conversion format" since 0001, and nothing has ever written one. `dam_media::profiles` says why: the built-in
-- set lives in code because "a tenant-defined profile set is a real requirement that needs a table of its own,
-- with the usual questions about who may edit one and what happens to derivatives already rendered under the old
-- definition". This is that table, and those questions are answered below.
--
-- ## What a conversion is for
--
-- A person downloading an asset should not be handed a raw 40 MB TIFF and left to resize it. They pick a named
-- format — "Web JPEG, 2048px" — with a description written for somebody choosing rather than for whoever
-- configured it. That is the whole feature: names, descriptions, and a recipe behind each one.
--
-- ## There is no revision column, deliberately
--
-- The obvious design carries a `revision` that an editor bumps, because a redefinition must not keep serving the
-- bytes rendered under the old definition. It is unnecessary here: `derivatives.op_hash` already covers every
-- field of the recipe, so changing any of them *is* a different cache key and the next request renders fresh. A
-- revision column would be a second mechanism for something the first one already guarantees, and the second
-- mechanism is the one that gets forgotten.
--
-- What `op_hash` cannot see is a change to the *renderer* — a different resampling filter would leave every field
-- identical. That is global rather than per-row, so it lives in one constant
-- (`dam_media::profiles::RENDERER_REVISION`) folded into every hash, built-in and tenant alike.
--
-- ## Colour management is not editable, because it is not implemented
--
-- `op_hash` takes a colour profile and a rendering intent (§18.1), and `derive::render` does not apply either —
-- they are hash inputs only. Exposing them as fields a tenant can set would make the cache key change while the
-- output did not, which is a way to make the same bytes appear under two names and call it a colour conversion.
-- So they are fixed in code at `srgb`/`perceptual` for every tenant conversion until the renderer honours them.
CREATE TABLE conversions (
    id              uuid PRIMARY KEY,
    -- Stable, lowercase, and what a delivery token carries: a conversion is named in URLs, so renaming the
    -- label must not change what an already-issued link resolves to.
    key             text NOT NULL UNIQUE
                        CHECK (key ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
    -- `original` is not a conversion — it is the untransformed bytes, served from the content-addressed key
    -- (`dam_media::profiles::ORIGINAL`). A row claiming that name would shadow it at the one place that
    -- resolves a transform, and the failure would look like a caching bug.
    CONSTRAINT conversions_key_not_original CHECK (key <> 'original'),

    label           text NOT NULL CHECK (length(btrim(label)) BETWEEN 1 AND 120),
    -- Required, not optional. The description is the feature: a list of format names with no explanation of
    -- which to pick is the thing this table exists to replace.
    description     text NOT NULL CHECK (length(btrim(description)) BETWEEN 1 AND 500),

    -- Which assets it applies to, so a document is not offered an image recipe.
    --
    -- Constrained to `image` alone rather than to the full vocabulary: `derive::render` is vips, and a video
    -- conversion needs a parameterised ffmpeg recipe that does not exist — `video::transcode_proxy` is one fixed
    -- proxy, not a format somebody chooses. A row for a class nothing can render would be a promise the download
    -- dialog would offer and the worker would fail. Widening this CHECK is what adding video conversions means,
    -- and it is one place.
    media_class     text NOT NULL CHECK (media_class IN ('image')),

    -- The recipe. These columns are exactly `dam_media::derive::Rendition`, because anything else would need a
    -- translation layer that could disagree with the renderer.
    max_width       int NOT NULL CHECK (max_width BETWEEN 16 AND 20000),
    max_height      int NOT NULL CHECK (max_height BETWEEN 16 AND 20000),
    format          text NOT NULL CHECK (format IN ('jpeg', 'png', 'webp', 'avif')),
    -- Ignored by PNG, and stored anyway: the renderer decides that, and a column that was NULL for one format
    -- would make every read handle two shapes.
    quality         int NOT NULL CHECK (quality BETWEEN 1 AND 100),
    fit             text NOT NULL CHECK (fit IN ('contain', 'cover')),
    -- What transparency is flattened onto when the target cannot carry it. Six hex digits, lowercase.
    background      text NOT NULL DEFAULT 'ffffff' CHECK (background ~ '^[0-9a-f]{6}$'),

    -- The fine-grained permission a role must carry to use this format, or NULL for "anybody who may download
    -- the asset at all". §policy: `Action` is coarse on purpose and "the fine-grained permission strings live in
    -- roles.permissions" — this is one of them. It never *widens* access: the asset's own download gate is
    -- checked first and this can only narrow what is offered.
    required_permission text CHECK (required_permission ~ '^[a-z][a-z0-9:_-]{2,62}$'),

    -- Withdrawn rather than deleted. A delivery token carries the conversion's key, and a link in somebody's
    -- email should not stop resolving because an administrator tidied a list — so a withdrawn conversion
    -- disappears from what is offered while what has already been rendered stays resolvable.
    is_active       boolean NOT NULL DEFAULT true,
    -- The order a dialog lists them in. A person picking a format reads a considered order, not an alphabet.
    sort_order      int NOT NULL DEFAULT 0,

    created_by      uuid,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- The dominant read: what to offer for an asset of this class, in order.
CREATE INDEX conversions_offer_idx ON conversions (media_class, sort_order) WHERE is_active;

COMMENT ON TABLE conversions IS
    'Named download formats. The recipe columns are dam_media::derive::Rendition; the cache key is '
    'derivatives.op_hash over those columns, so a redefinition renders fresh without a revision column.';
COMMENT ON COLUMN conversions.required_permission IS
    'A fine-grained roles.permissions string. Narrows only: the asset''s own Download gate is checked first, '
    'and a conversion can never grant what that refused.';
