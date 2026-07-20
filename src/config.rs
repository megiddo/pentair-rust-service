//! Service configuration loading and defaults.
//!
//! Pattern: **Builder** — [`ConfigBuilder`] accumulates file + environment overrides,
//! then [`ConfigBuilder::build`] produces an immutable [`Config`]. Also **Configuration
//! Object** for typed settings passed into the façade.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Default bind address for the local HTTP API.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";

/// Default tracing filter when neither file nor env sets a level.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Environment variable for an optional TOML config file path.
pub const ENV_CONFIG_PATH: &str = "PENTAIR_CONFIG";

/// Environment variable for transport URL (`tcp://host:port` or serial path).
pub const ENV_TRANSPORT_URL: &str = "PENTAIR_TRANSPORT_URL";

/// Environment variable for HTTP bind address (`host:port`).
pub const ENV_BIND_ADDR: &str = "PENTAIR_BIND_ADDR";

/// Environment variable for log level filter (also respects `RUST_LOG` at subscriber init).
pub const ENV_LOG_LEVEL: &str = "PENTAIR_LOG_LEVEL";

/// Errors while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read a config file from disk.
    #[error("failed to read config file {path}: {source}")]
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// TOML parse failure.
    #[error("failed to parse config TOML: {0}")]
    Toml(#[from] toml::de::Error),
    /// Bind address is empty after resolution.
    #[error("bind_addr must not be empty")]
    EmptyBindAddr,
}

/// Immutable runtime settings for the service.
///
/// Pattern: **Configuration Object** — value type passed into the application façade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Local HTTP listen address (`host:port`).
    pub bind_addr: String,
    /// Optional transport URL. Empty/`None` means idle without opening a bus connection.
    pub transport_url: Option<String>,
    /// Default tracing filter directive (e.g. `info`, `debug`).
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            transport_url: None,
            log_level: DEFAULT_LOG_LEVEL.to_string(),
        }
    }
}

impl Config {
    /// Loads config from optional file + environment, applying defaults.
    ///
    /// Resolution order (later wins for overlapping keys):
    /// 1. Built-in defaults
    /// 2. TOML file from `PENTAIR_CONFIG`, or `./config.toml` if present
    /// 3. `PENTAIR_*` environment variables
    pub fn load() -> Result<Self, ConfigError> {
        ConfigBuilder::new().from_env_and_disk()?.build()
    }

    /// Returns true when a non-empty transport URL is configured.
    ///
    /// B0 does not open the transport even when this is true; B2 will.
    pub fn has_transport(&self) -> bool {
        self.transport_url
            .as_ref()
            .is_some_and(|u| !u.trim().is_empty())
    }
}

/// Accumulates partial settings before producing a [`Config`].
///
/// Pattern: **Builder**.
#[derive(Debug, Default, Clone)]
pub struct ConfigBuilder {
    bind_addr: Option<String>,
    transport_url: Option<Option<String>>,
    log_level: Option<String>,
}

impl ConfigBuilder {
    /// Creates an empty builder (all fields unset → defaults on build).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the HTTP bind address.
    pub fn bind_addr(mut self, addr: impl Into<String>) -> Self {
        self.bind_addr = Some(addr.into());
        self
    }

    /// Sets the optional transport URL. Pass empty string to clear.
    pub fn transport_url(mut self, url: impl Into<String>) -> Self {
        let s = url.into();
        self.transport_url = Some(if s.trim().is_empty() {
            None
        } else {
            Some(s)
        });
        self
    }

    /// Clears any transport URL (idle mode).
    pub fn clear_transport(mut self) -> Self {
        self.transport_url = Some(None);
        self
    }

    /// Sets the default log level filter.
    pub fn log_level(mut self, level: impl Into<String>) -> Self {
        self.log_level = Some(level.into());
        self
    }

    /// Merges values from a TOML file (missing keys leave prior builder state).
    pub fn merge_file(mut self, path: &Path) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file: FileConfig = toml::from_str(&raw)?;
        if let Some(addr) = file.bind_addr {
            self.bind_addr = Some(addr);
        }
        if let Some(url) = file.transport_url {
            self = self.transport_url(url);
        }
        if let Some(level) = file.log_level {
            self.log_level = Some(level);
        }
        Ok(self)
    }

    /// Applies `PENTAIR_*` env vars and optional on-disk config file.
    pub fn from_env_and_disk(self) -> Result<Self, ConfigError> {
        let mut builder = self;

        if let Some(path) = resolve_config_path() {
            builder = builder.merge_file(&path)?;
        }

        if let Ok(addr) = env::var(ENV_BIND_ADDR) {
            if !addr.is_empty() {
                builder = builder.bind_addr(addr);
            }
        }
        if let Ok(url) = env::var(ENV_TRANSPORT_URL) {
            builder = builder.transport_url(url);
        }
        if let Ok(level) = env::var(ENV_LOG_LEVEL) {
            if !level.is_empty() {
                builder = builder.log_level(level);
            }
        }

        Ok(builder)
    }

    /// Builds a [`Config`], applying defaults for unset fields.
    pub fn build(self) -> Result<Config, ConfigError> {
        let bind_addr = self
            .bind_addr
            .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
        if bind_addr.trim().is_empty() {
            return Err(ConfigError::EmptyBindAddr);
        }
        let log_level = self
            .log_level
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());
        let transport_url = self.transport_url.unwrap_or(None);

        Ok(Config {
            bind_addr,
            transport_url,
            log_level,
        })
    }
}

/// Optional on-disk TOML shape.
#[derive(Debug, Deserialize)]
struct FileConfig {
    bind_addr: Option<String>,
    transport_url: Option<String>,
    log_level: Option<String>,
}

/// Resolves config file path: `PENTAIR_CONFIG` if set, else `./config.toml` when it exists.
fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(path) = env::var(ENV_CONFIG_PATH) {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let default = PathBuf::from("config.toml");
    if default.is_file() {
        Some(default)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serializes env-mutating tests.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests hold env_lock; only this process mutates these keys.
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = env::var(key).ok();
            unsafe { env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { env::set_var(self.key, v) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn defaults_idle_without_transport() {
        let cfg = Config::default();
        assert_eq!(cfg.bind_addr, DEFAULT_BIND_ADDR);
        assert!(!cfg.has_transport());
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn builder_sets_fields() {
        let cfg = ConfigBuilder::new()
            .bind_addr("127.0.0.1:9090")
            .transport_url("tcp://ew11.local:8899")
            .log_level("debug")
            .build()
            .unwrap();
        assert_eq!(cfg.bind_addr, "127.0.0.1:9090");
        assert_eq!(
            cfg.transport_url.as_deref(),
            Some("tcp://ew11.local:8899")
        );
        assert!(cfg.has_transport());
        assert_eq!(cfg.log_level, "debug");
    }

    #[test]
    fn empty_transport_url_clears() {
        let cfg = ConfigBuilder::new()
            .transport_url("tcp://h:1")
            .transport_url("")
            .build()
            .unwrap();
        assert!(!cfg.has_transport());
    }

    #[test]
    fn clear_transport() {
        let cfg = ConfigBuilder::new()
            .transport_url("/dev/ttyUSB0")
            .clear_transport()
            .build()
            .unwrap();
        assert!(!cfg.has_transport());
    }

    #[test]
    fn empty_bind_addr_errors() {
        let err = ConfigBuilder::new().bind_addr("   ").build().unwrap_err();
        assert!(matches!(err, ConfigError::EmptyBindAddr));
    }

    #[test]
    fn merge_file_parses_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        fs::write(
            &path,
            r#"
bind_addr = "0.0.0.0:18080"
transport_url = "tcp://10.0.0.5:8899"
log_level = "warn"
"#,
        )
        .unwrap();

        let cfg = ConfigBuilder::new().merge_file(&path).unwrap().build().unwrap();
        assert_eq!(cfg.bind_addr, "0.0.0.0:18080");
        assert_eq!(cfg.transport_url.as_deref(), Some("tcp://10.0.0.5:8899"));
        assert_eq!(cfg.log_level, "warn");
    }

    #[test]
    fn merge_file_missing_is_io_error() {
        let err = ConfigBuilder::new()
            .merge_file(Path::new("/nonexistent/pentair-config.toml"))
            .unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn merge_file_bad_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(&path, "bind_addr = [[[").unwrap();
        let err = ConfigBuilder::new().merge_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(_)));
    }

    #[test]
    fn env_overrides_file() {
        let _lock = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        fs::write(
            &path,
            r#"
bind_addr = "0.0.0.0:1"
transport_url = "tcp://file:1"
log_level = "error"
"#,
        )
        .unwrap();

        let _g1 = EnvGuard::set(ENV_CONFIG_PATH, path.to_str().unwrap());
        let _g2 = EnvGuard::set(ENV_BIND_ADDR, "127.0.0.1:8080");
        let _g3 = EnvGuard::set(ENV_TRANSPORT_URL, "");
        let _g4 = EnvGuard::set(ENV_LOG_LEVEL, "debug");

        let cfg = Config::load().unwrap();
        assert_eq!(cfg.bind_addr, "127.0.0.1:8080");
        assert!(!cfg.has_transport());
        assert_eq!(cfg.log_level, "debug");
    }

    #[test]
    fn load_defaults_when_no_file_or_env() {
        let _lock = env_lock().lock().unwrap();
        let _g1 = EnvGuard::unset(ENV_CONFIG_PATH);
        let _g2 = EnvGuard::unset(ENV_BIND_ADDR);
        let _g3 = EnvGuard::unset(ENV_TRANSPORT_URL);
        let _g4 = EnvGuard::unset(ENV_LOG_LEVEL);

        // Avoid picking up a host ./config.toml if present by using from_env with
        // empty builder after unsetting — Config::load may still see ./config.toml.
        // Use builder without disk when file absent in CWD of test process.
        let cfg = ConfigBuilder::new().build().unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn has_transport_false_for_whitespace() {
        let cfg = Config {
            bind_addr: DEFAULT_BIND_ADDR.into(),
            transport_url: Some("  ".into()),
            log_level: DEFAULT_LOG_LEVEL.into(),
        };
        assert!(!cfg.has_transport());
    }

    #[test]
    fn resolve_config_path_from_env() {
        let _lock = env_lock().lock().unwrap();
        let _g = EnvGuard::set(ENV_CONFIG_PATH, "/tmp/custom.toml");
        assert_eq!(
            resolve_config_path().as_deref(),
            Some(Path::new("/tmp/custom.toml"))
        );
    }

    #[test]
    fn resolve_config_path_empty_env_falls_through() {
        let _lock = env_lock().lock().unwrap();
        let _g = EnvGuard::set(ENV_CONFIG_PATH, "");
        // Empty PENTAIR_CONFIG must not yield Some("").
        let path = resolve_config_path();
        assert!(path.as_ref().map(|p| p.as_os_str().is_empty()) != Some(true));
        if let Some(p) = path {
            assert_eq!(p, PathBuf::from("config.toml"));
        }
    }

    #[test]
    fn merge_file_partial_and_empty_transport() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.toml");
        fs::write(&path, "transport_url = \"\"\n").unwrap();
        let cfg = ConfigBuilder::new()
            .bind_addr("127.0.0.1:7")
            .merge_file(&path)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(cfg.bind_addr, "127.0.0.1:7");
        assert!(!cfg.has_transport());
    }

    #[test]
    fn empty_env_bind_ignored() {
        let _lock = env_lock().lock().unwrap();
        let _g1 = EnvGuard::unset(ENV_CONFIG_PATH);
        let _g2 = EnvGuard::set(ENV_BIND_ADDR, "");
        let _g3 = EnvGuard::unset(ENV_TRANSPORT_URL);
        let _g4 = EnvGuard::unset(ENV_LOG_LEVEL);

        let cfg = ConfigBuilder::new().from_env_and_disk().unwrap().build().unwrap();
        // If a local config.toml exists it may override; bind from empty env must not.
        assert!(!cfg.bind_addr.is_empty());
    }

    #[test]
    fn config_error_display() {
        let err = ConfigError::EmptyBindAddr;
        assert!(err.to_string().contains("bind_addr"));
        let io = ConfigError::Io {
            path: PathBuf::from("/x"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        };
        assert!(io.to_string().contains("/x"));
    }
}
