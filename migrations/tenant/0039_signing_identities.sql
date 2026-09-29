-- A tenant's own C2PA signing identity (G10·3b).
--
-- Until now one deployment signed every tenant's derivatives with one identity, from
-- `security.signing_cert_pem` / `signing_key_pem`. That is correct for a single-tenant or dedicated
-- install and is not BYOK on a shared one — and a signing identity is the purpose where that
-- distinction reaches furthest outside the system, because the identity is what a consumer
-- *verifies a provenance claim against*. Two customers' assets signed by one certificate assert,
-- to anyone who checks, that they came from the same publisher.
--
-- ## Why the material lives here rather than in `dam_global.encryption_keys`
--
-- That table's `key_ref` is documented as "ARN or URI; never key material", which is what makes a
-- backup of it disclose nothing. A sealed PEM *is* material, even sealed, so putting it there would
-- break the one property that column has. It lives in the tenant schema instead, mirroring
-- `ai_credentials` — the other place a tenant hands damrs a secret damrs must be able to use later.
--
-- `encryption_keys` with purpose 'c2pa_signing' remains the right home for the *other* shape: a
-- signing key held in a KMS and referenced by ARN. The two are alternatives, not duplicates, and a
-- deployment picks one.
--
-- ## The certificate is not a secret and the key is
--
-- `cert_pem` is public by construction — it is handed to every verifier of every asset signed with
-- it. `sealed_key` is sealed with the deployment's sealing keyring and bound to this row, so a row
-- lifted into another tenant's schema fails to open rather than signing on their behalf.
CREATE TABLE signing_identities (
    id                  uuid PRIMARY KEY,

    -- What a person calls it. Required for the same reason `ai_credentials.label` is: a list of two
    -- rows that both say "signing identity" is a list nobody can act on.
    label               text NOT NULL CHECK (length(btrim(label)) BETWEEN 1 AND 120),

    -- Public. Stored as given, because a re-serialised certificate is a different byte string and
    -- the chain is verified over bytes.
    cert_pem            text NOT NULL CHECK (cert_pem LIKE '%BEGIN CERTIFICATE%'),

    -- Sealed, and the version prefix is checked so an unsealed key cannot be written here by a
    -- caller that forgot — the same guard `ai_credentials` uses.
    sealed_key          text NOT NULL CHECK (sealed_key ~ '^v[0-9]+\.'),
    sealing_key_id      text NOT NULL,

    -- Must match what the certificate actually uses; the signer refuses a mismatch at load rather
    -- than producing manifests nothing can verify.
    algorithm           text NOT NULL,
    -- RFC 3161 timestamper. Without one a signature cannot be shown to predate the certificate's
    -- expiry, so a manifest stops verifying when the certificate lapses rather than when the claim
    -- does.
    timestamp_authority text,

    is_active           boolean NOT NULL DEFAULT true,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- One identity signs at a time. Superseded rows are kept: a manifest signed last year is verified
-- against the certificate that signed it, so deleting the row would not invalidate the signature but
-- would remove the only record of which identity produced it.
CREATE UNIQUE INDEX signing_identities_active_idx ON signing_identities (is_active)
    WHERE is_active;

CREATE INDEX signing_identities_sealing_key_idx ON signing_identities (sealing_key_id);

COMMENT ON COLUMN signing_identities.sealed_key IS
    'The private key, sealed with the deployment keyring and bound to this row. Never plaintext.';
COMMENT ON COLUMN signing_identities.cert_pem IS
    'The certificate. Public by construction: every verifier of every signed asset receives it.';
