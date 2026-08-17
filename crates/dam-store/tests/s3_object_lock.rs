//! Versioning and object lock against a real server.
//!
//! These cases exist here and nowhere else. `FakeS3Store` deliberately does not claim
//! `object_lock`, because the entire point of a legal hold is that the **server**
//! refuses the delete — a fake that refuses proves only that the fake refuses
//! (ARCHITECTURE §20.3). Object lock is what backs the "Legal hold / EULA-encumbered →
//! pinned, S3 Object Lock, never tiers" row in §6.3, so it needs a server in the loop.
//!
//! Versioning matters for the same section's noncurrent-version tiering rule: superseded
//! originals go to `GLACIER_IR` at 30 d, which is only meaningful if a superseded version
//! remains individually addressable after an overwrite.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use bytes::Bytes;
use dam_core::StorageClass;
use dam_store::{
    BlobStore, Bypass, Key, RetentionMode,
    testing::{SeaweedfsHarness, unique_key},
};

/// Reads an object's current version, failing loudly rather than returning a ticket —
/// nothing in this file is archived.
async fn read(store: &dam_store::S3Store, key: &Key) -> Bytes {
    store
        .get(key, None)
        .await
        .expect("get")
        .into_bytes(key)
        .expect("hot object")
}

#[tokio::test]
async fn a_superseded_version_stays_readable_after_an_overwrite() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    store.enable_versioning().await.expect("enable versioning");

    let key = unique_key("overwrite");
    let first = store
        .put(&key, Bytes::from_static(b"first"), StorageClass::Standard)
        .await
        .expect("put v1");
    let second = store
        .put(&key, Bytes::from_static(b"second"), StorageClass::Standard)
        .await
        .expect("put v2");

    let v1 = first
        .version_id
        .expect("a put into a versioned bucket must report the version it created");
    let v2 = second.version_id.expect("version id for v2");
    assert_ne!(
        v1, v2,
        "an overwrite must create a new version, not replace"
    );

    assert_eq!(&read(&store, &key).await[..], b"second", "current version");
    assert_eq!(
        &store.get_version(&key, &v1).await.expect("read v1")[..],
        b"first",
        "the superseded bytes must remain addressable — §6.3 tiers them separately"
    );

    let versions = store.list_versions(key.as_str(), 10).await.expect("list");
    assert_eq!(versions.len(), 2, "both versions listed: {versions:?}");
    let latest: Vec<_> = versions.iter().filter(|v| v.is_latest).collect();
    assert_eq!(latest.len(), 1, "exactly one latest version");
    assert_eq!(latest[0].version_id, v2);
}

#[tokio::test]
async fn a_delete_leaves_a_marker_and_the_bytes_recoverable_by_version() {
    let harness = SeaweedfsHarness::start().await.expect("start");
    let store = harness.store();
    store.enable_versioning().await.expect("enable versioning");

    let key = unique_key("delete-marker");
    let placement = store
        .put(
            &key,
            Bytes::from_static(b"recoverable"),
            StorageClass::Standard,
        )
        .await
        .expect("put");
    let version = placement.version_id.expect("version id");

    // An unversioned delete on a versioned bucket writes a delete marker.
    store.delete(&key).await.expect("delete");

    assert!(
        store.head(&key).await.is_err(),
        "the current version reads as absent once a delete marker is on top"
    );
    assert_eq!(
        &store.get_version(&key, &version).await.expect("by version")[..],
        b"recoverable",
        "the bytes survive the marker — this is what makes accidental deletion recoverable"
    );

    let markers = store
        .list_versions(key.as_str(), 10)
        .await
        .expect("list")
        .into_iter()
        .filter(|v| v.is_delete_marker)
        .count();
    assert_eq!(markers, 1, "exactly one delete marker");
}

#[tokio::test]
async fn a_legal_hold_makes_the_server_refuse_a_version_delete_until_released() {
    let harness = SeaweedfsHarness::start_with_object_lock()
        .await
        .expect("start");
    let store = harness.store();

    let key = unique_key("legal-hold");
    let version = store
        .put(
            &key,
            Bytes::from_static(b"under litigation"),
            StorageClass::Standard,
        )
        .await
        .expect("put")
        .version_id
        .expect("an object-lock bucket is versioned, so a put reports a version");

    store
        .set_legal_hold(&key, &version, true)
        .await
        .expect("apply hold");
    assert!(
        store.legal_hold(&key, &version).await.expect("read hold"),
        "the hold must be readable back — a write-only hold cannot be audited"
    );

    let refused = store.delete_version(&key, &version, Bypass::No).await;
    assert!(
        refused.is_err(),
        "the SERVER must refuse a held version delete, got {refused:?}"
    );
    assert_eq!(
        &store
            .get_version(&key, &version)
            .await
            .expect("still there")[..],
        b"under litigation"
    );

    store
        .set_legal_hold(&key, &version, false)
        .await
        .expect("release hold");
    store
        .delete_version(&key, &version, Bypass::No)
        .await
        .expect("delete once released");
}

#[tokio::test]
async fn governance_retention_yields_to_the_bypass_header_and_compliance_does_not() {
    let harness = SeaweedfsHarness::start_with_object_lock()
        .await
        .expect("start");
    let store = harness.store();
    let until = chrono::Utc::now() + chrono::Duration::days(1);

    // GOVERNANCE: a privileged caller can override. This is the mode a normal retention
    // policy uses, because a mistaken policy must be correctable.
    let governed = unique_key("governance");
    let g_version = store
        .put(
            &governed,
            Bytes::from_static(b"governed"),
            StorageClass::Standard,
        )
        .await
        .expect("put")
        .version_id
        .expect("version id");
    store
        .set_retention(&governed, &g_version, RetentionMode::Governance, until)
        .await
        .expect("set governance retention");
    assert!(
        store
            .delete_version(&governed, &g_version, Bypass::No)
            .await
            .is_err(),
        "governance retention refuses an ordinary delete"
    );
    store
        .delete_version(&governed, &g_version, Bypass::Governance)
        .await
        .expect("bypass is exactly what GOVERNANCE mode is for");

    // COMPLIANCE: nobody overrides, including the account root. This is the mode a
    // regulatory hold uses, and choosing it by mistake is unrecoverable — hence the two
    // modes are separate variants a caller must pick between, never a default.
    let complied = unique_key("compliance");
    let c_version = store
        .put(
            &complied,
            Bytes::from_static(b"complied"),
            StorageClass::Standard,
        )
        .await
        .expect("put")
        .version_id
        .expect("version id");
    store
        .set_retention(&complied, &c_version, RetentionMode::Compliance, until)
        .await
        .expect("set compliance retention");
    assert!(
        store
            .delete_version(&complied, &c_version, Bypass::No)
            .await
            .is_err(),
        "compliance retention refuses an ordinary delete"
    );
    assert!(
        store
            .delete_version(&complied, &c_version, Bypass::Governance)
            .await
            .is_err(),
        "compliance retention must refuse the bypass too — that is the whole difference \
         from governance, and a driver that silently succeeded here would let a \
         regulatory hold be deleted"
    );
    assert_eq!(
        &store
            .get_version(&complied, &c_version)
            .await
            .expect("held")[..],
        b"complied"
    );
}

#[tokio::test]
async fn the_governance_bypass_is_a_permission_not_just_a_header() {
    let harness = SeaweedfsHarness::start_with_object_lock()
        .await
        .expect("start");
    let admin = harness.store();
    let limited = harness.store_without_bypass_permission();
    let key = unique_key("bypass-permission");

    let version = admin
        .put(
            &key,
            Bytes::from_static(b"governed"),
            StorageClass::Standard,
        )
        .await
        .expect("put")
        .version_id
        .expect("version id");
    limited
        .set_retention(
            &key,
            &version,
            RetentionMode::Governance,
            chrono::Utc::now() + chrono::Duration::days(1),
        )
        .await
        .expect("an ordinary credential may APPLY a retention");

    // The credential that applied the hold must not be able to lift it. If the header alone
    // sufficed, every credential able to write would also be able to delete through a
    // retention policy, which would make governance mode decorative.
    assert!(
        limited
            .delete_version(&key, &version, Bypass::Governance)
            .await
            .is_err(),
        "a credential without BypassGovernanceRetention must be refused even with the header"
    );
    assert_eq!(
        &admin.get_version(&key, &version).await.expect("intact")[..],
        b"governed"
    );

    admin
        .delete_version(&key, &version, Bypass::Governance)
        .await
        .expect("the privileged identity may bypass");
}

#[tokio::test]
async fn a_backend_that_does_not_claim_object_lock_refuses_before_the_wire() {
    // No container: the guard is in the driver, so a caller gets `Unsupported` — which it
    // can degrade on — rather than a backend-specific error it has to pattern-match.
    let narrow = dam_store::S3Store::compatible(
        "http://127.0.0.1:1", // never dialled; the guard fires first
        "irrelevant",
        "us-east-1",
        "k",
        "s",
        dam_store::Capabilities::minimal(),
        "narrow",
    );
    let key = unique_key("no-lock");

    for err in [
        narrow
            .set_legal_hold(&key, "v1", true)
            .await
            .expect_err("hold"),
        narrow
            .set_retention(&key, "v1", RetentionMode::Compliance, chrono::Utc::now())
            .await
            .expect_err("retention"),
        narrow.enable_versioning().await.expect_err("versioning"),
    ] {
        assert!(
            matches!(err, dam_store::Error::Unsupported { .. }),
            "expected Unsupported, got {err:?}"
        );
    }
}
