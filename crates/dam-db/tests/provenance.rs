//! The G1 regression detector (1.9), against a real database.
//!
//! `provenance_gaps` is the invariant D13 turns into something checkable: a derivative of an asset that
//! *had* inbound credentials, where we produced no signed manifest, means the pipeline stripped
//! provenance. TASKS.md states the test directly — the view is empty after deriving from an asset with
//! credentials — and the negative case matters just as much, because a view that is always empty would
//! pass that assertion while detecting nothing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::provenance::{self, NewManifest, Role};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

/// One container for the suite. Each case scopes itself to its own asset, so they do not interact —
/// and eight containers for eight cases put the whole workspace run over what the Docker host takes,
/// which surfaced as a *different* suite failing on a retryable SeaweedFS 500 and a bucket create that
/// never got scheduled. See the note in `dam-api/tests/tus.rs`: a shared pool across `#[tokio::test]`s
/// is not available, because each builds its own runtime.
async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

/// An asset row, with no credentials recorded yet.
async fn asset(pool: &PgPool, key: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 1024, $1)",
    )
    .bind(id)
    .bind(format!("blake3:{key}"))
    .bind(format!("{key}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

/// A derivative in a role the view considers externally served.
async fn derivative(pool: &PgPool, asset_id: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
         VALUES ($1, $2, $3, 'web-2048', $4, $5, 'image/jpeg', 512)",
    )
    .bind(id)
    .bind(asset_id)
    .bind(role)
    .bind(id.to_string())
    .bind(format!("acme/p/{id}"))
    .execute(pool)
    .await
    .expect("derivative");
    id
}

/// The gaps for one asset.
///
/// Scoped, not tenant-wide. The cases share a database, and one of them deliberately leaves a gap
/// behind — but more importantly an assertion about "no gaps anywhere" was never what any of these
/// cases meant. Each is about whether *its* derivative kept the credentials *its* master had.
async fn gaps_for(pool: &PgPool, asset_id: Uuid) -> Vec<provenance::Gap> {
    provenance::gaps(pool, 1000)
        .await
        .expect("gaps")
        .into_iter()
        .filter(|gap| gap.asset_id == asset_id)
        .collect()
}

fn manifest<'a>(key: &'a str, state: &'a str, actions: &[&str]) -> NewManifest<'a> {
    NewManifest {
        object_key: key,
        bytes: 4096,
        validation_state: state,
        validation_detail: serde_json::json!({"activeManifest": {"success": []}}),
        signer_cn: Some("damrs-test.local"),
        claim_generator: Some("damrs/0.1.0"),
        spec_version: Some("2.2"),
        captured_at: None,
        actions: actions.iter().map(|a| (*a).to_owned()).collect(),
    }
}

async fn a_derivative_of_a_credentialed_asset_leaves_no_gap_once_its_manifest_is_recorded(
    pool: &PgPool,
) {
    // The assertion TASKS.md asks for.
    let asset_id = asset(pool, "credentialed").await;

    let inbound = provenance::record_inbound(
        pool,
        asset_id,
        &manifest("acme/c2pa/inbound", "valid", &["c2pa.created"]),
    )
    .await
    .expect("record inbound");

    let derivative_id = derivative(pool, asset_id, "proxy").await;
    provenance::record_signed(
        pool,
        derivative_id,
        Some(inbound),
        &manifest("acme/c2pa/proxy", "valid", &["c2pa.opened", "c2pa.resized"]),
    )
    .await
    .expect("record signed");

    assert_eq!(
        gaps_for(pool, asset_id).await,
        vec![],
        "a derivative with its own signed manifest is not a gap"
    );
}

async fn a_derivative_with_no_manifest_is_reported_as_a_gap(pool: &PgPool) {
    // The half that gives the test above its meaning. Without this, a view that returned nothing under
    // all circumstances would satisfy the requirement while detecting nothing — and this is exactly the
    // regression G1 describes: the derivative exists and is served, the credentials are gone.
    let asset_id = asset(pool, "stripped").await;
    provenance::record_inbound(
        pool,
        asset_id,
        &manifest("acme/c2pa/inbound", "valid", &["c2pa.created"]),
    )
    .await
    .expect("record inbound");

    let derivative_id = derivative(pool, asset_id, "proxy").await;

    let gaps = gaps_for(pool, asset_id).await;
    assert_eq!(gaps.len(), 1, "got {gaps:?}");
    assert_eq!(gaps[0].derivative_id, derivative_id);
}

async fn an_asset_that_never_had_credentials_is_never_a_gap(pool: &PgPool) {
    // Most of a real library. A derivative of an uncredentialed original has nothing to preserve, and
    // counting it would make the alarm fire on every ordinary photograph — which is the same as turning
    // the alarm off.
    let asset_id = asset(pool, "plain").await;
    derivative(pool, asset_id, "proxy").await;

    assert!(gaps_for(pool, asset_id).await.is_empty());
}

async fn a_thumbnail_is_not_counted_but_a_proxy_is(pool: &PgPool) {
    // The view's role filter, asserted because the boundary is a judgement call worth pinning: a
    // thumbnail is a UI affordance nobody treats as the asset, while a proxy or rendition is what a
    // customer downloads and forwards — and that is where a missing credential actually costs them
    // something.
    let asset_id = asset(pool, "mixed").await;
    provenance::record_inbound(
        pool,
        asset_id,
        &manifest("acme/c2pa/inbound", "valid", &["c2pa.created"]),
    )
    .await
    .expect("record inbound");

    derivative(pool, asset_id, "thumbnail").await;
    assert!(
        gaps_for(pool, asset_id).await.is_empty(),
        "a thumbnail without credentials is not a gap"
    );

    let proxy = derivative(pool, asset_id, "proxy").await;
    let gaps = gaps_for(pool, asset_id).await;
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].derivative_id, proxy);
}

async fn a_failed_inbound_manifest_is_recorded_and_still_requires_derivative_coverage(
    pool: &PgPool,
) {
    // Decision C2PA 3: accepted, recorded, not re-signed. The asset *did* arrive with credentials, so
    // the coverage requirement stands — the alternative would let a tool that broke the chain also
    // silently exempt every derivative from the check.
    let asset_id = asset(pool, "broken").await;
    provenance::record_inbound(
        pool,
        asset_id,
        &manifest("acme/c2pa/inbound", "invalid", &["c2pa.created"]),
    )
    .await
    .expect("record inbound");

    let (state, had): (String, bool) =
        sqlx::query_as("SELECT provenance_state, had_inbound_manifest FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_one(pool)
            .await
            .expect("asset state");
    assert_eq!(state, "invalid");
    assert!(
        had,
        "a broken chain is still an inbound manifest; treating it as absent would disable the check"
    );

    derivative(pool, asset_id, "rendition").await;
    assert_eq!(
        gaps_for(pool, asset_id).await.len(),
        1,
        "a failed inbound manifest does not exempt derivatives from coverage"
    );
}

async fn the_chain_is_reconstructible_without_parsing_any_manifest_blob(pool: &PgPool) {
    // Why `parent_manifest_id` exists. Walking the chain by parsing every stored blob would mean
    // restoring an archived master to answer "where did this come from" — the question provenance is
    // for.
    let asset_id = asset(pool, "chained").await;
    let inbound = provenance::record_inbound(
        pool,
        asset_id,
        &manifest("acme/c2pa/inbound", "valid", &["c2pa.created"]),
    )
    .await
    .expect("inbound");
    let derivative_id = derivative(pool, asset_id, "proxy").await;
    let signed = provenance::record_signed(
        pool,
        derivative_id,
        Some(inbound),
        &manifest("acme/c2pa/proxy", "valid", &["c2pa.opened", "c2pa.resized"]),
    )
    .await
    .expect("signed");

    let stored = provenance::for_asset(pool, asset_id).await.expect("load");
    assert_eq!(stored.len(), 2, "got {stored:?}");

    let inbound_row = stored
        .iter()
        .find(|m| m.role == Role::Inbound)
        .expect("an inbound manifest");
    let signed_row = stored
        .iter()
        .find(|m| m.role == Role::DamrsSigned)
        .expect("a signed manifest");
    assert_eq!(inbound_row.id, inbound);
    assert_eq!(signed_row.id, signed);
    assert_eq!(
        signed_row.parent_manifest_id,
        Some(inbound),
        "the derivative's manifest must point at the master's"
    );
    assert_eq!(inbound_row.parent_manifest_id, None);
}

async fn the_action_chain_is_stored_in_order(pool: &PgPool) {
    // Recorded relationally as well as inside the signed manifest, so "what did damrs do to this file"
    // is a query — and so a manifest can be regenerated if a signing certificate is rotated or a blob
    // is lost. Order carries meaning: opened-then-resized and resized-then-opened describe different
    // histories, and only one of them is possible.
    let asset_id = asset(pool, "actions").await;
    let derivative_id = derivative(pool, asset_id, "proxy").await;
    let id = provenance::record_signed(
        pool,
        derivative_id,
        None,
        &manifest(
            "acme/c2pa/proxy",
            "valid",
            &["c2pa.opened", "c2pa.resized", "c2pa.converted"],
        ),
    )
    .await
    .expect("signed");

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM provenance_actions WHERE manifest_id = $1 ORDER BY seq",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .expect("actions");
    assert_eq!(
        actions,
        vec!["c2pa.opened", "c2pa.resized", "c2pa.converted"]
    );
}

async fn an_asset_can_only_have_one_inbound_manifest(pool: &PgPool) {
    // Enforced by a unique partial index, asserted here because "as received" has to mean one thing.
    // A second inbound row would make the customer's evidence ambiguous, and re-verification would
    // pick whichever came back first.
    let asset_id = asset(pool, "single").await;
    provenance::record_inbound(pool, asset_id, &manifest("acme/c2pa/first", "valid", &[]))
        .await
        .expect("first");

    let second =
        provenance::record_inbound(pool, asset_id, &manifest("acme/c2pa/second", "valid", &[]))
            .await;
    assert!(second.is_err(), "a second inbound manifest must be refused");
}

#[tokio::test]
async fn the_provenance_invariants_hold() {
    let (_pg, pool) = db().await;

    // Each case uses its own asset, so ordering does not matter — but the gap cases are grouped
    // together so a failure in one is read against the others.
    a_derivative_of_a_credentialed_asset_leaves_no_gap_once_its_manifest_is_recorded(&pool).await;
    an_asset_that_never_had_credentials_is_never_a_gap(&pool).await;
    a_derivative_with_no_manifest_is_reported_as_a_gap(&pool).await;
    a_thumbnail_is_not_counted_but_a_proxy_is(&pool).await;
    a_failed_inbound_manifest_is_recorded_and_still_requires_derivative_coverage(&pool).await;
    the_chain_is_reconstructible_without_parsing_any_manifest_blob(&pool).await;
    the_action_chain_is_stored_in_order(&pool).await;
    an_asset_can_only_have_one_inbound_manifest(&pool).await;
}
