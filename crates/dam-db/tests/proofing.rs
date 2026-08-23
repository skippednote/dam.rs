//! Proofing rounds (M6b).
//!
//! Four properties carry this, and each is a decision that could have gone the other way:
//!
//! **The outcome is derived, and `changes_requested` beats any number of approvals.** Three people approving
//! and one asking for changes is a round with changes to make. A majority rule would be inventing governance
//! nobody asked for, and would quietly overrule the person who found the problem.
//!
//! **The asset set is snapshotted and never widened.** 0025's argument about orders, unchanged: an approver who
//! agreed to forty photographs must not find they agreed to four hundred. There is no "add assets to an open
//! round", and a round that needs a bigger scope is a new round.
//!
//! **A round is visible only when every one of its assets is.** Showing somebody a partly-visible round would
//! tell them a larger set exists than they can read, and invite them to approve assets they have never seen.
//!
//! **A second pass is a new row.** Mutating a closed round would erase who approved what and when, which is the
//! one thing a review has to be able to answer afterwards.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, Utc};
use dam_core::policy::{self, AccessPredicate, Action, Grant, Grants};
use dam_db::proofing::{self, NewRound, Outcome, ProofRefusal, Verdict};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn access(groups: Option<&[Uuid]>) -> AccessPredicate {
    let (ids, all) = match groups {
        Some(ids) => (ids.to_vec(), false),
        None => (vec![], true),
    };
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned(), "asset:manage".to_owned()],
            asset_group_ids: ids,
            all_asset_groups: all,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Read,
        Utc::now(),
    )
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn held(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("acquire")
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

// ─── the derived outcome ────────────────────────────────────────────────────

fn the_outcome_rule_is_one_function() {
    // Pure, so the rule is asserted without a database — and so the query and the tests cannot disagree about
    // it, because they call the same function.
    assert_eq!(proofing::decide_outcome(false, 2, 0), Outcome::Open);
    assert_eq!(proofing::decide_outcome(false, 0, 0), Outcome::Approved);
    // The one that matters: changes beat approvals, however many.
    assert_eq!(
        proofing::decide_outcome(false, 0, 1),
        Outcome::ChangesRequested
    );
    assert_eq!(
        proofing::decide_outcome(false, 5, 1),
        Outcome::ChangesRequested
    );
    // And a cancellation beats everything, because it is a decision rather than a tally.
    assert_eq!(proofing::decide_outcome(true, 0, 0), Outcome::Cancelled);
    assert_eq!(proofing::decide_outcome(true, 3, 2), Outcome::Cancelled);

    assert!(!Outcome::Open.is_closed());
    for closed in [
        Outcome::Approved,
        Outcome::ChangesRequested,
        Outcome::Cancelled,
    ] {
        assert!(closed.is_closed(), "{closed:?}");
    }
}

async fn a_round_closes_when_the_last_reviewer_agrees(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let photo = asset(&fresh, "photo").await;
    let (ada, bob) = (Uuid::new_v4(), Uuid::new_v4());
    let round = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Spring campaign",
            brief: "check the crops",
            asset_ids: &[photo],
            reviewer_ids: &[ada, bob],
            due_at: Some(Utc::now() + Duration::days(3)),
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await
    .expect("open");

    let read = proofing::read(&mut *held(&fresh).await, round, &access(None))
        .await
        .expect("read");
    assert_eq!(read.outcome, Outcome::Open);
    assert_eq!(read.number, 1);
    assert_eq!(read.asset_count, 1);
    assert_eq!(read.reviewers.len(), 2);
    assert!(read.reviewers.iter().all(|r| r.verdict == Verdict::Pending));
    assert!(read.closed_at.is_none());

    // One approval leaves it open, because somebody has not answered.
    let outcome = proofing::decide(
        &mut *held(&fresh).await,
        round,
        ada,
        Verdict::Approved,
        "looks right",
    )
    .await
    .expect("decide");
    assert_eq!(outcome, Outcome::Open);
    assert!(
        proofing::read(&mut *held(&fresh).await, round, &access(None))
            .await
            .expect("read")
            .closed_at
            .is_none()
    );

    // The second closes it, in the same transaction as the verdict — not by a later sweep, which would leave a
    // window where everybody has approved and the round does not say so.
    let outcome = proofing::decide(&mut *held(&fresh).await, round, bob, Verdict::Approved, "")
        .await
        .expect("decide");
    assert_eq!(outcome, Outcome::Approved);
    let closed = proofing::read(&mut *held(&fresh).await, round, &access(None))
        .await
        .expect("read");
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.outcome, Outcome::Approved);
    let _ = pool;
}

async fn one_reviewer_asking_for_changes_closes_it_immediately(pool: &PgPool) {
    // No point asking the rest: there are changes to make. And the round closes without their verdicts, which
    // is the honest state — "we already know this needs work".
    let (_pg, fresh) = db().await;
    let photo = asset(&fresh, "reject").await;
    let (ada, bob, cara) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let round = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Round 1",
            brief: "",
            asset_ids: &[photo],
            reviewer_ids: &[ada, bob, cara],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await
    .expect("open");

    proofing::decide(&mut *held(&fresh).await, round, ada, Verdict::Approved, "")
        .await
        .expect("approve");
    let outcome = proofing::decide(
        &mut *held(&fresh).await,
        round,
        bob,
        Verdict::ChangesRequested,
        "the logo is the old one",
    )
    .await
    .expect("reject");
    assert_eq!(outcome, Outcome::ChangesRequested);

    // And cara cannot now vote, because the round is over. A verdict arriving after the close would change a
    // recorded outcome, which is the thing an audit trail must not allow.
    let refused =
        proofing::decide(&mut *held(&fresh).await, round, cara, Verdict::Approved, "").await;
    assert!(
        matches!(refused, Err(ProofRefusal::AlreadyClosed)),
        "{refused:?}"
    );

    // Bob's reason survives on the record.
    let read = proofing::read(&mut *held(&fresh).await, round, &access(None))
        .await
        .expect("read");
    let bobs = read
        .reviewers
        .iter()
        .find(|r| r.identity_id == bob)
        .expect("bob");
    assert_eq!(bobs.note, "the logo is the old one");
    assert!(bobs.decided_at.is_some());
    // And cara is still pending rather than silently marked as anything.
    let caras = read
        .reviewers
        .iter()
        .find(|r| r.identity_id == cara)
        .expect("cara");
    assert_eq!(caras.verdict, Verdict::Pending);
    assert!(caras.decided_at.is_none());
    let _ = pool;
}

// ─── the snapshot and its scope ─────────────────────────────────────────────

async fn a_round_over_assets_you_cannot_see_is_refused_not_narrowed(pool: &PgPool) {
    // Narrowing silently would be worse than refusing: a reviewer would approve a set the requester did not
    // choose, and neither would know the two differed.
    let (_pg, fresh) = db().await;
    let mine = asset(&fresh, "mine").await;
    let theirs = asset(&fresh, "theirs").await;
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&fresh)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(mine)
        .execute(&fresh)
        .await
        .expect("member");

    let scoped = access(Some(&[group]));
    let refused = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Mixed",
            brief: "",
            asset_ids: &[mine, theirs],
            reviewer_ids: &[Uuid::new_v4()],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &scoped,
    )
    .await;
    match refused {
        Err(ProofRefusal::AssetsOutOfScope(count)) => assert_eq!(count, 1),
        other => panic!("a partly-visible set must be refused, got {other:?}"),
    }

    // Nothing was written — the check runs before the insert, so the caller's transaction is untouched.
    let rounds: i64 = sqlx::query_scalar("SELECT count(*) FROM proof_rounds")
        .fetch_one(&fresh)
        .await
        .expect("count");
    assert_eq!(rounds, 0);
    let _ = pool;
}

async fn a_partly_visible_round_is_invisible(pool: &PgPool) {
    // The other direction. An admin opens a round over both assets; a scoped reviewer must not see it at all —
    // seeing it would tell them a larger set exists, and would invite them to approve pictures they cannot open.
    let (_pg, fresh) = db().await;
    let mine = asset(&fresh, "visible").await;
    let theirs = asset(&fresh, "hidden").await;
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'some', 'Some') RETURNING id",
    )
    .fetch_one(&fresh)
    .await
    .expect("group");
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(mine)
        .execute(&fresh)
        .await
        .expect("member");

    let reviewer = Uuid::new_v4();
    let round = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Both",
            brief: "",
            asset_ids: &[mine, theirs],
            reviewer_ids: &[reviewer],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await
    .expect("open");

    let scoped = access(Some(&[group]));
    assert!(matches!(
        proofing::read(&mut *held(&fresh).await, round, &scoped).await,
        Err(ProofRefusal::UnknownRound(_))
    ));
    assert!(
        proofing::list(&mut *held(&fresh).await, &scoped, 50)
            .await
            .expect("list")
            .is_empty()
    );
    // Even though they are a reviewer on it, which is the tempting case to allow.
    assert!(
        proofing::waiting_on(&mut *held(&fresh).await, reviewer, &scoped)
            .await
            .expect("waiting")
            .is_empty(),
        "being asked to review a set does not grant sight of it"
    );
    // The admin sees it.
    assert_eq!(
        proofing::list(&mut *held(&fresh).await, &access(None), 50)
            .await
            .expect("list")
            .len(),
        1
    );
    let _ = pool;
}

async fn a_round_shrinks_when_an_asset_is_deleted(pool: &PgPool) {
    // Cascade, following 0025. What the round was for is still a fact, and a round whose assets have all gone
    // is visibly empty rather than dangling.
    let (_pg, fresh) = db().await;
    let one = asset(&fresh, "kept").await;
    let two = asset(&fresh, "removed").await;
    let round = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Two",
            brief: "",
            asset_ids: &[one, two],
            reviewer_ids: &[Uuid::new_v4()],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await
    .expect("open");
    assert_eq!(
        proofing::read(&mut *held(&fresh).await, round, &access(None))
            .await
            .expect("read")
            .asset_count,
        2
    );

    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(two)
        .execute(&fresh)
        .await
        .expect("delete");
    assert_eq!(
        proofing::read(&mut *held(&fresh).await, round, &access(None))
            .await
            .expect("read")
            .asset_count,
        1
    );

    // And when the last one goes, the round is unreadable rather than an empty shell — the same refusal as a
    // round that never existed, because distinguishing them would confirm it did.
    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(one)
        .execute(&fresh)
        .await
        .expect("delete");
    assert!(matches!(
        proofing::read(&mut *held(&fresh).await, round, &access(None)).await,
        Err(ProofRefusal::UnknownRound(_))
    ));
    let _ = pool;
}

// ─── rounds follow rounds ───────────────────────────────────────────────────

async fn a_second_pass_is_a_new_round_that_points_at_the_first(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let photo = asset(&fresh, "iterated").await;
    let reviewer = Uuid::new_v4();
    let first = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Crops",
            brief: "",
            asset_ids: &[photo],
            reviewer_ids: &[reviewer],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await
    .expect("open");
    proofing::decide(
        &mut *held(&fresh).await,
        first,
        reviewer,
        Verdict::ChangesRequested,
        "tighter",
    )
    .await
    .expect("reject");

    let second = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Crops",
            brief: "tightened as asked",
            asset_ids: &[photo],
            reviewer_ids: &[reviewer],
            due_at: None,
            requested_by: None,
            supersedes: Some(first),
        },
        &access(None),
    )
    .await
    .expect("open second");

    let read = proofing::read(&mut *held(&fresh).await, second, &access(None))
        .await
        .expect("read");
    assert_eq!(
        read.number, 2,
        "the number is denormalised so a screen need not walk the chain"
    );
    assert_eq!(read.supersedes, Some(first));
    assert_eq!(read.outcome, Outcome::Open);

    // The first round is untouched: its verdict, its note and its closing time are the record of what happened.
    let original = proofing::read(&mut *held(&fresh).await, first, &access(None))
        .await
        .expect("read");
    assert_eq!(original.outcome, Outcome::ChangesRequested);
    assert_eq!(original.number, 1);
    assert!(original.closed_at.is_some());
    assert_eq!(original.reviewers[0].note, "tighter");

    // A third follows the second.
    let third = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Crops",
            brief: "",
            asset_ids: &[photo],
            reviewer_ids: &[reviewer],
            due_at: None,
            requested_by: None,
            supersedes: Some(second),
        },
        &access(None),
    )
    .await
    .expect("open third");
    assert_eq!(
        proofing::read(&mut *held(&fresh).await, third, &access(None))
            .await
            .expect("read")
            .number,
        3
    );
    let _ = pool;
}

async fn cancelling_keeps_the_verdicts_already_given(pool: &PgPool) {
    // A cancelled round is part of the record of what was asked and what came back. Deleting the answers would
    // make "why was this cancelled" unanswerable.
    let (_pg, fresh) = db().await;
    let photo = asset(&fresh, "withdrawn").await;
    let (ada, bob) = (Uuid::new_v4(), Uuid::new_v4());
    let round = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Withdrawn",
            brief: "",
            asset_ids: &[photo],
            reviewer_ids: &[ada, bob],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await
    .expect("open");
    proofing::decide(
        &mut *held(&fresh).await,
        round,
        ada,
        Verdict::Approved,
        "fine by me",
    )
    .await
    .expect("approve");

    assert!(
        proofing::cancel(&mut *held(&fresh).await, round, Some(ada))
            .await
            .expect("cancel")
    );

    let read = proofing::read(&mut *held(&fresh).await, round, &access(None))
        .await
        .expect("read");
    assert_eq!(
        read.outcome,
        Outcome::Cancelled,
        "a cancellation beats a tally"
    );
    assert!(read.closed_at.is_some(), "cancelled implies closed");
    assert_eq!(
        read.reviewers
            .iter()
            .find(|r| r.identity_id == ada)
            .expect("ada")
            .note,
        "fine by me"
    );

    // Twice is false rather than a second cancellation, so the recorded moment is the first one.
    assert!(
        !proofing::cancel(&mut *held(&fresh).await, round, Some(bob))
            .await
            .expect("cancel")
    );
    // And no further verdicts.
    assert!(matches!(
        proofing::decide(&mut *held(&fresh).await, round, bob, Verdict::Approved, "").await,
        Err(ProofRefusal::AlreadyClosed)
    ));
    let _ = pool;
}

// ─── who is being asked ─────────────────────────────────────────────────────

async fn a_round_needs_assets_and_reviewers(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let photo = asset(&fresh, "lonely").await;

    let no_assets = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Nothing",
            brief: "",
            asset_ids: &[],
            reviewer_ids: &[Uuid::new_v4()],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await;
    assert!(matches!(no_assets, Err(ProofRefusal::NoAssets)));

    let no_reviewers = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Nobody",
            brief: "",
            asset_ids: &[photo],
            reviewer_ids: &[],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await;
    assert!(matches!(no_reviewers, Err(ProofRefusal::NoReviewers)));
    let _ = pool;
}

async fn only_a_named_reviewer_may_decide(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let photo = asset(&fresh, "guarded").await;
    let reviewer = Uuid::new_v4();
    let round = proofing::open(
        &mut *held(&fresh).await,
        &NewRound {
            title: "Guarded",
            brief: "",
            asset_ids: &[photo],
            reviewer_ids: &[reviewer],
            due_at: None,
            requested_by: None,
            supersedes: None,
        },
        &access(None),
    )
    .await
    .expect("open");

    let stranger = Uuid::new_v4();
    let refused = proofing::decide(
        &mut *held(&fresh).await,
        round,
        stranger,
        Verdict::Approved,
        "",
    )
    .await;
    assert!(
        matches!(refused, Err(ProofRefusal::NotAReviewer(_))),
        "{refused:?}"
    );
    // And the round is still open, so an outsider cannot close it by accident either.
    assert_eq!(
        proofing::read(&mut *held(&fresh).await, round, &access(None))
            .await
            .expect("read")
            .outcome,
        Outcome::Open
    );
    let _ = pool;
}

fn pending_is_not_a_verdict_somebody_can_give() {
    // A starting state, not an answer. Accepting it would let a reviewer un-decide, which would move a
    // recorded outcome backwards.
    assert_eq!(Verdict::parse("pending"), Some(Verdict::Pending));
    assert_eq!(Verdict::parse_decision("pending"), None);
    assert_eq!(Verdict::parse_decision("approved"), Some(Verdict::Approved));
    assert_eq!(
        Verdict::parse_decision("changes_requested"),
        Some(Verdict::ChangesRequested)
    );
    assert_eq!(Verdict::parse_decision("maybe"), None);
}

async fn waiting_on_puts_the_dated_ones_first(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let photo = asset(&fresh, "queued").await;
    let reviewer = Uuid::new_v4();
    for (title, due) in [
        ("no deadline", None),
        ("next week", Some(Utc::now() + Duration::days(7))),
        ("tomorrow", Some(Utc::now() + Duration::days(1))),
    ] {
        proofing::open(
            &mut *held(&fresh).await,
            &NewRound {
                title,
                brief: "",
                asset_ids: &[photo],
                reviewer_ids: &[reviewer],
                due_at: due,
                requested_by: None,
                supersedes: None,
            },
            &access(None),
        )
        .await
        .expect("open");
    }

    let waiting = proofing::waiting_on(&mut *held(&fresh).await, reviewer, &access(None))
        .await
        .expect("waiting");
    let titles: Vec<&str> = waiting.iter().map(|r| r.title.as_str()).collect();
    // A review with a deadline is the one that matters today; the undated one goes last rather than first.
    assert_eq!(titles, vec!["tomorrow", "next week", "no deadline"]);

    // A decided round leaves the list, even though it is still open for others.
    proofing::decide(
        &mut *held(&fresh).await,
        waiting[0].id,
        reviewer,
        Verdict::Approved,
        "",
    )
    .await
    .expect("approve");
    let after = proofing::waiting_on(&mut *held(&fresh).await, reviewer, &access(None))
        .await
        .expect("waiting");
    assert_eq!(after.len(), 2);
    let _ = pool;
}

#[tokio::test]
async fn a_round_records_what_people_agreed() {
    let (_pg, pool) = db().await;

    the_outcome_rule_is_one_function();
    pending_is_not_a_verdict_somebody_can_give();

    a_round_closes_when_the_last_reviewer_agrees(&pool).await;
    one_reviewer_asking_for_changes_closes_it_immediately(&pool).await;

    a_round_over_assets_you_cannot_see_is_refused_not_narrowed(&pool).await;
    a_partly_visible_round_is_invisible(&pool).await;
    a_round_shrinks_when_an_asset_is_deleted(&pool).await;

    a_second_pass_is_a_new_round_that_points_at_the_first(&pool).await;
    cancelling_keeps_the_verdicts_already_given(&pool).await;

    a_round_needs_assets_and_reviewers(&pool).await;
    only_a_named_reviewer_may_decide(&pool).await;
    waiting_on_puts_the_dated_ones_first(&pool).await;
}
