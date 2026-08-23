//! Site branding: what a tenant's library calls itself, and what it looks like (Q.20d).
//!
//! A singleton, like `enrichment_settings`, created by its own migration so every reader can assume the row
//! exists. Two things it fixes: the application called itself "damrs" in the nav of every tenant's library —
//! a vendor's name where a customer's belongs — and every portal carried its own accent with our hard-coded
//! default, so a tenant with six press kits set the same colour six times.
//!
//! ## The name falls back rather than defaulting
//!
//! An empty `site_name` means "use the tenant's display name". A tenant that has never opened the branding
//! screen should see their own name, which they already gave us at provisioning — not a placeholder, and not
//! ours. That is why the fallback lives in [`Branding::name_or`] rather than in the column default: the column
//! cannot see `dam_global.tenants`, and a copy of the display name here would go stale the day it changed.

use crate::Error;
use uuid::Uuid;

/// What this tenant's library looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branding {
    /// Empty means fall back — see [`Branding::name_or`].
    pub site_name: String,
    pub logo_asset_id: Option<Uuid>,
    /// Lowercase `#rrggbb`, guaranteed by a CHECK constraint. Safe to interpolate into CSS *because* of that.
    pub accent: String,
    pub support_email: Option<String>,
}

impl Branding {
    /// The name to show, falling back to the tenant's display name.
    #[must_use]
    pub fn name_or(&self, tenant_display_name: &str) -> String {
        if self.site_name.trim().is_empty() {
            tenant_display_name.to_owned()
        } else {
            self.site_name.clone()
        }
    }
}

/// Reads the row. Present by construction — the migration inserts it.
///
/// Falls back to the defaults rather than erroring if it is somehow absent, because branding is decoration:
/// a library that will not load because nobody set a colour would be a worse failure than a blue accent.
pub async fn read(conn: &mut sqlx::PgConnection) -> Result<Branding, Error> {
    let row: Option<(String, Option<Uuid>, String, Option<String>)> = sqlx::query_as(
        "SELECT site_name, logo_asset_id, accent, support_email FROM site_branding WHERE id",
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map_or_else(
        || Branding {
            site_name: String::new(),
            logo_asset_id: None,
            accent: DEFAULT_ACCENT.to_owned(),
            support_email: None,
        },
        |(site_name, logo_asset_id, accent, support_email)| Branding {
            site_name,
            logo_asset_id,
            accent,
            support_email,
        },
    ))
}

/// The accent a tenant gets before choosing one, and the one a portal inherits.
///
/// Duplicated as a Rust constant *and* a column default deliberately: the column default is what the database
/// guarantees, and this is what a caller uses when there is no row to read. They must agree, which a test
/// asserts.
pub const DEFAULT_ACCENT: &str = "#2563eb";

/// Saves the branding.
///
/// Validates the accent here as well as in the CHECK, so a bad value is a sentence naming the format rather
/// than a constraint violation the caller has to decode. The CHECK stays because it is what makes the value
/// safe to interpolate into a stylesheet — a Rust check protects this path, and the constraint protects every
/// path including a future one nobody has written yet.
pub async fn write(conn: &mut sqlx::PgConnection, branding: &Branding) -> Result<(), Error> {
    let accent = branding.accent.trim().to_ascii_lowercase();
    if !is_hex_colour(&accent) {
        return Err(Error::Unsupported(format!(
            "{:?} is not a colour; use lowercase #rrggbb, as in {DEFAULT_ACCENT}",
            branding.accent
        )));
    }

    sqlx::query(
        "UPDATE site_branding \
         SET site_name = $1, logo_asset_id = $2, accent = $3, support_email = $4, updated_at = now() \
         WHERE id",
    )
    .bind(branding.site_name.trim())
    .bind(branding.logo_asset_id)
    .bind(&accent)
    .bind(
        branding
            .support_email
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Whether a string is a lowercase six-digit hex colour.
///
/// The same rule as the CHECK constraint, written out rather than delegated, because the point is to refuse it
/// before the database does and say something useful. Six digits only — three-digit shorthand is valid CSS and
/// deliberately not accepted, because two spellings of one colour makes comparing two tenants' settings a
/// string-normalisation problem for no benefit.
#[must_use]
pub fn is_hex_colour(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..]
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_ACCENT, is_hex_colour};

    #[test]
    fn the_colour_rule_matches_the_check_constraint() {
        // Lowercase six-digit hex, and nothing else. The uppercase and shorthand cases are the ones somebody
        // will type; the rest are what makes interpolating this into CSS safe.
        assert!(is_hex_colour("#2563eb"));
        assert!(is_hex_colour("#000000"));
        assert!(is_hex_colour("#ffffff"));
        assert!(is_hex_colour(DEFAULT_ACCENT));

        for refused in [
            "#2563EB",  // uppercase: normalised before this is called, refused if it reaches here
            "#25e",     // shorthand: valid CSS, deliberately not accepted
            "2563eb",   // no hash
            "#2563eb ", // trailing space
            "#2563ebb", // too long
            "#2563e",   // too short
            "#2563eg",  // not hex
            "red",
            "",
            // The reason the constraint exists at all.
            "#000;} body{display:none",
        ] {
            assert!(!is_hex_colour(refused), "{refused:?} should be refused");
        }
    }
}
