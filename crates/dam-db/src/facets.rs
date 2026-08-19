//! Faceted search (2.7): counts that respect the access predicate.
//!
//! ## Why the counts are computed in SQL and not in the index
//!
//! Search 1 in DECISIONS.md says Tantivy ranks and Postgres authorises, because an asset's group membership
//! changes in Postgres immediately and the index catches up on reindex — so between those moments the index
//! is *permissive*. For a result list that is survivable: the ids are re-filtered on hydration and the
//! extra ones disappear.
//!
//! A facet count cannot be re-filtered. It **is** the disclosure. "brand: Acme (5)" shown to a caller who
//! may see three of them tells them two assets exist that they cannot see, and §7 names exactly that shape:
//! *pagination counts alone disclose the existence of assets a caller cannot see.* A facet rail is a
//! pagination count with better presentation.
//!
//! So facets are counted over the same access-filtered query the search uses, in the database that owns the
//! membership. If profiling later shows this is too slow for a filter rail, the fix is to make group
//! membership fresh in the index — not to move counts onto a stale one.
//!
//! ## A value with no visible assets does not appear
//!
//! Not "appears with count 0". A zero-count bucket discloses that the value exists, which for a facet like
//! `client` or `campaign` is often the sensitive part — knowing a competitor is a client is the leak, and
//! the count is beside the point. Buckets come from the filtered set rather than from an enumeration of the
//! field's values, so an invisible value has nothing to produce a row.
//!
//! ## Facets are governed
//!
//! Only `field_defs.facetable` fields may be faceted. Faceting free text produces one bucket per distinct
//! value — a million buckets on a million-asset library — so the flag is a resource guard as well as an
//! administrator's decision.

use crate::Error;
use dam_core::fields::{FieldDef, FieldKind};
use dam_core::query::Planned;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

/// Most buckets returned for one facet.
///
/// A filter rail shows a handful and a "more" affordance. Returning everything would be a response
/// proportional to the tenant's distinct values, which is unbounded from the caller's point of view.
pub const MAX_BUCKETS: i64 = 100;

/// What to count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FacetRequest {
    /// Distinct values of a metadata field.
    Field { key: String, limit: i64 },
    /// Confirmed taxonomy tags, rolled up to a depth.
    ///
    /// Rolled up because a filter rail shows "Outdoor (240)" rather than 40 leaf terms — and because the
    /// leaf counts do not sum to the ancestor's when an asset carries two leaves under it.
    Taxonomy { taxonomy_id: Uuid, limit: i64 },
}

/// One bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    /// The value, as text. A number or a boolean is rendered rather than typed, because a facet rail
    /// displays it and the round trip back into a query goes through the field's kind anyway.
    pub value: String,
    /// A stable identifier where one exists — a taxonomy term id. `None` for a metadata value, whose
    /// identity *is* its text.
    pub id: Option<Uuid>,
    pub count: i64,
}

/// The counted result for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facet {
    /// The field key, or the taxonomy id rendered.
    pub key: String,
    pub buckets: Vec<Bucket>,
    /// Whether buckets were dropped by the limit.
    ///
    /// Reported rather than left implicit: a rail that silently truncates makes "no other brands" and
    /// "ninety other brands" look identical, and a user filters on the wrong assumption.
    pub truncated: bool,
}

/// Counts each request over the access-filtered set `planned` describes.
///
/// One query per facet. Not one query for all of them: a single query would need a `GROUP BY GROUPING
/// SETS` over unrelated shapes, and the plan for that is worse than N indexed aggregates — and it makes
/// per-facet limits inexpressible.
pub async fn count(
    pool: &PgPool,
    planned: &Planned,
    defs: &[FieldDef],
    requests: &[FacetRequest],
) -> Result<Vec<Facet>, Error> {
    let mut conn = pool.acquire().await?;
    count_on(&mut conn, planned, defs, requests).await
}

/// The same counting, on a connection.
///
/// A request handler holds a [`crate::TenantConn`], whose `search_path` is transaction-scoped — so the
/// counts have to run on *that* connection or they resolve against the wrong schema. Taking a pool here
/// would mean a second, differently-scoped connection, and the failure mode is not an error: unqualified
/// table names would resolve to whatever the pooled connection last had, which in a schema-per-tenant system
/// is a cross-tenant read with nothing attached to it.
pub async fn count_on(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    defs: &[FieldDef],
    requests: &[FacetRequest],
) -> Result<Vec<Facet>, Error> {
    let mut facets = Vec::with_capacity(requests.len());
    for request in requests {
        facets.push(match request {
            FacetRequest::Field { key, limit } => {
                count_field(&mut *conn, planned, defs, key, *limit).await?
            }
            FacetRequest::Taxonomy { taxonomy_id, limit } => {
                count_taxonomy(&mut *conn, planned, *taxonomy_id, *limit).await?
            }
        });
    }
    Ok(facets)
}

async fn count_field(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    defs: &[FieldDef],
    key: &str,
    limit: i64,
) -> Result<Facet, Error> {
    let Some(def) = defs.iter().find(|d| d.key == key) else {
        return Err(Error::Core(dam_core::Error::NotFound {
            kind: dam_core::ResourceKind::FieldDef,
            id: key.to_owned(),
        }));
    };
    if !def.facetable {
        return Err(Error::Unsupported(format!(
            "field {key:?} is not marked facetable; faceting it would produce one bucket per distinct \
             value, which on a large library is a response nobody asked for"
        )));
    }
    if def.kind == FieldKind::Geo {
        return Err(Error::Unsupported(
            "a coordinate has no discrete values to count; geographic faceting needs a grid, which is \
             not this"
                .to_owned(),
        ));
    }

    let effective = limit.clamp(1, MAX_BUCKETS);

    // The `+ 1` is how truncation is detected: asking for one more than needed distinguishes "exactly
    // this many" from "at least this many" without a second count query.
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        // `jsonb_array_elements_text` over the array case, unioned with the scalar case — the same
        // both-shapes handling `query_sql` needs, and for the same reason: without it a multivalued field
        // facets as zero buckets.
        "WITH visible AS (SELECT assets.id, asset_metadata.values FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    // Current versions only. A facet count describes the library, and counting three versions of one asset as three
    // would make the rail's numbers disagree with the grid beside it.
    builder.push(crate::versions::CURRENT_ONLY);
    builder.push(
        "), exploded AS (SELECT DISTINCT visible.id, value FROM visible, LATERAL (\
         SELECT CASE WHEN jsonb_typeof(visible.values -> ",
    );
    builder.push_bind(key.to_owned());
    builder.push(
        ") = 'array' \
              THEN (SELECT array_agg(v) FROM jsonb_array_elements_text(visible.values -> ",
    );
    builder.push_bind(key.to_owned());
    builder.push(
        ") AS v) \
              ELSE ARRAY[visible.values ->> ",
    );
    builder.push_bind(key.to_owned());
    builder.push(
        "] END AS values_out) AS shaped, \
         LATERAL unnest(shaped.values_out) AS value WHERE value IS NOT NULL) \
         SELECT value, count(*) AS n FROM exploded GROUP BY value \
         ORDER BY n DESC, value LIMIT ",
    );
    builder.push_bind(effective + 1);

    let rows: Vec<(String, i64)> = builder.build_query_as().fetch_all(&mut *conn).await?;
    Ok(finish(key.to_owned(), rows, effective, None))
}

async fn count_taxonomy(
    conn: &mut sqlx::PgConnection,
    planned: &Planned,
    taxonomy_id: Uuid,
    limit: i64,
) -> Result<Facet, Error> {
    let effective = limit.clamp(1, MAX_BUCKETS);

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    // `DISTINCT` on `(ancestor, asset)`: an asset tagged with two leaves under one ancestor must count
    // once for that ancestor, or a rollup exceeds the number of assets that exist and a user sees
    // "Outdoor (7)" over a library of five.
    builder.push(
        ") SELECT ancestor.id::text, ancestor.label, count(DISTINCT visible.id) AS n \
         FROM visible \
         JOIN asset_tags at ON at.asset_id = visible.id AND at.state = 'confirmed' \
         JOIN taxonomy_terms tagged ON tagged.id = at.term_id \
         JOIN taxonomy_terms ancestor ON tagged.path <@ ancestor.path \
              AND ancestor.taxonomy_id = tagged.taxonomy_id \
         WHERE tagged.taxonomy_id = ",
    );
    builder.push_bind(taxonomy_id);
    builder.push(" GROUP BY ancestor.id, ancestor.label ORDER BY n DESC, ancestor.label LIMIT ");
    builder.push_bind(effective + 1);

    let rows: Vec<(String, String, i64)> = builder.build_query_as().fetch_all(&mut *conn).await?;
    let with_ids: Vec<(String, i64)> = rows
        .iter()
        .map(|(_, label, n)| (label.clone(), *n))
        .collect();
    let ids: Vec<Option<Uuid>> = rows
        .iter()
        .map(|(id, _, _)| Uuid::parse_str(id).ok())
        .collect();

    let mut facet = finish(taxonomy_id.to_string(), with_ids, effective, None);
    for (bucket, id) in facet.buckets.iter_mut().zip(ids) {
        bucket.id = id;
    }
    Ok(facet)
}

/// Trims to the limit and reports whether anything was dropped.
fn finish(key: String, rows: Vec<(String, i64)>, limit: i64, id: Option<Uuid>) -> Facet {
    let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
    let buckets = rows
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        // A zero count cannot arise — the buckets come from the filtered set — but filtering here makes
        // that explicit rather than a property a reader has to derive from the SQL.
        .filter(|(_, count)| *count > 0)
        .map(|(value, count)| Bucket { value, id, count })
        .collect();
    Facet {
        key,
        buckets,
        truncated,
    }
}
