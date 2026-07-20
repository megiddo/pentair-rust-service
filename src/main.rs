//! Thin binary entrypoint for `pentairservice`.
//!
//! Pattern: **Facade** — loads config, installs logging, then delegates to the library
//! [`pentairservice::run`] façade. Keep this file minimal so coverage gates target lib code.

use pentairservice::config::Config;
use pentairservice::logging;
use tracing::error;

#[tokio::main]
async fn main() {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load config: {e}");
            std::process::exit(1);
        }
    };

    logging::init(&config.log_level);

    if let Err(e) = pentairservice::run(config).await {
        error!(error = %e, "service exited with error");
        std::process::exit(1);
    }
}
