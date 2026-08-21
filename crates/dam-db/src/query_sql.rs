//! Rendering the query IR into SQL (2.4). One of §12's consumers.
//!
//! The other is Tantivy (2.6). Neither decides anything: the access rules were compiled in
//! `dam_core::policy`, the query was validated in `dam_core::query`, and this turns the pair into a
//! `WHERE` fragment. That split is the whole point — §12 says divergence between the consumers is a data
//! leak, and the only way to be sure they agree is for neither to have an opinion.
//!
//! ## The access filter is the outer conjunct, always
//!
//! Rendered first, at the top level, ANDed with the user's query. Not appended by the caller and not
//! reachable from inside a `Not`: [`dam_core::query::Planned`] keeps the two trees separate precisely so
//! an access term cannot end up negated. §7 requires the filter to be *in* the query rather than applied
//! after, because pagination counts alone disclose assets a caller cannot see.
//!
//! ## Nothing is interpolated
//!
//! Every literal goes through `push_bind`. A search string is the most attacker-controlled input in the
//! system, so the fragment is injection-proof by construction rather than by review — and `LIKE` patterns
//! have their metacharacters escaped, which is the part that looks safe and is not: an unescaped `%`
//! turns `contains("50%")` into a prefix match, and a `\` turns it into a syntax error.
//!
//! ## Values live in `asset_metadata.values`
//!
//! A `jsonb` column with a `jsonb_path_ops` GIN index. So equality goes through `@>` where it can — that
//! is the operator the index serves — and comparisons that need typing cast the extracted value instead.

use crate::Error;
use dam_core::fields::FieldKind;
use dam_core::query::{Comparison, Endpoint, Literal, Personal, Planned, Query};
use sqlx::{Postgres, QueryBuilder};

/// Pushes the complete `WHERE` condition for `planned`.
///
/// The caller supplies everything up to and including `WHERE`, and may append `ORDER BY` / `LIMIT`. The
/// fragment is parenthesised so appending `AND …` cannot change its meaning through precedence.
pub fn push_where(builder: &mut QueryBuilder<Postgres>, planned: &Planned) -> Result<(), Error> {
    // A predicate that matches nothing short-circuits to a false condition. The distinction from an
    // omitted filter is the safety property: an omitted filter is a full scan of the tenant's library.
    if planned.matches_nothing() {
        builder.push("(false)");
        return Ok(());
    }

    builder.push("(");
    crate::access::push_asset_filter(builder, planned.access())?;
    builder.push(" AND ");
    push_query(builder, planned.query(), planned)?;
    builder.push(")");
    Ok(())
}

fn push_query(
    builder: &mut QueryBuilder<Postgres>,
    query: &Query,
    planned: &Planned,
) -> Result<(), Error> {
    match query {
        Query::All => {
            builder.push("(true)");
        }
        Query::Text(text) => push_text(builder, text, planned),
        Query::Field { key, op } => push_field(builder, key, op, planned)?,
        Query::Term {
            term_id,
            include_descendants,
        } => push_term(builder, *term_id, *include_descendants),
        Query::Rating(op) => push_rating(builder, op)?,
        Query::Status(status) => {
            builder.push("assets.status = ");
            builder.push_bind(status.clone());
        }
        Query::Orientation(shape) => push_orientation(builder, *shape),
        Query::HasAttachment => {
            // The attachment side of `assets.attached_to`, and soft-deleted paperwork does not count: a
            // release form somebody deleted is not paperwork the asset has.
            builder.push(
                "EXISTS (SELECT 1 FROM assets att \
                  WHERE att.attached_to = assets.id AND att.deleted_at IS NULL)",
            );
        }
        Query::Mine(state) => push_personal(builder, *state, planned)?,
        Query::InCollection(collection_id) => {
            builder
                .push("assets.id IN (SELECT asset_id FROM collection_items WHERE collection_id = ");
            builder.push_bind(*collection_id);
            builder.push(")");
        }
        Query::And(children) => push_junction(builder, children, planned, "AND", "true")?,
        Query::Or(children) => push_junction(builder, children, planned, "OR", "false")?,
        Query::Not(inner) => {
            // `NOT (…)`, parenthesised. Without the parentheses `NOT a AND b` binds as `(NOT a) AND b`,
            // which is a different query and one that returns more rows.
            builder.push("NOT (");
            push_query(builder, inner, planned)?;
            builder.push(")");
        }
    }
    Ok(())
}

/// Renders `AND`/`OR` over children, with the identity for the empty case.
///
/// The empty case is the one worth being careful about. `Or([])` must be `false`: rendering it as nothing
/// would drop the filter entirely and return every asset in the tenant, and rendering it as `()` is a
/// syntax error — the first is a leak, the second at least fails loudly.
fn push_junction(
    builder: &mut QueryBuilder<Postgres>,
    children: &[Query],
    planned: &Planned,
    operator: &str,
    identity: &str,
) -> Result<(), Error> {
    if children.is_empty() {
        builder.push("(");
        builder.push(identity);
        builder.push(")");
        return Ok(());
    }
    builder.push("(");
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            builder.push(" ");
            builder.push(operator);
            builder.push(" ");
        }
        push_query(builder, child, planned)?;
    }
    builder.push(")");
    Ok(())
}

/// Free text across the searchable fields, plus the filename.
///
/// This is the deliberately unsophisticated back end: real ranking is Tantivy's job (2.6), and SQL's role
/// is to answer the same *set* so the two can be compared. Matching the set rather than the order is what
/// makes the differential test in 2.6 meaningful.
fn push_text(builder: &mut QueryBuilder<Postgres>, text: &str, planned: &Planned) {
    let pattern = format!("%{}%", escape_like(text));

    builder.push("(assets.filename ILIKE ");
    builder.push_bind(pattern.clone());
    builder.push(" ESCAPE '\\'");

    for key in planned.text_fields() {
        // `->>` yields text for any scalar, and `jsonb_array_elements_text` covers a multivalued field —
        // without the second, a text search would silently miss every value in an array field, which is
        // most tag-like fields.
        builder.push(" OR (SELECT bool_or(v ILIKE ");
        builder.push_bind(pattern.clone());
        builder.push(
            " ESCAPE '\\') FROM (\
             SELECT asset_metadata.values ->> ",
        );
        builder.push_bind(key.clone());
        builder.push(
            " AS v UNION ALL SELECT jsonb_array_elements_text(\
             CASE WHEN jsonb_typeof(asset_metadata.values -> ",
        );
        builder.push_bind(key.clone());
        builder.push(") = 'array' THEN asset_metadata.values -> ");
        builder.push_bind(key.clone());
        builder.push(" ELSE '[]'::jsonb END)) AS candidates)");
    }
    builder.push(")");
}

fn push_field(
    builder: &mut QueryBuilder<Postgres>,
    key: &str,
    op: &Comparison,
    planned: &Planned,
) -> Result<(), Error> {
    // The kind decides how the value is extracted, and `Planned` guarantees the field exists — it was
    // validated before this type could be constructed.
    let kind = planned.field_kind(key).ok_or_else(|| {
        Error::Inconsistent(format!(
            "query references field {key:?}, which validation should have refused"
        ))
    })?;

    match op {
        Comparison::Exists => {
            builder.push("(asset_metadata.values ? ");
            builder.push_bind(key.to_owned());
            builder.push(" AND jsonb_typeof(asset_metadata.values -> ");
            builder.push_bind(key.to_owned());
            builder.push(") <> 'null')");
        }
        Comparison::Missing => {
            // The negation of `Exists`, plus the empty-array case. A multivalued field holding `[]` is
            // present and empty, and a user asking for "no brand" means that too.
            builder.push("(NOT (asset_metadata.values ? ");
            builder.push_bind(key.to_owned());
            builder.push(") OR jsonb_typeof(asset_metadata.values -> ");
            builder.push_bind(key.to_owned());
            builder.push(") = 'null' OR asset_metadata.values -> ");
            builder.push_bind(key.to_owned());
            builder.push(" = '[]'::jsonb)");
        }
        Comparison::Equals(literal) => push_equals(builder, key, literal),
        Comparison::NotEquals(literal) => {
            // `NOT (equals)` rather than `<>`. On a multivalued field `<>` compares the whole array, so
            // "brand is not Acme" would match an asset tagged both Acme and Globex.
            builder.push("NOT (");
            push_equals(builder, key, literal);
            builder.push(")");
        }
        Comparison::Range { lower, upper } => push_range(builder, key, kind, lower, upper),
        Comparison::Contains(text) => {
            push_like(builder, key, &format!("%{}%", escape_like(text)));
        }
        Comparison::StartsWith(text) => {
            push_like(builder, key, &format!("{}%", escape_like(text)));
        }
    }
    Ok(())
}

/// Equality, via containment so the GIN index applies — in **both** shapes.
///
/// The trap here is worth stating exactly, because the obvious version is silently wrong. jsonb's
/// "an array contains a primitive" rule applies only at the *top level*; nested inside an object it does
/// not. Measured:
///
/// ```text
/// '{"c":["red","blue"]}' @> '{"c":"blue"}'    -> false
/// '{"c":["red","blue"]}' @> '{"c":["blue"]}'  -> true
/// '{"c":"blue"}'         @> '{"c":"blue"}'    -> true
/// '{"c":"blue"}'         @> '{"c":["blue"]}'  -> false
/// ```
///
/// So a single `@>` with the scalar form matches scalar fields and **silently misses every multivalued
/// field** — which is most tag-like fields, and the symptom is "search does not find my tags" with no
/// error anywhere. Both forms are emitted, and both are GIN-indexable; `->> =` would be correct for
/// scalars only and could not use the index at all.
fn push_equals(builder: &mut QueryBuilder<Postgres>, key: &str, literal: &Literal) {
    builder.push("(asset_metadata.values @> ");
    builder.push_bind(containment(key, literal));
    builder.push(" OR asset_metadata.values @> ");
    builder.push_bind(containment_array(key, literal));
    builder.push(")");
}

/// The single-element-array form, for a multivalued field.
fn containment_array(key: &str, literal: &Literal) -> serde_json::Value {
    serde_json::json!({ key: [literal_value(literal)] })
}

/// The `jsonb` document `@>` is tested against.
fn containment(key: &str, literal: &Literal) -> serde_json::Value {
    serde_json::json!({ key: literal_value(literal) })
}

/// A literal as the `jsonb` scalar the validator would have stored.
fn literal_value(literal: &Literal) -> serde_json::Value {
    match literal {
        Literal::Text(text) => serde_json::json!(text),
        Literal::Int(number) => serde_json::json!(number),
        Literal::Decimal(number) => serde_json::json!(number),
        Literal::Bool(flag) => serde_json::json!(flag),
        // The same strings the validator normalised, so the comparison is exact rather than dependent
        // on how Postgres would parse a date inside jsonb.
        Literal::Date(date) => serde_json::json!(date.format("%Y-%m-%d").to_string()),
        Literal::DateTime(at) => serde_json::json!(at.to_rfc3339()),
        Literal::Uuid(id) => serde_json::json!(id.to_string()),
    }
}

fn push_range(
    builder: &mut QueryBuilder<Postgres>,
    key: &str,
    kind: FieldKind,
    lower: &Endpoint,
    upper: &Endpoint,
) {
    builder.push("(");
    let mut wrote = false;
    for (endpoint, operator) in [(lower, ">"), (upper, "<")] {
        let (literal, inclusive) = match endpoint {
            Endpoint::Unbounded => continue,
            Endpoint::Inclusive(literal) => (literal, true),
            Endpoint::Exclusive(literal) => (literal, false),
        };
        if wrote {
            builder.push(" AND ");
        }
        wrote = true;
        push_typed_extract(builder, key, kind);
        builder.push(" ");
        builder.push(operator);
        if inclusive {
            builder.push("=");
        }
        builder.push(" ");
        push_typed_bind(builder, literal);
    }
    if !wrote {
        // Unreachable: validation refuses a range with no bounds. Rendered as false rather than as an
        // empty fragment, because an empty fragment here would produce `( )` — and if a caller had
        // already appended something, a condition that matches everything.
        builder.push("false");
    }
    builder.push(")");
}

/// Extracts the value cast to the type its kind implies.
///
/// A cast rather than a text comparison: `'9' > '10'` is true as text and false as a number, and a date
/// range compared lexically breaks the moment a format varies.
fn push_typed_extract(builder: &mut QueryBuilder<Postgres>, key: &str, kind: FieldKind) {
    builder.push("(asset_metadata.values ->> ");
    builder.push_bind(key.to_owned());
    builder.push(match kind {
        FieldKind::Int => ")::bigint",
        FieldKind::Decimal => ")::numeric",
        FieldKind::Date => ")::date",
        FieldKind::DateTime => ")::timestamptz",
        // Validation refuses a range on anything else, so this is only reachable through an
        // inconsistency; text ordering is the least surprising fallback.
        _ => ")",
    });
}

fn push_typed_bind(builder: &mut QueryBuilder<Postgres>, literal: &Literal) {
    match literal {
        Literal::Int(number) => {
            builder.push_bind(*number);
        }
        Literal::Decimal(number) => {
            // Bound as text with an explicit cast: binding an f64 against `numeric` would round-trip
            // through a float and change the value being compared.
            builder.push_bind(number.to_string());
            builder.push("::numeric");
        }
        Literal::Date(date) => {
            builder.push_bind(*date);
        }
        Literal::DateTime(at) => {
            builder.push_bind(*at);
        }
        Literal::Text(text) => {
            builder.push_bind(text.clone());
        }
        Literal::Bool(flag) => {
            builder.push_bind(*flag);
        }
        Literal::Uuid(id) => {
            builder.push_bind(id.to_string());
        }
    }
}

/// A case-insensitive `LIKE` over the field, covering array values.
fn push_like(builder: &mut QueryBuilder<Postgres>, key: &str, pattern: &str) {
    builder.push("(SELECT bool_or(v ILIKE ");
    builder.push_bind(pattern.to_owned());
    builder.push(" ESCAPE '\\') FROM (SELECT asset_metadata.values ->> ");
    builder.push_bind(key.to_owned());
    builder.push(
        " AS v UNION ALL SELECT jsonb_array_elements_text(\
         CASE WHEN jsonb_typeof(asset_metadata.values -> ",
    );
    builder.push_bind(key.to_owned());
    builder.push(") = 'array' THEN asset_metadata.values -> ");
    builder.push_bind(key.to_owned());
    // The trailing paren closes the outer `(SELECT bool_or(...)`. Leaving it off produced a syntax error
    // only once an `ORDER BY` followed, because until then the unbalanced fragment was the end of the
    // statement and Postgres reported the next token instead.
    builder.push(" ELSE '[]'::jsonb END)) AS candidates)");
}

/// Taxonomy membership, optionally including descendants.
///
/// Only `confirmed` tags. A `suggested` AI tag is a proposal in a review queue, and letting one affect
/// search results would make unreviewed machine output indistinguishable from a curator's decision —
/// which §8's review gate exists to prevent.
fn push_term(builder: &mut QueryBuilder<Postgres>, term_id: uuid::Uuid, descendants: bool) {
    if descendants {
        // `<@` against the term's own path, so the whole subtree matches without a recursive CTE. The
        // subquery resolves the path rather than the caller passing it, because a caller holding a stale
        // path after a `move_term` would silently match the wrong branch.
        builder.push(
            "assets.id IN (SELECT at.asset_id FROM asset_tags at \
             JOIN taxonomy_terms t ON t.id = at.term_id \
             WHERE at.state = 'confirmed' AND t.path <@ \
                   (SELECT path FROM taxonomy_terms WHERE id = ",
        );
        builder.push_bind(term_id);
        builder.push("))");
    } else {
        builder.push(
            "assets.id IN (SELECT asset_id FROM asset_tags \
             WHERE state = 'confirmed' AND term_id = ",
        );
        builder.push_bind(term_id);
        builder.push(")");
    }
}

/// The asset's average rating, compared.
///
/// `avg` over `asset_ratings`, in a correlated subquery rather than a join: a join would multiply the asset rows
/// by their ratings and every count downstream would be wrong. Unrated assets have a null average, so they fall
/// out of every comparison — which is what "3 stars and up" means and is not what `Missing` means, hence the two
/// being separate branches.
/// The frame's shape, from the dimensions already stored.
///
/// Both dimensions must be present and positive. An asset with none — a PDF, a WAV — matches no orientation
/// rather than one: `NULL > NULL` is null and would be excluded anyway, but saying so here is what keeps the
/// facet and the filter agreeing, since the facet has to make the same decision to produce its buckets.
fn push_orientation(builder: &mut QueryBuilder<Postgres>, shape: dam_core::query::Orientation) {
    use dam_core::query::Orientation;
    let comparison = match shape {
        Orientation::Landscape => "assets.width > assets.height",
        Orientation::Portrait => "assets.width < assets.height",
        Orientation::Square => "assets.width = assets.height",
    };
    builder.push(format!(
        "(assets.width IS NOT NULL AND assets.height IS NOT NULL \
          AND assets.width > 0 AND assets.height > 0 AND {comparison})"
    ));
}

fn push_rating(builder: &mut QueryBuilder<Postgres>, op: &Comparison) -> Result<(), Error> {
    const AVERAGE: &str = "(SELECT avg(stars) FROM asset_ratings r WHERE r.asset_id = assets.id)";
    const RATED: &str = "EXISTS (SELECT 1 FROM asset_ratings r WHERE r.asset_id = assets.id)";

    match op {
        // `Exists` and `Missing` are about *whether* anybody rated it, and are the two buckets a rail needs
        // beside the stars. Expressed with EXISTS rather than a null test on the average, because they are
        // questions about rows and not about a number.
        Comparison::Exists => builder.push(RATED),
        Comparison::Missing => builder.push(format!("NOT {RATED}")),
        Comparison::Equals(literal) => {
            // Rounded, because an average of 3.5 is what a person calls "4 stars" on a screen showing whole
            // stars — and `stars:4` clicked from a bucket labelled 4 has to return what that bucket counted.
            builder.push(format!("round({AVERAGE}) = "));
            push_stars(builder, literal)?;
            builder
        }
        Comparison::NotEquals(literal) => {
            // Unrated assets are *not* "not 4 stars": they have no rating, and sweeping them in would make the
            // complement of a bucket bigger than the library minus the bucket.
            builder.push(format!("({RATED} AND round({AVERAGE}) <> "));
            push_stars(builder, literal)?;
            builder.push(")")
        }
        Comparison::Range { lower, upper } => {
            builder.push("(");
            let mut first = true;
            for (endpoint, operator) in [(lower, (">=", ">")), (upper, ("<=", "<"))] {
                let (symbol, literal) = match endpoint {
                    Endpoint::Inclusive(literal) => (operator.0, literal),
                    Endpoint::Exclusive(literal) => (operator.1, literal),
                    Endpoint::Unbounded => continue,
                };
                if !first {
                    builder.push(" AND ");
                }
                first = false;
                builder.push(format!("{AVERAGE} {symbol} "));
                push_stars(builder, literal)?;
            }
            if first {
                // Validation refuses a range with no bounds, so this is unreachable — and `(true)` rather than
                // an empty parenthesis, because a malformed fragment would be a syntax error at the database
                // instead of a wrong answer here.
                builder.push("true");
            }
            builder.push(")")
        }
        // Refused by validation before a renderer sees it. Rendered as false rather than ignored: a filter that
        // silently disappears widens the result set, which is the wrong direction to be wrong in.
        Comparison::Contains(_) | Comparison::StartsWith(_) => builder.push("(false)"),
    };
    Ok(())
}

/// A star count as a bound.
fn push_stars(builder: &mut QueryBuilder<Postgres>, literal: &Literal) -> Result<(), Error> {
    match literal {
        Literal::Int(stars) => {
            // Bound, not interpolated, and typed as numeric so the comparison against `avg` does not depend on
            // Postgres inferring a type for a bare parameter.
            builder.push_bind(*stars);
            builder.push("::numeric");
            Ok(())
        }
        // Validation refuses anything else. An error rather than a guess, because a rating compared against
        // text is a question with no answer.
        other => Err(Error::Migrate(format!(
            "a rating cannot be compared with {other:?}"
        ))),
    }
}

/// One of the caller's own engagement states.
///
/// Fails when nobody has said who the caller is. Not an empty result: "you have no favourites" and "the code
/// forgot to name you" look identical on a screen, and only one of them is a bug worth finding.
fn push_personal(
    builder: &mut QueryBuilder<Postgres>,
    state: Personal,
    planned: &Planned,
) -> Result<(), Error> {
    let Some(viewer) = planned.viewer() else {
        return Err(Error::Migrate(format!(
            "`is:{}` needs a viewer; the plan was built without one",
            state.as_str()
        )));
    };
    let table = match state {
        Personal::Favourite => "asset_favourites",
        Personal::Watched => "asset_watches",
        Personal::Rated => "asset_ratings",
    };
    builder.push(format!(
        "EXISTS (SELECT 1 FROM {table} e WHERE e.asset_id = assets.id AND e.identity_id = "
    ));
    builder.push_bind(viewer);
    builder.push(")");
    Ok(())
}

/// Escapes the `LIKE` metacharacters, using `\` as the escape character.
///
/// The part that looks safe and is not. Unescaped, `contains("50%")` becomes a prefix match on "50" and
/// silently returns far more than asked; `_` matches any character; and a trailing `\` is a syntax error
/// the user cannot understand. The backslash goes first, or it would escape the escapes.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::escape_like;

    #[test]
    fn like_metacharacters_are_escaped_backslash_first() {
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        // Backslash first, or the escapes we add would themselves be escaped.
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(escape_like("100%\\_"), "100\\%\\\\\\_");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(escape_like("beach holiday"), "beach holiday");
    }
}
