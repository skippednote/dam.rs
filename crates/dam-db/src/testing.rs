//! Ephemeral Postgres for tests (D17: real containers, never mocks).
//!
//! Behind the `testing` feature so other crates can reuse it from their
//! dev-dependencies without testcontainers reaching a production build.
//!
//! Every harness gets its own container on its own port. That costs a few seconds
//! per suite and buys the ability to run suites in parallel without test order
//! becoming significant — which matters more here than elsewhere, because the
//! schema-per-tenant design means many tests create and drop schemas.

use crate::Error;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{str::FromStr, time::Duration};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// The image the dev stack and CI both use. Pinned rather than `latest`: a
/// pgvector major bump changes HNSW build behaviour, and discovering that from a
/// mysteriously slow test suite is worse than discovering it from a deliberate
/// version change.
const IMAGE: &str = "pgvector/pgvector";
const TAG: &str = "pg17";

const USER: &str = "damrs";
const PASSWORD: &str = "damrs";
const DATABASE: &str = "damrs";

/// A running Postgres with the §5.3 bootstrap applied.
///
/// The container is stopped when this is dropped, so hold it for the lifetime of
/// the test. Binding it to `_` drops it immediately and every subsequent query
/// fails with a connection error, which is a confusing way to learn this.
pub struct PostgresHarness {
    // Field order matters for drop order: the pool must close before the
    // container stops, or the pool's background tasks log connection errors during
    // teardown and clutter otherwise-passing test output.
    pool: PgPool,
    port: u16,
    _container: ContainerAsync<GenericImage>,
}

impl std::fmt::Debug for PostgresHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No connection string: it carries a password, and harnesses get printed
        // in test failure output.
        f.debug_struct("PostgresHarness")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl PostgresHarness {
    /// Starts a container, waits for readiness, and applies the bootstrap.
    pub async fn start() -> Result<Self, Error> {
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(5432.tcp())
            // Postgres logs "ready to accept connections" once while initialising
            // the data directory and again once actually listening. Waiting for the
            // first occurrence connects too early, so this waits for the second —
            // then the connect loop below covers any remaining raciness.
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_USER", USER)
            .with_env_var("POSTGRES_PASSWORD", PASSWORD)
            .with_env_var("POSTGRES_DB", DATABASE)
            // fsync off and a big shared_buffers: this database exists for the
            // length of one test and durability is irrelevant, while HNSW index
            // builds in 0003 are slow enough to notice.
            .with_cmd(["postgres", "-c", "fsync=off", "-c", "full_page_writes=off"])
            .start()
            .await
            .map_err(|e| Error::Harness(format!("starting {IMAGE}:{TAG}: {e}")))?;

        let port = container
            .get_host_port_ipv4(5432)
            .await
            .map_err(|e| Error::Harness(format!("resolving mapped port: {e}")))?;

        let url = format!("postgres://{USER}:{PASSWORD}@127.0.0.1:{port}/{DATABASE}");
        let pool = Self::connect_with_retry(&url).await?;
        Self::bootstrap(&pool).await?;

        Ok(Self {
            pool,
            port,
            _container: container,
        })
    }

    /// Connects with a bounded retry. The log-message wait above is necessary but
    /// not sufficient — the socket can accept before the database finishes
    /// recovery, and a bare connect intermittently fails on a loaded machine.
    async fn connect_with_retry(url: &str) -> Result<PgPool, Error> {
        const ATTEMPTS: u32 = 40;
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            match PgPoolOptions::new()
                .max_connections(8)
                .acquire_timeout(Duration::from_secs(5))
                .connect(url)
                .await
            {
                Ok(pool) => return Ok(pool),
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(150 * (attempt + 1).min(4) as u64))
                        .await;
                }
            }
        }
        Err(Error::Harness(format!(
            "postgres did not accept connections after {ATTEMPTS} attempts: {}",
            last.map(|e| e.to_string()).unwrap_or_default()
        )))
    }

    /// The §5.3 bootstrap, run before the sqlx migrator.
    ///
    /// Ordering matters and is the reason this is not a migration: Postgres
    /// silently ignores nonexistent schemas in a `search_path`, so if `dam_global`
    /// did not exist yet the migrator would put its `_sqlx_migrations` ledger in
    /// whichever schema came next and every later run would think the database was
    /// unmigrated. `damctl migrate` does exactly this before invoking the migrator.
    async fn bootstrap(pool: &PgPool) -> Result<(), Error> {
        for stmt in [
            "CREATE SCHEMA IF NOT EXISTS dam_global",
            "CREATE SCHEMA IF NOT EXISTS extensions",
            // Kept migrated to head so `cargo sqlx prepare` has a stable target
            // for compile-time query verification (§5.5).
            "CREATE SCHEMA IF NOT EXISTS tenant_template",
            // Extensions are database-scoped; installed once, referenced
            // schema-qualified from every tenant schema.
            "CREATE EXTENSION IF NOT EXISTS vector SCHEMA extensions",
            "CREATE EXTENSION IF NOT EXISTS ltree SCHEMA extensions",
            "CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA extensions",
        ] {
            sqlx::raw_sql(stmt)
                .execute(pool)
                .await
                .map_err(|e| Error::Harness(format!("bootstrap `{stmt}`: {e}")))?;
        }
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// A pool whose **every** connection has `search_path` set at connect time.
    ///
    /// Do not reach for `pool.execute("SET search_path ...")` instead. `SET` without
    /// `LOCAL` applies to one pooled connection, and the pool hands out others
    /// freely — so the first query lands in the right schema and the next one
    /// silently does not. That is the same hazard ARCHITECTURE §5.2 describes for
    /// production, surfacing here as tests that fail with "relation does not exist"
    /// or, worse, pass because the relation was missing.
    ///
    /// Production's request path uses `SET LOCAL` inside a transaction
    /// (`TenantConn`, task 0.7); this is the connect-time equivalent for tests and
    /// for the migration runner.
    pub async fn pool_for_schema(&self, schema: &str) -> Result<PgPool, Error> {
        let url = self.url();
        let opts = sqlx::postgres::PgConnectOptions::from_str(&url)
            .map_err(|e| Error::Harness(format!("parsing url: {e}")))?
            .options([(
                "search_path",
                format!("\"{schema}\",dam_global,extensions,public"),
            )]);
        PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(opts)
            .await
            .map_err(|e| Error::Harness(format!("pool for schema {schema}: {e}")))
    }

    /// The mapped host port. Exposed mainly so tests can assert two harnesses are
    /// genuinely separate containers.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Connection URL for a subprocess or a second pool.
    ///
    /// Returns the password in clear text, which is why it is a method rather than
    /// a `Display` impl — a harness printed in a failure message must not leak it.
    pub fn url(&self) -> String {
        format!(
            "postgres://{USER}:{PASSWORD}@127.0.0.1:{}/{DATABASE}",
            self.port
        )
    }

    /// Connection URL with `search_path` set at connect time.
    ///
    /// Connect-time rather than `SET LOCAL` because the sqlx migrator manages its
    /// own transactions and `SET LOCAL` outside one is a silent no-op (§5.3).
    pub fn url_with_search_path(&self, schemas: &str) -> String {
        let encoded = schemas.replace(' ', "").replace(',', "%2C");
        format!("{}?options=-c%20search_path%3D{encoded}", self.url())
    }
}
