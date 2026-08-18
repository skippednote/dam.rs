//! Search evaluation: nDCG and MRR (2.9, GAPS G8).
//!
//! In `dam-core` rather than `dam-search`, because it is arithmetic over grades and ranks with no dependency on
//! Tantivy — and because `dam-search` already depends on `dam-db`, so putting it there while loading judgements
//! from the database would be a dependency cycle. The compiler said so before I did.
//!
//! Wired so a ranking change **reports its effect instead of being argued about**. That is the whole purpose:
//! without a number, "does boosting the title field help?" is settled by whoever is most confident in the room.
//!
//! ## Unjudged results score zero, they are not skipped
//!
//! The single most important choice here. If an unjudged result were dropped from the calculation, a system that
//! returned ten random assets would score the same as one that returned ten relevant ones — because none of the
//! random ten are judged, so none of them count. Treating them as grade 0 is what makes the metric able to
//! *fall*.
//!
//! ## A query with no judgements has no score, not a perfect one
//!
//! `0/0`. Left as a float it is `NaN`, and "helpfully" defaulting it to 1.0 means a corpus where nobody has
//! judged anything reports perfect relevance — which is worse than no harness at all, because it is a number
//! somebody will quote. [`QueryResult::ndcg`] is an `Option` for that reason, and [`Report`] counts how many
//! queries were scoreable.
//!
//! ## Gain is exponential, discount is logarithmic
//!
//! `2^grade − 1` over `log2(rank + 1)`, which is the standard formulation and matters because the grades are
//! graded for a reason: the schema notes that nDCG "needs gradations to distinguish a perfect hit from
//! plausibly related". Linear gain would make three mediocre results worth one perfect one.

use std::collections::HashMap;
use uuid::Uuid;

/// The highest grade a judgement may carry, matching the schema's `CHECK (grade BETWEEN 0 AND 3)`.
pub const MAX_GRADE: u8 = 3;

/// One query's judged assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgements {
    pub query_text: String,
    /// Asset to grade. A grade of 0 is a judgement that the asset is **not** relevant, which is different from
    /// being unjudged — it says somebody looked.
    pub grades: HashMap<Uuid, u8>,
}

impl Judgements {
    pub fn new(query_text: impl Into<String>) -> Self {
        Self {
            query_text: query_text.into(),
            grades: HashMap::new(),
        }
    }

    #[must_use]
    pub fn with(mut self, asset_id: Uuid, grade: u8) -> Self {
        self.grades.insert(asset_id, grade.min(MAX_GRADE));
        self
    }

    /// Whether anything here can produce a score.
    ///
    /// A query judged entirely at grade 0 is *scoreable* — the ideal DCG is zero, so any ranking scores zero,
    /// which is the correct answer for "nothing relevant exists". A query with **no** judgements is not.
    fn is_judged(&self) -> bool {
        !self.grades.is_empty()
    }
}

/// How one query scored.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub query_text: String,
    /// `None` when the query has no judgements at all. See the module docs — this is deliberately not 1.0.
    pub ndcg: Option<f64>,
    /// Reciprocal rank of the first result graded 1 or better. `None` when nothing relevant was returned, which
    /// is distinct from "no judgements": one is a miss, the other is unmeasurable.
    pub reciprocal_rank: Option<f64>,
    /// Results returned that nobody has judged.
    ///
    /// Reported because it is the health of the *judgement set*, not of the ranking: a high count means the
    /// corpus needs more judging before its numbers mean much.
    pub unjudged_returned: usize,
    pub judged_total: usize,
}

/// A whole evaluation run.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub queries: Vec<QueryResult>,
    /// Mean nDCG over the **scoreable** queries only.
    ///
    /// Averaging unscoreable queries in as zero would punish a ranking for a gap in the judgements, and
    /// averaging them in as one would reward it. Excluding them and reporting the count is the honest option.
    pub mean_ndcg: Option<f64>,
    /// Mean reciprocal rank over scoreable queries, counting a query with no relevant hit as 0.
    ///
    /// Zero rather than excluded, because "the ranking returned nothing relevant" *is* the measurement. Only an
    /// unjudged query is unmeasurable.
    pub mrr: Option<f64>,
    pub scoreable: usize,
    pub unscoreable: usize,
}

/// Scores one query's returned ranking.
///
/// `returned` is in rank order, best first. `at` truncates both the ranking and the ideal, which is what makes
/// `nDCG@10` comparable across queries with different numbers of judged assets.
pub fn score_query(judgements: &Judgements, returned: &[Uuid], at: usize) -> QueryResult {
    let truncated = &returned[..returned.len().min(at)];

    let unjudged_returned = truncated
        .iter()
        .filter(|id| !judgements.grades.contains_key(id))
        .count();

    let ndcg = if judgements.is_judged() {
        let actual = dcg(truncated.iter().map(|id| grade_of(judgements, *id)));
        let ideal = dcg(ideal_grades(judgements, at).into_iter());
        // `ideal == 0` means every judged asset is grade 0: nothing relevant exists, so any ranking is as good
        // as any other. Reported as 1.0 rather than NaN, because the ranking did nothing wrong — and 0.0 would
        // punish it for a corpus with no relevant documents.
        Some(if ideal == 0.0 { 1.0 } else { actual / ideal })
    } else {
        None
    };

    let reciprocal_rank = truncated
        .iter()
        .position(|id| grade_of(judgements, *id) >= 1)
        // 1-based rank: the first result has rank 1, so its reciprocal rank is 1.0. Using the 0-based index
        // would divide by zero on the best possible answer.
        .map(|index| 1.0 / (index as f64 + 1.0));

    QueryResult {
        query_text: judgements.query_text.clone(),
        ndcg,
        reciprocal_rank,
        unjudged_returned,
        judged_total: judgements.grades.len(),
    }
}

/// Scores a whole run.
pub fn score_run(results: Vec<QueryResult>) -> Report {
    let scoreable: Vec<&QueryResult> = results.iter().filter(|r| r.ndcg.is_some()).collect();
    let unscoreable = results.len() - scoreable.len();

    let mean_ndcg = if scoreable.is_empty() {
        None
    } else {
        let total: f64 = scoreable.iter().filter_map(|r| r.ndcg).sum();
        Some(total / scoreable.len() as f64)
    };
    let mrr = if scoreable.is_empty() {
        None
    } else {
        // A scoreable query with no relevant hit contributes 0. That is the measurement, not a gap.
        let total: f64 = scoreable
            .iter()
            .map(|r| r.reciprocal_rank.unwrap_or(0.0))
            .sum();
        Some(total / scoreable.len() as f64)
    };

    Report {
        scoreable: scoreable.len(),
        unscoreable,
        mean_ndcg,
        mrr,
        queries: results,
    }
}

/// Discounted cumulative gain over a ranking of grades.
fn dcg(grades: impl Iterator<Item = u8>) -> f64 {
    grades
        .enumerate()
        .map(|(index, grade)| {
            // `2^grade - 1`: grade 0 contributes nothing, and the gap between grades widens with the grade. The
            // schema grades judgements precisely so a perfect hit outweighs several plausible ones, which linear
            // gain would not express.
            let gain = 2_f64.powi(i32::from(grade)) - 1.0;
            // `log2(rank + 1)` with rank 1-based, so position 1 divides by log2(2) = 1.
            let discount = ((index as f64) + 2.0).log2();
            gain / discount
        })
        .sum()
}

/// The grades of a perfect ranking, truncated to `at`.
///
/// **Every** judged grade, sorted descending — not only the grades of the assets that were returned. Building
/// the ideal from the returned set would make a ranking that missed the best asset entirely still score 1.0,
/// because it would be compared against its own best order rather than against the best possible.
fn ideal_grades(judgements: &Judgements, at: usize) -> Vec<u8> {
    let mut grades: Vec<u8> = judgements.grades.values().copied().collect();
    grades.sort_unstable_by(|a, b| b.cmp(a));
    grades.truncate(at);
    grades
}

/// A result's grade, treating unjudged as 0.
///
/// See the module docs: skipping unjudged results would make a ranking of random assets score as well as a good
/// one, because none of the random ones would count.
fn grade_of(judgements: &Judgements, asset_id: Uuid) -> u8 {
    judgements.grades.get(&asset_id).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn a_perfect_ranking_scores_one() {
        let judged = Judgements::new("beach")
            .with(id(1), 3)
            .with(id(2), 2)
            .with(id(3), 1);
        let scored = score_query(&judged, &[id(1), id(2), id(3)], 10);
        let ndcg = scored.ndcg.expect("scoreable");
        assert!((ndcg - 1.0).abs() < 1e-9, "got {ndcg}");
        assert_eq!(scored.reciprocal_rank, Some(1.0));
    }

    #[test]
    fn reversing_a_ranking_lowers_the_score() {
        let judged = Judgements::new("beach")
            .with(id(1), 3)
            .with(id(2), 2)
            .with(id(3), 1);
        let best = score_query(&judged, &[id(1), id(2), id(3)], 10)
            .ndcg
            .expect("scoreable");
        let worst = score_query(&judged, &[id(3), id(2), id(1)], 10)
            .ndcg
            .expect("scoreable");
        assert!(worst < best, "{worst} should be worse than {best}");
    }

    #[test]
    fn an_unjudged_result_costs_the_ranking_rather_than_being_skipped() {
        // The most important property. If unjudged results were dropped, a system returning junk would score the
        // same as one returning the right answers — because none of the junk is judged, so none of it counts.
        let judged = Judgements::new("beach").with(id(1), 3);

        let good = score_query(&judged, &[id(1)], 10).ndcg.expect("scoreable");
        let padded = score_query(&judged, &[id(99), id(1)], 10)
            .ndcg
            .expect("scoreable");
        assert!(
            padded < good,
            "an unjudged result ahead of the right answer must cost the ranking: {padded} vs {good}"
        );

        let all_junk = score_query(&judged, &[id(97), id(98), id(99)], 10)
            .ndcg
            .expect("scoreable");
        assert_eq!(
            all_junk, 0.0,
            "a ranking of entirely unjudged results scores zero"
        );
    }

    #[test]
    fn the_ideal_uses_every_judgement_not_only_what_was_returned() {
        // Building the ideal from the returned set would make a ranking that missed the best asset entirely
        // still score 1.0 — compared against its own best order rather than the best possible.
        let judged = Judgements::new("beach").with(id(1), 3).with(id(2), 1);
        let missed_the_best = score_query(&judged, &[id(2)], 10).ndcg.expect("scoreable");
        assert!(
            missed_the_best < 1.0,
            "returning only the weaker asset must not score perfectly: {missed_the_best}"
        );
    }

    #[test]
    fn a_query_with_no_judgements_has_no_score_rather_than_a_perfect_one() {
        // 0/0. Defaulting it to 1.0 means a corpus nobody has judged reports perfect relevance, which is worse
        // than no harness at all because it is a number somebody will quote.
        let unjudged = Judgements::new("nobody judged this");
        let scored = score_query(&unjudged, &[id(1), id(2)], 10);
        assert_eq!(scored.ndcg, None);
        assert_eq!(scored.reciprocal_rank, None);
    }

    #[test]
    fn a_query_judged_entirely_irrelevant_is_scoreable_and_scores_one() {
        // Distinct from unjudged: somebody looked and said none of these are relevant. Any ranking is then as
        // good as any other, so the ranking did nothing wrong — and 0.0 would punish it for the corpus.
        let judged = Judgements::new("nothing matches")
            .with(id(1), 0)
            .with(id(2), 0);
        let scored = score_query(&judged, &[id(1), id(2)], 10);
        assert_eq!(scored.ndcg, Some(1.0));
        assert_eq!(
            scored.reciprocal_rank, None,
            "nothing graded 1 or better was returned"
        );
    }

    #[test]
    fn reciprocal_rank_is_one_based() {
        // A 0-based index would divide by zero on the best possible answer.
        let judged = Judgements::new("beach").with(id(5), 2);
        assert_eq!(
            score_query(&judged, &[id(5)], 10).reciprocal_rank,
            Some(1.0)
        );
        assert_eq!(
            score_query(&judged, &[id(9), id(5)], 10).reciprocal_rank,
            Some(0.5)
        );
        assert_eq!(
            score_query(&judged, &[id(9), id(8), id(5)], 10)
                .reciprocal_rank
                .map(|r| (r * 1000.0).round() / 1000.0),
            Some(0.333)
        );
    }

    #[test]
    fn a_grade_of_zero_does_not_count_as_a_relevant_hit() {
        // Grade 0 is "somebody looked and it is not relevant". Counting it for MRR would make a judged-irrelevant
        // result look like a success.
        let judged = Judgements::new("beach").with(id(1), 0).with(id(2), 2);
        assert_eq!(
            score_query(&judged, &[id(1), id(2)], 10).reciprocal_rank,
            Some(0.5)
        );
    }

    #[test]
    fn truncation_applies_to_the_ideal_as_well() {
        // Otherwise nDCG@1 against five judged assets could never reach 1.0, and the metric would be
        // incomparable between queries with different numbers of judgements.
        let judged = Judgements::new("beach")
            .with(id(1), 3)
            .with(id(2), 3)
            .with(id(3), 3);
        let at_one = score_query(&judged, &[id(1)], 1).ndcg.expect("scoreable");
        assert!(
            (at_one - 1.0).abs() < 1e-9,
            "the best asset at rank 1 is a perfect nDCG@1, got {at_one}"
        );
    }

    #[test]
    fn exponential_gain_makes_one_perfect_hit_beat_several_weak_ones() {
        // The reason grades are graded. Under linear gain three grade-1 results would equal one grade-3, and the
        // schema's note about distinguishing "perfect hit" from "plausibly related" would be lost.
        let one_perfect = dcg([3_u8].into_iter());
        let three_weak = dcg([1_u8, 1, 1].into_iter());
        assert!(
            one_perfect > three_weak,
            "one grade-3 ({one_perfect}) should beat three grade-1s ({three_weak})"
        );
    }

    #[test]
    fn a_run_excludes_unscoreable_queries_from_the_mean_and_counts_them() {
        // Averaging them in as zero punishes a ranking for a gap in the judgements; as one, rewards it.
        let judged = Judgements::new("judged").with(id(1), 3);
        let scored = vec![
            score_query(&judged, &[id(1)], 10),
            score_query(&Judgements::new("unjudged"), &[id(2)], 10),
        ];
        let report = score_run(scored);
        assert_eq!(report.scoreable, 1);
        assert_eq!(report.unscoreable, 1);
        assert_eq!(report.mean_ndcg, Some(1.0));
    }

    #[test]
    fn a_run_with_nothing_scoreable_reports_no_mean_at_all() {
        let report = score_run(vec![score_query(&Judgements::new("none"), &[id(1)], 10)]);
        assert_eq!(report.mean_ndcg, None);
        assert_eq!(report.mrr, None);
        assert_eq!(report.scoreable, 0);
    }

    #[test]
    fn a_miss_contributes_zero_to_mrr_rather_than_being_excluded() {
        // "The ranking returned nothing relevant" is the measurement. Only an unjudged query is unmeasurable.
        let judged = Judgements::new("beach").with(id(1), 3);
        let hit = score_query(&judged, &[id(1)], 10);
        let miss = score_query(&judged, &[id(99)], 10);
        let report = score_run(vec![hit, miss]);
        assert_eq!(report.scoreable, 2);
        assert_eq!(
            report.mrr,
            Some(0.5),
            "one perfect hit and one miss averages 0.5"
        );
    }

    #[test]
    fn an_empty_ranking_scores_zero_rather_than_panicking() {
        let judged = Judgements::new("beach").with(id(1), 3);
        let scored = score_query(&judged, &[], 10);
        assert_eq!(scored.ndcg, Some(0.0));
        assert_eq!(scored.reciprocal_rank, None);
    }

    #[test]
    fn a_grade_above_the_schema_maximum_is_clamped() {
        // The CHECK constraint stops it reaching the database, so a grade of 9 can only come from a caller. Left
        // unclamped it would make one query's ideal astronomically large and its nDCG meaninglessly small.
        let judged = Judgements::new("beach").with(id(1), 99);
        assert_eq!(judged.grades.get(&id(1)), Some(&MAX_GRADE));
    }
}
