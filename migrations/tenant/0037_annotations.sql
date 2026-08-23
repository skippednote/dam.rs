-- Annotations: a comment pinned to a region of a picture, or a moment in a video (M6).
--
-- An extension of `asset_comments` rather than a table of its own. A join per comment read would be the cost
-- of separating them, and the read that matters is "every comment on this asset" — which a screen runs on every
-- detail panel open. Most comments carry no region, so these columns are mostly null; that is the cheaper
-- shape than a left join whose right side is usually absent.
--
-- ## Normalised coordinates, and this is the load-bearing decision
--
-- `0..1` fractions of the image, not pixels. An annotation has to land in the same place on the thumbnail, the
-- preview, the proxy and the original — four different pixel sizes for one asset, and more once a tenant
-- defines its own conversions. Pixels would pin a mark to whichever derivative happened to be on screen when
-- somebody drew it, and every other rendering would put it somewhere else. `real` rather than `double
-- precision` because four significant figures is a quarter of a pixel on a 4000-pixel edge.
--
-- ## All four or none
--
-- A CHECK rather than four independent nullable columns, because three-quarters of a rectangle is not a
-- smaller rectangle — it is a bug that renders as a mark in the wrong place, which is worse than no mark.
--
-- ## The timecode is separate from the region, and both are optional
--
-- A video annotation may be a moment with no region ("the music stops here"), a region with no moment (a
-- watermark present throughout), or both. So they are independent, and neither implies the other.

ALTER TABLE asset_comments
    -- The rectangle, as fractions of the image. Origin top-left, matching every image API and the CSS box.
    ADD COLUMN region_x real,
    ADD COLUMN region_y real,
    ADD COLUMN region_w real,
    ADD COLUMN region_h real,
    -- Where in a video or audio track this comment is about. Milliseconds, matching `assets.duration_ms`.
    ADD COLUMN at_ms bigint;

ALTER TABLE asset_comments
    -- All four or none. See the note above on three-quarters of a rectangle.
    ADD CONSTRAINT asset_comments_region_complete CHECK (
        num_nonnulls(region_x, region_y, region_w, region_h) IN (0, 4)),

    -- Inside the picture, and with a non-zero extent. A zero-width box is a click that missed, and a box
    -- running past the edge means the coordinates were computed against the wrong element — both of which
    -- render as a mark somewhere surprising rather than as an error anybody sees.
    ADD CONSTRAINT asset_comments_region_bounds CHECK (
        region_x IS NULL OR (
            region_x >= 0 AND region_y >= 0
            AND region_w > 0 AND region_h > 0
            AND region_x + region_w <= 1.0001
            AND region_y + region_h <= 1.0001)),

    -- Not negative, and not absurd. Twenty-four hours in milliseconds is past any asset a DAM holds, and the
    -- bound is what stops a millisecond value that was actually a Unix timestamp.
    ADD CONSTRAINT asset_comments_at_ms_sane CHECK (
        at_ms IS NULL OR (at_ms >= 0 AND at_ms <= 86400000));

-- The overlay's read: every annotation on one asset. Partial, because most comments are not annotations and an
-- index over all of them would be mostly rows the overlay never wants.
CREATE INDEX asset_comments_annotated_idx ON asset_comments (asset_id, created_at)
    WHERE region_x IS NOT NULL OR at_ms IS NOT NULL;

COMMENT ON COLUMN asset_comments.region_x IS
    'Fractions of the image, 0..1, origin top-left — never pixels. One asset has a thumbnail, a preview, a '
    'proxy and an original at four different sizes, and a mark stored in pixels would land correctly on '
    'exactly one of them.';

COMMENT ON COLUMN asset_comments.at_ms IS
    'Where in a video or audio track this comment is about. Independent of the region: a moment with no '
    'rectangle and a rectangle with no moment are both ordinary.';
