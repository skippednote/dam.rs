//! Running the eval corpus against the real search path (2.9, G8).
//!
//! `dam_core::eval` does the arithmetic and `dam_db::judgements` holds the labels. This is the part that makes
//! either of them worth having: it takes each judged query, puts it through the *same* parse → plan → rank path a
//! user's search takes, and scores the ranking that comes back. A fusion weight, a field boost, an analyser
//! change — each reports its effect instead of being argued about.
//!
//! ## A query that cannot run is reported, never dropped
//!
//! A corpus whose queries half fail to parse, and which quietly scores the remainder, reports a fine mean over a
//! shrinking sample — and the number gets better the more of the corpus breaks. So [`Outcome`] carries the
//! refusals alongside the report, and [`Run::is_trustworthy`] is what a CI gate should consult before comparing
//! means at all.

use crate::pool::IndexPool;
use crate::schema::IndexSchema;
use crate::{Error, Result};
use dam_core::eval::{self, Judgements, Report};
use dam_core::policy::AccessPredicate;
use dam_core::shorthand;
use dam_core::{TenantSlug, query::Planned};

/// How many results to score. nDCG@10 is the usual reporting depth, and a depth that varied per run would make
/// two runs incomparable.
pub const DEFAULT_AT: usize = 10;

/// Why one query could not be scored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub query_text: String,
    pub reason: String,
}

/// The result of one eval run.
#[derive(Debug, Clone)]
pub struct Run {
    pub report: Report,
    /// Queries that never reached the ranker. Reported rather than dropped — see the module docs.
    pub refused: Vec<Refusal>,
    /// How deep the run scored.
    pub at: usize,
}

impl Run {
    /// Whether the numbers can be compared to another run's.
    ///
    /// False when any query was refused: the mean would be over a different sample than the run it is being
    /// compared against, and a corpus that broke would score *better*.
    pub fn is_trustworthy(&self) -> bool {
        self.refused.is_empty() && self.report.scoreable > 0
    }
}

/// Scores every judged query in `corpus` against the tenant's index.
///
/// The corpus is passed in rather than loaded here, because `dam_db` depends on this crate and not the other way
/// round. `damctl eval` loads it with [`dam_db::judgements::corpus`] and hands it over.
///
/// `access` is the predicate the run searches under, and it matters which one: an eval run under an
/// administrator's unrestricted scope measures the ranking, while a run under a restricted scope measures the
/// ranking *and* the filter together. `damctl eval` uses the unrestricted one and says so, because a regression
/// that turns out to be a permission change is a different bug from a regression in relevance.
pub async fn run(
    indexes: &IndexPool,
    tenant: &TenantSlug,
    index_schema: &IndexSchema,
    parse_schema: &shorthand::Schema,
    access: &AccessPredicate,
    corpus: Vec<Judgements>,
    at: usize,
) -> Result<Run> {
    let at = at.max(1);
    let open = indexes.get(tenant, index_schema).await?;
    // The reader is refreshed once per run rather than per query: a run that saw the index change under it would
    // score its first queries against a different corpus than its last.
    open.reload()?;

    let mut scored = Vec::with_capacity(corpus.len());
    let mut refused = Vec::new();

    for judgements in corpus {
        let parsed = match shorthand::parse(&judgements.query_text, parse_schema) {
            Ok(query) => query,
            Err(e) => {
                refused.push(Refusal {
                    query_text: judgements.query_text,
                    reason: format!("does not parse: {e}"),
                });
                continue;
            }
        };
        let planned = match Planned::new(parsed, access.clone(), parse_schema.fields()) {
            Ok(planned) => planned,
            Err(rejections) => {
                refused.push(Refusal {
                    query_text: judgements.query_text,
                    reason: format!("rejected by validation: {rejections:?}"),
                });
                continue;
            }
        };
        match crate::query::search(&open, index_schema, &planned, at) {
            Ok(returned) => scored.push(eval::score_query(&judgements, &returned, at)),
            Err(Error::Unsupported(reason)) => refused.push(Refusal {
                query_text: judgements.query_text,
                // Not a scoring failure: a relational clause is routed through SQL, and a corpus containing one
                // is asking for a hybrid run this harness does not do yet. Saying which is which is the point.
                reason: format!("the index cannot answer this query: {reason}"),
            }),
            Err(other) => return Err(other),
        }
    }

    Ok(Run {
        report: eval::score_run(scored),
        refused,
        at,
    })
}
