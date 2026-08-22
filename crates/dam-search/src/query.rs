//! Rendering the query IR into a Tantivy query (2.6) — §12's second consumer, and 0.10's remainder.
//!
//! The SQL renderer is `dam_db::query_sql`. Neither decides anything: the access rules were compiled in
//! `dam_core::policy` and the query was validated in `dam_core::query`. §12's reason for that split is
//! that divergence between the consumers is a data leak, and the differential test in this crate's tests
//! is what keeps it honest — the same [`Planned`] through both back ends must return the same set.
//!
//! ## What Tantivy cannot answer, it refuses
//!
//! Taxonomy and collection membership are relational and are not in the index. A renderer that quietly
//! dropped those clauses would return **more** than the caller asked for, which for a filter over a
//! governed library is the wrong direction to be wrong in. So [`render`] returns
//! [`Unsupported`](crate::Error::Unsupported) naming the clause, and a planner routes such queries through
//! SQL or intersects the two.
//!
//! ## The access filter here is an optimisation; Postgres is the authority
//!
//! Group membership is written into the document at index time and changes in Postgres immediately. In the
//! window between an administrator revoking a group and the asset being reindexed, the index is
//! *permissive* — it still says the asset is in the group. Rendering the predicate here narrows the
//! candidate set and keeps scoring sane; it is not what makes the answer correct.
//!
//! [`search`] therefore returns ids for Postgres to filter and hydrate with the same predicate. Tantivy
//! ranks, Postgres authorises. Treating an eventually-consistent index as the gate on a governed library
//! is the failure this note exists to prevent.

use crate::schema::IndexSchema;
use crate::{Error, Result};
use dam_core::query::{Comparison, Endpoint, Literal, Planned, Query};
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, ExistsQuery, Occur, PhraseQuery, Query as TantivyQuery,
    RangeQuery, TermQuery, TermSetQuery,
};
use tantivy::schema::{IndexRecordOption, Term};

/// The ids `planned` matches, **best first**.
///
/// The ordered counterpart to [`render`], and the input the eval harness scores. nDCG and MRR are functions of
/// position, so a set-valued accessor makes every ranking indistinguishable — which is the state 2.9 exists to end.
///
/// Ties break on document address, so the order is deterministic for a given index state. It is not stable across
/// a reindex that lays segments out differently, which is why an eval run reports a corpus rather than asserting
/// exact positions.
///
/// The access predicate is rendered here as it is in [`render`] — see the module docs for why that narrows the
/// candidate set rather than authorising it. Postgres filters and hydrates the ids this returns, with the same
/// predicate.
pub fn search(
    open: &crate::pool::OpenIndex,
    schema: &IndexSchema,
    planned: &Planned,
    limit: usize,
) -> Result<Vec<uuid::Uuid>> {
    // Refusals happen before any searching, so this cannot become a way around them: a dropped taxonomy clause
    // would return more than the caller asked for whether the results were ordered or not.
    let query = render(planned, schema)?;

    let searcher = open.searcher();
    // `order_by_score` is what makes this ordered: in tantivy 0.26 a bare `TopDocs` is not a collector at all
    // until it is told what to order by, which is a good deal more honest than defaulting silently.
    let hits = searcher.search(
        &query,
        &tantivy::collector::TopDocs::with_limit(limit.max(1)).order_by_score(),
    )?;

    let mut ids = Vec::with_capacity(hits.len());
    for (_score, address) in hits {
        let doc: tantivy::TantivyDocument = searcher.doc(address)?;
        let raw = doc
            .get_first(schema.asset_id())
            .and_then(|value| {
                use tantivy::schema::Value as _;
                value.as_str().map(str::to_owned)
            })
            .ok_or_else(|| {
                Error::Tantivy(format!(
                    "indexed document at {address:?} has no stored asset id"
                ))
            })?;
        // A malformed id is an error rather than a skipped hit: silently dropping one would make a ranking look
        // worse than it is, and the eval harness would report a regression the ranking did not cause.
        ids.push(
            uuid::Uuid::parse_str(&raw).map_err(|e| {
                Error::Tantivy(format!("stored asset id {raw:?} is not a uuid: {e}"))
            })?,
        );
    }
    Ok(ids)
}

/// Renders `planned` into a Tantivy query.
pub fn render(planned: &Planned, schema: &IndexSchema) -> Result<Box<dyn TantivyQuery>> {
    // A predicate that matches nothing short-circuits, exactly as in SQL. `EmptyQuery` rather than a
    // filter that happens to exclude everything: the intent is visible and cannot be optimised away.
    if planned.matches_nothing() {
        return Ok(Box::new(EmptyQuery));
    }

    let mut clauses: Vec<(Occur, Box<dyn TantivyQuery>)> = Vec::new();

    // Soft-deleted assets are excluded on every query, so this is a `MustNot` on the fixed field rather
    // than something each caller adds.
    clauses.push((
        Occur::MustNot,
        Box::new(TermQuery::new(
            Term::from_field_bool(schema.deleted(), true),
            IndexRecordOption::Basic,
        )),
    ));

    if !planned.access().all_groups() {
        let groups = planned.access().allowed_groups();
        if groups.is_empty() {
            // Scoped to no groups. Unreachable through `matches_nothing`, and rendered explicitly rather
            // than left as an empty term set — an empty `TermSetQuery` matches nothing, but relying on
            // that is relying on a library's edge case for a security property.
            return Ok(Box::new(EmptyQuery));
        }
        let terms: Vec<Term> = groups
            .iter()
            .map(|id| Term::from_field_text(schema.group_ids(), &id.to_string()))
            .collect();
        clauses.push((Occur::Must, Box::new(TermSetQuery::new(terms))));
    }

    clauses.push((Occur::Must, render_query(planned.query(), schema)?));
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn render_query(query: &Query, schema: &IndexSchema) -> Result<Box<dyn TantivyQuery>> {
    let rendered: Box<dyn TantivyQuery> = match query {
        Query::All => Box::new(AllQuery),
        Query::Text(text) => render_text(text, schema),
        Query::Field { key, op } => render_field(key, op, schema)?,
        Query::Term { .. } => {
            return Err(Error::Unsupported(
                "taxonomy membership is relational and is not in the index; dropping the clause would \
                 return more than the caller asked for, so it is refused and routed through SQL"
                    .to_owned(),
            ));
        }
        Query::InCollection(_) => {
            return Err(Error::Unsupported(
                "collection membership is relational and is not in the index".to_owned(),
            ));
        }
        Query::Rating(_) => {
            // An average over a table the index does not hold. It *could* be indexed as a fast field, but then
            // every rating would have to reindex the asset — and a star click that silently made search stale
            // is worse than a query that goes to SQL and says it is unranked.
            return Err(Error::Unsupported(
                "a rating is an aggregate over `asset_ratings` and is not in the index".to_owned(),
            ));
        }
        Query::Filename(_) => {
            // The index holds a filename as *tokens*, which is why `DSC_0043` is findable through free text
            // and `0043` is not. A substring over the column is precisely the question the index cannot
            // answer, and answering the equality case from the index while sending the substring case to SQL
            // would make one selector mean two different things depending on its value.
            return Err(Error::Unsupported(
                "a filename comparison is a substring over a column, not a token match".to_owned(),
            ));
        }
        Query::Status(_) => {
            // `assets.status` is not an index field, and making it one would mean reindexing an asset every
            // time it was archived. Refused rather than dropped, like every other clause the index cannot
            // answer: dropping it would return archived assets to somebody who asked for active ones.
            return Err(Error::Unsupported(
                "an asset's status is a column and is not in the index".to_owned(),
            ));
        }
        Query::Orientation(_) => {
            // Derivable from two stored numbers, which is exactly why it is not indexed: the index would hold
            // a third value that has to agree with them.
            return Err(Error::Unsupported(
                "orientation is derived from the stored dimensions and is not in the index"
                    .to_owned(),
            ));
        }
        Query::HasAttachment => {
            return Err(Error::Unsupported(
                "what is attached to an asset is relational and is not in the index".to_owned(),
            ));
        }
        Query::Mine(state) => {
            // Per-caller by nature, so it could never be a shared index field: the index holds one document per
            // asset, and "is this a favourite" has a different answer for every person reading it.
            return Err(Error::Unsupported(format!(
                "`is:{}` is per-caller and cannot be an index field",
                state.as_str()
            )));
        }
        Query::And(children) => {
            if children.is_empty() {
                // The identity, matching SQL's `(true)`.
                Box::new(AllQuery)
            } else {
                let mut clauses = Vec::with_capacity(children.len());
                for child in children {
                    clauses.push((Occur::Must, render_query(child, schema)?));
                }
                Box::new(BooleanQuery::new(clauses))
            }
        }
        Query::Or(children) => {
            if children.is_empty() {
                // `false`, not "no filter". The SQL renderer has the same case for the same reason: an
                // empty disjunction that matched everything would return the tenant's whole library.
                Box::new(EmptyQuery)
            } else {
                let mut clauses = Vec::with_capacity(children.len());
                for child in children {
                    clauses.push((Occur::Should, render_query(child, schema)?));
                }
                Box::new(BooleanQuery::new(clauses))
            }
        }
        Query::Not(inner) => {
            // `MustNot` alone matches nothing in Tantivy, so the `Must(AllQuery)` is required rather than
            // decorative — without it a negated query returns an empty set whatever it negates.
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(AllQuery) as Box<dyn TantivyQuery>),
                (Occur::MustNot, render_query(inner, schema)?),
            ]))
        }
    };
    Ok(rendered)
}

/// Free text over the concatenated blob.
///
/// A multi-word input becomes a phrase query, because the shorthand only produces a multi-word
/// [`Query::Text`] from a quoted phrase — an unquoted `beach holiday` arrives as two conjoined terms.
fn render_text(text: &str, schema: &IndexSchema) -> Box<dyn TantivyQuery> {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [] => Box::new(AllQuery),
        [single] => Box::new(TermQuery::new(
            Term::from_field_text(schema.text(), &single.to_lowercase()),
            IndexRecordOption::WithFreqs,
        )),
        many => {
            let terms: Vec<Term> = many
                .iter()
                .map(|word| Term::from_field_text(schema.text(), &word.to_lowercase()))
                .collect();
            Box::new(PhraseQuery::new(terms))
        }
    }
}

fn render_field(key: &str, op: &Comparison, schema: &IndexSchema) -> Result<Box<dyn TantivyQuery>> {
    let field = schema.metadata();
    let rendered: Box<dyn TantivyQuery> = match op {
        Comparison::Exists => Box::new(ExistsQuery::new(
            format!("{}.{key}", crate::schema::METADATA),
            true,
        )),
        Comparison::Missing => Box::new(BooleanQuery::new(vec![
            (Occur::Must, Box::new(AllQuery) as Box<dyn TantivyQuery>),
            (
                Occur::MustNot,
                Box::new(ExistsQuery::new(
                    format!("{}.{key}", crate::schema::METADATA),
                    true,
                )),
            ),
        ])),
        Comparison::Equals(literal) => Box::new(TermQuery::new(
            json_term(field, key, literal)?,
            IndexRecordOption::Basic,
        )),
        Comparison::NotEquals(literal) => Box::new(BooleanQuery::new(vec![
            (Occur::Must, Box::new(AllQuery) as Box<dyn TantivyQuery>),
            (
                Occur::MustNot,
                Box::new(TermQuery::new(
                    json_term(field, key, literal)?,
                    IndexRecordOption::Basic,
                )),
            ),
        ])),
        Comparison::Range { lower, upper } => render_range(field, key, lower, upper)?,
        Comparison::Contains(_) | Comparison::StartsWith(_) => {
            // Both are expressible with a regex or an automaton query, and both would then disagree with
            // SQL's `ILIKE` at the margins — accent folding, tokenisation, case. Refused until the
            // differential test can cover them rather than rendered approximately.
            return Err(Error::Unsupported(format!(
                "substring matching on {key} is not yet rendered here; SQL's ILIKE and a Tantivy \
                 automaton disagree at the margins, and an approximate answer that differs between \
                 back ends is what §12 forbids"
            )));
        }
    };
    Ok(rendered)
}

/// A term addressing `key` inside the JSON metadata field.
fn json_term(field: tantivy::schema::Field, key: &str, literal: &Literal) -> Result<Term> {
    let mut term = Term::from_field_json_path(field, key, false);
    match literal {
        // Verbatim, **not** lowercased. The JSON field is indexed with the raw tokeniser (see
        // `crate::schema`), so a stored "Acme Corp" is the single token "Acme Corp" — which is what makes
        // this equality test agree with SQL's jsonb containment. An earlier version lowercased here to match
        // the default tokeniser, and that combination made `brand:acme` match "Acme Corp" in the index and
        // not in SQL: 22 results against 11 on a real corpus.
        Literal::Text(text) => term.append_type_and_str(text),
        Literal::Uuid(id) => term.append_type_and_str(&id.to_string()),
        Literal::Date(date) => term.append_type_and_str(&date.format("%Y-%m-%d").to_string()),
        Literal::DateTime(at) => term.append_type_and_str(&at.to_rfc3339()),
        Literal::Int(number) => term.append_type_and_fast_value(*number),
        Literal::Bool(flag) => term.append_type_and_fast_value(*flag),
        Literal::Decimal(number) => term.append_type_and_fast_value(*number),
    }
    Ok(term)
}

fn render_range(
    field: tantivy::schema::Field,
    key: &str,
    lower: &Endpoint,
    upper: &Endpoint,
) -> Result<Box<dyn TantivyQuery>> {
    use std::ops::Bound;

    // `RangeQuery::new` derives the field it searches from whichever bound is set, so the term must carry
    // the real field handle. Passing a placeholder id resolves to whatever field happens to hold that id
    // — the query would run, against the wrong column, and return a plausible wrong answer.
    let bound = |endpoint: &Endpoint| -> Result<Bound<Term>> {
        Ok(match endpoint {
            Endpoint::Unbounded => Bound::Unbounded,
            Endpoint::Inclusive(literal) => Bound::Included(range_term(field, key, literal)?),
            Endpoint::Exclusive(literal) => Bound::Excluded(range_term(field, key, literal)?),
        })
    };

    Ok(Box::new(RangeQuery::new(bound(lower)?, bound(upper)?)))
}

/// A range endpoint term, addressing `key` inside the JSON field.
fn range_term(field: tantivy::schema::Field, key: &str, literal: &Literal) -> Result<Term> {
    let mut term = Term::from_field_json_path(field, key, false);
    match literal {
        Literal::Int(number) => term.append_type_and_fast_value(*number),
        Literal::Decimal(number) => term.append_type_and_fast_value(*number),
        Literal::Date(date) => term.append_type_and_str(&date.format("%Y-%m-%d").to_string()),
        Literal::DateTime(at) => term.append_type_and_str(&at.to_rfc3339()),
        other => {
            return Err(Error::Unsupported(format!(
                "a range endpoint of {other:?} has no ordering"
            )));
        }
    }
    Ok(term)
}
