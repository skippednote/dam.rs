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
    /// This harness's own database. Distinct per harness in both modes — see [`SHARED_URL_ENV`].
    database: String,
    /// `None` when a shared server is being reused rather than a container started.
    _container: Option<ContainerAsync<GenericImage>>,
}

/// When set, harnesses create a database on this server instead of starting a container.
///
/// A container per harness is the right isolation and costs about ten seconds each; at thirty
/// container-backed suites that is most of the workspace test run, and the run had grown past the point
/// where it could be executed as one command. Creating a database on a warm server costs milliseconds.
///
/// The isolation is unchanged: each harness still gets a database of its own, so nothing is shared but
/// the process. What is given up is the guarantee that a test cannot affect a *server* another test is
/// using — which matters for exactly one thing, a test that deliberately breaks the server, and there
/// isn't one.
///
/// Pools are sized down in this mode. A harness opens two — its own and usually a `pool_for_schema` — and
/// eight connections each is generous for a test but ruinous shared: nineteen suites of several harneses
/// apiece exhausts any server's `max_connections`, and the symptom is `PoolTimedOut` in an unrelated suite.
///
/// **Deliberately not `DAMRS_`-prefixed.** `Config::load` claims that whole namespace and refuses unknown
/// keys, so a `DAMRS_TEST_PG_URL` in the environment does not merely get ignored — it makes every config
/// load fail with "unknown field". That strictness is right (a typo'd config key should not be silently
/// dropped) and it means a test-only variable has to live outside the prefix.
pub const SHARED_URL_ENV: &str = "DAM_TEST_PG_URL";

/// Connections per pool when a server is shared. See [`SHARED_URL_ENV`].
const SHARED_POOL_CONNECTIONS: u32 = 3;

/// Connections per pool when this harness owns its container, where nothing else is competing.
const OWNED_POOL_CONNECTIONS: u32 = 8;

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
    ///
    /// Reuses a shared server when [`SHARED_URL_ENV`] is set, creating a fresh database on it.
    pub async fn start() -> Result<Self, Error> {
        if let Ok(shared) = std::env::var(SHARED_URL_ENV) {
            return Self::start_on_shared(&shared).await;
        }
        Self::start_container().await
    }

    /// Creates a database on an already-running server.
    async fn start_on_shared(admin_url: &str) -> Result<Self, Error> {
        let admin = Self::connect_with_retry(admin_url).await?;

        // Named from a UUID rather than a counter: suites run as separate processes, so a counter would
        // collide across them and the collision would look like test pollution.
        let database = format!("damrs_t_{}", uuid::Uuid::new_v4().simple());
        // `AssertSqlSafe` because `CREATE DATABASE` takes no bind parameters. The name is a UUID's
        // hex rendering with a fixed prefix, so there is nothing here that came from outside this
        // function — which is the assertion the wrapper is asking for.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE \"{database}\""
        )))
        .execute(&admin)
        .await
        .map_err(|e| Error::Harness(format!("creating {database}: {e}")))?;
        admin.close().await;

        let opts = sqlx::postgres::PgConnectOptions::from_str(admin_url)
            .map_err(|e| Error::Harness(format!("parsing {SHARED_URL_ENV}: {e}")))?
            .database(&database);
        let port = opts.get_port();
        let pool = PgPoolOptions::new()
            .max_connections(Self::pool_size())
            .acquire_timeout(Duration::from_secs(20))
            .connect_with(opts)
            .await
            .map_err(|e| Error::Harness(format!("connecting to {database}: {e}")))?;
        Self::bootstrap(&pool).await?;

        // The database is deliberately left behind. Dropping it needs an async round trip and `Drop` is
        // synchronous; blocking in `Drop` inside a test runtime deadlocks. The shared server is
        // ephemeral — the task that started it removes it — so the databases go with it.
        Ok(Self {
            pool,
            port,
            database,
            _container: None,
        })
    }

    async fn start_container() -> Result<Self, Error> {
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(5432.tcp())
            // Postgres logs "ready to accept connections" once while initialising
            // the data directory and again once actually listening. Waiting for the
            // first occurrence connects too early, so this waits for the second —
            // then the connect loop below covers any remaining raciness.
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            // See the note in dam-store's harness: a full-workspace run starts several
            // containers concurrently and the default startup window is tight enough to flake.
            .with_startup_timeout(Duration::from_secs(120))
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
            database: DATABASE.to_owned(),
            _container: Some(container),
        })
    }

    /// Connects with a bounded retry. The log-message wait above is necessary but
    /// not sufficient — the socket can accept before the database finishes
    /// recovery, and a bare connect intermittently fails on a loaded machine.
    /// Connections per pool, which depends on whether the server is shared.
    fn pool_size() -> u32 {
        if std::env::var(SHARED_URL_ENV).is_ok() {
            SHARED_POOL_CONNECTIONS
        } else {
            OWNED_POOL_CONNECTIONS
        }
    }

    async fn connect_with_retry(url: &str) -> Result<PgPool, Error> {
        const ATTEMPTS: u32 = 40;
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            match PgPoolOptions::new()
                .max_connections(Self::pool_size())
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
            .max_connections(Self::pool_size())
            // Twenty seconds, not five. Under a shared server a pool can legitimately wait behind other
            // suites' connections, and a five-second timeout turns ordinary contention into a failure in
            // whichever suite happened to ask last.
            .acquire_timeout(Duration::from_secs(20))
            .connect_with(opts)
            .await
            .map_err(|e| Error::Harness(format!("pool for schema {schema}: {e}")))
    }

    /// The mapped host port.
    ///
    /// Not a proxy for "which harness is this": under [`SHARED_URL_ENV`] every harness reports the same
    /// port and they are still isolated. Use [`Self::database`] to tell two harnesses apart.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// This harness's database name. Distinct per harness in both modes.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// Connection URL for a subprocess or a second pool.
    ///
    /// Returns the password in clear text, which is why it is a method rather than
    /// a `Display` impl — a harness printed in a failure message must not leak it.
    pub fn url(&self) -> String {
        format!(
            "postgres://{USER}:{PASSWORD}@127.0.0.1:{}/{}",
            self.port, self.database
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
