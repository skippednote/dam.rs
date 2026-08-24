# Your bucket, your keys, your bill

The most consequential storage feature in dam.rs is the one the product does not provide. It does not rent a hidden pool of bytes to the customer. The operator selects the object store, owns the account and bucket, controls the encryption policy, and can inspect the provider's bill without asking the DAM vendor to explain an overage.

> [!TLDR]
> A bring-your-own-bucket DAM changes the ownership boundary: application code manages assets, while the customer controls object storage, encryption policy, lifecycle, and raw cost. S3 compatibility helps only when drivers declare and test their real capabilities, because restore semantics, checksums, Object Lock, and KMS support vary. Content-addressed keys and customer-managed encryption reduce migration and integrity risk, but they do not remove the need to operate Postgres, policies, backups, and the media pipeline.

"Your bucket" is easy to reduce to a procurement checkbox. The interesting consequences are technical. Object identity can be derived rather than assigned. Storage-class policy can be evaluated in the customer's cost context. Encryption can be enforced by the bucket rather than promised by an application setting. Stopping the application does not initiate a bulk transfer because the objects remain in the account where they were written.

## Ownership changes the failure boundary

In a vendor-owned storage model, the DAM and the bytes are one service boundary. Export, network egress, lifecycle configuration, storage pricing, and encryption evidence all pass through that boundary.

In dam.rs, Postgres and the object store remain distinct systems under the operator's control. The application stores metadata, jobs, rights, and placements in Postgres. The bucket stores originals, proxies, derivatives, and detached manifests. A private delivery path authorises short-lived access to object bytes.

```press-diagram
{"type":"stack","title":"Storage ownership boundary","layers":[{"label":"clients","nodes":[{"label":"web app"},{"label":"connector"}]},{"label":"dam.rs","nodes":[{"label":"API"},{"label":"worker"},{"label":"Postgres"}]},{"label":"your account","nodes":[{"label":"object store"},{"label":"KMS"},{"label":"billing"}]}],"footer":"The application coordinates storage it does not own on the customer's behalf."}
```

This does not eliminate coupling. The schema contains object keys, checksums, classes, and restore state. A replacement application must understand or migrate that metadata. The difference is that no export API stands between the operator and the bytes.

## S3-compatible is a protocol claim, not a capability claim

The S3 API has become a common object-storage interface, but implementations do not behave identically. AWS S3, MinIO, Ceph RGW, SeaweedFS, Cloudflare R2, Backblaze B2, and Wasabi expose overlapping S3-shaped APIs. Their support for individual operations and headers differs, sometimes intentionally.

Several features matter to a DAM:

- storage classes and lifecycle transitions;
- `RestoreObject` and restore status headers;
- bucket versioning;
- Object Lock, legal hold, and retention;
- presigned reads and writes;
- ranged `GET`;
- server-side checksums returned by `HEAD`;
- provider-specific encryption integration.

Treating "S3-compatible" as one boolean would force two bad options. The driver could assume AWS behaviour and break against a gateway, or target the smallest common subset and leave useful features unused.

dam.rs makes capabilities data:

```rust
pub struct Capabilities {
    pub storage_classes: bool,
    pub restore: bool,
    pub versioning: bool,
    pub object_lock: bool,
    pub presigned_urls: bool,
    pub ranged_get: bool,
    pub server_checksums: bool,
}
```

The shared conformance suite verifies each claim. Under-claiming produces explicit skips. Over-claiming fails. SeaweedFS, for example, can echo a storage-class header without changing retrieval behaviour. dam.rs does not count that as storage-class support. It also does not return a stored checksum on `HEAD`, so the driver declares `server_checksums: false` rather than manufacturing one.

That honesty lets higher layers choose a safe fallback. The integrity scrub compares a server checksum only when one exists. The archive UI appears only when restore semantics are available. Object Lock cannot be asserted from a local fake that never asks a server to refuse deletion.

> [!NOTE]
> API compatibility is useful because it narrows the adapter surface. It is not evidence that operational semantics match. A conformance report belongs beside every driver name.

## Keys are derived from content and tenant

Originals use a validated key built from the tenant UUID and the BLAKE3 digest of the bytes:

```rust
pub fn original(tenant: Uuid, blake3_hex: &str) -> Result<Self, Error> {
    let hash = Self::validated_hash(blake3_hex)?;
    Self::new(format!(
        "{tenant}/o/{}/{}/{hash}",
        &hash[0..2],
        &hash[2..4]
    ))
}
```

A master therefore lands at a shape like:

```text
8f3d3d9e-0e53-4b38-8706-5191f8ef90cd/o/64/37/6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85
```

The two digest-prefix directories keep listings manageable. The tenant prefix is the isolation boundary. Identical bytes uploaded twice inside one tenant resolve to one original object. Identical bytes in two tenants remain in two prefixes, which intentionally trades cross-tenant deduplication for isolation and simpler deletion accounting.

Content addressing has several useful consequences.

### A duplicate upload can skip the transfer

Once the stream has been hashed, the application can `HEAD` the derived key. If an object exists at the expected size, the upload is already present. A repeated upload links to the existing bytes instead of creating another opaque key.

Presence alone is not enough. If the object at that digest-derived key has the wrong size, dam.rs treats it as corruption and rewrites it. Calling it a duplicate would make the corruption permanent because every later upload of the correct bytes would also skip the write.

### Object names do not come from callers

A client cannot select another tenant's prefix or smuggle `..` into a path. `Key` validates every constructed object name and rejects leading slashes, empty segments, dot segments, control characters, and lengths beyond the S3 limit.

### Streaming keeps large masters bounded

The digest is computed while bytes flow. A 200 GB video does not need a 200 GB memory buffer. Streamed uploads first land under a tenant-scoped staging key because the final digest is unknown, then move through server-side copy once hashing completes. Objects above S3's single-copy limit use a multipart copy plan.

Content addressing is not a backup. A valid key can still point to a missing object if the database and bucket diverge. It provides stable identity and a checkable expectation; the integrity scrub still has to ask the store whether reality agrees.

## Customer-managed encryption needs two layers

The deployment setting `DAMRS_STORAGE__SSE_KMS_KEY_ID` makes dam.rs attach a customer-managed AWS KMS key to object-creating requests. Both `damd` and `dam-worker` need the same setting because workers promote uploads, generate derivatives, and transition storage classes.

There are seven object-creating call sites across ordinary `PUT`, copy, lifecycle copy, multipart creation, promotion, resumable upload, and presigned upload. Missing one does not necessarily cause an error. The object can land under the bucket default and look healthy until an encryption audit.

The SDK integration centralises the two required headers across the three builder types that create objects:

```rust
Some(key) => self
    .server_side_encryption(
        aws_sdk_s3::types::ServerSideEncryption::AwsKms
    )
    .ssekms_key_id(key),
None => self,
```

A structural test scans each object-creating builder chain for `.encrypted_with(...)` and asserts that exactly seven current call sites were examined. The count prevents a refactor that makes the scanner match nothing from passing vacuously.

That application check still cannot enforce a browser's presigned upload. The browser executes the `PUT` and can omit signed encryption headers unless the signature requires them. Even then, bucket policy is the final enforcement boundary. A valid deployment policy refuses writes that do not name the expected key:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "DenyWrongKmsKey",
      "Effect": "Deny",
      "Principal": "*",
      "Action": "s3:PutObject",
      "Resource": "arn:aws:s3:::example-dam-bucket/*",
      "Condition": {
        "StringNotEquals": {
          "s3:x-amz-server-side-encryption-aws-kms-key-id": "arn:aws:kms:eu-west-2:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab"
        }
      }
    }
  ]
}
```

The KMS key policy also needs `kms:GenerateDataKey*` for writes and `kms:Decrypt` for reads. A deployment that grants only decrypt can serve its existing library and fail every new upload, which is a particularly unhelpful form of read-only mode.

## Bring your own key is narrower than per-tenant keying

The current implementation applies one KMS key to one dam.rs deployment. It is useful for a customer-operated single-tenant deployment and for an organisation that wants its own revocation boundary.

It is not yet a distinct key per tenant inside a shared process. The control plane has tenant-scoped encryption-key references, and storage pools belong to tenants, but `damd` currently constructs one store from deployment configuration. Per-pool store resolution must exist before a request can choose a tenant-specific KMS key safely.

Naming that limitation matters. "BYOK" can describe several different controls:

| Control | Current state |
|---|---|
| Customer selects deployment KMS key | Implemented |
| Bucket policy enforces that key | Deployment responsibility |
| Separate key per tenant in one process | Not implemented |
| Customer can revoke key and make objects unreadable | Provided by KMS and policy |
| Application-layer encryption independent of provider | Deliberately not built |

We rejected application-layer encryption because it would require designing key envelopes, rotation, range-read behaviour, multipart semantics, and recovery on top of mature provider mechanisms. That would make dam.rs the cryptographic storage product it is trying not to become.

## The bucket does not remove exit work

Owning storage removes the largest physical export, but it does not make another DAM understand the library automatically.

The replacement still needs:

- Postgres data or a semantic export;
- the relationship between asset IDs, versions, and content hashes;
- transformation recipes for derivatives;
- rights and provenance records;
- integration URL migration;
- a delivery service that honours private bucket policy;
- a plan for cold objects that need restore before validation or transfer.

The key advantage is leverage, not magic. The organisation can perform that work without first asking a vendor to release or transmit the masters.

The same applies to cost. A customer-owned bucket exposes request, storage, retrieval, and network charges directly. It does not make them disappear. Bad lifecycle policy can be expensive. A public bucket can be catastrophic. A lost KMS key is not recoverable through a DAM support ticket.

## Proving the abstraction at both ends

Local development uses SeaweedFS because it is quick and exercises the S3 wire path, multipart upload, versioning, and Object Lock behaviour available in that driver. Pure state-machine tests use a fake store with a controllable clock.

Archive completion needs real AWS. The manual conformance run creates an isolated bucket, exercises the supported operations, waits for a Glacier restore, verifies that the storage class remains `GLACIER`, and reads the original bytes back. One recorded run passed 20 cases with zero skips and completed the restore in about 76.7 seconds.

Those layers prove different things:

| Test layer | What it proves | What it cannot prove |
|---|---|---|
| Fake store | State transitions and time boundaries | Provider behaviour |
| Local S3 gateway | SDK requests and supported gateway semantics | AWS archive retrieval |
| Real AWS | Declared S3 capabilities and restore completion | Every compatible provider |

No single green suite licenses the phrase "works everywhere." Each backend earns only the capabilities its own conformance run demonstrates.

## Where this is the wrong architecture

For a small team with a few thousand assets, ordinary rights needs, and nobody responsible for infrastructure, a hosted service is likely the better choice. Running this stack means operating Postgres backups and restore drills, object-store policy, KMS rotation, worker queues, media tools, observability, and upgrades.

Customer ownership also moves incident responsibility. A bucket policy typo, expired cloud identity, region outage, or revoked key is now your outage. Support can explain the code path; it cannot override the account boundary you deliberately chose.

The architecture starts earning its weight when storage class materially affects cost, data residency is contractual, rights must be enforced at delivery, the organisation already operates cloud infrastructure, or migration risk has become a strategic concern.

## FAQ

### Does S3-compatible mean every feature works on every object store?

No. It means a backend implements some portion of the S3 API. Storage classes, restore, Object Lock, versioning, checksums, ranged reads, and encryption integrations must be declared and tested separately.

### Does content addressing deduplicate assets across tenants?

No. The tenant UUID is part of the object key, so deduplication occurs within a tenant. That preserves isolation and keeps one tenant's deletion or billing independent of another's identical bytes.

### Is setting an SSE-KMS key enough to guarantee BYOK encryption?

No. Application requests must carry the key, presigned clients must send the required headers, and bucket policy must deny writes under any other key. The policy is the enforcement boundary.

### Can I leave dam.rs without exporting the bucket?

Yes, because the objects already live in the operator's bucket. You still need a semantic metadata migration and integration cutover, but the masters do not need to be bought back or transferred out of a vendor-owned store. Ownership does not erase migration work; it removes the party that can prevent you from starting it.
