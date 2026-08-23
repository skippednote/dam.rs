//! The tamper-evident governance record (G10).
//!
//! `audit_log` has carried `prev_hash`, `hash`, and two rules refusing UPDATE and DELETE since migration
//! 0007, and nothing has ever written to it. What these cases defend:
//!
//! **The alarm actually goes off.** An altered row, a removed row, and an appended forgery each have to be
//! detected, and detected as *different things*: "this row was edited" and "a row is missing between these
//! two" send an investigator to different places.
//!
//! **The rules are a fence, not the proof.** UPDATE and DELETE are refused in the database, so the realistic
//! attack from an application-level compromise is an *append* — a plausible extra entry. That one needs no
//! DDL rights, so it is the one that must not work. The cases that alter and remove rows disable the rules
//! first, deliberately: that is what an attacker with DDL does, and the chain is what remains.
//!
//! **A gap is not a break.** `nextval` is non-transactional, so every rolled-back governance action leaves a
//! hole in `seq`. A verifier that counted numbers would report routine failures as deleted evidence, which
//! trains everybody to ignore it.
//!
//! **Concurrent writers do not fork the chain.** Two entries claiming the same predecessor is a failure that
//! is silent when it happens and unrepairable when it is found.
//!
//! **What is hashed is what is stored.** `jsonb` rewrites numbers, so a payload hashed as submitted would
//! make a row read as tampered from the moment it was written.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::audit::{self, Action, ActorKind, Break, NewEntry};
use dam_db::{migrate, testing::PostgresHarness};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

fn actor() -> Uuid {
    Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("a fixed uuid")
}

async fn write(pool: &PgPool, action: Action, target: &str) -> audit::Entry {
    let mut conn = pool.acquire().await.expect("conn");
    audit::record(
        &mut conn,
        NewEntry::by(action, actor(), "asset")
            .on(target)
            .with(json!({ "matter": "2026-114" })),
    )
    .await
    .expect("record")
}

#[tokio::test]
async fn the_first_entry_opens_the_chain_and_the_rest_link_to_it() {
    let (_pg, pool) = db().await;

    let first = write(&pool, Action::LegalHoldPlaced, "a").await;
    assert_eq!(first.prev_hash, None, "the genesis entry links to nothing");

    let second = write(&pool, Action::LegalHoldLifted, "a").await;
    assert_eq!(second.prev_hash.as_deref(), Some(first.hash.as_str()));

    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    assert!(result.is_intact(), "{result:?}");
    assert_eq!(result.checked, 2);
}

#[tokio::test]
async fn the_database_refuses_to_update_or_delete_an_entry() {
    let (_pg, pool) = db().await;
    let entry = write(&pool, Action::LegalHoldPlaced, "a").await;

    // Both statements report success and do nothing: the rules are DO INSTEAD NOTHING, not errors.
    sqlx::query("UPDATE audit_log SET action = 'nothing.happened'")
        .execute(&pool)
        .await
        .expect("the update is accepted and ignored");
    sqlx::query("DELETE FROM audit_log")
        .execute(&pool)
        .await
        .expect("the delete is accepted and ignored");

    let (count, action): (i64, String) =
        sqlx::query_as("SELECT count(*), max(action) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("read back");
    assert_eq!(count, 1);
    assert_eq!(action, entry.action);
}

#[tokio::test]
async fn an_appended_forgery_is_caught_without_anyone_needing_ddl_rights() {
    // The attack the rules leave open: INSERT is how the writer works, so it stays permitted. An attacker
    // with the application's credentials can add an entry — and cannot make it hash.
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;

    sqlx::query(
        "INSERT INTO audit_log (actor_kind, action, target_kind, target_id, payload, prev_hash, hash) \
         VALUES ('user', 'legal_hold.lifted', 'asset', 'a', '{}'::jsonb, \
                 (SELECT hash FROM audit_log ORDER BY seq DESC LIMIT 1), \
                 repeat('f', 64))",
    )
    .execute(&pool)
    .await
    .expect("the insert succeeds — that is the point");

    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    match result.first_break {
        Some(Break::Altered { seq, .. }) => assert_eq!(seq, 2),
        other => panic!("expected the forged row to be reported as altered, got {other:?}"),
    }
}

#[tokio::test]
async fn editing_a_row_with_the_rule_disabled_is_still_reported() {
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;
    write(&pool, Action::LegalHoldLifted, "a").await;

    // What a superuser can do, which is the honest limit of in-database append-only.
    sqlx::query("ALTER TABLE audit_log DISABLE RULE audit_log_no_update")
        .execute(&pool)
        .await
        .expect("disable rule");
    sqlx::query(
        "UPDATE audit_log SET payload = '{\"matter\": \"something else\"}'::jsonb WHERE seq = 1",
    )
    .execute(&pool)
    .await
    .expect("tamper");

    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    match result.first_break {
        Some(Break::Altered {
            seq,
            stored,
            recomputed,
        }) => {
            assert_eq!(seq, 1);
            assert_ne!(stored, recomputed);
        }
        other => panic!("expected an alteration at seq 1, got {other:?}"),
    }
    assert_eq!(result.checked, 1, "the walk stops at the first break");
}

#[tokio::test]
async fn removing_a_row_is_reported_as_a_broken_link_rather_than_an_edit() {
    // The distinction matters to whoever reads the report: "edited" points at a row, "unlinked" points at a
    // gap, and a gap is where the missing evidence was.
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;
    write(&pool, Action::LegalHoldLifted, "a").await;
    write(&pool, Action::RetentionChanged, "a").await;

    sqlx::query("ALTER TABLE audit_log DISABLE RULE audit_log_no_delete")
        .execute(&pool)
        .await
        .expect("disable rule");
    sqlx::query("DELETE FROM audit_log WHERE seq = 2")
        .execute(&pool)
        .await
        .expect("remove the middle");

    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    match result.first_break {
        Some(Break::Unlinked {
            seq,
            claimed_prev,
            actual_prev,
        }) => {
            assert_eq!(seq, 3);
            assert_ne!(claimed_prev, actual_prev);
        }
        other => panic!("expected a broken link at seq 3, got {other:?}"),
    }
}

#[tokio::test]
async fn a_gap_in_the_sequence_is_not_evidence_of_tampering() {
    // A rolled-back governance action consumes a sequence number and leaves a hole. That is the normal case,
    // not an incident.
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;

    let burned: i64 =
        sqlx::query_scalar("SELECT nextval(pg_get_serial_sequence('audit_log', 'seq'))")
            .fetch_one(&pool)
            .await
            .expect("burn a number");
    assert_eq!(burned, 2);

    let after = write(&pool, Action::LegalHoldLifted, "a").await;
    assert_eq!(after.seq, 3, "the hole stays a hole");

    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    assert!(result.is_intact(), "{result:?}");
}

#[tokio::test]
async fn concurrent_writers_do_not_fork_the_chain() {
    let (_pg, pool) = db().await;

    let mut writers = Vec::new();
    for index in 0..8 {
        let pool = pool.clone();
        writers.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.expect("conn");
            audit::record(
                &mut conn,
                NewEntry::by(Action::RoleGranted, actor(), "identity")
                    .on(format!("subject-{index}"))
                    .with(json!({ "role": "editor" })),
            )
            .await
            .expect("record")
        }));
    }
    let mut written = Vec::new();
    for writer in writers {
        written.push(writer.await.expect("join"));
    }

    // No two entries may name the same predecessor. Without the lock this is where the fork appears, and it
    // appears only sometimes — which is why the assertion is on the data rather than on a timing.
    let mut claimed: Vec<Option<String>> = written.iter().map(|e| e.prev_hash.clone()).collect();
    claimed.sort();
    let before = claimed.len();
    claimed.dedup();
    assert_eq!(
        before,
        claimed.len(),
        "two entries claimed the same predecessor"
    );

    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    assert!(result.is_intact(), "{result:?}");
    assert_eq!(result.checked, 8);
}

#[tokio::test]
async fn a_payload_is_hashed_as_the_database_will_store_it() {
    // `jsonb` is a normalising type, and negative zero is the case that proves it: serde_json renders the
    // f64 as `-0.0`, jsonb stores it as numeric and reads it back as `0.0`. Hash the submitted value and this
    // row is unverifiable from the instant it was written — tamper evidence for an entry nobody touched.
    let (_pg, pool) = db().await;
    let submitted = json!({ "delta": -0.0, "ratio": 0.5 });
    assert_eq!(
        dam_core::audit::canonical_json(&submitted),
        r#"{"delta":-0.0,"ratio":0.5}"#,
        "the submitted form"
    );

    let mut conn = pool.acquire().await.expect("conn");
    let entry = audit::record(
        &mut conn,
        NewEntry::by_system(Action::RetentionChanged, "policy")
            .on("default")
            .with(submitted),
    )
    .await
    .expect("record");
    assert_eq!(
        dam_core::audit::canonical_json(&entry.payload),
        r#"{"delta":0.0,"ratio":0.5}"#,
        "the stored form, which is what the digest has to cover"
    );

    let result = audit::verify(&mut conn, 0).await.expect("verify");
    assert!(result.is_intact(), "{result:?}");
}

#[tokio::test]
async fn verifying_from_the_middle_still_checks_the_link_into_it() {
    // Otherwise a caller could hide a break by choosing where to start.
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;
    write(&pool, Action::LegalHoldLifted, "a").await;
    write(&pool, Action::RetentionChanged, "a").await;

    sqlx::query("ALTER TABLE audit_log DISABLE RULE audit_log_no_delete")
        .execute(&pool)
        .await
        .expect("disable rule");
    sqlx::query("DELETE FROM audit_log WHERE seq = 2")
        .execute(&pool)
        .await
        .expect("remove the middle");

    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 3).await.expect("verify");
    assert_eq!(result.first_break.as_ref().map(Break::seq), Some(3));
}

#[tokio::test]
async fn an_empty_log_verifies_rather_than_erroring() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    assert!(result.is_intact());
    assert_eq!(result.checked, 0);
    assert_eq!(result.through_seq, None);
}

#[tokio::test]
async fn an_export_records_that_it_happened_and_does_not_contain_that_record() {
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;
    write(&pool, Action::LegalHoldLifted, "b").await;

    let mut conn = pool.acquire().await.expect("conn");
    let extract = audit::export(&mut conn, 0, 100, Some(actor()), ActorKind::User)
        .await
        .expect("export");

    assert_eq!(extract.entries.len(), 2);
    assert_eq!(
        extract.entries[0].seq, 1,
        "oldest first, so it can be walked"
    );
    assert_eq!(
        extract.anchor, None,
        "an export from the start anchors to nothing"
    );
    assert_eq!(extract.recorded_as.action, "audit.exported");
    assert!(
        !extract
            .entries
            .iter()
            .any(|e| e.seq == extract.recorded_as.seq),
        "an extract cannot contain the record of its own creation"
    );
    assert_eq!(extract.recorded_as.payload["through_seq"], json!(2));
    assert_eq!(extract.recorded_as.payload["truncated"], json!(false));

    // And the chain is still intact afterwards — the export appended, it did not reorder.
    let result = audit::verify(&mut conn, 0).await.expect("verify");
    assert!(result.is_intact(), "{result:?}");
}

#[tokio::test]
async fn an_export_from_the_middle_carries_the_hash_its_first_entry_links_back_to() {
    // Without the anchor, an extract's first entry names a predecessor the reader does not have, and a
    // verifier cannot tell a legitimate window from a chain that starts with a forgery.
    let (_pg, pool) = db().await;
    let first = write(&pool, Action::LegalHoldPlaced, "a").await;
    write(&pool, Action::LegalHoldLifted, "b").await;

    let mut conn = pool.acquire().await.expect("conn");
    let extract = audit::export(&mut conn, 2, 100, None, ActorKind::System)
        .await
        .expect("export");
    assert_eq!(extract.anchor.as_deref(), Some(first.hash.as_str()));
    assert_eq!(extract.entries.first().map(|e| e.seq), Some(2));
    assert_eq!(extract.entries[0].prev_hash, extract.anchor);
}

#[tokio::test]
async fn an_export_that_hit_its_limit_says_so() {
    let (_pg, pool) = db().await;
    for _ in 0..3 {
        write(&pool, Action::LegalHoldPlaced, "a").await;
    }
    let mut conn = pool.acquire().await.expect("conn");
    let extract = audit::export(&mut conn, 0, 2, None, ActorKind::Support)
        .await
        .expect("export");
    assert_eq!(extract.entries.len(), 2);
    assert_eq!(extract.recorded_as.payload["truncated"], json!(true));
    assert_eq!(extract.recorded_as.actor_kind, "support");
    assert_eq!(extract.recorded_as.actor_id, None);
}

#[tokio::test]
async fn a_page_filters_and_pages_backwards_from_a_cursor() {
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;
    write(&pool, Action::RoleGranted, "b").await;
    write(&pool, Action::LegalHoldPlaced, "c").await;

    let mut conn = pool.acquire().await.expect("conn");

    let all = audit::page(&mut conn, &audit::Filter::default(), 10)
        .await
        .expect("page");
    assert_eq!(
        all.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 2, 1],
        "newest first"
    );

    let held = audit::page(
        &mut conn,
        &audit::Filter {
            action: Some("legal_hold.placed".to_owned()),
            ..audit::Filter::default()
        },
        10,
    )
    .await
    .expect("page");
    assert_eq!(held.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 1]);

    let next = audit::page(
        &mut conn,
        &audit::Filter {
            before_seq: Some(3),
            ..audit::Filter::default()
        },
        1,
    )
    .await
    .expect("page");
    assert_eq!(next.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2]);

    let by_target = audit::page(
        &mut conn,
        &audit::Filter {
            target_kind: Some("asset".to_owned()),
            target_id: Some("b".to_owned()),
            ..audit::Filter::default()
        },
        10,
    )
    .await
    .expect("page");
    assert_eq!(by_target.len(), 1);
    assert_eq!(by_target[0].action, "role.granted");
}

#[tokio::test]
async fn an_action_string_this_code_does_not_know_is_read_back_as_itself() {
    // The column is deliberately free text so a later subsystem can record something without a migration.
    // Surfacing an unknown value as itself is what makes that safe; guessing at it would not be.
    let (_pg, pool) = db().await;
    write(&pool, Action::LegalHoldPlaced, "a").await;
    sqlx::query(
        "INSERT INTO audit_log (actor_kind, action, target_kind, payload, prev_hash, hash) \
         VALUES ('system', 'something.new', 'tenant', '{}'::jsonb, NULL, 'x')",
    )
    .execute(&pool)
    .await
    .expect("insert");

    let mut conn = pool.acquire().await.expect("conn");
    let rows = audit::page(&mut conn, &audit::Filter::default(), 10)
        .await
        .expect("page");
    assert_eq!(rows[0].action, "something.new");
    assert_eq!(ActorKind::parse("something"), None);
    assert_eq!(ActorKind::parse("support"), Some(ActorKind::Support));
}
