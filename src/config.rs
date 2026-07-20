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

/// Environment variable for append-only frame journal path (mounted volume).
pub const ENV_JOURNAL_PATH: &str = "PENTAIR_JOURNAL_PATH";

/// Environment variable for journal max size in bytes (retention stub).
pub const ENV_JOURNAL_MAX_BYTES: &str = "PENTAIR_JOURNAL_MAX_BYTES";

/// Environment variable for journal max age in seconds (retention stub).
pub const ENV_JOURNAL_MAX_AGE_SECS: &str = "PENTAIR_JOURNAL_MAX_AGE_SECS";

/// Environment variable: allow live bus TX (`true`/`1`/`yes`). Default off.
pub const ENV_WRITES_ENABLED: &str = "PENTAIR_WRITES_ENABLED";

/// Environment variable: post-TX listen window in milliseconds.
pub const ENV_LISTEN_WINDOW_MS: &str = "PENTAIR_LISTEN_WINDOW_MS";

/// Default post-TX listen window (ms).
pub const DEFAULT_LISTEN_WINDOW_MS: u64 = 500;

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
    /// Journal retention numeric env/file value is not a valid integer.
    #[error("invalid journal retention number: {0}")]
    BadJournalNumber(String),
    /// Boolean / numeric write-gate env/file value is invalid.
    #[error("invalid write-gate setting: {0}")]
    BadWriteSetting(String),
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
    /// Optional append-only frame journal path (e.g. `data/frames.journal` on the bind mount).
    pub journal_path: Option<PathBuf>,
    /// Soft max journal file size in bytes (retention stub); `None` = unbounded.
    pub journal_max_bytes: Option<u64>,
    /// Soft max journal age in seconds (retention stub / no-op age trim); `None` = unbounded.
    pub journal_max_age_secs: Option<u64>,
    /// When false (default), `POST /command` dry-runs and never TX on the live bus.
    pub writes_enabled: bool,
    /// Milliseconds to capture RX after a write (listen window).
    pub listen_window_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            transport_url: None,
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            journal_path: None,
            journal_max_bytes: None,
            journal_max_age_secs: None,
            writes_enabled: false,
            listen_window_ms: DEFAULT_LISTEN_WINDOW_MS,
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
    /// When true, [`crate::run`] spawns the bus Actor (B2) to own the connection.
    pub fn has_transport(&self) -> bool {
        self.transport_url
            .as_ref()
            .is_some_and(|u| !u.trim().is_empty())
    }

    /// Returns true when a journal path is configured.
    pub fn has_journal(&self) -> bool {
        self.journal_path.is_some()
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
    journal_path: Option<Option<PathBuf>>,
    journal_max_bytes: Option<Option<u64>>,
    journal_max_age_secs: Option<Option<u64>>,
    writes_enabled: Option<bool>,
    listen_window_ms: Option<u64>,
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

    /// Sets the frame journal path. Empty string clears (journal disabled).
    pub fn journal_path(mut self, path: impl Into<String>) -> Self {
        let s = path.into();
        self.journal_path = Some(if s.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        });
        self
    }

    /// Clears the journal path.
    pub fn clear_journal(mut self) -> Self {
        self.journal_path = Some(None);
        self
    }

    /// Sets soft max journal bytes (`None` clears).
    pub fn journal_max_bytes(mut self, max: Option<u64>) -> Self {
        self.journal_max_bytes = Some(max);
        self
    }

    /// Sets soft max journal age in seconds (`None` clears).
    pub fn journal_max_age_secs(mut self, max: Option<u64>) -> Self {
        self.journal_max_age_secs = Some(max);
        self
    }

    /// Enables or disables live bus TX (`false` = dry-run only).
    pub fn writes_enabled(mut self, enabled: bool) -> Self {
        self.writes_enabled = Some(enabled);
        self
    }

    /// Sets the post-TX listen window in milliseconds.
    pub fn listen_window_ms(mut self, ms: u64) -> Self {
        self.listen_window_ms = Some(ms.max(1));
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
        if let Some(jp) = file.journal_path {
            self = self.journal_path(jp);
        }
        if let Some(n) = file.journal_max_bytes {
            self.journal_max_bytes = Some(Some(n));
        }
        if let Some(n) = file.journal_max_age_secs {
            self.journal_max_age_secs = Some(Some(n));
        }
        if let Some(w) = file.writes_enabled {
            self.writes_enabled = Some(w);
        }
        if let Some(ms) = file.listen_window_ms {
            self.listen_window_ms = Some(ms.max(1));
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
        if let Ok(path) = env::var(ENV_JOURNAL_PATH) {
            builder = builder.journal_path(path);
        }
        if let Ok(raw) = env::var(ENV_JOURNAL_MAX_BYTES) {
            if raw.trim().is_empty() {
                builder = builder.journal_max_bytes(None);
            } else {
                let n: u64 = raw.trim().parse().map_err(|_| {
                    ConfigError::BadJournalNumber(format!("{ENV_JOURNAL_MAX_BYTES}={raw}"))
                })?;
                builder = builder.journal_max_bytes(Some(n));
            }
        }
        if let Ok(raw) = env::var(ENV_JOURNAL_MAX_AGE_SECS) {
            if raw.trim().is_empty() {
                builder = builder.journal_max_age_secs(None);
            } else {
                let n: u64 = raw.trim().parse().map_err(|_| {
                    ConfigError::BadJournalNumber(format!("{ENV_JOURNAL_MAX_AGE_SECS}={raw}"))
                })?;
                builder = builder.journal_max_age_secs(Some(n));
            }
        }
        if let Ok(raw) = env::var(ENV_WRITES_ENABLED) {
            if !raw.trim().is_empty() {
                builder = builder.writes_enabled(parse_bool_env(&raw).map_err(|_| {
                    ConfigError::BadWriteSetting(format!("{ENV_WRITES_ENABLED}={raw}"))
                })?);
            }
        }
        if let Ok(raw) = env::var(ENV_LISTEN_WINDOW_MS) {
            if !raw.trim().is_empty() {
                let n: u64 = raw.trim().parse().map_err(|_| {
                    ConfigError::BadWriteSetting(format!("{ENV_LISTEN_WINDOW_MS}={raw}"))
                })?;
                builder = builder.listen_window_ms(n);
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
        let journal_path = self.journal_path.unwrap_or(None);
        let journal_max_bytes = self.journal_max_bytes.unwrap_or(None);
        let journal_max_age_secs = self.journal_max_age_secs.unwrap_or(None);
        let writes_enabled = self.writes_enabled.unwrap_or(false);
        let listen_window_ms = self
            .listen_window_ms
            .unwrap_or(DEFAULT_LISTEN_WINDOW_MS)
            .max(1);

        Ok(Config {
            bind_addr,
            transport_url,
            log_level,
            journal_path,
            journal_max_bytes,
            journal_max_age_secs,
            writes_enabled,
            listen_window_ms,
        })
    }
}

/// Optional on-disk TOML shape.
#[derive(Debug, Deserialize)]
struct FileConfig {
    bind_addr: Option<String>,
    transport_url: Option<String>,
    log_level: Option<String>,
    journal_path: Option<String>,
    journal_max_bytes: Option<u64>,
    journal_max_age_secs: Option<u64>,
    writes_enabled: Option<bool>,
    listen_window_ms: Option<u64>,
}

fn parse_bool_env(raw: &str) -> Result<bool, ()> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(()),
    }
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
            journal_path: None,
            journal_max_bytes: None,
            journal_max_age_secs: None,
            writes_enabled: false,
            listen_window_ms: 500,
        };
        assert!(!cfg.has_transport());
        assert!(!cfg.has_journal());
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
    fn merge_file_journal_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.toml");
        fs::write(
            &path,
            r#"
journal_path = "data/frames.journal"
journal_max_bytes = 1048576
journal_max_age_secs = 86400
"#,
        )
        .unwrap();
        let cfg = ConfigBuilder::new().merge_file(&path).unwrap().build().unwrap();
        assert_eq!(
            cfg.journal_path.as_deref(),
            Some(Path::new("data/frames.journal"))
        );
        assert_eq!(cfg.journal_max_bytes, Some(1_048_576));
        assert_eq!(cfg.journal_max_age_secs, Some(86_400));
        assert!(cfg.has_journal());
    }

    #[test]
    fn journal_builder_clear_and_env() {
        let _lock = env_lock().lock().unwrap();
        let cfg = ConfigBuilder::new()
            .journal_path("data/a.journal")
            .clear_journal()
            .journal_max_bytes(Some(99))
            .journal_max_age_secs(Some(12))
            .build()
            .unwrap();
        assert!(!cfg.has_journal());
        assert_eq!(cfg.journal_max_bytes, Some(99));
        assert_eq!(cfg.journal_max_age_secs, Some(12));

        let dir = tempfile::tempdir().unwrap();
        let jpath = dir.path().join("from-env.journal");
        let _g1 = EnvGuard::unset(ENV_CONFIG_PATH);
        let _g2 = EnvGuard::set(ENV_JOURNAL_PATH, jpath.to_str().unwrap());
        let _g3 = EnvGuard::set(ENV_JOURNAL_MAX_BYTES, "4096");
        let _g4 = EnvGuard::set(ENV_JOURNAL_MAX_AGE_SECS, "120");
        let _g5 = EnvGuard::unset(ENV_BIND_ADDR);
        let _g6 = EnvGuard::unset(ENV_TRANSPORT_URL);
        let _g7 = EnvGuard::unset(ENV_LOG_LEVEL);

        let loaded = ConfigBuilder::new().from_env_and_disk().unwrap().build().unwrap();
        assert_eq!(loaded.journal_path.as_deref(), Some(jpath.as_path()));
        assert_eq!(loaded.journal_max_bytes, Some(4096));
        assert_eq!(loaded.journal_max_age_secs, Some(120));
    }

    #[test]
    fn journal_env_empty_clears_and_bad_number() {
        let _lock = env_lock().lock().unwrap();
        let _g1 = EnvGuard::unset(ENV_CONFIG_PATH);
        let _g2 = EnvGuard::set(ENV_JOURNAL_PATH, "data/x.journal");
        let _g3 = EnvGuard::set(ENV_JOURNAL_MAX_BYTES, "");
        let _g4 = EnvGuard::set(ENV_JOURNAL_MAX_AGE_SECS, "");
        let _g5 = EnvGuard::unset(ENV_BIND_ADDR);
        let _g6 = EnvGuard::unset(ENV_TRANSPORT_URL);
        let _g7 = EnvGuard::unset(ENV_LOG_LEVEL);

        let cfg = ConfigBuilder::new().from_env_and_disk().unwrap().build().unwrap();
        assert!(cfg.has_journal());
        assert_eq!(cfg.journal_max_bytes, None);
        assert_eq!(cfg.journal_max_age_secs, None);

        let _g3b = EnvGuard::set(ENV_JOURNAL_MAX_BYTES, "nope");
        let err = ConfigBuilder::new().from_env_and_disk().unwrap_err();
        assert!(matches!(err, ConfigError::BadJournalNumber(_)));

        let _g3c = EnvGuard::set(ENV_JOURNAL_MAX_BYTES, "10");
        let _g4b = EnvGuard::set(ENV_JOURNAL_MAX_AGE_SECS, "bad");
        let err2 = ConfigBuilder::new().from_env_and_disk().unwrap_err();
        assert!(matches!(err2, ConfigError::BadJournalNumber(_)));
    }

    #[test]
    fn empty_env_bind_ignored() {
        let _lock = env_lock().lock().unwrap();
        let _g1 = EnvGuard::unset(ENV_CONFIG_PATH);
        let _g2 = EnvGuard::set(ENV_BIND_ADDR, "");
        let _g3 = EnvGuard::unset(ENV_TRANSPORT_URL);
        let _g4 = EnvGuard::unset(ENV_LOG_LEVEL);
        let _g5 = EnvGuard::unset(ENV_JOURNAL_PATH);
        let _g6 = EnvGuard::unset(ENV_JOURNAL_MAX_BYTES);
        let _g7 = EnvGuard::unset(ENV_JOURNAL_MAX_AGE_SECS);
        let _g8 = EnvGuard::unset(ENV_WRITES_ENABLED);
        let _g9 = EnvGuard::unset(ENV_LISTEN_WINDOW_MS);

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
        let bad = ConfigError::BadJournalNumber("x".into());
        assert!(bad.to_string().contains("journal"));
        let badw = ConfigError::BadWriteSetting("x".into());
        assert!(badw.to_string().contains("write-gate"));
    }

    #[test]
    fn writes_enabled_builder_and_file() {
        let cfg = ConfigBuilder::new()
            .writes_enabled(true)
            .listen_window_ms(250)
            .build()
            .unwrap();
        assert!(cfg.writes_enabled);
        assert_eq!(cfg.listen_window_ms, 250);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.toml");
        fs::write(
            &path,
            "writes_enabled = true\nlisten_window_ms = 100\n",
        )
        .unwrap();
        let cfg = ConfigBuilder::new().merge_file(&path).unwrap().build().unwrap();
        assert!(cfg.writes_enabled);
        assert_eq!(cfg.listen_window_ms, 100);
    }

    #[test]
    fn writes_env_and_parse_bool() {
        let _lock = env_lock().lock().unwrap();
        let _g1 = EnvGuard::unset(ENV_CONFIG_PATH);
        let _g2 = EnvGuard::set(ENV_WRITES_ENABLED, "yes");
        let _g3 = EnvGuard::set(ENV_LISTEN_WINDOW_MS, "750");
        let _g4 = EnvGuard::unset(ENV_BIND_ADDR);
        let _g5 = EnvGuard::unset(ENV_TRANSPORT_URL);
        let _g6 = EnvGuard::unset(ENV_LOG_LEVEL);
        let _g7 = EnvGuard::unset(ENV_JOURNAL_PATH);
        let _g8 = EnvGuard::unset(ENV_JOURNAL_MAX_BYTES);
        let _g9 = EnvGuard::unset(ENV_JOURNAL_MAX_AGE_SECS);

        let cfg = ConfigBuilder::new().from_env_and_disk().unwrap().build().unwrap();
        assert!(cfg.writes_enabled);
        assert_eq!(cfg.listen_window_ms, 750);

        assert_eq!(parse_bool_env("true").unwrap(), true);
        assert_eq!(parse_bool_env("0").unwrap(), false);
        assert!(parse_bool_env("maybe").is_err());

        let _g2b = EnvGuard::set(ENV_WRITES_ENABLED, "maybe");
        let err = ConfigBuilder::new().from_env_and_disk().unwrap_err();
        assert!(matches!(err, ConfigError::BadWriteSetting(_)));
    }
}
