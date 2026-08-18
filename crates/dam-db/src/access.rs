//! Rendering the access predicate into SQL.
//!
//! One of §12's three consumers. The predicate is compiled in `dam_core::policy`; this turns it into a
//! `WHERE` fragment, and the Tantivy renderer turns the same value into a filter query. Neither decides
//! anything — that is the point of compiling once.
//!
//! ## Applied in the query, never after it
//!
//! §7 requires this and gives the reason: pagination counts alone disclose the existence of assets a
//! caller cannot see. A post-filter returns the same *rows* as an in-query filter, so the two are
//! indistinguishable until somebody compares a `count(*)` with the row set — which
//! `the_count_matches_the_row_set_so_pagination_cannot_leak` does.
//!
//! ## Nothing is interpolated
//!
//! Group ids reach the query through `push_bind`, so the fragment is injection-proof by construction
//! rather than by review. A variable-length `IN` list is one bind of a UUID array rather than N binds,
//! which also keeps the statement shape stable for the query planner.

use crate::Error;
use dam_core::policy::AccessPredicate;
use sqlx::{PgPool, Postgres, QueryBuilder};

/// Pushes the visibility condition for `predicate` onto a builder.
///
/// The caller supplies everything up to and including `WHERE`, and may push `ORDER BY` / `LIMIT`
/// afterwards. The fragment is always parenthesised, so appending `AND …` cannot change its meaning
/// through operator precedence.
pub fn push_asset_filter(
    builder: &mut QueryBuilder<Postgres>,
    predicate: &AccessPredicate,
) -> Result<(), Error> {
    // A predicate that matches nothing renders as a false condition rather than as an omitted filter.
    // The distinction is the whole safety property: an omitted group filter is a full scan of the
    // tenant's library, and it is one early `return` away.
    if predicate.matches_nothing() {
        builder.push("(false)");
        return Ok(());
    }

    builder.push("(assets.deleted_at IS NULL");

    if !predicate.all_groups() {
        // `IN (SELECT …)` rather than a join: a join returns the asset once per matching group, which
        // inflates counts and breaks pagination as soon as somebody grants overlapping groups.
        //
        // Note this excludes assets in *no* group for a non-administrator, which is intended: an
        // ungrouped asset has no scope, so nobody scoped to groups can see it. An administrator's
        // `all_asset_groups` branch skips this clause entirely, which is how a mis-grouped upload stays
        // reachable by the person who can fix it.
        builder.push(
            " AND assets.id IN (SELECT asset_id FROM asset_group_members WHERE group_id = ANY(",
        );
        builder.push_bind(predicate.allowed_groups().to_vec());
        builder.push("))");
    }

    // Release and expiry are deliberately absent. They gate *distribution*, not visibility (decision 2):
    // somebody has to find an expired asset in order to renew its licence, and an asset that vanishes on
    // expiry is one nobody renews. The download path calls `policy::evaluate` per asset instead.
    builder.push(")");
    Ok(())
}

/// Refuses a predicate whose granted groups cannot yet be rendered.
///
/// Decision 4 says rule-based groups are evaluated live, and the language their predicates are written
/// in is the query IR — task 2.4, which does not exist yet. Ignoring a group's predicate would grant
/// *less* access than the administrator configured: fail-closed, but silently, so the first anyone would
/// know is an asset that should have been visible and was not. Refusing names the gap instead.
///
/// Separate from [`push_asset_filter`] because it needs a round trip to the database, and the renderer
/// must stay a pure function over the predicate.
pub async fn check_groups_are_renderable(
    pool: &PgPool,
    predicate: &AccessPredicate,
) -> Result<(), Error> {
    if predicate.all_groups() || predicate.allowed_groups().is_empty() {
        return Ok(());
    }

    let rule_based: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM asset_groups WHERE id = ANY($1) AND predicate IS NOT NULL ORDER BY key",
    )
    .bind(predicate.allowed_groups().to_vec())
    .fetch_all(pool)
    .await?;

    if rule_based.is_empty() {
        Ok(())
    } else {
        Err(Error::Unsupported(format!(
            "asset group(s) {} carry a rule predicate, and evaluating one needs the query IR \
             (task 2.4). Refusing rather than ignoring it: ignoring would silently grant less access \
             than configured.",
            rule_based.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use dam_core::policy::{self, Action, Grant, Grants};
    use uuid::Uuid;

    fn predicate_for(groups: &[Uuid], all: bool, permission: &str) -> AccessPredicate {
        policy::compile(
            &Grants::from(vec![Grant {
                permissions: vec![permission.to_owned()],
                asset_group_ids: groups.to_vec(),
                all_asset_groups: all,
                valid_from: None,
                valid_until: None,
                requires_eula: false,
                eula_accepted: false,
            }]),
            Action::Read,
            Utc::now(),
        )
    }

    /// The statement text, for asserting on its shape.
    ///
    /// sqlx 0.9 returns a  rather than a  — it keeps static SQL borrowed to avoid an
    /// allocation per query, which is invisible until a test tries to own the result.
    /// The statement text, for asserting on its shape.
    ///
    /// sqlx 0.9 returns a SqlStr rather than a String: it keeps static SQL borrowed to avoid an
    /// allocation per query, which is invisible until a test tries to own the result.
    fn rendered(predicate: &AccessPredicate) -> String {
        let mut builder: QueryBuilder<Postgres> = QueryBuilder::new("SELECT 1 WHERE ");
        push_asset_filter(&mut builder, predicate).expect("render");
        builder.into_sql().as_str().to_owned()
    }

    #[test]
    fn an_empty_predicate_renders_a_false_condition_not_an_empty_string() {
        // An empty string would produce `SELECT 1 WHERE ` — a syntax error if lucky, and if the caller
        // had already appended `AND 1=1`, a query returning everything.
        let sql = rendered(&predicate_for(&[], false, "asset:read"));
        assert!(sql.contains("(false)"), "got {sql}");
    }

    #[test]
    fn no_uuid_is_ever_interpolated_into_the_sql() {
        // Injection-proofness by construction: the ids go through `push_bind`, so the statement text
        // contains a placeholder and never a value.
        let group = Uuid::from_u128(0x1234);
        let sql = rendered(&predicate_for(&[group], false, "asset:read"));
        assert!(!sql.contains(&group.to_string()), "got {sql}");
        assert!(sql.contains('$'), "expected a bind placeholder in {sql}");
    }

    #[test]
    fn the_fragment_is_parenthesised_so_appending_and_is_safe() {
        // Without the parentheses, `<fragment> AND foo` would bind as `a AND (b AND foo)` in some
        // shapes and silently change which rows match.
        let sql = rendered(&predicate_for(&[Uuid::from_u128(1)], false, "asset:read"));
        // `strip_prefix`, not `split("WHERE ")`: the fragment contains its own inner WHERE inside the
        // group subquery, so splitting truncated it mid-subquery and the assertion failed on a
        // half-fragment rather than on anything real.
        let fragment = sql
            .strip_prefix("SELECT 1 WHERE ")
            .expect("the harness builds this prefix");
        assert!(fragment.starts_with('('), "got {fragment}");
        assert!(fragment.trim_end().ends_with(')'), "got {fragment}");
    }

    #[test]
    fn an_administrator_gets_no_group_clause_at_all() {
        let sql = rendered(&predicate_for(&[], true, "asset:read"));
        assert!(!sql.contains("asset_group_members"), "got {sql}");
        assert!(sql.contains("deleted_at IS NULL"), "got {sql}");
    }

    #[test]
    fn release_and_expiry_never_appear_in_a_visibility_filter() {
        // Decision 2 as a structural assertion. Adding them here is the obvious optimisation and the
        // wrong one — it would make expired assets unfindable and therefore unrenewable.
        let sql = rendered(&predicate_for(&[Uuid::from_u128(1)], false, "asset:read"));
        assert!(!sql.contains("release_at"), "got {sql}");
        assert!(!sql.contains("expires_at"), "got {sql}");
    }
}
