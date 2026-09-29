-- Per-pool encryption keys (G10·3b).
--
-- `storage_pools` already carries everything else a bucket needs — endpoint, bucket, prefix,
-- credentials_ref — so the key that bucket's writes should use belongs beside them rather than in
-- a second place. This is the *infrastructure* key: it says "objects in this bucket are encrypted
-- with this", and it is a property of the pool whoever runs the deployment configured.
--
-- It is not the same fact as `encryption_keys` with purpose 'blob', and the two are deliberately
-- both kept. That row is a *tenant's* key — a customer-managed key a customer can rotate and
-- revoke, which is what BYOK means and what an RFP asks about. Resolution is therefore ordered:
-- the tenant's own active key, else the pool's, else whatever the deployment configured in
-- `storage.sse_kms_key_id`. Each step is more specific than the last and none of them is a
-- fallback to something weaker than "no key", which is what the process did before any of this.
--
-- Nullable, because most pools do not have one and a NOT NULL here would need a sentinel that
-- every reader would then have to know means "none".
ALTER TABLE storage_pools ADD COLUMN kms_key_ref text;

COMMENT ON COLUMN storage_pools.kms_key_ref IS
    'KMS key ARN or URI for objects written to this pool. Never key material. '
    'Overridden by an active dam_global.encryption_keys row for the tenant with purpose ''blob''.';
