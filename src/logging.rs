//! Structured logging setup.
//!
//! Pattern: **Facade** — thin wrapper around `tracing-subscriber` so the rest of the
//! service depends on a single init entrypoint.

use tracing_subscriber::EnvFilter;

/// Initializes the global tracing subscriber.
///
/// Prefer `RUST_LOG` when set; otherwise use `default_level` (typically from config).
/// Safe to call once at process start; subsequent calls are ignored if a subscriber
/// is already set.
pub fn init(default_level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_does_not_panic() {
        init("info");
        // Second call should also be fine (subscriber already set).
        init("debug");
    }
}
