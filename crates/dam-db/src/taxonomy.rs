//! Taxonomy lifecycle: move, merge, deprecate (2.2).
//!
//! Every operation here is destructive in its obvious implementation, and the thing being destroyed is
//! the meaning of assets a customer tagged years ago.
//!
//! - Deleting a term takes its `asset_tags` with it (`ON DELETE CASCADE`), so "retiring a term" silently
//!   untags every asset that used it. Nobody notices until a search comes back empty.
//! - Reparenting a term without moving its descendants leaves them on a path that no longer exists, so
//!   every ancestor query below it returns nothing — and returns it quietly.
//! - Hard-deleting after a merge breaks every id stored outside this database: a saved search, a Drupal
//!   field, an API client's cache.
//!
//! So terms are deprecated rather than deleted, a merge records where the meaning went, and a move is
//! one statement over the whole subtree. Each of those is why [`deprecate`], [`merge`] and [`move_term`]
//! exist instead of the callers writing their own UPDATE.
//!
//! ## Every operation runs in a transaction it opens itself
//!
//! Not for tidiness. A merge is a retag plus a deprecation, and a move is a path rewrite across N rows;
//! either one half-applied leaves the taxonomy in a state no query renders correctly. The functions take
//! a pool rather than an executor for exactly this reason — a caller cannot accidentally run one of these
//! outside a transaction, which is the mistake that would only show up under a failure.

use crate::Error as DbError;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// How long a supersession chain may be before it is treated as broken.
///
/// A vocabulary cleaned up once a month for eighty years would not reach this. It exists so a cycle that
/// somehow got past [`merge`]'s check cannot turn [`resolve`] into a hung request — bounding the walk is
/// cheaper than trusting that nothing ever wrote a cycle directly.
const MAX_CHAIN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no taxonomy term {0}")]
    NotFound(Uuid),

    #[error("term {id} is deprecated, so it cannot be {operation}")]
    Deprecated { id: Uuid, operation: &'static str },

    #[error(
        "term {target} is deprecated and cannot be the surviving term of a merge{}",
        match successor {
            Some(id) => format!(" — it was itself merged into {id}, which is probably the term you want"),
            None => String::new(),
        }
    )]
    TargetDeprecated {
        target: Uuid,
        /// Where the target's own meaning went, when it was merged rather than simply retired. Named in
        /// the message because "that term is deprecated" without it leaves the operator to go and look.
        successor: Option<Uuid>,
    },

    #[error(
        "terms {left} and {right} are in different taxonomies; merging or moving across a taxonomy \
         would change what an asset means, not just which term carries it"
    )]
    DifferentTaxonomies { left: Uuid, right: Uuid },

    #[error(
        "term {id} has {count} live child term(s); retire them first, so the tree never has active \
         terms hanging under a retired ancestor"
    )]
    HasLiveChildren { id: Uuid, count: i64 },

    #[error("that would create a cycle: {detail}")]
    WouldCycle { detail: String },

    #[error("path {path:?} already exists in this taxonomy")]
    PathTaken { path: String },

    #[error(transparent)]
    Database(#[from] DbError),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(DbError::from(error))
    }
}

type Result<T> = std::result::Result<T, Error>;

/// A term, and where its meaning currently lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTerm {
    /// The id that was asked for.
    pub id: Uuid,
    /// The term that carries the meaning now: `id` itself unless a merge moved it.
    ///
    /// Separate from `id` so a caller can both honour the reference and tell the user it has moved. One
    /// field for both would force a choice between working links and honest ones.
    pub effective_id: Uuid,
    pub taxonomy_id: Uuid,
    pub path: String,
    pub label: String,
    pub deprecated_at: Option<DateTime<Utc>>,
}

/// A term available for new assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignableTerm {
    pub id: Uuid,
    pub path: String,
    pub slug: String,
    pub label: String,
}

/// Resolves a term id, following any merges.
///
/// `Ok(None)` means no such term — a reference whose taxonomy was deleted outright. That is different
/// from a retired term, and reporting it as an error would make an ordinary stale reference look like a
/// fault.
pub async fn resolve(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<ResolvedTerm>> {
    let Some(row) = load(pool, id).await? else {
        return Ok(None);
    };

    // Walk to the end of the chain. A→B→C is ordinary: vocabularies get cleaned up more than once.
    let mut effective_id = row.id;
    let mut successor = row.superseded_by;
    let mut seen = 1usize;
    while let Some(next) = successor {
        if seen >= MAX_CHAIN {
            return Err(Error::WouldCycle {
                detail: format!(
                    "the supersession chain from {id} is longer than {MAX_CHAIN} hops, which means \
                     it loops"
                ),
            });
        }
        match load(pool, next).await? {
            Some(hop) => {
                effective_id = hop.id;
                successor = hop.superseded_by;
            }
            // The successor was deleted and `ON DELETE SET NULL` has not been reached, or the row went
            // in by hand. Stopping here leaves the term meaning itself, which is the safe answer.
            None => break,
        }
        seen += 1;
    }

    Ok(Some(ResolvedTerm {
        id: row.id,
        effective_id,
        taxonomy_id: row.taxonomy_id,
        path: row.path,
        label: row.label,
        deprecated_at: row.deprecated_at,
    }))
}

/// Every term in a taxonomy that may be assigned to an asset.
///
/// Deprecated terms are excluded. Resolvable and assignable are different questions: a picker that keeps
/// offering retired terms means the vocabulary never actually gets cleaned up, which is why somebody
/// retired one in the first place.
pub async fn assignable(pool: &sqlx::PgPool, taxonomy_id: Uuid) -> Result<Vec<AssignableTerm>> {
    let rows = sqlx::query_as::<_, (Uuid, String, String, String)>(
        "SELECT id, path::text, slug, label FROM taxonomy_terms \
         WHERE taxonomy_id = $1 AND deprecated_at IS NULL ORDER BY path",
    )
    .bind(taxonomy_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, path, slug, label)| AssignableTerm {
            id,
            path,
            slug,
            label,
        })
        .collect())
}

/// Retires a term from new assignment, leaving it fully resolvable.
///
/// Refuses while live children remain. Cascading instead would retire terms the operator never asked
/// about; leaving them would put active terms under a retired ancestor, which no rollup query renders
/// sensibly. Naming the problem is the only option that does not surprise someone.
pub async fn deprecate(pool: &sqlx::PgPool, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let term = require(&mut tx, id).await?;
    if term.deprecated_at.is_some() {
        // Idempotent: retiring a retired term is what a retried request looks like.
        tx.commit().await?;
        return Ok(());
    }

    let live_children: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM taxonomy_terms WHERE parent_id = $1 AND deprecated_at IS NULL",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if live_children > 0 {
        return Err(Error::HasLiveChildren {
            id,
            count: live_children,
        });
    }

    sqlx::query(
        "UPDATE taxonomy_terms SET deprecated_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Merges `from` into `into`: the assets move, and `from` is retired pointing at `into`.
///
/// The retag and the deprecation are one transaction. Half of this is worse than none — assets moved but
/// the source still live means two active terms for one concept, and the source retired but the assets
/// left behind means they are tagged with something no picker offers.
pub async fn merge(pool: &sqlx::PgPool, from: Uuid, into: Uuid) -> Result<()> {
    if from == into {
        return Err(Error::WouldCycle {
            detail: format!("term {from} cannot be merged into itself"),
        });
    }

    let mut tx = pool.begin().await?;
    let source = require(&mut tx, from).await?;
    let target = require(&mut tx, into).await?;

    if source.taxonomy_id != target.taxonomy_id {
        return Err(Error::DifferentTaxonomies {
            left: from,
            right: into,
        });
    }
    // The cycle check runs before the deprecated-target check, because both are true when someone tries
    // to close a loop — A is deprecated *because* it was merged — and "that would create a cycle" is the
    // specific diagnosis while "the target is deprecated" is the generic one.
    //
    // Walk the target's chain looking for the source. Without this, merging C into A after A→B→C makes
    // the chain loop, and `resolve` would walk it forever if it were not bounded.
    let mut cursor = target.superseded_by;
    let mut hops = 0usize;
    while let Some(next) = cursor {
        if next == from {
            return Err(Error::WouldCycle {
                detail: format!(
                    "term {into} already resolves through {from}, so merging {from} into {into} \
                     would close the loop"
                ),
            });
        }
        hops += 1;
        if hops >= MAX_CHAIN {
            return Err(Error::WouldCycle {
                detail: format!("the chain from {into} is already longer than {MAX_CHAIN} hops"),
            });
        }
        cursor = match load_tx(&mut tx, next).await? {
            Some(hop) => hop.superseded_by,
            None => None,
        };
    }

    if target.deprecated_at.is_some() {
        return Err(Error::TargetDeprecated {
            target: into,
            successor: target.superseded_by,
        });
    }

    // Drop the tags that would collide first. `asset_tags` is keyed on (asset_id, term_id), so an asset
    // already carrying both terms would make the UPDATE below violate the primary key and abandon the
    // whole merge — over an asset that already has the meaning being merged in.
    sqlx::query(
        "DELETE FROM asset_tags WHERE term_id = $1 \
           AND asset_id IN (SELECT asset_id FROM asset_tags WHERE term_id = $2)",
    )
    .bind(from)
    .bind(into)
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE asset_tags SET term_id = $2 WHERE term_id = $1")
        .bind(from)
        .bind(into)
        .execute(&mut *tx)
        .await?;

    // The other references to a term. Left as separate statements rather than a trigger so the set is
    // visible here: a table that starts pointing at terms and is not added to this list would silently
    // keep pointing at a retired term.
    sqlx::query("UPDATE tag_feedback SET term_id = $2 WHERE term_id = $1")
        .bind(from)
        .bind(into)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "UPDATE taxonomy_terms \
         SET deprecated_at = coalesce(deprecated_at, now()), superseded_by = $2, updated_at = now() \
         WHERE id = $1",
    )
    .bind(from)
    .bind(into)
    .execute(&mut *tx)
    .await?;

    // `asset_count` is denormalised and worker-maintained, but leaving it stale across a merge would
    // show a retired term still holding assets it no longer has.
    sqlx::query(
        "UPDATE taxonomy_terms SET asset_count = (\
             SELECT count(*) FROM asset_tags WHERE term_id = taxonomy_terms.id AND state = 'confirmed'\
         ) WHERE id = ANY($1)",
    )
    .bind(vec![from, into])
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// Reparents `id` under `new_parent`, or to the root when it is `None`.
///
/// The subtree moves with it, in one statement. Updating only the moved term is the classic version of
/// this bug: its descendants keep a path whose prefix no longer exists, so `path <@ 'outdoor'` stops
/// finding them and reports nothing rather than failing.
pub async fn move_term(pool: &sqlx::PgPool, id: Uuid, new_parent: Option<Uuid>) -> Result<()> {
    let mut tx = pool.begin().await?;
    let term = require(&mut tx, id).await?;
    if term.deprecated_at.is_some() {
        // Its path is what old rollup queries were written against, and nothing new can be assigned to
        // it, so reorganising it changes the shape of history for no benefit.
        return Err(Error::Deprecated {
            id,
            operation: "moved",
        });
    }

    let new_path = match new_parent {
        None => term.slug.clone(),
        Some(parent_id) => {
            if parent_id == id {
                return Err(Error::WouldCycle {
                    detail: format!("term {id} cannot be its own parent"),
                });
            }
            let parent = require(&mut tx, parent_id).await?;
            if parent.taxonomy_id != term.taxonomy_id {
                return Err(Error::DifferentTaxonomies {
                    left: id,
                    right: parent_id,
                });
            }
            // The parent must not be inside the subtree being moved. Otherwise the new path is computed
            // from the term itself and the subtree detaches from the tree entirely — every term in it
            // becomes unreachable from any ancestor query.
            if parent.path == term.path || parent.path.starts_with(&format!("{}.", term.path)) {
                return Err(Error::WouldCycle {
                    detail: format!(
                        "term {parent_id} at {:?} is inside the subtree of {id} at {:?}",
                        parent.path, term.path
                    ),
                });
            }
            format!("{}.{}", parent.path, term.slug)
        }
    };

    if new_path == term.path {
        tx.commit().await?;
        return Ok(());
    }

    // Checked before the UPDATE rather than caught after. `(taxonomy_id, path)` is unique, so a
    // collision would abort the statement — and the point of the check is the error, which names the
    // path instead of a constraint.
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM taxonomy_terms \
         WHERE taxonomy_id = $1 AND path = text2ltree($2) AND id <> $3)",
    )
    .bind(term.taxonomy_id)
    .bind(&new_path)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(Error::PathTaken { path: new_path });
    }

    // One statement for the term and every descendant. `@>` finds the subtree and the two-argument
    // `subpath` re-roots each path at the new prefix, so a 10,000-term subtree is one UPDATE rather
    // than 10,000 — and there is no window in which half of it has moved.
    let updated = sqlx::query(
        "UPDATE taxonomy_terms \
         SET path = text2ltree($2) || subpath(path, nlevel(text2ltree($3)) - 1), \
             updated_at = now() \
         WHERE taxonomy_id = $1 AND path <@ text2ltree($3)",
    )
    .bind(term.taxonomy_id)
    .bind(parent_prefix(&new_path))
    .bind(&term.path)
    .execute(&mut *tx)
    .await?;
    debug_assert!(
        updated.rows_affected() >= 1,
        "the term itself must have moved"
    );

    sqlx::query("UPDATE taxonomy_terms SET parent_id = $2 WHERE id = $1")
        .bind(id)
        .bind(new_parent)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

/// The path with its last label removed, or `""` for a root path.
///
/// `text2ltree('')` is the empty label path, and `'' || subpath(...)` yields the subpath unchanged —
/// which is exactly right for a move to the root.
fn parent_prefix(path: &str) -> String {
    match path.rfind('.') {
        Some(index) => path[..index].to_owned(),
        None => String::new(),
    }
}

/// The columns every operation here needs.
struct TermRow {
    id: Uuid,
    taxonomy_id: Uuid,
    path: String,
    slug: String,
    label: String,
    deprecated_at: Option<DateTime<Utc>>,
    superseded_by: Option<Uuid>,
}

const SELECT_TERM: &str = "SELECT id, taxonomy_id, path::text, slug, label, deprecated_at, \
                           superseded_by FROM taxonomy_terms WHERE id = $1";

type TermTuple = (
    Uuid,
    Uuid,
    String,
    String,
    String,
    Option<DateTime<Utc>>,
    Option<Uuid>,
);

fn to_row(tuple: TermTuple) -> TermRow {
    let (id, taxonomy_id, path, slug, label, deprecated_at, superseded_by) = tuple;
    TermRow {
        id,
        taxonomy_id,
        path,
        slug,
        label,
        deprecated_at,
        superseded_by,
    }
}

async fn load(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<TermRow>> {
    Ok(sqlx::query_as::<_, TermTuple>(SELECT_TERM)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(to_row))
}

async fn load_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> Result<Option<TermRow>> {
    Ok(sqlx::query_as::<_, TermTuple>(SELECT_TERM)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .map(to_row))
}

async fn require(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: Uuid) -> Result<TermRow> {
    load_tx(tx, id).await?.ok_or(Error::NotFound(id))
}

#[cfg(test)]
mod tests {
    use super::parent_prefix;

    #[test]
    fn a_root_path_has_an_empty_prefix() {
        // `text2ltree('')` concatenated with a subpath yields the subpath, which is what makes a move
        // to the root work through the same statement as any other move.
        assert_eq!(parent_prefix("beach"), "");
    }

    #[test]
    fn a_nested_path_drops_only_its_last_label() {
        assert_eq!(parent_prefix("outdoor.beach.sand"), "outdoor.beach");
        assert_eq!(parent_prefix("outdoor.beach"), "outdoor");
    }
}
