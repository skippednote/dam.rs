//! Configuration, loaded with precedence: **env > file > default**.
//!
//! That order is the whole reason this layer exists. An operator setting
//! `DAMRS_DATABASE__URL` in a container must beat the checked-in TOML, and both
//! must beat the built-in default. Nested keys use `__` as the separator, so
//! `DAMRS_DATABASE__MAX_CONNECTIONS` reaches `database.max_connections`.
//!
//! Secrets are typed as [`Secret`], so a `Debug` of the whole config — which is
//! exactly what gets logged at startup — cannot leak them.

use crate::{Error, Secret};
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Which deployment this is. Gates the production sanity checks in
/// [`Config::validate`] — a dev placeholder that reaches production should fail
/// startup, not emit a warning nobody reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Development,
    Test,
    Staging,
    Production,
}

impl Environment {
    pub fn is_production(self) -> bool {
        matches!(self, Self::Production)
    }
}

/// The dev-only default for the URL signing key. Named as a constant so
/// [`Config::validate`] can reject exactly this value in production rather than
/// guessing at what looks insecure.
pub const DEV_SIGNING_KEY: &str = "dev-insecure-signing-key";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub environment: Environment,
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    pub search: SearchConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// HMAC key for signed transform URLs (ARCHITECTURE §10). Shared with
    /// connectors so they can sign render URLs locally without an API call.
    pub url_signing_key: Secret<String>,
    /// Wall-clock budget for a single request, in seconds.
    pub request_timeout_secs: u64,
    /// Origins the browser API accepts in production.
    ///
    /// Empty outside production, where any origin is allowed so a Vite dev server on a different port works
    /// without configuration. In production an empty list means no cross-origin browser client can reach the
    /// API at all — fail-closed, and loud, rather than a wildcard nobody remembers setting.
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: Secret<String>,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout_secs: u64,
    /// Schema holding the control plane (§5.1).
    pub global_schema: String,
    /// Schema holding `vector`, `ltree`, `pgcrypto` (§5.1).
    pub extensions_schema: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    /// Required for SeaweedFS, MinIO, Ceph RGW and every other non-AWS endpoint.
    pub force_path_style: bool,
    pub access_key_id: Option<Secret<String>>,
    pub secret_access_key: Option<Secret<String>>,
    /// Cap on a single multipart part, in MiB. Tuned for G21 file sizes.
    pub multipart_part_mib: u64,
}

/// Where the per-tenant Tantivy indexes live (§19).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Root directory holding one subdirectory per tenant.
    pub index_root: std::path::PathBuf,
    /// How many tenant indexes may be open at once. §19's LRU bound: at a thousand tenants the working
    /// set is what fits in file descriptors and page cache, and cold-open latency sits on p99.
    pub max_open_indexes: u64,
    /// How many writers may be open at once. Far smaller than the reader bound — a writer holds a heap
    /// arena, so this is the memory knob.
    pub max_open_writers: u64,
    pub writer_memory_mib: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// `json` for deployed environments, `pretty` for a terminal.
    pub log_format: LogFormat,
    pub otlp_endpoint: Option<String>,
    pub service_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            environment: Environment::Development,
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            storage: StorageConfig::default(),
            search: SearchConfig::default(),
            telemetry: TelemetryConfig::default(),
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            // Under the working directory, not `/var/lib`: a default that needs root to create is a
            // default that fails on a developer's first run.
            index_root: std::path::PathBuf::from("./data/search"),
            max_open_indexes: 64,
            max_open_writers: 4,
            writer_memory_mib: 64,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            url_signing_key: Secret::new(DEV_SIGNING_KEY.into()),
            request_timeout_secs: 30,
            allowed_origins: Vec::new(),
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: Secret::new("postgres://damrs:damrs@localhost:5440/damrs".into()),
            max_connections: 16,
            min_connections: 1,
            acquire_timeout_secs: 10,
            global_schema: "dam_global".into(),
            extensions_schema: "extensions".into(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            endpoint: Some("http://localhost:8333".into()),
            region: "us-east-1".into(),
            bucket: "damrs-dev".into(),
            force_path_style: true,
            access_key_id: None,
            secret_access_key: None,
            multipart_part_mib: 16,
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_format: LogFormat::Pretty,
            otlp_endpoint: None,
            service_name: "damrs".into(),
        }
    }
}

impl Config {
    /// Loads from the process environment, optionally layered over a TOML file.
    ///
    /// Precedence is merge order: **defaults -> file -> env**, later wins.
    ///
    /// There is deliberately only one code path here. An earlier draft took an
    /// explicit env slice so tests could avoid mutating the process environment
    /// (`std::env::set_var` is `unsafe` in edition 2024 for good reason). That
    /// meant tests exercised a path production never took. `figment::Jail` solves
    /// the same problem properly — it isolates the environment under a mutex — so
    /// the test-only path is gone.
    pub fn load(path: Option<impl AsRef<Path>>) -> Result<Self, Error> {
        // Deliberately NOT `Figment::from(Serialized::defaults(Self::default()))`.
        //
        // That layer serialises the default config to feed it in as a provider, and
        // `Secret`'s Serialize is lossy by design — so every secret default came
        // back as the literal string "[REDACTED]". The default database URL became
        // unusable and the production signing-key check silently compared against
        // the wrong value. Two individually-correct decisions (redact on
        // serialise; seed defaults via a provider) combining into a real bug.
        //
        // Every config struct carries `#[serde(default)]`, so serde fills missing
        // keys from `Default` during extraction without any round-trip.
        let mut figment = Figment::new();

        if let Some(p) = path {
            figment = figment.merge(Toml::file(p.as_ref()));
        }

        // `split("__")` maps DAMRS_DATABASE__MAX_CONNECTIONS -> database.max_connections.
        let cfg: Self = figment
            .merge(Env::prefixed("DAMRS_").split("__"))
            .extract()
            .map_err(|e| {
                // Figment's error carries the offending key path, which is what
                // makes a config failure actionable. Surface it explicitly rather
                // than relying on Display alone.
                let keys: Vec<String> = e
                    .clone()
                    .into_iter()
                    .filter_map(|err| err.path.last().cloned())
                    .collect();
                let where_ = if keys.is_empty() {
                    String::new()
                } else {
                    format!(" (key: {})", keys.join("."))
                };
                Error::Config(format!("{e}{where_}"))
            })?;

        cfg.validate()?;
        Ok(cfg)
    }

    /// Sanity checks that depend on more than one field.
    ///
    /// Production checks fail startup rather than warn. A dev placeholder signing
    /// key in production means every signed URL is forgeable by anyone who has read
    /// the source, which is not a warning-level problem.
    pub fn validate(&self) -> Result<(), Error> {
        if self.database.min_connections > self.database.max_connections {
            return Err(Error::Config(format!(
                "database.min_connections ({}) exceeds max_connections ({})",
                self.database.min_connections, self.database.max_connections
            )));
        }

        if self.storage.multipart_part_mib < 5 {
            // S3 requires every part except the last to be at least 5 MiB.
            return Err(Error::Config(
                "storage.multipart_part_mib must be at least 5 (S3 minimum part size)".into(),
            ));
        }

        if self.environment.is_production() {
            if self.server.url_signing_key.expose() == DEV_SIGNING_KEY {
                return Err(Error::Config(
                    "server.url_signing_key is still the development placeholder; \
                     signed URLs would be forgeable"
                        .into(),
                ));
            }
            if matches!(self.telemetry.log_format, LogFormat::Pretty) {
                return Err(Error::Config(
                    "telemetry.log_format must be `json` in production".into(),
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Jail;

    #[test]
    fn secret_defaults_are_real_values_not_redaction_placeholders() {
        // Regression: seeding figment with a serialised default config turned every
        // Secret into the literal "[REDACTED]", because Secret::serialize is lossy
        // on purpose. Defaults must survive extraction intact.
        Jail::expect_with(|_| {
            let cfg = Config::load(None::<&str>).expect("load");
            assert_eq!(cfg.server.url_signing_key.expose(), DEV_SIGNING_KEY);
            assert!(
                cfg.database.url.expose().starts_with("postgres://"),
                "default database URL must be a real URL, got {:?}",
                cfg.database.url.expose()
            );
            Ok(())
        });
    }

    #[test]
    fn min_above_max_connections_is_rejected() {
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_DATABASE__MIN_CONNECTIONS", 32);
            jail.set_env("DAMRS_DATABASE__MAX_CONNECTIONS", 8);
            let err = Config::load(None::<&str>).expect_err("min > max must be rejected");
            assert!(err.to_string().contains("min_connections"));
            Ok(())
        });
    }

    #[test]
    fn multipart_below_the_s3_minimum_is_rejected() {
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_STORAGE__MULTIPART_PART_MIB", 1);
            let err = Config::load(None::<&str>).expect_err("below 5 MiB must be rejected");
            assert!(err.to_string().contains('5'));
            Ok(())
        });
    }

    #[test]
    fn debug_of_the_whole_config_leaks_no_secret() {
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_DATABASE__URL", "postgres://u:hunter2@db/x");
            let cfg = Config::load(None::<&str>).expect("load");
            let rendered = format!("{cfg:?}");
            assert!(!rendered.contains("hunter2"), "leaked: {rendered}");
            Ok(())
        });
    }

    #[test]
    fn production_requires_json_logs() {
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_ENVIRONMENT", "production");
            jail.set_env("DAMRS_SERVER__URL_SIGNING_KEY", "a-real-key");
            let err = Config::load(None::<&str>).expect_err("pretty logs in production");
            assert!(err.to_string().contains("log_format"));
            Ok(())
        });
    }

    #[test]
    fn a_fully_configured_production_config_is_accepted() {
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_ENVIRONMENT", "production");
            jail.set_env("DAMRS_SERVER__URL_SIGNING_KEY", "a-real-key");
            jail.set_env("DAMRS_TELEMETRY__LOG_FORMAT", "json");
            let cfg = Config::load(None::<&str>).expect("valid production config");
            assert!(cfg.environment.is_production());
            Ok(())
        });
    }
}
