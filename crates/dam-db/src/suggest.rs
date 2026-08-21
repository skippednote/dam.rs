//! Predictive search: what somebody is probably about to type (Q.17).
//!
//! ## A suggestion is a disclosure
//!
//! The rule `facets` opens with applies here with less room to argue. A facet count at least needs a reader to
//! infer something from a number; a suggestion *names* the value. Offering "client: Northwind" to somebody
//! who may see none of Northwind's assets hands them the fact directly, and §7 names that shape exactly.
//!
//! So every source here is counted over the caller's own access-filtered library, through the same
//! `query_sql::push_where` every other read uses. A value with no visible asset behind it produces no row —
//! not a row with a zero, and not a row from an enumeration of what the field *could* hold.
//!
//! ## Why the suggestion carries the query fragment
//!
//! Each row says what to insert — `brand:acme`, `in:exterior.yellow`, `filename:DSC_0043.jpg` — for the same
//! reason a facet key is a selector: the client's job is to put a string in a box, and a suggestion it has to
//! *assemble* is a second place where the query language is spoken and can be got wrong.
//!
//! ## What is not here
//!
//! No history and no popularity. Suggesting what other people searched for is a cross-caller disclosure with
//! no access filter available — the search that produced it belonged to somebody with different grants — and
//! ranking by frequency across a tenant leaks which clients are busy. What is offered is what the caller can
//! already see, ordered by how much of it there is.
use crate::Error;
use dam_core::fields::{FieldDef, FieldKind};
use dam_core::query::Planned;
use sqlx::{Postgres, QueryBuilder};

/// How many rows one source may contribute.
///
/// Small on purpose: a type-ahead list past about five rows is a list nobody reads, and each source costs a
/// query. The client shows them grouped, so five each is a usable panel rather than a wall.
pub const PER_SOURCE: i64 = 5;

/// Where a suggestion came from, which is also what the client groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A value of a facetable metadata field.
    Field,
    /// A confirmed category or vocabulary term.
    Term,
    /// An asset's own filename.
    Filename,
}

impl Source {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Field => "field",
            Self::Term => "term",
            Self::Filename => "filename",
        }
    }
}

/// One thing somebody might have been about to type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub source: Source,
    /// What to show: the value, the term's label, the filename.
    pub label: String,
    /// Which field or taxonomy it came from, for a grouped list. `None` for a filename, which is its own
    /// category.
    ///
    /// The field *key* rather than its label, because a `FieldDef` does not carry one — the tenant's label is
    /// a schema-admin concern and the client already renders keys for the facet rail. A taxonomy has a real
    /// label and that is what travels.
    pub within: Option<String>,
    /// The query fragment to insert. See the module docs on why this is here rather than assembled by a client.
    pub fragment: String,
    /// How many visible assets carry it. A type-ahead orders by this: the most common value is the one most
    /// likely to be wanted, and it is the only ranking signal available that does not cross callers.
    pub count: i64,
}

/// Suggestions for a partially typed word, over the caller's visible library.
///
/// `planned` is the *current* query — so suggestions narrow as a search narrows, exactly as facet counts do.
/// Somebody two clauses into a search is offered what is left rather than what the library holds.
///
/// `typed` is matched as a prefix, not a substring. A prefix is what a type-ahead means: somebody four letters
/// into a word wants what starts that way, and substring matching turns "ac" into a list where the thing they
/// are typing is fourth.
pub async fn for_prefix(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    defs: &[FieldDef],
    typed: &str,
) -> Result<Vec<Suggestion>, Error> {
    let typed = typed.trim();
    // One character is every value in the library, ordered by count — a list that is technically correct and
    // useless, and three queries to produce it.
    if typed.chars().count() < 2 {
        return Ok(Vec::new());
    }

    let mut found = Vec::new();
    for def in defs
        .iter()
        .filter(|def| def.facetable && def.kind != FieldKind::Geo)
    {
        found.extend(field_values(&mut *conn, planned, def, typed).await?);
    }
    found.extend(terms(&mut *conn, planned, typed).await?);
    found.extend(filenames(&mut *conn, planned, typed).await?);
    Ok(found)
}

/// Values of one facetable field that start with `typed`.
async fn field_values(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    def: &FieldDef,
    typed: &str,
) -> Result<Vec<Suggestion>, Error> {
    // The same both-shapes handling `facets` needs, and for the same reason: without the array case a
    // multivalued field suggests nothing at all, and those are most tag-like fields.
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id, asset_metadata.values FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    builder.push(crate::versions::LIBRARY_ROWS);
    builder.push(
        "), exploded AS (SELECT DISTINCT visible.id, value FROM visible, LATERAL (\
         SELECT CASE WHEN jsonb_typeof(visible.values -> ",
    );
    builder.push_bind(def.key.clone());
    builder.push(
        ") = 'array' \
              THEN (SELECT array_agg(v) FROM jsonb_array_elements_text(visible.values -> ",
    );
    builder.push_bind(def.key.clone());
    builder.push(
        ") AS v) \
              ELSE ARRAY[visible.values ->> ",
    );
    builder.push_bind(def.key.clone());
    builder.push(
        "] END AS values_out) AS shaped, \
         LATERAL unnest(shaped.values_out) AS value WHERE value IS NOT NULL) \
         SELECT value, count(*) AS n FROM exploded WHERE value ILIKE ",
    );
    builder.push_bind(format!("{}%", crate::query_sql::escape_like(typed)));
    builder.push(" ESCAPE '\\' GROUP BY value ORDER BY n DESC, value LIMIT ");
    builder.push_bind(PER_SOURCE);

    let rows: Vec<(String, i64)> = builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(rows
        .into_iter()
        .map(|(value, count)| Suggestion {
            source: Source::Field,
            fragment: fragment(&def.key, &value),
            label: value,
            within: Some(def.key.clone()),
            count,
        })
        .collect())
}

/// Confirmed terms whose label starts with `typed`.
///
/// Confirmed only, like every other read of `asset_tags`: a suggested AI tag is a proposal in a review queue,
/// and offering one as a filter would put unreviewed machine output in front of somebody as though a curator
/// had agreed to it.
async fn terms(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    typed: &str,
) -> Result<Vec<Suggestion>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    builder.push(crate::versions::LIBRARY_ROWS);
    builder.push(
        ") SELECT t.label, x.label, t.path::text, count(DISTINCT visible.id) AS n \
         FROM visible \
         JOIN asset_tags at ON at.asset_id = visible.id AND at.state = 'confirmed' \
         JOIN taxonomy_terms t ON t.id = at.term_id \
         JOIN taxonomies x ON x.id = t.taxonomy_id \
         WHERE t.label ILIKE ",
    );
    builder.push_bind(format!("{}%", crate::query_sql::escape_like(typed)));
    builder.push(" ESCAPE '\\' GROUP BY t.label, x.label, t.path ORDER BY n DESC, t.label LIMIT ");
    builder.push_bind(PER_SOURCE);

    let rows: Vec<(String, String, String, i64)> =
        builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(rows
        .into_iter()
        .map(|(label, taxonomy, path, count)| Suggestion {
            source: Source::Term,
            // `in:` takes the path, and the path is what makes the descendants come with it — a fragment
            // naming the label would filter by a word rather than by a place in the tree.
            fragment: format!(
                "{}:{}",
                dam_core::shorthand::CATEGORY_SELECTOR,
                quote_if_needed(&path)
            ),
            label,
            within: Some(taxonomy),
            count,
        })
        .collect())
}

/// Filenames starting with `typed`.
///
/// A prefix here even though `filename:` can express a substring, because this is the type-ahead: somebody
/// who has typed `DSC_00` wants the names that begin that way. The substring stays available as a query.
async fn filenames(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    typed: &str,
) -> Result<Vec<Suggestion>, Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.filename, 1::bigint AS n FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    builder.push(crate::versions::LIBRARY_ROWS);
    builder.push(" AND assets.filename ILIKE ");
    builder.push_bind(format!("{}%", crate::query_sql::escape_like(typed)));
    builder.push(" ESCAPE '\\' ORDER BY assets.filename LIMIT ");
    builder.push_bind(PER_SOURCE);

    let rows: Vec<(String, i64)> = builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(rows
        .into_iter()
        .map(|(filename, count)| Suggestion {
            source: Source::Filename,
            fragment: format!(
                "{}:{}",
                dam_core::shorthand::FILENAME_SELECTOR,
                quote_if_needed(&filename)
            ),
            label: filename,
            within: None,
            count,
        })
        .collect())
}

/// A `key:value` fragment, quoted where the parser would otherwise read structure.
fn fragment(key: &str, value: &str) -> String {
    format!("{key}:{}", quote_if_needed(value))
}

/// Quotes a value the shorthand would not read literally.
///
/// The same rule the client's composer follows, and it has to be: a fragment this module quotes differently
/// from the way the box quotes it is a suggestion that changes the query when it is clicked. Whitespace and
/// the structural characters, plus a leading `-` or `*`, which are operators.
fn quote_if_needed(value: &str) -> String {
    let structural = value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, ':' | '"' | '(' | ')' | '>' | '<' | '=' | '|'));
    let leading_operator = value.starts_with('-') || value.starts_with('*');
    if value.is_empty() || structural || leading_operator {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

/// The existing value closest to `typed`, among the values of `def` the caller can see (Q.17).
///
/// For the did-you-mean on an empty result set. The candidate pool is the *whole visible library* rather than
/// the failing query's result set, which would be empty by definition — somebody who typed `brand:acmee` is
/// asking about the brands they can see, not about the brands inside a search that matched nothing.
///
/// Bounded by `CANDIDATES`: the most common values, compared in memory. A `SELECT` ordered by similarity would
/// need an extension and a full scan to answer a question that only arises on an empty page.
pub async fn nearest_value(
    conn: &mut sqlx::PgConnection,
    visible: &Planned,
    def: &FieldDef,
    typed: &str,
) -> Result<Option<String>, Error> {
    /// How many of a field's values to consider. A hundred covers every field a person filters by hand; a
    /// field with more distinct values than this is one where the typo is more likely in the field name.
    const CANDIDATES: i64 = 100;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id, asset_metadata.values FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, visible)?;
    builder.push(crate::versions::LIBRARY_ROWS);
    builder.push(
        "), exploded AS (SELECT DISTINCT visible.id, value FROM visible, LATERAL (\
         SELECT CASE WHEN jsonb_typeof(visible.values -> ",
    );
    builder.push_bind(def.key.clone());
    builder.push(
        ") = 'array' \
              THEN (SELECT array_agg(v) FROM jsonb_array_elements_text(visible.values -> ",
    );
    builder.push_bind(def.key.clone());
    builder.push(
        ") AS v) \
              ELSE ARRAY[visible.values ->> ",
    );
    builder.push_bind(def.key.clone());
    builder.push(
        "] END AS values_out) AS shaped, \
         LATERAL unnest(shaped.values_out) AS value WHERE value IS NOT NULL) \
         SELECT value, count(*) AS n FROM exploded GROUP BY value ORDER BY n DESC, value LIMIT ",
    );
    builder.push_bind(CANDIDATES);

    let rows: Vec<(String, i64)> = builder.build_query_as().fetch_all(&mut *conn).await?;
    let values: Vec<&str> = rows.iter().map(|(value, _)| value.as_str()).collect();
    Ok(dam_core::similar::closest(values, typed).map(str::to_owned))
}
