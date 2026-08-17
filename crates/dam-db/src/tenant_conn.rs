//! The only way to reach a tenant's schema.
//!
//! ## Why this type exists
//!
//! `SET LOCAL search_path` is transaction-scoped, which is exactly what we want: the
//! pooled connection returns to the pool clean, so no tenant's path can leak onto a
//! later request. But `SET LOCAL` **outside** a transaction is a silent no-op — it
//! emits a warning to a log nobody reads and leaves the path unchanged, so queries
//! run against whatever schema that connection last had. In a schema-per-tenant
//! system that is a cross-tenant read with no error attached to it.
//!
//! The compliance-gate suite demonstrated how easy the mistake is: setting
//! `search_path` on a *pool* affected one connection, later queries silently used
//! another, and three gate tests passed while proving nothing (DECISIONS.md,
//! 0.4/0.5/0.6).
//!
//! So this type has exactly one constructor, and it begins a transaction. There is no
//! `from_pool`, no `set_schema`, no way to hold a `TenantConn` that is not inside a
//! transaction. The invariant is structural rather than procedural.
//!
//! ## Cost
//!
//! Every tenant-scoped read runs in a transaction, including single-statement ones.
//! In Postgres that is close to free — a single-statement transaction is what an
//! autocommit statement already is — and it buys an invariant that cannot be
//! forgotten under deadline.

use crate::Error;
use dam_core::TenantSlug;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

/// A transaction whose `search_path` resolves one tenant's schema first.
///
/// Drop without [`Self::commit`] rolls back, which is sqlx's `Transaction` behaviour
/// and the right default: a handler that returns early on an error must not
/// half-apply its writes.
pub struct TenantConn<'p> {
    tx: Transaction<'p, Postgres>,
    schema: String,
}

impl std::fmt::Debug for TenantConn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantConn")
            .field("schema", &self.schema)
            .finish_non_exhaustive()
    }
}

impl<'p> TenantConn<'p> {
    /// Begins a transaction scoped to the tenant's schema.
    ///
    /// Fails if the schema does not exist. Checking up front is deliberate: the
    /// alternative is every subsequent query failing with its own confusing
    /// "relation does not exist", one at a time, while the real problem is that the
    /// tenant was never provisioned.
    pub async fn begin(pool: &'p PgPool, slug: &TenantSlug) -> Result<Self, Error> {
        let schema = slug.schema_name();
        let mut tx = pool.begin().await?;

        // Existence check inside the transaction, before the path is set — a missing
        // schema is silently ignored in a search_path, so setting it first would
        // hide the problem rather than surface it.
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = $1)")
                .bind(&schema)
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(Error::TenantNotProvisioned(schema));
        }

        // SET LOCAL, not SET: scoped to this transaction so nothing leaks back onto
        // the pooled connection. `dam_global` is on the path because tenant code
        // legitimately reads control-plane tables (storage_pools, feature_flags);
        // `extensions` because tenant tables reference `extensions.vector` and
        // `extensions.ltree`.
        //
        // The schema name is interpolated because SET LOCAL takes no bind
        // parameters. `TenantSlug` has already restricted it to lowercase ASCII,
        // digits, and underscore, and it is quoted here as well.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SET LOCAL search_path TO \"{schema}\", dam_global, extensions, public"
        )))
        .execute(&mut *tx)
        .await?;

        Ok(Self { tx, schema })
    }

    /// The executor to run tenant-scoped queries against.
    ///
    /// Named `executor` rather than `as_mut` so it does not shadow
    /// `std::convert::AsMut::as_mut` — and because it reads better at the call site:
    /// `.fetch_one(tc.executor())` says what the argument is for.
    pub fn executor(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// The schema this connection resolves. Useful in error messages and spans —
    /// it is a schema name, not tenant data, so it is safe to log.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub async fn commit(self) -> Result<(), Error> {
        self.tx.commit().await?;
        Ok(())
    }

    pub async fn rollback(self) -> Result<(), Error> {
        self.tx.rollback().await?;
        Ok(())
    }
}
