//! Which filters the refine-search rail offers, and in which order (Q.19).
//!
//! ## An entry is not a field
//!
//! The rail shows three kinds of thing — a facetable metadata field, a vocabulary, and one of the four
//! built-ins every library has (Q.15) — and a configuration that only knew about fields could not express "we
//! do not use ratings" or "put Campaign above Brand". So an entry is a *kind and a name*:
//! `field:brand`, `taxonomy:<uuid>`, `builtin:stars`. The kind prefix is what keeps a vocabulary called
//! `brand` from colliding with a field called `brand`.
//!
//! ## Absent means default
//!
//! A tenant that has never configured anything has no rows here, and gets the order the schema implies: every
//! facetable field by `display_order`, then every vocabulary by label, then the built-ins. Storing the default
//! as rows at provision time was the alternative and it is worse: a field defined next month would be missing
//! from a table that looked complete, and the rail would silently stop offering it.
//!
//! ## Disabling is not the same as un-facetable
//!
//! `field_defs.facetable` is a resource decision — faceting free text produces a bucket per distinct value —
//! and it governs whether the *count* may be computed at all. This table is presentation: an administrator
//! deciding a rail is too long. A field can be facetable and hidden here, which is how somebody keeps
//! `stars:4` typeable in the search box while taking the star rail off the screen.
use crate::Error;
use sqlx::Row as _;

/// One rail entry as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `field:<key>`, `taxonomy:<uuid>` or `builtin:<name>`.
    pub entry: String,
    pub position: i32,
    pub is_enabled: bool,
}

/// The tenant's configuration, in position order. Empty when nothing has been configured.
pub async fn read(conn: &mut sqlx::PgConnection) -> Result<Vec<Entry>, Error> {
    let rows = sqlx::query(
        "SELECT entry, position, is_enabled FROM search_facets ORDER BY position, entry",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .iter()
        .map(|row| Entry {
            entry: row.get("entry"),
            position: row.get("position"),
            is_enabled: row.get("is_enabled"),
        })
        .collect())
}

/// Replaces the whole configuration with `enabled`, in the order given.
///
/// A whole-list write rather than per-entry patches, because the order *is* the value: two clients each
/// moving one entry would otherwise produce an order neither of them asked for. Anything not named is written
/// as disabled rather than deleted, so "this tenant has decided about this entry" survives — a deleted row
/// reads as "never configured", which would make a hidden facet reappear the moment a default changed.
///
/// Positions are spaced by ten so a later insertion between two entries does not renumber the rest.
pub async fn replace(
    conn: &mut sqlx::PgConnection,
    enabled: &[String],
    known: &[String],
) -> Result<(), Error> {
    for (index, entry) in enabled.iter().enumerate() {
        let position = i32::try_from(index.saturating_mul(10)).unwrap_or(i32::MAX);
        sqlx::query(
            "INSERT INTO search_facets (entry, position, is_enabled) VALUES ($1, $2, true) \
             ON CONFLICT (entry) DO UPDATE \
                SET position = excluded.position, is_enabled = true, updated_at = now()",
        )
        .bind(entry)
        .bind(position)
        .execute(&mut *conn)
        .await?;
    }

    // Everything the rail *could* show and this list did not name is recorded as disabled, at a position past
    // the enabled ones so re-enabling it lands at the end rather than in the middle.
    let hidden: Vec<&String> = known.iter().filter(|one| !enabled.contains(one)).collect();
    for (index, entry) in hidden.iter().enumerate() {
        let position = i32::try_from(
            enabled
                .len()
                .saturating_add(index)
                .saturating_mul(10)
                .saturating_add(1000),
        )
        .unwrap_or(i32::MAX);
        sqlx::query(
            "INSERT INTO search_facets (entry, position, is_enabled) VALUES ($1, $2, false) \
             ON CONFLICT (entry) DO UPDATE \
                SET position = excluded.position, is_enabled = false, updated_at = now()",
        )
        .bind(entry)
        .bind(position)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Orders and filters `candidates` by the configuration.
///
/// Pure, and separate from the read for the reason every other renderer in this workspace is: the ordering
/// rule is the part worth testing without a database. Candidates the configuration does not mention keep their
/// incoming order and sit *after* everything configured — a field defined after somebody last touched the rail
/// appears rather than vanishing, which is the safe direction for a filter to be wrong in.
#[must_use]
pub fn arrange<T: Clone>(candidates: &[(String, T)], configured: &[Entry]) -> Vec<T> {
    let mut chosen: Vec<(i32, usize, T)> = Vec::new();
    for (index, (entry, value)) in candidates.iter().enumerate() {
        match configured.iter().find(|one| &one.entry == entry) {
            Some(one) if !one.is_enabled => {}
            Some(one) => chosen.push((one.position, index, value.clone())),
            // Unconfigured entries sort after configured ones, in their incoming order.
            None => chosen.push((i32::MAX, index, value.clone())),
        }
    }
    chosen.sort_by_key(|(position, index, _)| (*position, *index));
    chosen.into_iter().map(|(_, _, value)| value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(entries: &[(&str, i32, bool)]) -> Vec<Entry> {
        entries
            .iter()
            .map(|(entry, position, is_enabled)| Entry {
                entry: (*entry).to_owned(),
                position: *position,
                is_enabled: *is_enabled,
            })
            .collect()
    }

    #[test]
    fn nothing_configured_leaves_the_order_alone() {
        let candidates = vec![
            ("field:brand".to_owned(), "brand"),
            ("builtin:stars".to_owned(), "stars"),
        ];
        assert_eq!(arrange(&candidates, &[]), vec!["brand", "stars"]);
    }

    #[test]
    fn configuration_orders_and_hides() {
        let candidates = vec![
            ("field:brand".to_owned(), "brand"),
            ("builtin:stars".to_owned(), "stars"),
            ("field:campaign".to_owned(), "campaign"),
        ];
        let config = configured(&[
            ("field:campaign", 0, true),
            ("field:brand", 10, true),
            ("builtin:stars", 20, false),
        ]);
        assert_eq!(arrange(&candidates, &config), vec!["campaign", "brand"]);
    }

    #[test]
    fn a_field_defined_after_the_rail_was_configured_still_appears() {
        // The safe direction: a filter that vanishes because somebody configured the rail last year is a
        // filter nobody knows to ask for. It appears at the end, where an administrator can move it.
        let candidates = vec![
            ("field:brand".to_owned(), "brand"),
            ("field:new".to_owned(), "new"),
        ];
        let config = configured(&[("field:brand", 0, true)]);
        assert_eq!(arrange(&candidates, &config), vec!["brand", "new"]);
    }
}
