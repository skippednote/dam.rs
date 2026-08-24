//! Bring-your-own-key encryption on every write (G10·3).
//!
//! The risk this suite exists for is not that the feature does not work. It is that **one of the seven paths
//! that create an object forgets it**, and a write without the key does not fail — the object lands under the
//! bucket's default, which is indistinguishable from success until somebody audits the bucket. A test that
//! exercised `put` and stopped would pass for years while the promote copy, the storage-class transition or
//! the resumable upload quietly wrote unencrypted objects.
//!
//! So the load-bearing case here is structural: it reads the source and asserts that *every* call which
//! creates an object applies the store's key. That is deliberately a test about the code rather than about
//! behaviour, for the same reason `migrate::the_embedded_migration_counts_match_the_files_on_disk` is — the
//! failure it prevents is a future path added without the line, and no behavioural test of today's paths can
//! catch that.
//!
//! Neither SeaweedFS nor MinIO implements KMS, so there is no local backend that can confirm an object really
//! is encrypted under a given key. That is what `tests/aws_conformance.rs` and the nightly AWS workflow are
//! for; what can be proved here is that the request carries the key, which is the part this code controls.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_store::S3Store;

const KEY: &str = "arn:aws:kms:eu-west-2:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab";

/// The three SDK calls that bring an object into existence.
///
/// `upload_part` and `complete_multipart_upload` are deliberately absent: encryption is chosen when the
/// upload is *created*, and S3 rejects the headers on a part. `get_object`, `head_object` and `delete_object`
/// cannot create anything.
const CREATES_AN_OBJECT: [&str; 3] = [
    ".put_object()",
    ".copy_object()",
    ".create_multipart_upload()",
];

/// Every file that may talk to S3 directly. Listed rather than walked, so adding a module is a deliberate
/// decision to include it here.
const SOURCES: [(&str, &str); 2] = [
    ("s3.rs", include_str!("../src/s3.rs")),
    ("multipart.rs", include_str!("../src/multipart.rs")),
];

#[test]
fn every_call_that_creates_an_object_applies_the_customer_key() {
    let mut checked = 0;
    for (name, source) in SOURCES {
        for (index, line) in source.lines().enumerate() {
            let Some(call) = CREATES_AN_OBJECT
                .iter()
                .find(|call| line.trim_start().starts_with(*call))
            else {
                continue;
            };
            // The applicator is somewhere in the same builder chain, which runs until the statement ends.
            //
            // Comments are skipped when looking for that end, and the first version of this did not: a
            // comment in one of the chains reads "…carries metadata across;", so `ends_with(';')` matched it
            // and the scan stopped three lines short of the `.encrypted_with` it was looking for. The test
            // failed on correct code, which is the more embarrassing direction but at least the loud one.
            let rest = chain(source, index);
            assert!(
                rest.contains(".encrypted_with("),
                "{name}:{} — `{call}` creates an object and does not apply the store's key. A write \
                 without it lands under the bucket's default, which looks exactly like success. Add \
                 `.encrypted_with(self.sse_kms_key_id())` to the chain.\n\n{rest}",
                index + 1
            );
            checked += 1;
        }
    }
    // The count is asserted so that a refactor which *removes* a write path — or breaks this test's own
    // parsing, so it silently matches nothing — fails here rather than passing vacuously.
    assert_eq!(
        checked, 7,
        "expected seven object-creating calls across the S3 driver; found {checked}. If a path was added or \
         removed deliberately, update this number and say why."
    );
}

#[test]
fn a_store_carries_the_key_it_was_given_and_nothing_by_default() {
    let plain = S3Store::seaweedfs("http://127.0.0.1:1", "bucket", "key", "secret");
    assert_eq!(
        plain.sse_kms_key_id(),
        None,
        "BYOK is opt-in: a default key would fail every write for every deployment that does not use KMS"
    );

    let byok =
        S3Store::seaweedfs("http://127.0.0.1:1", "bucket", "key", "secret").with_sse_kms(KEY);
    assert_eq!(byok.sse_kms_key_id(), Some(KEY));

    // Blank means no key, and whitespace is trimmed — the same normalisation `StorageConfig` applies to the
    // environment variable. Without it the two paths disagree, and `Some("")` sends an empty key id that
    // fails every write with an error naming the key rather than its absence.
    for blank in ["", "   ", "\t"] {
        assert_eq!(
            S3Store::seaweedfs("http://127.0.0.1:1", "bucket", "key", "secret")
                .with_sse_kms(blank)
                .sse_kms_key_id(),
            None,
            "{blank:?} should mean no key"
        );
    }
    assert_eq!(
        S3Store::seaweedfs("http://127.0.0.1:1", "bucket", "key", "secret")
            .with_sse_kms(format!("  {KEY}  "))
            .sse_kms_key_id(),
        Some(KEY),
        "surrounding whitespace reaches S3 and is rejected on every write"
    );
}

#[tokio::test]
async fn a_presigned_put_carries_the_encryption_choice_into_the_signature() {
    // The path that cannot be enforced from here, so it is worth seeing exactly what it does offer. The
    // browser executes this request; signing the headers means it *may* satisfy them, and the bucket policy
    // is what makes it have to. `docker/DEPLOY.md` states that policy as required.
    use dam_store::{BlobStore, Key};
    use std::time::Duration;

    let key = Key::original(uuid::Uuid::nil(), &"a".repeat(64)).expect("key");

    let plain = S3Store::seaweedfs("http://127.0.0.1:1", "bucket", "ak", "sk");
    let unencrypted = plain
        .presign_put(&key, Duration::from_secs(300))
        .await
        .expect("presign");
    assert!(
        !unencrypted
            .to_ascii_lowercase()
            .contains("server-side-encryption"),
        "a store with no key must not sign headers the client would then have to send: {unencrypted}"
    );

    let byok = S3Store::seaweedfs("http://127.0.0.1:1", "bucket", "ak", "sk").with_sse_kms(KEY);
    let encrypted = byok
        .presign_put(&key, Duration::from_secs(300))
        .await
        .expect("presign");
    let lowered = encrypted.to_ascii_lowercase();
    assert!(
        lowered.contains("server-side-encryption"),
        "the encryption choice has to reach the signature, or the client cannot satisfy it: {encrypted}"
    );
    // And the key id specifically, not merely "encrypt somehow" — SSE-S3 would satisfy the algorithm header
    // and use the provider's key, which is the thing BYOK exists to avoid.
    assert!(
        lowered.contains("aws-kms") || lowered.contains("kms-key-id"),
        "the *customer's* key has to be named: {encrypted}"
    );
}

/// The builder chain starting at `index`, up to and including the line that ends the statement.
///
/// Comment lines are skipped when deciding where the statement ends: prose ending in a semicolon is prose,
/// not the end of an expression. Bounded, so a malformed source cannot make this run to the end of the file
/// and match something from an unrelated function.
fn chain(source: &str, index: usize) -> String {
    const MOST_LINES_A_CHAIN_TAKES: usize = 24;
    let mut out = Vec::new();
    for line in source.lines().skip(index).take(MOST_LINES_A_CHAIN_TAKES) {
        out.push(line);
        let code = line.trim_start();
        if !code.starts_with("//") && line.trim_end().ends_with(';') {
            break;
        }
    }
    out.join("\n")
}
