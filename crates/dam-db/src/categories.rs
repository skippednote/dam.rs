//! Categories: the tree assets are filed in (Q.2).
//!
//! ## Not a new hierarchy
//!
//! Everything structural for this already existed. `taxonomies.kind` admits `'category'` alongside
//! `'vocabulary'` and `'product_attribute'`; `taxonomy_terms` carries an ltree `path` with a GiST index;
//! `asset_tags` is the asset↔term join with a state and a source; and [`crate::query_sql`]'s `push_term`
//! already filters by a term *including its descendants*. Building a parallel `categories` table would have
//! duplicated the ltree handling, the supersession chain, the term embeddings and the merge logic — four
//! places to keep in step instead of one.
//!
//! So this module is the two things that were missing: reading the tree as a tree, and putting an asset in it.
//!
//! ## Why a category is not a vocabulary
//!
//! A vocabulary is a *field's value set*, reached through a `taxonomy_ref` field: "which colours is this?".
//! A category is *where the asset is filed*: "this belongs under Exterior → Yellow". `kind` keeps them apart
//! so the browse tree does not offer "Colours" as a branch, and so a field picker does not offer "Exterior"
//! as a value.
//!
//! ## Counts respect the caller, and count each asset once
//!
//! `taxonomy_terms.asset_count` is a denormalised global number, and it is therefore the wrong number for a
//! scoped caller: §7 says counts disclose, so showing it would tell somebody how much of the library they
//! cannot see. Counts here run through the same `Planned` the search does. And a rollup counts *distinct*
//! assets beneath a branch, because an asset filed under two leaves of one branch would otherwise make
//! "Exterior (7)" appear over a library of five.

use crate::Error;
use dam_core::query::Planned;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

/// The columns every category read selects, in order.
///
/// A named alias rather than the tuple repeated three times: the three readers must agree on the shape, and a
/// column added to one of them is a bug the compiler should catch rather than a silent mismatch.
type CategoryRow = (
    Uuid,
    Option<Uuid>,
    String,
    String,
    String,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// Why a category operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum CategoryRefusal {
    #[error("taxonomy {0} is not a category tree")]
    NotACategoryTree(Uuid),

    #[error("no category {0} exists")]
    UnknownCategory(Uuid),

    #[error("category {0} is retired and cannot take new assets")]
    Retired(Uuid),

    #[error("a category `{0}` already exists under that parent")]
    DuplicatePath(String),

    #[error(transparent)]
    Database(#[from] Error),
}

/// A category tree — a taxonomy whose `kind` is `category`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryTree {
    pub id: Uuid,
    pub key: String,
    pub label: String,
}

/// One node of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryNode {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    /// The ltree path, exposed because the descendant filter uses it and a client linking to a subtree
    /// should not need a second round trip to learn it.
    pub path: String,
    pub slug: String,
    pub label: String,
    /// How deep, so a client indents without parsing `path`.
    pub depth: usize,
    pub retired: bool,
}

/// A node with the count of assets the caller can see beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountedCategory {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub path: String,
    pub slug: String,
    pub label: String,
    pub depth: usize,
    pub retired: bool,
    /// Distinct visible assets in this category *or any beneath it*.
    pub assets: i64,
}

/// A category to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCategory {
    pub taxonomy_id: Uuid,
    /// `None` makes it a root.
    pub parent_id: Option<Uuid>,
    /// The ltree label for this level. Must be an ltree-safe token.
    pub slug: String,
    pub label: String,
}

/// Creates a category tree.
pub async fn create_tree(
    conn: &mut sqlx::PgConnection,
    key: &str,
    label: &str,
) -> Result<Uuid, CategoryRefusal> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) VALUES (gen_random_uuid(), $1, $2, 'category') \
         RETURNING id",
    )
    .bind(key)
    .bind(label)
    .fetch_one(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(id)
}

/// Every category tree, by key.
pub async fn trees(conn: &mut sqlx::PgConnection) -> Result<Vec<CategoryTree>, CategoryRefusal> {
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, key, label FROM taxonomies WHERE kind = 'category' ORDER BY label, key",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(rows
        .into_iter()
        .map(|(id, key, label)| CategoryTree { id, key, label })
        .collect())
}

/// Creates a category under `parent_id`, or at the root.
pub async fn create(
    conn: &mut sqlx::PgConnection,
    spec: NewCategory,
) -> Result<Uuid, CategoryRefusal> {
    require_tree(&mut *conn, spec.taxonomy_id).await?;

    // The path is the parent's path plus this slug, which is what makes the descendant filter and the rollup
    // work. Built here rather than by a trigger so the failure is a named refusal instead of a constraint.
    let parent_path: Option<String> = match spec.parent_id {
        Some(parent) => {
            let row: Option<(String, Uuid)> =
                sqlx::query_as("SELECT path::text, taxonomy_id FROM taxonomy_terms WHERE id = $1")
                    .bind(parent)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(Error::from)?;
            let Some((path, taxonomy_id)) = row else {
                return Err(CategoryRefusal::UnknownCategory(parent));
            };
            if taxonomy_id != spec.taxonomy_id {
                // Re-parenting across trees would produce a path that no longer describes where the node
                // lives, and the rollup joins on `taxonomy_id` as well as path.
                return Err(CategoryRefusal::UnknownCategory(parent));
            }
            Some(path)
        }
        None => None,
    };

    let path = match &parent_path {
        Some(parent) => format!("{parent}.{}", spec.slug),
        None => spec.slug.clone(),
    };

    // Checked rather than left to the index, because a sibling slug clash is the ordinary mistake here and
    // "a category `yellow` already exists under that parent" is the only version of it anybody can act on.
    let clash: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM taxonomy_terms WHERE taxonomy_id = $1 AND path = text2ltree($2)",
    )
    .bind(spec.taxonomy_id)
    .bind(&path)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    if clash.is_some() {
        return Err(CategoryRefusal::DuplicatePath(path));
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, parent_id, path, slug, label) \
         VALUES (gen_random_uuid(), $1, $2, text2ltree($3), $4, $5) RETURNING id",
    )
    .bind(spec.taxonomy_id)
    .bind(spec.parent_id)
    .bind(&path)
    .bind(&spec.slug)
    .bind(&spec.label)
    .fetch_one(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(id)
}

/// The whole tree, depth-first with siblings alphabetical.
///
/// Ordering by `path` gives depth-first for free — that is what an ltree path is — and it means a client can
/// render the tree by walking the list once, using `depth` to indent.
pub async fn tree(
    conn: &mut sqlx::PgConnection,
    taxonomy_id: Uuid,
) -> Result<Vec<CategoryNode>, CategoryRefusal> {
    require_tree(&mut *conn, taxonomy_id).await?;
    let rows: Vec<CategoryRow> = sqlx::query_as(
        "SELECT id, parent_id, path::text, slug, label, deprecated_at \
             FROM taxonomy_terms WHERE taxonomy_id = $1 ORDER BY path",
    )
    .bind(taxonomy_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(rows
        .into_iter()
        .map(
            |(id, parent_id, path, slug, label, deprecated_at)| CategoryNode {
                id,
                parent_id,
                depth: depth_of(&path),
                path,
                slug,
                label,
                retired: deprecated_at.is_some(),
            },
        )
        .collect())
}

/// The tree with, for each node, the number of distinct assets the caller can see at or beneath it.
///
/// One query rather than one per node: a tree of two hundred categories would otherwise be two hundred
/// round trips, each running the access predicate again.
pub async fn tree_with_counts(
    conn: &mut sqlx::PgConnection,
    taxonomy_id: Uuid,
    planned: &Planned,
) -> Result<Vec<CountedCategory>, CategoryRefusal> {
    let nodes = tree(&mut *conn, taxonomy_id).await?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    // `count(DISTINCT visible.id)` against an ancestor join: an asset filed under two leaves of one branch
    // counts once for that branch. A plain count would exceed the number of assets that exist, and nothing
    // on screen would reveal it.
    builder.push(
        ") SELECT ancestor.id, count(DISTINCT visible.id) AS n \
         FROM taxonomy_terms ancestor \
         LEFT JOIN taxonomy_terms tagged \
              ON tagged.path <@ ancestor.path AND tagged.taxonomy_id = ancestor.taxonomy_id \
         LEFT JOIN asset_tags at ON at.term_id = tagged.id AND at.state = 'confirmed' \
         LEFT JOIN visible ON visible.id = at.asset_id \
         WHERE ancestor.taxonomy_id = ",
    );
    builder.push_bind(taxonomy_id);
    builder.push(" GROUP BY ancestor.id");

    let counts: Vec<(Uuid, i64)> = builder
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;

    Ok(nodes
        .into_iter()
        .map(|node| {
            let assets = counts
                .iter()
                .find(|(id, _)| *id == node.id)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            CountedCategory {
                id: node.id,
                parent_id: node.parent_id,
                path: node.path,
                slug: node.slug,
                label: node.label,
                depth: node.depth,
                retired: node.retired,
                assets,
            }
        })
        .collect())
}

/// One category by its ltree path within a tree.
pub async fn by_path(
    conn: &mut sqlx::PgConnection,
    taxonomy_id: Uuid,
    path: &str,
) -> Result<Option<CategoryNode>, CategoryRefusal> {
    let row: Option<CategoryRow> = sqlx::query_as(
        "SELECT id, parent_id, path::text, slug, label, deprecated_at FROM taxonomy_terms \
             WHERE taxonomy_id = $1 AND path = text2ltree($2)",
    )
    .bind(taxonomy_id)
    .bind(path)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(row.map(node))
}

/// One category by id.
pub async fn by_id(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
) -> Result<Option<CategoryNode>, CategoryRefusal> {
    let row: Option<CategoryRow> = sqlx::query_as(
        "SELECT id, parent_id, path::text, slug, label, deprecated_at FROM taxonomy_terms \
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(row.map(node))
}

/// Files an asset in a category.
///
/// Written as `confirmed`/`human`, not `suggested`: filing something is a decision, and a suggested row would
/// sit in the AI review queue waiting for somebody to approve what a person already did. `reviewed_by` records
/// who, which is what makes the placement auditable.
///
/// Idempotent — filing is a state, not an event, so the same asset in the same category twice is one
/// placement rather than a unique violation surfacing as a 500. An upsert also means a person filing something
/// a model had merely suggested *confirms* that row, keeping one history instead of two.
pub async fn file(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    category_id: Uuid,
    by: Option<Uuid>,
) -> Result<(), CategoryRefusal> {
    let row: Option<Option<chrono::DateTime<chrono::Utc>>> =
        sqlx::query_scalar("SELECT deprecated_at FROM taxonomy_terms WHERE id = $1")
            .bind(category_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(Error::from)?;
    let Some(deprecated_at) = row else {
        return Err(CategoryRefusal::UnknownCategory(category_id));
    };
    if deprecated_at.is_some() {
        // Retiring a category is how a tree gets tidied. If filing still worked, the tidy-up would never
        // finish — somebody would keep adding to the branch somebody else was trying to empty.
        return Err(CategoryRefusal::Retired(category_id));
    }

    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source, reviewed_by, reviewed_at) \
         VALUES ($1, $2, 'confirmed', 'human', $3, now()) \
         ON CONFLICT (asset_id, term_id) DO UPDATE \
           SET state = 'confirmed', source = 'human', reviewed_by = $3, reviewed_at = now()",
    )
    .bind(asset_id)
    .bind(category_id)
    .bind(by)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(())
}

/// Removes one placement.
///
/// Deletes rather than marking rejected. Rejection is model feedback and belongs to the tag-review surface;
/// taking an asset out of a category is filing, and conflating the two would teach the tagger from an action
/// that said nothing about whether its suggestion was any good.
///
/// Unfiling something that was never filed succeeds: the caller's intent is "not in this category", and that
/// already holds.
pub async fn unfile(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    category_id: Uuid,
) -> Result<(), CategoryRefusal> {
    sqlx::query("DELETE FROM asset_tags WHERE asset_id = $1 AND term_id = $2")
        .bind(asset_id)
        .bind(category_id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    Ok(())
}

/// The categories an asset is filed in, deepest first.
///
/// Deepest first so a breadcrumb or a chip list renders in the order a reader expects without sorting.
pub async fn of_asset(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
) -> Result<Vec<CategoryNode>, CategoryRefusal> {
    let rows: Vec<CategoryRow> = sqlx::query_as(
        "SELECT t.id, t.parent_id, t.path::text, t.slug, t.label, t.deprecated_at \
             FROM asset_tags at \
             JOIN taxonomy_terms t ON t.id = at.term_id \
             JOIN taxonomies x ON x.id = t.taxonomy_id AND x.kind = 'category' \
             WHERE at.asset_id = $1 AND at.state = 'confirmed' \
             ORDER BY nlevel(t.path) DESC, t.path",
    )
    .bind(asset_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(rows
        .into_iter()
        .map(
            |(id, parent_id, path, slug, label, deprecated_at)| CategoryNode {
                id,
                parent_id,
                depth: depth_of(&path),
                path,
                slug,
                label,
                retired: deprecated_at.is_some(),
            },
        )
        .collect())
}

/// The assets filed directly in one category.
pub async fn assets_in(
    conn: &mut sqlx::PgConnection,
    category_id: Uuid,
) -> Result<Vec<Uuid>, CategoryRefusal> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT at.asset_id FROM asset_tags at \
         JOIN assets a ON a.id = at.asset_id AND a.deleted_at IS NULL \
         WHERE at.term_id = $1 AND at.state = 'confirmed' ORDER BY a.created_at",
    )
    .bind(category_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(ids)
}

/// How many visible assets are in no category of this tree, and a sample of them.
///
/// The query that makes categories enforceable rather than decorative: the comparator surfaces it on its admin
/// dashboard as a number with a link, and a library where filing is optional *and unmeasured* is one where
/// filing quietly stops happening. Scoped through the caller's plan for the same reason the counts are —
/// telling somebody "61 uncategorised" when they can see nine of them is a disclosure, not a worklist.
pub async fn uncategorised(
    conn: &mut sqlx::PgConnection,
    taxonomy_id: Uuid,
    planned: &Planned,
    sample: i64,
) -> Result<(i64, Vec<Uuid>), CategoryRefusal> {
    require_tree(&mut *conn, taxonomy_id).await?;

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "WITH visible AS (SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    crate::query_sql::push_where(&mut builder, planned)?;
    builder.push(
        "), filed AS (SELECT DISTINCT at.asset_id FROM asset_tags at \
             JOIN taxonomy_terms t ON t.id = at.term_id \
             WHERE t.taxonomy_id = ",
    );
    builder.push_bind(taxonomy_id);
    builder.push(
        " AND at.state = 'confirmed') \
         SELECT visible.id FROM visible \
         LEFT JOIN filed ON filed.asset_id = visible.id \
         WHERE filed.asset_id IS NULL ORDER BY visible.id",
    );

    // The full id list, then the count from its length: the count and the sample must agree, and two queries
    // could disagree if something is filed between them — reporting "61" beside a sample drawn from 60.
    let ids: Vec<Uuid> = builder
        .build_query_scalar()
        .fetch_all(&mut *conn)
        .await
        .map_err(Error::from)?;
    let total = i64::try_from(ids.len()).unwrap_or(i64::MAX);
    let take = usize::try_from(sample.max(0)).unwrap_or(0);
    Ok((total, ids.into_iter().take(take).collect()))
}

/// Refuses a taxonomy that is not a category tree.
async fn require_tree(
    conn: &mut sqlx::PgConnection,
    taxonomy_id: Uuid,
) -> Result<(), CategoryRefusal> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM taxonomies WHERE id = $1")
        .bind(taxonomy_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?;
    match kind.as_deref() {
        Some("category") => Ok(()),
        // Both "no such taxonomy" and "a vocabulary" answer the same way, because from the caller's side the
        // request is the same mistake: this id is not a category tree.
        _ => Err(CategoryRefusal::NotACategoryTree(taxonomy_id)),
    }
}

/// One selected row as a node.
///
/// Shared by every reader so the depth and the retired flag are derived in exactly one place; three copies of
/// this mapping is three chances for them to drift.
fn node(row: CategoryRow) -> CategoryNode {
    let (id, parent_id, path, slug, label, deprecated_at) = row;
    CategoryNode {
        id,
        parent_id,
        depth: depth_of(&path),
        path,
        slug,
        label,
        retired: deprecated_at.is_some(),
    }
}

/// Depth from an ltree path: `exterior` is 0, `exterior.yellow` is 1.
fn depth_of(path: &str) -> usize {
    path.matches('.').count()
}
