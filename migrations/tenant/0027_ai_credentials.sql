-- A tenant's own model-provider credentials (M5).
--
-- BYO keys, per tenant, because the alternative is a deployment-wide key and a bill nobody can attribute. A
-- tenant that brings its own key owns its spend, its rate limits and its provider relationship — and damrs is
-- not in the position of reselling somebody else's tokens.
--
-- ## The key is encrypted, not hashed
--
-- Unlike a share passcode or one of our own API keys, this secret has to come *back*: damrs signs a request with
-- it later. So it is sealed rather than digested — `dam_core::sealed`, ChaCha20-Poly1305 with the ciphertext bound
-- to its tenant and this row's id, so a row copied anywhere else refuses to open. A dump, a backup, a replica or
-- a stray SELECT yields `v1.<key_id>.<nonce>.<ciphertext>` and nothing more.
--
-- The database never sees plaintext, and neither does `dam_db`: sealing happens in the layer that holds the
-- keyring, and this column's type is "opaque text" as far as everything below the API is concerned.
--
-- ## Two providers, not one per vendor
--
-- `anthropic` speaks its own wire format and has the two features §8.3 builds the cost model on — the Batch API
-- at half price and prompt caching at ~90% off a shared prefix. Everything else worth using speaks
-- `/chat/completions`: OpenAI, Kimi/Moonshot, DeepSeek, Together, Groq, and any local server pretending to be
-- them. So the vocabulary is two clients, and a vendor is a *base URL plus a model name* rather than a new
-- branch in the code.
CREATE TABLE ai_credentials (
    id                  uuid PRIMARY KEY,

    provider            text NOT NULL CHECK (provider IN ('anthropic', 'openai_compatible')),
    -- What a person calls it: "OpenAI (production)", "Kimi", "our Bedrock gateway". Required, because a list of
    -- two rows both saying `openai_compatible` is a list nobody can act on.
    label               text NOT NULL CHECK (length(btrim(label)) BETWEEN 1 AND 120),

    -- Where to send requests. For `openai_compatible` this is what distinguishes OpenAI from Kimi from a local
    -- llama.cpp, so it is required there; for `anthropic` it is an override for a gateway or a proxy and null
    -- means the vendor's own endpoint.
    base_url            text CHECK (base_url IS NULL OR base_url ~ '^https?://[^[:space:]]+$'),
    CONSTRAINT ai_credentials_compatible_needs_a_url CHECK (
        provider <> 'openai_compatible' OR base_url IS NOT NULL
    ),

    -- The sealed key, and which sealing key sealed it. The id is in the sealed text too; it is a column as well
    -- so a rotation can find the rows it still has to re-seal *without opening any of them*.
    sealed_key          text NOT NULL CHECK (sealed_key ~ '^v[0-9]+\.'),
    sealing_key_id      text NOT NULL,
    -- The last four characters, so somebody can tell two keys apart in a list without either being shown. Empty
    -- for a secret too short to reveal four characters of safely.
    hint                text NOT NULL DEFAULT '',

    -- Which model this credential is used with by default. Required: a caller asking for enrichment should not
    -- have to know a vendor's model names, and a null here would make every call site guess.
    default_model       text NOT NULL CHECK (length(btrim(default_model)) BETWEEN 1 AND 200),

    -- Withdrawn rather than deleted, like a conversion: a credential named by a run that is still in flight
    -- should not vanish under it, and an audit of what enriched an asset should still resolve.
    is_active           boolean NOT NULL DEFAULT true,
    -- The one enrichment uses when nothing says otherwise.
    --
    -- Exactly one, enforced below. Routing *per task* — §8.2's cheap model for bulk classification and the good
    -- one for anything a person reads — is a real requirement and a later slice; a tenant needs one working
    -- credential before it needs two working ones.
    is_default          boolean NOT NULL DEFAULT false,

    created_by          uuid,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- One default, and only among the active ones: a withdrawn credential that kept the flag would make the default
-- unreachable while looking set.
CREATE UNIQUE INDEX ai_credentials_default_idx ON ai_credentials ((true))
    WHERE is_default AND is_active;

-- The rotation question: which rows are still sealed under an old key.
CREATE INDEX ai_credentials_sealing_key_idx ON ai_credentials (sealing_key_id);

COMMENT ON TABLE ai_credentials IS
    'Per-tenant model-provider keys, sealed with dam_core::sealed. The plaintext never reaches this table, and '
    'the ciphertext is bound to (tenant, provider, id) so a copied row does not open.';
COMMENT ON COLUMN ai_credentials.sealed_key IS
    'v1.<sealing key id>.<nonce>.<ciphertext>. Opening needs the deployment sealing keyring and the same '
    'associated data it was sealed under; see dam_core::sealed.';
