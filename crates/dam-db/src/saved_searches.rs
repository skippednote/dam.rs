//! Saved searches and smart collections (3.7, GAPS G15).
//!
//! A saved search stores the **query IR**, not a result set and not a rendered SQL string. `saved_searches.query`
//! is `jsonb` for that reason, and the reason matters more than it looks.
//!
//! ## Re-evaluated against current access, never the access at save time
//!
//! TASKS.md names this, and it is the property that decides whether a saved search is safe to share. If the
//! results were stored — or if the query were stored *with* its access filter baked in — then a search saved by
//! an administrator and later opened by a contractor would return the administrator's results. The saved object
//! would be a permanent leak wearing the shape of a bookmark.
//!
//! So what is stored is only what the *user* asked for. The access predicate is compiled fresh for whoever
//! opens it, and `Planned::new` is what joins the two — the same type the search path and the delivery path use,
//! so there is no route to running a saved query without one.
//!
//! `result_count` is a cache for a sidebar badge and the schema says so: "recomputed by the worker, never
//! trusted for access decisions". It is stored per search rather than per viewer, which means it is *somebody
//! else's* count — see [`SavedSearch::result_count`].

use crate::Error;
use chrono::{DateTime, Duration, Utc};
use dam_core::fields::FieldDef;
use dam_core::policy::AccessPredicate;
use dam_core::query::{Planned, Query};
use uuid::Uuid;

/// How stale `last_used_at` must be before opening a search rewrites it.
///
/// The column orders a "recently used" sidebar. Writing it on every open turns browsing into a write per click,
/// and an hour's resolution sorts that list identically — the same argument `auth::LAST_USED_RESOLUTION` and
/// `derivatives::SERVED_RESOLUTION` make.
pub const USED_RESOLUTION: Duration = Duration::hours(1);

/// A stored search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSearch {
    pub id: Uuid,
    pub owner_id: Option<Uuid>,
    pub name: String,
    /// The user's query, as stored. **Without** an access filter — see the module docs.
    pub query: serde_json::Value,
    pub is_smart_collection: bool,
    pub shared: bool,
    pub shared_with_roles: Vec<String>,
    pub notify_path_id: Option<Uuid>,
    /// A cached count for a sidebar badge, computed for nobody in particular.
    ///
    /// Not the viewer's count, and not usable as one: two people opening the same shared search see different
    /// numbers of assets, and this is at best one of them. The schema calls it "never trusted for access
    /// decisions"; showing it as *the* count would leak how many assets exist beyond a viewer's scope, which is
    /// the §7 disclosure in a sidebar.
    pub result_count: Option<i64>,
    pub counted_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// What to save.
#[derive(Debug, Clone)]
pub struct SaveSpec<'a> {
    pub owner_id: Option<Uuid>,
    pub name: &'a str,
    /// The parsed query. Serialised as-is; nothing about the saver's access is recorded.
    pub query: &'a Query,
    pub is_smart_collection: bool,
    pub shared: bool,
    pub shared_with_roles: &'a [String],
    pub notify_path_id: Option<Uuid>,
}

/// Saves a search.
///
/// Takes a [`Query`] rather than raw JSON, so a caller cannot store something that never parsed. A saved search
/// that fails to load is a broken bookmark whose owner has no way to fix it.
pub async fn save(pool: &sqlx::PgPool, spec: &SaveSpec<'_>) -> Result<SavedSearch, Error> {
    let id = Uuid::new_v4();
    let query = serialise(spec.query)?;

    sqlx::query(
        "INSERT INTO saved_searches \
         (id, owner_id, name, query, is_smart_collection, shared, shared_with_roles, notify_path_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(spec.owner_id)
    .bind(spec.name)
    .bind(&query)
    .bind(spec.is_smart_collection)
    .bind(spec.shared)
    .bind(spec.shared_with_roles)
    .bind(spec.notify_path_id)
    .execute(pool)
    .await?;

    load(pool, id).await?.ok_or_else(|| {
        Error::Inconsistent(format!(
            "saved search {id} vanished immediately after being saved"
        ))
    })
}

/// Loads a saved search.
pub async fn load(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<SavedSearch>, Error> {
    let row = sqlx::query_as::<_, SavedRow>(
        "SELECT id, owner_id, name, query, is_smart_collection, shared, shared_with_roles, \
                notify_path_id, result_count, counted_at, last_used_at \
         FROM saved_searches WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(into_search))
}

/// Turns a saved search into something runnable, for **this** caller.
///
/// The whole point of the module. `access` is compiled for whoever is opening it, now — so a search saved by an
/// administrator and opened by a contractor runs against the contractor's scope. There is deliberately no
/// variant of this that takes a stored predicate: `Planned` has one constructor and it requires a live one.
///
/// `defs` are the tenant's current field definitions, so a search referencing a field that has since been
/// deleted is **refused** rather than silently dropping the clause. Dropping it would widen the result set — the
/// same argument `dam_core::query` makes about unknown fields, and the direction that matters for a filter.
pub fn plan(
    search: &SavedSearch,
    access: AccessPredicate,
    defs: &[FieldDef],
) -> Result<Planned, Error> {
    let query: Query = deserialise(&search.query)?;
    Planned::new(query, access, defs).map_err(|rejections| {
        let detail: Vec<String> = rejections
            .iter()
            .map(|r| format!("{}: {}", r.key, r.code))
            .collect();
        // Named as a validation failure rather than a not-found, because the search still exists and its owner
        // can fix it — the message has to say which clause stopped being valid.
        Error::Core(dam_core::Error::Validation {
            field: "saved_search.query".into(),
            reason: format!(
                "saved search {} no longer validates against the current field definitions ({}); a clause \
                 referring to a deleted field is refused rather than dropped, because dropping it would \
                 widen the results",
                search.name,
                detail.join(", ")
            ),
        })
    })
}

/// Notes that a search was opened, at most once per [`USED_RESOLUTION`].
pub async fn mark_used(pool: &sqlx::PgPool, id: Uuid, now: DateTime<Utc>) -> Result<bool, Error> {
    let updated = sqlx::query(
        "UPDATE saved_searches SET last_used_at = $2 \
         WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < $3)",
    )
    .bind(id)
    .bind(now)
    .bind(now - USED_RESOLUTION)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Records a recomputed count.
///
/// Deliberately takes no viewer: this is the worker's count, for a badge. See [`SavedSearch::result_count`].
pub async fn record_count(
    pool: &sqlx::PgPool,
    id: Uuid,
    count: i64,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE saved_searches SET result_count = $2, counted_at = $3, updated_at = $3 WHERE id = $1",
    )
    .bind(id)
    .bind(count)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// The searches a caller may open: their own, plus shared ones their roles reach.
///
/// Sharing is checked here rather than left to the caller, because "I can see it in a list" and "I may run it"
/// have to be the same question — a search visible but unopenable is a UI bug, and one openable but invisible is
/// a leak.
///
/// A search shared with **no** roles is shared with everyone in the tenant. That is what `shared` alone means,
/// and `shared_with_roles` narrows it.
///
/// **A viewer with no identity owns nothing.** `owner_id` is nullable with no defined meaning for NULL, and the
/// ownership test used `IS NOT DISTINCT FROM` — so a caller with no identity matched every ownerless search and
/// an identified caller matched none of them. That is not a coherent rule in either direction, and it was
/// nobody's decision: it reads as NULL-handling politeness. Plain `=` gives the only coherent reading — an
/// identity-less caller owns nothing, and an ownerless search is visible only if it is shared.
///
/// Narrowing rather than widening, which is the safe direction for an access predicate, and consistent with
/// `dam_api::caller::authorize` already refusing an identity-less caller outright. Found by a surviving
/// mutation: swapping the operator changed no test.
pub async fn visible_to(
    pool: &sqlx::PgPool,
    viewer: Option<Uuid>,
    roles: &[String],
    limit: i64,
) -> Result<Vec<SavedSearch>, Error> {
    let mut conn = pool.acquire().await?;
    visible_to_on(&mut conn, viewer, roles, limit).await
}

/// [`visible_to`] on a caller's connection.
///
/// The variant a request path needs, and the reason it exists is a bug this repo has already had once:
/// `saved_searches` is a *tenant* table, and `TenantConn` sets `search_path` for the duration of its transaction.
/// Handed the global pool, the pool-taking form above resolves `FROM saved_searches` against `dam_global` and
/// fails — which is exactly how `check_groups_are_renderable` came to return a 500 to every group-scoped caller.
///
/// Until the dashboard, nothing outside this module's own tests called any of this, so the shape had never been
/// exercised from a request at all.
pub async fn visible_to_on(
    conn: &mut sqlx::PgConnection,
    viewer: Option<Uuid>,
    roles: &[String],
    limit: i64,
) -> Result<Vec<SavedSearch>, Error> {
    let rows = sqlx::query_as::<_, SavedRow>(
        "SELECT id, owner_id, name, query, is_smart_collection, shared, shared_with_roles, \
                notify_path_id, result_count, counted_at, last_used_at \
         FROM saved_searches \
         WHERE (owner_id = $1) \
            OR (shared AND (cardinality(shared_with_roles) = 0 OR shared_with_roles && $2)) \
         ORDER BY last_used_at DESC NULLS LAST, name LIMIT $3",
    )
    .bind(viewer)
    .bind(roles)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(into_search).collect())
}

/// Smart collections, for the collections UI.
pub async fn smart_collections(pool: &sqlx::PgPool, limit: i64) -> Result<Vec<SavedSearch>, Error> {
    let rows = sqlx::query_as::<_, SavedRow>(
        "SELECT id, owner_id, name, query, is_smart_collection, shared, shared_with_roles, \
                notify_path_id, result_count, counted_at, last_used_at \
         FROM saved_searches WHERE is_smart_collection ORDER BY name LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(into_search).collect())
}

/// Deletes a saved search.
pub async fn delete(pool: &sqlx::PgPool, id: Uuid) -> Result<bool, Error> {
    let deleted = sqlx::query("DELETE FROM saved_searches WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}

/// A `Query` as JSON.
///
/// Hand-written rather than `serde` derives on `Query`, because the stored form is a **wire format**: rows
/// written today have to load after the enum gains a variant, and a derive would rename itself out from under
/// them the first time somebody reorders the definition.
fn serialise(query: &Query) -> Result<serde_json::Value, Error> {
    Ok(match query {
        Query::All => serde_json::json!({"kind": "all"}),
        Query::Text(text) => serde_json::json!({"kind": "text", "text": text}),
        Query::Field { key, op } => serde_json::json!({
            "kind": "field",
            "key": key,
            "op": serialise_op(op)?,
        }),
        Query::Term {
            term_id,
            include_descendants,
        } => serde_json::json!({
            "kind": "term",
            "term_id": term_id,
            "descendants": include_descendants,
        }),
        Query::InCollection(id) => serde_json::json!({"kind": "collection", "id": id}),
        Query::Rating(op) => serde_json::json!({"kind": "rating", "op": serialise_op(op)?}),
        // No identity, and that is the whole point. A saved search is shareable, so storing *who* "mine" meant
        // would make a colleague opening it see the author's favourites — the leak wearing the shape of a
        // bookmark this module's docs open with. Who is asking is resolved at evaluation time, from the caller.
        Query::Mine(state) => serde_json::json!({"kind": "mine", "state": state.as_str()}),
        Query::And(children) => serde_json::json!({
            "kind": "and",
            "children": children.iter().map(serialise).collect::<Result<Vec<_>, _>>()?,
        }),
        Query::Or(children) => serde_json::json!({
            "kind": "or",
            "children": children.iter().map(serialise).collect::<Result<Vec<_>, _>>()?,
        }),
        Query::Not(inner) => serde_json::json!({"kind": "not", "child": serialise(inner)?}),
    })
}

fn serialise_op(op: &dam_core::query::Comparison) -> Result<serde_json::Value, Error> {
    use dam_core::query::Comparison;
    Ok(match op {
        Comparison::Equals(literal) => {
            serde_json::json!({"op": "eq", "value": serialise_literal(literal)})
        }
        Comparison::NotEquals(literal) => {
            serde_json::json!({"op": "ne", "value": serialise_literal(literal)})
        }
        Comparison::Exists => serde_json::json!({"op": "exists"}),
        Comparison::Missing => serde_json::json!({"op": "missing"}),
        Comparison::Contains(text) => serde_json::json!({"op": "contains", "value": text}),
        Comparison::StartsWith(text) => serde_json::json!({"op": "starts_with", "value": text}),
        Comparison::Range { lower, upper } => serde_json::json!({
            "op": "range",
            "lower": serialise_endpoint(lower),
            "upper": serialise_endpoint(upper),
        }),
    })
}

fn serialise_endpoint(endpoint: &dam_core::query::Endpoint) -> serde_json::Value {
    use dam_core::query::Endpoint;
    match endpoint {
        Endpoint::Unbounded => serde_json::json!({"bound": "unbounded"}),
        Endpoint::Inclusive(literal) => {
            serde_json::json!({"bound": "inclusive", "value": serialise_literal(literal)})
        }
        Endpoint::Exclusive(literal) => {
            serde_json::json!({"bound": "exclusive", "value": serialise_literal(literal)})
        }
    }
}

fn serialise_literal(literal: &dam_core::query::Literal) -> serde_json::Value {
    use dam_core::query::Literal;
    // The type is tagged rather than inferred from the JSON shape. A bare `2026` could be an int, a decimal or
    // a year in a text field, and guessing on load would compare the wrong column type.
    match literal {
        Literal::Text(text) => serde_json::json!({"type": "text", "v": text}),
        Literal::Int(n) => serde_json::json!({"type": "int", "v": n}),
        Literal::Decimal(n) => serde_json::json!({"type": "decimal", "v": n}),
        Literal::Bool(b) => serde_json::json!({"type": "bool", "v": b}),
        Literal::Date(d) => {
            serde_json::json!({"type": "date", "v": d.format("%Y-%m-%d").to_string()})
        }
        Literal::DateTime(at) => serde_json::json!({"type": "datetime", "v": at.to_rfc3339()}),
        Literal::Uuid(id) => serde_json::json!({"type": "uuid", "v": id}),
    }
}

fn deserialise(value: &serde_json::Value) -> Result<Query, Error> {
    let bad = |what: &str| {
        Error::Core(dam_core::Error::Validation {
            field: "saved_search.query".into(),
            reason: format!("stored query is not readable: {what}"),
        })
    };
    let kind = value["kind"].as_str().ok_or_else(|| bad("no kind"))?;
    Ok(match kind {
        "all" => Query::All,
        "text" => Query::Text(
            value["text"]
                .as_str()
                .ok_or_else(|| bad("text without a string"))?
                .to_owned(),
        ),
        "field" => Query::Field {
            key: value["key"]
                .as_str()
                .ok_or_else(|| bad("field without a key"))?
                .to_owned(),
            op: deserialise_op(&value["op"])?,
        },
        "term" => Query::Term {
            term_id: serde_json::from_value(value["term_id"].clone())
                .map_err(|_| bad("term without a uuid"))?,
            include_descendants: value["descendants"].as_bool().unwrap_or(true),
        },
        "collection" => Query::InCollection(
            serde_json::from_value(value["id"].clone())
                .map_err(|_| bad("collection without a uuid"))?,
        ),
        "rating" => Query::Rating(deserialise_op(&value["op"])?),
        "mine" => Query::Mine(
            match value["state"]
                .as_str()
                .ok_or_else(|| bad("mine without a state"))?
            {
                "favourite" => dam_core::query::Personal::Favourite,
                "watched" => dam_core::query::Personal::Watched,
                "rated" => dam_core::query::Personal::Rated,
                other => return Err(bad(&format!("unknown personal state {other:?}"))),
            },
        ),
        "and" | "or" => {
            let children = value["children"]
                .as_array()
                .ok_or_else(|| bad("a junction without children"))?
                .iter()
                .map(deserialise)
                .collect::<Result<Vec<_>, _>>()?;
            if kind == "and" {
                Query::And(children)
            } else {
                Query::Or(children)
            }
        }
        "not" => Query::Not(Box::new(deserialise(&value["child"])?)),
        // An unknown kind is refused rather than treated as `All`. `All` would turn an unreadable saved search
        // into "every asset", which is the widest possible answer to a query nobody can read.
        other => return Err(bad(&format!("unknown kind {other:?}"))),
    })
}

fn deserialise_op(value: &serde_json::Value) -> Result<dam_core::query::Comparison, Error> {
    use dam_core::query::Comparison;
    let bad = |what: &str| {
        Error::Core(dam_core::Error::Validation {
            field: "saved_search.query".into(),
            reason: format!("stored comparison is not readable: {what}"),
        })
    };
    let op = value["op"].as_str().ok_or_else(|| bad("no op"))?;
    Ok(match op {
        "eq" => Comparison::Equals(deserialise_literal(&value["value"])?),
        "ne" => Comparison::NotEquals(deserialise_literal(&value["value"])?),
        "exists" => Comparison::Exists,
        "missing" => Comparison::Missing,
        "contains" => Comparison::Contains(
            value["value"]
                .as_str()
                .ok_or_else(|| bad("contains without text"))?
                .to_owned(),
        ),
        "starts_with" => Comparison::StartsWith(
            value["value"]
                .as_str()
                .ok_or_else(|| bad("starts_with without text"))?
                .to_owned(),
        ),
        "range" => Comparison::Range {
            lower: deserialise_endpoint(&value["lower"])?,
            upper: deserialise_endpoint(&value["upper"])?,
        },
        other => return Err(bad(&format!("unknown op {other:?}"))),
    })
}

fn deserialise_endpoint(value: &serde_json::Value) -> Result<dam_core::query::Endpoint, Error> {
    use dam_core::query::Endpoint;
    let bad = || {
        Error::Core(dam_core::Error::Validation {
            field: "saved_search.query".into(),
            reason: "stored range endpoint is not readable".to_owned(),
        })
    };
    Ok(match value["bound"].as_str().ok_or_else(bad)? {
        "unbounded" => Endpoint::Unbounded,
        "inclusive" => Endpoint::Inclusive(deserialise_literal(&value["value"])?),
        "exclusive" => Endpoint::Exclusive(deserialise_literal(&value["value"])?),
        _ => return Err(bad()),
    })
}

fn deserialise_literal(value: &serde_json::Value) -> Result<dam_core::query::Literal, Error> {
    use dam_core::query::Literal;
    let bad = |what: &str| {
        Error::Core(dam_core::Error::Validation {
            field: "saved_search.query".into(),
            reason: format!("stored literal is not readable: {what}"),
        })
    };
    let ty = value["type"].as_str().ok_or_else(|| bad("no type tag"))?;
    let v = &value["v"];
    Ok(match ty {
        "text" => Literal::Text(v.as_str().ok_or_else(|| bad("text"))?.to_owned()),
        "int" => Literal::Int(v.as_i64().ok_or_else(|| bad("int"))?),
        "decimal" => Literal::Decimal(v.as_f64().ok_or_else(|| bad("decimal"))?),
        "bool" => Literal::Bool(v.as_bool().ok_or_else(|| bad("bool"))?),
        "date" => Literal::Date(
            chrono::NaiveDate::parse_from_str(v.as_str().unwrap_or_default(), "%Y-%m-%d")
                .map_err(|_| bad("date"))?,
        ),
        "datetime" => Literal::DateTime(
            chrono::DateTime::parse_from_rfc3339(v.as_str().unwrap_or_default())
                .map_err(|_| bad("datetime"))?
                .into(),
        ),
        "uuid" => Literal::Uuid(serde_json::from_value(v.clone()).map_err(|_| bad("uuid"))?),
        other => return Err(bad(&format!("unknown literal type {other:?}"))),
    })
}

type SavedRow = (
    Uuid,
    Option<Uuid>,
    String,
    serde_json::Value,
    bool,
    bool,
    Vec<String>,
    Option<Uuid>,
    Option<i64>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

fn into_search(row: SavedRow) -> SavedSearch {
    let (
        id,
        owner_id,
        name,
        query,
        is_smart_collection,
        shared,
        shared_with_roles,
        notify_path_id,
        result_count,
        counted_at,
        last_used_at,
    ) = row;
    SavedSearch {
        id,
        owner_id,
        name,
        query,
        is_smart_collection,
        shared,
        shared_with_roles,
        notify_path_id,
        result_count,
        counted_at,
        last_used_at,
    }
}
