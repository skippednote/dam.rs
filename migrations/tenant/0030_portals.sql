-- Portals: a named, branded, long-lived share of a set (Q.14).
--
-- The comparator has four — Standard, Brand, Video and Channel — and reads as though they were four products. They are
-- not: they are four *presentations* of the same thing, which is "here is a set of assets, under our name, for
-- people who do not have accounts". So this is one table with a `kind` that chooses a layout and a default
-- filter, and **no kind changes who may see or take what**. That is worth stating in the schema because the
-- naming invites the opposite assumption, and a portal type that quietly widened access would be the worst
-- possible place for that mistake.
--
-- ## A portal is a share with a name
--
-- Every portal has a `share_links` row of kind `portal` pointing at it, and every visit resolves through it. So
-- expiry, passcode, the download cap, revocation and the rights re-evaluation at delivery are the machinery that
-- already exists (0001, 3.4, Q.13d) rather than a second implementation beside it. The alternative — a portal
-- with its own access columns — would be a second place where "may this person have these bytes" is decided, and
-- §12's argument applies exactly: divergence between the two would be a leak nobody could see.
--
-- ## The slug is an alias, and only for a public portal
--
-- A portal wants a URL somebody can type: `/portal/press-kit` rather than forty characters of base64. But a slug
-- is *guessable*, which is precisely what a share token is designed not to be. So `is_public` decides whether
-- the slug resolves at all: a public portal is reachable by name, a private one only by its token. One column,
-- and it is the difference between a press kit and a client's unreleased campaign.
--
-- ## Exactly one source
--
-- A collection, a saved search, or a media class — never two. A portal with two sources has no defined content,
-- and the CHECK below is cheaper than the support call. `media_class` is what makes a Video or Channel portal
-- possible without a curated collection: "everything in this library that is a video" is a legitimate set.
--
-- ## Searching inside a portal narrows and never widens
--
-- `allow_search` lets a visitor filter what they were given. The query they type is composed *with* the portal's
-- own source, exactly as a signed-in caller's query is composed with their predicate — the same two-gate shape,
-- for the same reason. Enforced in the code that renders the set; recorded here because the column is what
-- suggests it is possible.

CREATE TABLE portals (
    id                  uuid PRIMARY KEY,

    -- The URL name, when public. Unique regardless, so a portal cannot be made public into a collision later.
    key                 text NOT NULL UNIQUE
                            CHECK (key ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
    title               text NOT NULL CHECK (length(btrim(title)) BETWEEN 1 AND 200),
    -- Shown above the set. A portal with nothing to say is a folder; this is where it stops being one.
    intro               text NOT NULL DEFAULT '',

    kind                text NOT NULL DEFAULT 'standard'
                            CHECK (kind IN ('standard', 'brand', 'video', 'channel')),

    -- ── the set ─────────────────────────────────────────────────────────────
    collection_id       uuid REFERENCES collections (id) ON DELETE CASCADE,
    saved_search_id     uuid REFERENCES saved_searches (id) ON DELETE CASCADE,
    -- 'image' | 'video' | 'audio' | 'document', matching `dam_media`'s classes.
    media_class         text CHECK (media_class IS NULL OR media_class IN ('image', 'video', 'audio', 'document')),
    CONSTRAINT portals_one_source CHECK (
        num_nonnulls(collection_id, saved_search_id, media_class) = 1),

    -- ── branding ────────────────────────────────────────────────────────────
    -- An asset rather than an uploaded file: a logo is an asset, it is already governed, and a second upload
    -- path for it would be a second thing to back up and a second place for an unlicensed image to appear.
    logo_asset_id       uuid REFERENCES assets (id) ON DELETE SET NULL,
    accent              text NOT NULL DEFAULT '#2563eb'
                            CHECK (accent ~ '^#[0-9a-f]{6}$'),

    -- ── access ──────────────────────────────────────────────────────────────
    -- Whether the slug resolves. See the note above: this is the whole difference between named and secret.
    is_public           boolean NOT NULL DEFAULT false,
    allow_search        boolean NOT NULL DEFAULT true,

    created_by          uuid,                    -- dam_global.identities.id
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    -- Retired rather than deleted, so a URL that was handed out stops working with a reason rather than a 404
    -- that reads as a mistake. The share link is revoked at the same moment; this is the copy a person reads.
    retired_at          timestamptz
);

-- The public list, and the lookup a slug visit does.
CREATE INDEX portals_public_idx ON portals (key) WHERE is_public AND retired_at IS NULL;
CREATE INDEX portals_live_idx ON portals (created_at DESC) WHERE retired_at IS NULL;

COMMENT ON TABLE portals IS
    'A named, branded share of a set, for people without accounts. Access is the share link that points at it — '
    'this table is presentation and content, never permission.';

COMMENT ON COLUMN portals.is_public IS
    'Whether `key` resolves as a URL. A slug is guessable and a token is not, so this is the line between a '
    'press kit anybody may read and a set that is secret until somebody is sent the link.';

-- `share_links.kind` gains the portal, the way 0026 added the order.
ALTER TABLE share_links
    DROP CONSTRAINT share_links_kind_check;

ALTER TABLE share_links
    ADD CONSTRAINT share_links_kind_check
        CHECK (kind IN ('asset', 'collection', 'search', 'order', 'portal'));

COMMENT ON COLUMN share_links.kind IS
    'What the token shares. `asset`, `order` and `portal` are the three the portal page renders: one asset, the '
    'set an order was placed for, or a named portal. `collection` and `search` are reserved and answered with a '
    'refusal that says so — a portal is how a set gets shared.';
