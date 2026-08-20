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
use std::collections::BTreeMap;
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

/// The dev-only default for the credential sealing key. Rejected in production by [`Config::validate`], for a
/// blunter reason than the signing key: this one encrypts tenants' own API keys, and a placeholder here means a
/// database dump is a list of usable credentials belonging to somebody else.
pub const DEV_SEALING_KEY: &str = "dev-insecure-sealing-key";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub environment: Environment,
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    pub search: SearchConfig,
    pub telemetry: TelemetryConfig,
    pub ai: AiConfig,
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
    /// The origin this API is reached at, when it is not the same as where it binds.
    ///
    /// Used to make delivery URLs absolute. Without it they are root-relative, which is correct for a client
    /// served from the same origin and wrong for one that is not — a browser resolving `/d/<token>` against
    /// the *frontend's* origin gets a 404 from the wrong server, which is exactly what happened the first time
    /// a thumbnail was fetched from a Vite dev server on another port.
    ///
    /// Optional rather than required, because a required value is one that is wrong in every deployment behind
    /// a proxy until somebody sets it, and a root-relative URL at least works for the same-origin case.
    pub public_url: Option<String>,
    /// Which tenant the delivery routes serve, by slug.
    ///
    /// Needed because the delivery path resolves its tenant from configuration rather than from the
    /// signed claim (3.x moves it into the token). With one active tenant this can be left unset and
    /// the tenant is inferred; with several, inferring would mint delivery URLs against the wrong
    /// tenant's objects, so it must be named. Naming it is also the right posture for a deployment
    /// that later grows a second tenant: the answer does not silently change under it.
    pub delivery_tenant: Option<String>,
    /// Origins the browser API accepts in production.
    ///
    /// Empty outside production, where any origin is allowed so a Vite dev server on a different port works
    /// without configuration. In production an empty list means no cross-origin browser client can reach the
    /// API at all — fail-closed, and loud, rather than a wildcard nobody remembers setting.
    pub allowed_origins: Vec<String>,
    /// Whether to mount the MCP server at `/mcp` (§8.5).
    ///
    /// **Off by default**, like every other switch in this system that opens something: an MCP endpoint grants
    /// nothing a key does not already grant — the tools call the REST handlers, under the same predicate — but
    /// it is a second protocol surface with its own framing, session handling and rebinding checks. A deployment
    /// that wants agents talking to its library says so; one that does not should not have to reason about
    /// whether anybody holds a key.
    pub mcp_enabled: bool,
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

/// Hosted-model enrichment (§8.3, M5).
///
/// Two unrelated things live here because both are deployment-level and neither belongs to a tenant: the key
/// that seals tenants' provider credentials, and what a model call costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    /// The key credentials are sealed under. Never a provider's key — this is the key that encrypts *those*.
    pub sealing_key: Secret<String>,
    /// Names the key in every ciphertext, so a rotation can tell which rows still need re-sealing.
    ///
    /// Short and stable: it is written into every sealed value and matching it is how [`Self::keyring`] decides
    /// which secret opens which row.
    pub sealing_key_id: String,
    /// Keys that no longer seal but must still open.
    ///
    /// A rotation is a deploy, not a migration: the new key goes in `sealing_key`, the old one moves here, and
    /// every existing row keeps opening while `sealed_under_other_keys` works through them. Removing an entry
    /// before that finishes makes those credentials unreadable — which is a recoverable mistake only if the
    /// tenant still has the original key to paste in again.
    pub retired_sealing_keys: BTreeMap<String, Secret<String>>,
    /// Price overrides, keyed by model name or a prefix of one, in dollars per million tokens.
    ///
    /// Merged over the built-in table rather than replacing it, so correcting one model's price does not silently
    /// unprice every other. A vendor's announcement should not need a release, which is the whole reason this is
    /// configuration — see `dam_ai::pricing`.
    pub prices: BTreeMap<String, ModelPrice>,
}

/// What one model costs, as vendors publish it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input_dollars_per_mtok: f64,
    pub output_dollars_per_mtok: f64,
}

impl AiConfig {
    /// The keyring: the current key first, every retired key after.
    ///
    /// Order is the contract — `dam_core::sealed` seals with the first entry and opens with any — so a rotation
    /// takes effect for new writes the moment this is rebuilt, with no window in which old rows cannot be read.
    pub fn keyring(&self) -> crate::sealed::SealingKeyring {
        let mut keyring =
            crate::sealed::SealingKeyring::single(&self.sealing_key_id, &self.sealing_key);
        for (key_id, secret) in &self.retired_sealing_keys {
            keyring = keyring.with_retired(key_id, secret);
        }
        keyring
    }
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
            ai: AiConfig::default(),
        }
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            sealing_key: Secret::new(DEV_SEALING_KEY.into()),
            sealing_key_id: "dev".into(),
            retired_sealing_keys: BTreeMap::new(),
            // Empty, not a copy of the built-in table: an override that shipped as a default would be a price
            // list nobody edited, going stale in a file rather than visibly in one place.
            prices: BTreeMap::new(),
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
            public_url: None,
            delivery_tenant: None,
            allowed_origins: Vec::new(),
            mcp_enabled: false,
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
            if self.ai.sealing_key.expose() == DEV_SEALING_KEY {
                return Err(Error::Config(
                    "ai.sealing_key is still the development placeholder; every tenant's model \
                     credential would be readable by anyone who has read the source"
                        .into(),
                ));
            }
        }

        // Not production-only. A retired key sharing the current id makes the keyring ambiguous — which of two
        // secrets opens a row named `dev`? — and the failure would arrive as a credential that cannot be
        // decrypted long after the deploy that caused it.
        if self
            .ai
            .retired_sealing_keys
            .contains_key(&self.ai.sealing_key_id)
        {
            return Err(Error::Config(format!(
                "ai.sealing_key_id `{}` also appears in ai.retired_sealing_keys; \
                 a key id must name one secret",
                self.ai.sealing_key_id
            )));
        }
        if self.ai.sealing_key_id.trim().is_empty() {
            return Err(Error::Config(
                "ai.sealing_key_id must not be empty: it is written into every sealed value".into(),
            ));
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
            jail.set_env("DAMRS_AI__SEALING_KEY", "a-real-sealing-key");
            let cfg = Config::load(None::<&str>).expect("valid production config");
            assert!(cfg.environment.is_production());
            Ok(())
        });
    }

    #[test]
    fn the_mcp_server_is_off_until_a_deployment_says_otherwise() {
        // The same posture as every other switch that opens something: a second protocol surface is a decision,
        // not a default.
        assert!(!Config::default().server.mcp_enabled);
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_SERVER__MCP_ENABLED", "true");
            let cfg = Config::load(None::<&str>).expect("valid config");
            assert!(cfg.server.mcp_enabled);
            Ok(())
        });
    }

    #[test]
    fn production_refuses_the_placeholder_sealing_key() {
        // Blunter than the signing-key check it sits beside: this key encrypts tenants' own provider
        // credentials, so a placeholder means a database dump is a list of usable keys belonging to somebody
        // else.
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_ENVIRONMENT", "production");
            jail.set_env("DAMRS_SERVER__URL_SIGNING_KEY", "a-real-key");
            jail.set_env("DAMRS_TELEMETRY__LOG_FORMAT", "json");
            let err = Config::load(None::<&str>).expect_err("the dev sealing key in production");
            assert!(err.to_string().contains("sealing_key"), "{err}");
            Ok(())
        });
    }

    #[test]
    fn a_key_id_may_not_name_two_secrets() {
        // Not production-only: an ambiguous keyring surfaces as a credential that cannot be decrypted, long
        // after the deploy that caused it.
        Jail::expect_with(|jail| {
            jail.set_env("DAMRS_AI__SEALING_KEY_ID", "k1");
            jail.set_env(
                "DAMRS_AI__RETIRED_SEALING_KEYS",
                r#"{k1="an older passphrase"}"#,
            );
            let err = Config::load(None::<&str>).expect_err("an ambiguous keyring");
            assert!(err.to_string().contains("retired_sealing_keys"), "{err}");
            Ok(())
        });
    }

    #[test]
    fn the_keyring_seals_with_the_current_key_and_opens_with_a_retired_one() {
        // The rotation contract, asserted through config rather than through `sealed` alone: what an operator
        // writes in a file has to produce a keyring that can still read what the previous deploy wrote.
        let old = AiConfig {
            sealing_key: Secret::new("first passphrase".into()),
            sealing_key_id: "k1".into(),
            ..AiConfig::default()
        };
        let sealed = old
            .keyring()
            .seal(&Secret::new("a provider key".into()), "aad")
            .expect("seal");

        let mut rotated = AiConfig {
            sealing_key: Secret::new("second passphrase".into()),
            sealing_key_id: "k2".into(),
            ..AiConfig::default()
        };
        rotated
            .retired_sealing_keys
            .insert("k1".into(), Secret::new("first passphrase".into()));

        let ring = rotated.keyring();
        assert_eq!(
            ring.current_key_id(),
            "k2",
            "new values seal under the new key"
        );
        assert_eq!(
            ring.open(&sealed, "aad")
                .expect("opens under the retired key")
                .expose(),
            "a provider key"
        );

        // And without the retired key it cannot: which is why removing one before a re-seal pass finishes is
        // the mistake the config docs warn about.
        let alone = AiConfig {
            sealing_key: Secret::new("second passphrase".into()),
            sealing_key_id: "k2".into(),
            ..AiConfig::default()
        };
        assert!(alone.keyring().open(&sealed, "aad").is_err());
    }
}
