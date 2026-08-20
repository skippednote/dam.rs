//! The two-track migration runner (ARCHITECTURE §5.3).
//!
//! Global and tenant migrations are independent version tracks. Global applies once
//! to `dam_global`; tenant applies to `tenant_template` and to every `t_*` schema,
//! each with its own `_sqlx_migrations` ledger.
//!
//! ## Why `search_path` is set at connect time
//!
//! The sqlx migrator manages its own transactions, and `SET LOCAL` outside a
//! transaction is a **silent no-op** — it emits a warning nobody sees and leaves the
//! path unchanged, so migrations would apply to whatever schema the connection last
//! had. Setting it via `PgConnectOptions::options` makes it a property of the
//! connection instead, which the migrator cannot undo.
//!
//! sqlx puts its ledger in the first schema on the path, which is what gives each
//! tenant an independent migration history for free.

use crate::Error;
use sqlx::{
    Connection, PgConnection,
    migrate::Migrator,
    postgres::{PgConnectOptions, PgSslMode},
};
use std::str::FromStr;

/// Control-plane migrations. Embedded at compile time, so a binary always carries
/// the migrations it was built with — no risk of a container running one version's
/// code against another version's SQL files.
static GLOBAL: Migrator = sqlx::migrate!("../../migrations/global");

/// Per-tenant migrations, applied N times.
static TENANT: Migrator = sqlx::migrate!("../../migrations/tenant");

/// The control-plane schema. Not configurable: it is baked into the migration SQL.
pub const GLOBAL_SCHEMA: &str = "dam_global";
/// Where `vector`, `ltree` and `pgcrypto` live.
pub const EXTENSIONS_SCHEMA: &str = "extensions";
/// Kept at head purely so `cargo sqlx prepare` has a stable verification target.
pub const TEMPLATE_SCHEMA: &str = "tenant_template";

/// Applies the global migrations.
///
/// The caller must have created `dam_global` and `extensions` first — Postgres
/// silently ignores nonexistent schemas in a `search_path`, so a missing
/// `dam_global` would put the ledger in whichever schema came next and every later
/// run would think the database was unmigrated. [`crate::testing::PostgresHarness`]
/// and `damctl migrate` both bootstrap before calling this.
pub async fn global(url: &str) -> Result<(), Error> {
    let mut conn = connect_with_search_path(url, GLOBAL_SCHEMA).await?;
    GLOBAL
        .run(&mut conn)
        .await
        .map_err(|e| Error::Migrate(format!("global: {e}")))?;
    conn.close()
        .await
        .map_err(|e| Error::Migrate(format!("closing global connection: {e}")))?;
    Ok(())
}

/// Applies the tenant migrations to one schema, creating it if absent.
///
/// `schema` must be a valid tenant schema name — `tenant_template`, or `t_` plus a
/// slug. It is interpolated into DDL, so it is validated here rather than trusted:
/// a caller that has not sanitised its input gets an error, not an injection.
pub async fn tenant(url: &str, schema: &str) -> Result<(), Error> {
    validate_schema_name(schema)?;

    // Create the schema on a plain connection first. It cannot be created by a
    // connection whose search_path already names it, and quoting is belt-and-braces
    // on top of the validation above.
    {
        let mut conn = connect(url).await?;
        // sqlx 0.9 requires dynamic SQL to be explicitly asserted safe, which is
        // the right ergonomics here: `schema` is validated against
        // ^t_[a-z][a-z0-9_]{1,38}$ above and double-quoted, so the only characters
        // that reach the DDL are lowercase ASCII, digits, and underscore.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\""
        )))
        .execute(&mut conn)
        .await
        .map_err(|e| Error::Migrate(format!("creating schema {schema}: {e}")))?;
        conn.close()
            .await
            .map_err(|e| Error::Migrate(format!("closing setup connection: {e}")))?;
    }

    let mut conn = connect_with_search_path(url, schema).await?;
    TENANT
        .run(&mut conn)
        .await
        .map_err(|e| Error::Migrate(format!("tenant {schema}: {e}")))?;
    conn.close()
        .await
        .map_err(|e| Error::Migrate(format!("closing tenant connection: {e}")))?;
    Ok(())
}

/// Applies tenant migrations to the template schema.
///
/// Its own function because the template is not a tenant — nothing is provisioned
/// into it and no tenant row points at it. It exists so compile-time query
/// verification has a schema at head to check against (§5.5).
pub async fn template(url: &str) -> Result<(), Error> {
    tenant(url, TEMPLATE_SCHEMA).await
}

/// Number of tenant migrations this binary carries. Used by `damctl` to report
/// whether a tenant is behind head without connecting to it twice.
pub fn tenant_migration_count() -> usize {
    TENANT.iter().count()
}

pub fn global_migration_count() -> usize {
    GLOBAL.iter().count()
}

/// Accepts `tenant_template` or `t_<slug>` where slug matches
/// `^[a-z][a-z0-9_]{1,38}$` — the same shape the `tenants` table enforces with a
/// CHECK constraint, so the two cannot disagree.
fn validate_schema_name(schema: &str) -> Result<(), Error> {
    if schema == TEMPLATE_SCHEMA {
        return Ok(());
    }
    let Some(slug) = schema.strip_prefix("t_") else {
        return Err(Error::Migrate(format!(
            "invalid tenant schema name {schema:?}: must be `{TEMPLATE_SCHEMA}` or start with `t_`"
        )));
    };
    let valid = (2..=39).contains(&slug.len().saturating_add(1))
        && slug.starts_with(|c: char| c.is_ascii_lowercase())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        return Err(Error::Migrate(format!(
            "invalid tenant schema name {schema:?}: slug must match ^[a-z][a-z0-9_]{{1,38}}$"
        )));
    }
    Ok(())
}

async fn connect(url: &str) -> Result<PgConnection, Error> {
    let opts = base_options(url)?;
    PgConnection::connect_with(&opts)
        .await
        .map_err(|e| Error::Migrate(format!("connecting: {e}")))
}

async fn connect_with_search_path(url: &str, first: &str) -> Result<PgConnection, Error> {
    let opts = base_options(url)?.options([(
        "search_path",
        format!("\"{first}\",{EXTENSIONS_SCHEMA},public"),
    )]);
    PgConnection::connect_with(&opts)
        .await
        .map_err(|e| Error::Migrate(format!("connecting with search_path={first}: {e}")))
}

fn base_options(url: &str) -> Result<PgConnectOptions, Error> {
    let opts = PgConnectOptions::from_str(url)
        .map_err(|e| Error::Migrate(format!("parsing connection url: {e}")))?;
    // `prefer` rather than `require`: the dev stack and testcontainers speak plain
    // TCP on loopback, while deployed Postgres should be behind TLS. Callers that
    // need to mandate it set `sslmode=require` in the URL, which wins over this.
    Ok(opts.ssl_mode(PgSslMode::Prefer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_schema_is_accepted() {
        assert!(validate_schema_name("tenant_template").is_ok());
    }

    #[test]
    fn well_formed_tenant_schemas_are_accepted() {
        for s in ["t_acme", "t_a", "t_acme_corp_2", "t_x9"] {
            assert!(validate_schema_name(s).is_ok(), "{s} should be valid");
        }
    }

    #[test]
    fn injection_shaped_and_malformed_names_are_rejected() {
        for s in [
            "t_acme; DROP SCHEMA dam_global CASCADE",
            "t_Acme",
            "t_1acme",
            "t_",
            "acme",
            "public",
            "",
            "t_acme-corp",
            "t_acme\"",
        ] {
            assert!(
                validate_schema_name(s).is_err(),
                "{s:?} should have been rejected"
            );
        }
    }

    #[test]
    fn a_slug_at_the_length_limit_is_accepted_and_one_past_is_not() {
        let ok = format!("t_a{}", "b".repeat(37)); // slug = 38 chars
        assert!(validate_schema_name(&ok).is_ok(), "{ok}");
        let too_long = format!("t_a{}", "b".repeat(38)); // slug = 39 chars
        assert!(validate_schema_name(&too_long).is_err(), "{too_long}");
    }

    #[test]
    fn the_embedded_migration_counts_match_the_files_on_disk() {
        // Guards against a migration file that was added but not committed, or a
        // stale build: the macro embeds at compile time, so a mismatch means the
        // binary and the repository disagree.
        assert_eq!(global_migration_count(), 3);
        assert_eq!(tenant_migration_count(), 29);
    }
}
