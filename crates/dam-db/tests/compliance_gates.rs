//! The D12–D15 guarantees, enforced by the database.
//!
//! Every case here was proved by hand during design; this makes them permanent. If
//! one regresses, the build fails — which is the point. These are not defence in
//! depth around application logic, they are the floor beneath it: a support engineer
//! with a psql prompt, a bulk import, or a stray migration cannot get underneath
//! them.
//!
//! Each test states the obligation, not just the mechanism, because in six months
//! the mechanism will be obvious and the obligation will not.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_db::{migrate, testing::PostgresHarness};
use sqlx::{Executor, PgPool};

/// Boots a container with the control plane and one tenant schema at head, and
/// returns a pool whose every connection resolves `t_acme` first.
///
/// The harness is returned alongside the pool because dropping it stops the
/// container — binding it to `_` makes every query fail with a connection error.
async fn tenant_db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global migrations");
    migrate::tenant(&url, "t_acme")
        .await
        .expect("tenant migrations");
    let pool = pg
        .pool_for_schema("t_acme")
        .await
        .expect("schema-scoped pool");
    (pg, pool)
}

/// Asserts the database refused a statement **because a constraint rejected it**.
///
/// Deliberately not `.is_err()`. A statement that fails with "relation does not
/// exist" is also an error, so a bare `is_err()` lets a test pass while proving
/// nothing — which is exactly what happened on the first run of this suite, when a
/// mis-scoped `search_path` made several gates look enforced when the tables were
/// simply invisible. A gate test that can pass for the wrong reason is worse than
/// no test.
///
/// SQLSTATE class 23 is "integrity constraint violation" (check, unique, foreign
/// key, not-null). Class 42 — undefined table or column — panics loudly instead.
async fn refused_by_constraint(pool: &PgPool, sql: &str) -> bool {
    match pool.execute(sqlx::AssertSqlSafe(sql.to_owned())).await {
        Ok(_) => false,
        Err(sqlx::Error::Database(db)) => {
            let code = db.code().unwrap_or_default().to_string();
            assert!(
                code.starts_with("23") || code == "P0001",
                "statement failed, but not because a constraint refused it \
                 (SQLSTATE {code}: {db}).\n{sql}"
            );
            true
        }
        Err(e) => panic!("unexpected non-database error:\n{sql}\n{e}"),
    }
}

async fn must_succeed(pool: &PgPool, sql: &str) {
    pool.execute(sqlx::AssertSqlSafe(sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("expected success:\n{sql}\n{e}"));
}

async fn scalar_i64(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("query failed:\n{sql}\n{e}"))
}

// ─── D14: biometric consent ─────────────────────────────────────────────────

/// GDPR Art. 9 prohibits biometric processing by default. Naming a face cluster is
/// the moment a vector becomes identification, so it requires a live consent record.
#[tokio::test]
async fn naming_a_face_cluster_without_consent_is_refused() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(
        p,
        "INSERT INTO people (id) VALUES ('bbbbbbbb-0000-0000-0000-000000000001')",
    )
    .await;

    assert!(
        refused_by_constraint(
            p,
            "UPDATE people SET label='Jane Doe' WHERE id='bbbbbbbb-0000-0000-0000-000000000001'"
        )
        .await,
        "a cluster must not be nameable without a consent record"
    );

    // With consent it must work — a gate that refuses everything is not a gate.
    must_succeed(p, "INSERT INTO consent_records (id, person_id, subject_name, legal_basis, granted_at) \
        VALUES (gen_random_uuid(), 'bbbbbbbb-0000-0000-0000-000000000001', 'Jane Doe', 'explicit_consent', now())").await;
    must_succeed(
        p,
        "UPDATE people SET label='Jane Doe' WHERE id='bbbbbbbb-0000-0000-0000-000000000001'",
    )
    .await;

    // Withdrawal is unconditional under GDPR, so it must re-close the gate.
    must_succeed(p, "UPDATE consent_records SET withdrawn_at=now()").await;
    assert!(
        refused_by_constraint(
            p,
            "UPDATE people SET label='Jane D.' WHERE id='bbbbbbbb-0000-0000-0000-000000000001'"
        )
        .await,
        "withdrawn consent must re-close the gate"
    );
}

/// An expired consent record is as good as none.
#[tokio::test]
async fn expired_consent_does_not_permit_naming() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(
        p,
        "INSERT INTO people (id) VALUES ('bbbbbbbb-0000-0000-0000-000000000002')",
    )
    .await;
    must_succeed(p, "INSERT INTO consent_records (id, person_id, subject_name, legal_basis, granted_at, expires_at) \
        VALUES (gen_random_uuid(), 'bbbbbbbb-0000-0000-0000-000000000002', 'Old', 'explicit_consent', now() - interval '2 years', now() - interval '1 day')").await;
    assert!(
        refused_by_constraint(
            p,
            "UPDATE people SET label='Old' WHERE id='bbbbbbbb-0000-0000-0000-000000000002'"
        )
        .await
    );
}

/// A DPIA-requiring feature cannot be switched on without a reference and a recorded
/// legal basis — enforced in the database so a support engineer cannot flip it.
#[tokio::test]
async fn a_dpia_gated_feature_cannot_be_enabled_without_a_dpia() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(
        p,
        "INSERT INTO dam_global.tenants (id, slug, schema_name, display_name, storage_prefix) \
        VALUES ('aaaaaaaa-0000-0000-0000-000000000001', 'acme', 't_acme', 'Acme', 'acme/')",
    )
    .await;

    assert!(
        refused_by_constraint(
            p,
            "INSERT INTO dam_global.feature_flags (tenant_id, key, enabled, requires_dpia) \
            VALUES ('aaaaaaaa-0000-0000-0000-000000000001', 'face_identify', true, true)"
        )
        .await,
        "face_identify must not be enableable without a DPIA reference and legal basis"
    );

    // Off is always allowed — the default state must not require paperwork.
    must_succeed(
        p,
        "INSERT INTO dam_global.feature_flags (tenant_id, key, enabled, requires_dpia) \
        VALUES ('aaaaaaaa-0000-0000-0000-000000000001', 'face_identify', false, true)",
    )
    .await;
}

// ─── D12: rights ────────────────────────────────────────────────────────────

/// An unknown licence must not be assumed permissive. The cost of guessing wrong is
/// a rights claim, not a missing feature.
#[tokio::test]
async fn ai_training_and_generation_default_to_denied() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(
        p,
        "INSERT INTO licenses (id, name, license_type) VALUES (gen_random_uuid(), 'stock', 'rights_managed')",
    )
    .await;
    let permissive = scalar_i64(
        p,
        "SELECT count(*) FROM licenses WHERE ai_training_allowed OR ai_generation_allowed",
    )
    .await;
    assert_eq!(
        permissive, 0,
        "AI training/generation must default to denied"
    );

    // Enrichment defaults to allowed: it is internal cataloguing, not redistribution.
    assert_eq!(
        scalar_i64(
            p,
            "SELECT count(*) FROM licenses WHERE ai_processing_allowed"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn a_perpetual_licence_cannot_also_have_an_end_date() {
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO licenses (id, name, license_type, perpetual, ends_at) \
            VALUES (gen_random_uuid(), 'bad', 'royalty_free', true, now())"
        )
        .await,
        "perpetual + ends_at is contradictory and must be refused"
    );
}

/// A minor's release without guardian consent must not be recorded as valid — that
/// is the field a downstream rights evaluation trusts.
#[tokio::test]
async fn a_minors_release_without_guardian_consent_cannot_be_valid() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    assert!(
        refused_by_constraint(
            p,
            "INSERT INTO releases (id, kind, subject_is_minor, guardian_consent, status) \
            VALUES (gen_random_uuid(), 'model', true, false, 'valid')"
        )
        .await
    );
    // Recording it as missing is fine — that is how the gap gets tracked.
    must_succeed(
        p,
        "INSERT INTO releases (id, kind, subject_is_minor, guardian_consent, status) \
        VALUES (gen_random_uuid(), 'model', true, false, 'missing')",
    )
    .await;
}

// ─── D13/D15: provenance and AI disclosure ──────────────────────────────────

/// The G1 regression detector. A publicly-served derivative of an asset that
/// arrived with credentials, but carries no damrs-signed manifest, means the
/// pipeline stripped provenance.
#[tokio::test]
async fn the_provenance_gap_view_detects_a_stripped_credential_chain() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(p, "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, had_inbound_manifest) \
        VALUES ('cccccccc-0000-0000-0000-000000000001', 'h', 'shot.jpg', 'image/jpeg', 1, gen_random_uuid(), true)").await;
    must_succeed(p, "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
        VALUES ('dddddddd-0000-0000-0000-000000000001', 'cccccccc-0000-0000-0000-000000000001', 'rendition', 'web_1200', 'oh1', 'k1', 'image/jpeg', 1)").await;

    assert_eq!(
        scalar_i64(p, "SELECT count(*) FROM provenance_gaps").await,
        1,
        "a stripped credential chain must be visible"
    );

    must_succeed(p, "INSERT INTO provenance_manifests (id, derivative_id, role, object_key, bytes, validation_state) \
        VALUES (gen_random_uuid(), 'dddddddd-0000-0000-0000-000000000001', 'damrs_signed', 'm1', 100, 'valid')").await;
    assert_eq!(
        scalar_i64(p, "SELECT count(*) FROM provenance_gaps").await,
        0,
        "re-signing must clear the gap"
    );
}

/// EU AI Act Art. 50 has applied since 2 August 2026: synthetic or substantially
/// modified content must carry machine-readable marking. Unmarked rows are an
/// exposure and must be findable.
#[tokio::test]
async fn unmarked_synthetic_content_is_detectable() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(p, "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
        VALUES ('cccccccc-0000-0000-0000-000000000002', 'h2', 'gen.png', 'image/png', 1, gen_random_uuid())").await;
    must_succeed(p, "INSERT INTO ai_disclosures (id, asset_id, disclosure_kind, marked_in) \
        VALUES (gen_random_uuid(), 'cccccccc-0000-0000-0000-000000000002', 'fully_generated', '{}')").await;

    assert_eq!(
        scalar_i64(
            p,
            "SELECT count(*) FROM ai_disclosures \
            WHERE disclosure_kind IN ('fully_generated','substantially_modified') \
              AND cardinality(marked_in) = 0"
        )
        .await,
        1,
        "unmarked synthetic content must be detectable"
    );

    // Metadata-only AI involvement is not synthetic content and is not in scope.
    must_succeed(
        p,
        "INSERT INTO ai_disclosures (id, asset_id, disclosure_kind, marked_in) \
        VALUES (gen_random_uuid(), 'cccccccc-0000-0000-0000-000000000002', 'metadata_only', '{}')",
    )
    .await;
    assert_eq!(
        scalar_i64(
            p,
            "SELECT count(*) FROM ai_disclosures \
            WHERE disclosure_kind IN ('fully_generated','substantially_modified') \
              AND cardinality(marked_in) = 0"
        )
        .await,
        1,
        "metadata-only disclosure must not be flagged as unmarked synthetic content"
    );
}

// ─── D10 / G10: audit and retention ─────────────────────────────────────────

/// "Append-only by convention" is not what a security questionnaire means. The
/// database refuses mutation; the hash chain detects anyone who drops the rule.
#[tokio::test]
async fn the_audit_log_refuses_update_and_delete() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(
        p,
        "INSERT INTO audit_log (action, target_kind, hash) VALUES ('asset.deleted', 'asset', 'h1')",
    )
    .await;
    // The RULE makes these no-ops rather than errors, so assert on the effect.
    must_succeed(p, "UPDATE audit_log SET action='nothing.happened'").await;
    must_succeed(p, "DELETE FROM audit_log").await;

    assert_eq!(scalar_i64(p, "SELECT count(*) FROM audit_log").await, 1);
    let action: String = sqlx::query_scalar(sqlx::AssertSqlSafe(
        "SELECT action FROM audit_log".to_owned(),
    ))
    .fetch_one(p)
    .await
    .expect("read back");
    assert_eq!(action, "asset.deleted", "the row must be unchanged");
}

/// Legal hold always wins. A retention policy that could override it would let a
/// misconfigured rule purge material under litigation hold.
#[tokio::test]
async fn a_retention_policy_cannot_override_legal_hold() {
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO retention_policies (id, name, retain_days, overrides_legal_hold) \
            VALUES (gen_random_uuid(), 'aggressive', 1, true)"
        )
        .await
    );
}

// ─── storage invariants ─────────────────────────────────────────────────────

/// An archive-class pool registered as instant-latency would produce
/// "works locally, 403s in production" behaviour.
#[tokio::test]
async fn an_archive_class_pool_cannot_claim_instant_latency() {
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(&pool, "INSERT INTO dam_global.storage_pools (id, name, bucket, credentials_ref, storage_class, latency_class) \
            VALUES (gen_random_uuid(), 'bad', 'b', 'env:X', 'DEEP_ARCHIVE', 'instant')").await
    );
}

/// A restore creates a temporary copy with an expiry. Without one, the copy is
/// unreclaimable state and cache invalidation has nothing to key on.
#[tokio::test]
async fn an_available_restore_must_carry_an_expiry() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(p, "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
        VALUES ('11111111-1111-1111-1111-111111111111', 'abc', 'a.jpg', 'image/jpeg', 1, gen_random_uuid())").await;
    assert!(
        refused_by_constraint(p, "INSERT INTO object_placements (object_key, pool_id, asset_id, size_bytes, checksum, restore_state) \
            VALUES ('k2', gen_random_uuid(), '11111111-1111-1111-1111-111111111111', 1, 'c', 'available')").await
    );
}

/// One object belongs to an asset or a derivative, never both.
#[tokio::test]
async fn a_placement_cannot_own_both_an_asset_and_a_derivative() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(p, "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
        VALUES ('11111111-1111-1111-1111-111111111112', 'abc', 'a.jpg', 'image/jpeg', 1, gen_random_uuid())").await;
    must_succeed(p, "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
        VALUES ('22222222-2222-2222-2222-222222222222', '11111111-1111-1111-1111-111111111112', 'proxy', 'p', 'h', 'k', 'image/jpeg', 1)").await;
    assert!(
        refused_by_constraint(p, "INSERT INTO object_placements (object_key, pool_id, asset_id, derivative_id, size_bytes, checksum) \
            VALUES ('k', gen_random_uuid(), '11111111-1111-1111-1111-111111111112', '22222222-2222-2222-2222-222222222222', 1, 'c')").await
    );
}

/// Duplicate in-flight restores for one object must coalesce rather than pay twice.
#[tokio::test]
async fn duplicate_in_flight_restores_are_coalesced() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(
        p,
        "INSERT INTO restore_requests (id, object_key, pool_id, state) \
        VALUES (gen_random_uuid(), 'k3', '33333333-3333-3333-3333-333333333333', 'requested')",
    )
    .await;
    assert!(
        refused_by_constraint(
            p,
            "INSERT INTO restore_requests (id, object_key, pool_id, state) \
            VALUES (gen_random_uuid(), 'k3', '33333333-3333-3333-3333-333333333333', 'queued')"
        )
        .await
    );
}

/// Exactly one current version per version group, or "give me the current version"
/// becomes ambiguous.
#[tokio::test]
async fn a_version_group_can_have_only_one_current_version() {
    let (_pg, pool) = tenant_db().await;
    let p = &pool;
    must_succeed(p, "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, version_no) \
        VALUES (gen_random_uuid(), 'abc', 'a.jpg', 'image/jpeg', 1, '99999999-9999-9999-9999-999999999999', 1)").await;
    assert!(
        refused_by_constraint(p, "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, version_no) \
            VALUES (gen_random_uuid(), 'abc', 'a.jpg', 'image/jpeg', 1, '99999999-9999-9999-9999-999999999999', 2)").await,
        "two is_current versions in one group must be refused"
    );
}

/// The tenant slug shape is enforced in the database, not only in Rust — it gates
/// schema-name construction, so a caller that skips validation must still be refused.
#[tokio::test]
async fn an_injection_shaped_tenant_slug_is_refused() {
    let (_pg, pool) = tenant_db().await;
    assert!(
        refused_by_constraint(
            &pool,
            "INSERT INTO dam_global.tenants (id, slug, schema_name, display_name, storage_prefix) \
            VALUES (gen_random_uuid(), 'Bad-Slug; DROP', 't_acme', 'x', 'x/')"
        )
        .await
    );
}
