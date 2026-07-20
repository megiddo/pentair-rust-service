//! Local HTTP API surface.
//!
//! Pattern: **Facade** — aggregates route modules into the Axum router exposed by
//! the library façade.

pub mod health;

use axum::Router;

/// Builds the HTTP router for the local API.
pub fn router() -> Router {
    Router::new().merge(health::routes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_builds() {
        let _r = router();
    }
}
