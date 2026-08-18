//! The relevance judgement corpus (2.9, G8).
//!
//! `dam_core::eval` has its own unit tests for the arithmetic. This suite covers the part that can only be wrong
//! against a real database: that the corpus a run scores against is the one a reviewer actually recorded, and that
//! a query nobody has judged comes back as *unjudged* rather than as an empty set which would score perfectly.
//!
//! The last case runs the whole loop — record judgements, score two rankings — because the point of the harness is
//! that a ranking change reports its effect instead of being argued about, and that only holds if a worse ranking
//! actually scores lower.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::eval;
use dam_db::{judgements, migrate, testing::PostgresHarness};
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

async fn asset(pool: &PgPool, filename: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(filename.as_bytes()).to_hex().to_string())
    .bind(filename)
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn a_recorded_judgement_round_trips(pool: &PgPool) {
    let hero = asset(pool, "round-trip-hero.jpg").await;
    let vaguely = asset(pool, "round-trip-vaguely.jpg").await;
    judgements::record(pool, "round trip", hero, 3, None)
        .await
        .expect("record");
    judgements::record(pool, "round trip", vaguely, 1, None)
        .await
        .expect("record");

    let judged = judgements::for_query(pool, "round trip")
        .await
        .expect("load")
        .expect("judged");
    assert_eq!(judged.query_text, "round trip");
    assert_eq!(judged.grades.get(&hero), Some(&3));
    assert_eq!(
        judged.grades.get(&vaguely),
        Some(&1),
        "grades are graded, not binary — the distinction is the whole reason for nDCG"
    );
}

async fn an_unjudged_query_is_none_and_therefore_unscoreable(pool: &PgPool) {
    // The load-bearing case. An empty `Judgements` is *scoreable*, and scoring one reports nDCG 1.0 — every
    // ranking perfect, on a query nobody has looked at. `None` is what keeps an unjudged query out of the mean.
    assert!(
        judgements::for_query(pool, "nobody has judged this")
            .await
            .expect("load")
            .is_none(),
        "an unjudged query must be absent, not empty"
    );

    // And what that absence buys, stated so the two halves cannot drift apart: an empty set would score 1.0.
    let empty = eval::Judgements::new("nobody has judged this");
    let scored = eval::score_query(&empty, &[Uuid::new_v4()], 10);
    assert_eq!(
        scored.ndcg, None,
        "eval refuses to score an unjudged query too — both layers, because either alone would be enough to \
         report a perfect run over a corpus nobody labelled"
    );
}

async fn judging_the_same_pair_twice_is_a_correction_not_an_error(pool: &PgPool) {
    // Judging is iterative: a reviewer revisits a query and changes their mind. A conflict error here would make
    // the UI's obvious action fail.
    let id = asset(pool, "revisited.jpg").await;
    judgements::record(pool, "revisited", id, 3, None)
        .await
        .expect("first opinion");
    judgements::record(pool, "revisited", id, 1, None)
        .await
        .expect("second opinion must not conflict");

    let judged = judgements::for_query(pool, "revisited")
        .await
        .expect("load")
        .expect("judged");
    assert_eq!(judged.grades.len(), 1, "one pair, one judgement");
    assert_eq!(
        judged.grades.get(&id),
        Some(&1),
        "the later opinion is the one that counts"
    );
}

async fn a_grade_of_zero_is_recorded_rather_than_treated_as_absent(pool: &PgPool) {
    // "Judged irrelevant" and "not judged" are different facts, and eval treats them differently: a grade-0 result
    // is a labelled miss that counts against nothing for MRR but does mean the corpus has an opinion. Dropping
    // zeroes would silently turn every deliberate negative back into an unknown.
    let id = asset(pool, "deliberately-irrelevant.jpg").await;
    judgements::record(pool, "explicit zero", id, 0, None)
        .await
        .expect("record");
    let judged = judgements::for_query(pool, "explicit zero")
        .await
        .expect("load")
        .expect("a query judged entirely irrelevant is still judged");
    assert_eq!(judged.grades.get(&id), Some(&0));
}

async fn the_corpus_groups_by_query_however_the_rows_arrive(pool: &PgPool) {
    // `corpus` groups in Rust and relies on the ORDER BY to do it, which is exactly the kind of coupling that
    // works until somebody inserts in a different order. So the judgements go in interleaved.
    let first = asset(pool, "grouping-a.jpg").await;
    let second = asset(pool, "grouping-b.jpg").await;
    for (query, id, grade) in [
        ("grouping alpha", first, 3u8),
        ("grouping beta", second, 2),
        ("grouping alpha", second, 1),
        ("grouping beta", first, 0),
    ] {
        judgements::record(pool, query, id, grade, None)
            .await
            .expect("record");
    }

    let corpus = judgements::corpus(pool).await.expect("corpus");
    let alpha = corpus
        .iter()
        .find(|j| j.query_text == "grouping alpha")
        .expect("alpha present");
    let beta = corpus
        .iter()
        .find(|j| j.query_text == "grouping beta")
        .expect("beta present");
    assert_eq!(alpha.grades.len(), 2, "both judgements land on one query");
    assert_eq!(beta.grades.len(), 2);
    assert_eq!(alpha.grades.get(&first), Some(&3));
    assert_eq!(beta.grades.get(&first), Some(&0));
    assert_eq!(
        corpus
            .iter()
            .filter(|j| j.query_text == "grouping alpha")
            .count(),
        1,
        "one entry per query, not one per row"
    );
}

async fn a_soft_deleted_asset_leaves_the_corpus(pool: &PgPool) {
    // Otherwise the ideal ranking includes assets no search can return, and every run reports a regression the
    // ranking did not cause — which is the failure mode that gets an eval harness ignored.
    let kept = asset(pool, "still-here.jpg").await;
    let removed = asset(pool, "soft-deleted.jpg").await;
    judgements::record(pool, "tiering", kept, 3, None)
        .await
        .expect("record");
    judgements::record(pool, "tiering", removed, 3, None)
        .await
        .expect("record");
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(removed)
        .execute(pool)
        .await
        .expect("soft delete");

    let judged = judgements::for_query(pool, "tiering")
        .await
        .expect("load")
        .expect("judged");
    assert_eq!(judged.grades.len(), 1);
    assert!(judged.grades.contains_key(&kept));
    assert!(!judged.grades.contains_key(&removed));

    let corpus = judgements::corpus(pool).await.expect("corpus");
    let entry = corpus
        .iter()
        .find(|j| j.query_text == "tiering")
        .expect("present");
    assert!(!entry.grades.contains_key(&removed));
}

async fn a_query_whose_every_judged_asset_is_gone_becomes_unjudged(pool: &PgPool) {
    // Not an empty set. Same reason as the unjudged case: an empty one is scoreable and would report the query as
    // perfectly ranked, so a library that deleted its labelled assets would start reporting better numbers.
    let doomed = asset(pool, "all-gone.jpg").await;
    judgements::record(pool, "all gone", doomed, 2, None)
        .await
        .expect("record");
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(doomed)
        .execute(pool)
        .await
        .expect("soft delete");

    assert!(
        judgements::for_query(pool, "all gone")
            .await
            .expect("load")
            .is_none(),
        "every judged asset gone means the query is unjudged, not perfectly ranked"
    );
    assert!(
        !judgements::corpus(pool)
            .await
            .expect("corpus")
            .iter()
            .any(|j| j.query_text == "all gone"),
        "and it must not appear in the corpus as an empty entry either"
    );
}

async fn hard_deleting_an_asset_cascades_its_judgements(pool: &PgPool) {
    let id = asset(pool, "hard-deleted.jpg").await;
    judgements::record(pool, "cascade", id, 3, None)
        .await
        .expect("record");
    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("hard delete");

    let orphans: i64 =
        sqlx::query_scalar("SELECT count(*) FROM relevance_judgements WHERE asset_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("count");
    assert_eq!(
        orphans, 0,
        "ON DELETE CASCADE, or the corpus accumulates ghosts"
    );
}

async fn removing_a_judgement_reports_whether_it_existed(pool: &PgPool) {
    let id = asset(pool, "removable.jpg").await;
    judgements::record(pool, "removable", id, 2, None)
        .await
        .expect("record");
    assert!(
        judgements::remove(pool, "removable", id)
            .await
            .expect("remove")
    );
    assert!(
        !judgements::remove(pool, "removable", id)
            .await
            .expect("remove"),
        "removing what is not there is false, not an error and not a silent success"
    );
    assert!(
        judgements::for_query(pool, "removable")
            .await
            .expect("load")
            .is_none()
    );
}

async fn a_worse_ranking_scores_lower(pool: &PgPool) {
    // The property the whole harness exists for. If two rankings of the same corpus can score the same, the
    // numbers cannot settle an argument about a fusion weight, and the harness is decoration.
    let best = asset(pool, "scoring-best.jpg").await;
    let good = asset(pool, "scoring-good.jpg").await;
    let related = asset(pool, "scoring-related.jpg").await;
    let junk = asset(pool, "scoring-junk.jpg").await;
    for (id, grade) in [(best, 3u8), (good, 2), (related, 1)] {
        judgements::record(pool, "brand photography", id, grade, None)
            .await
            .expect("record");
    }

    let judged = judgements::for_query(pool, "brand photography")
        .await
        .expect("load")
        .expect("judged");

    let ideal = eval::score_query(&judged, &[best, good, related], 10);
    let inverted = eval::score_query(&judged, &[related, good, best], 10);
    let junk_first = eval::score_query(&judged, &[junk, best, good, related], 10);

    assert_eq!(
        ideal.ndcg,
        Some(1.0),
        "the ideal ordering of the judged set scores 1.0"
    );
    assert!(
        inverted.ndcg < ideal.ndcg,
        "inverting the order must cost something: {:?} vs {:?}",
        inverted.ndcg,
        ideal.ndcg
    );
    assert!(
        junk_first.ndcg < ideal.ndcg,
        "and an unjudged result at rank 1 must cost something too — otherwise junk is free"
    );
    assert_eq!(
        junk_first.unjudged_returned, 1,
        "the run must say how much of what it scored was unlabelled, because that number is how much to trust it"
    );
    assert_eq!(ideal.reciprocal_rank, Some(1.0));
    assert_eq!(
        junk_first.reciprocal_rank,
        Some(0.5),
        "first relevant at rank 2"
    );

    // And the run-level report only averages what it could score.
    let report = eval::score_run(vec![ideal, junk_first]);
    assert_eq!(report.scoreable, 2);
    assert_eq!(report.unscoreable, 0);
    assert!(report.mean_ndcg.expect("mean") < 1.0);
}

#[tokio::test]
async fn the_judgement_corpus_holds() {
    let (_pg, pool) = db().await;

    a_recorded_judgement_round_trips(&pool).await;
    an_unjudged_query_is_none_and_therefore_unscoreable(&pool).await;
    judging_the_same_pair_twice_is_a_correction_not_an_error(&pool).await;
    a_grade_of_zero_is_recorded_rather_than_treated_as_absent(&pool).await;
    the_corpus_groups_by_query_however_the_rows_arrive(&pool).await;
    a_soft_deleted_asset_leaves_the_corpus(&pool).await;
    a_query_whose_every_judged_asset_is_gone_becomes_unjudged(&pool).await;
    hard_deleting_an_asset_cascades_its_judgements(&pool).await;
    removing_a_judgement_reports_whether_it_existed(&pool).await;
    a_worse_ranking_scores_lower(&pool).await;
}
