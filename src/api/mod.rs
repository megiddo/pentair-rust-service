//! Local HTTP API surface.
//!
//! Pattern: **Facade** — aggregates route modules into the Axum router exposed by
//! the library façade.

pub mod health;
pub mod status;

use axum::Router;

use crate::state::SharedBusState;

/// Builds the HTTP router for the local API (`/health`, `/status`, `/frames`).
pub fn router(state: SharedBusState) -> Router {
    Router::new()
        .merge(health::routes())
        .merge(status::routes(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::shared_bus_state;

    #[test]
    fn router_builds() {
        let _r = router(shared_bus_state());
    }
}
