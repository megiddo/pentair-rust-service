//! Local HTTP API surface.
//!
//! Pattern: **Facade** — aggregates route modules into the Axum router exposed by
//! the library façade.

pub mod command;
pub mod health;
pub mod status;

use axum::Router;

use crate::state::SharedBusState;
use crate::write::SharedWriteGate;

use self::command::CommandState;

/// Builds the HTTP router for the local API
/// (`/health`, `/status`, `/frames`, `POST /command`).
pub fn router(state: SharedBusState, gate: SharedWriteGate) -> Router {
    Router::new()
        .merge(health::routes())
        .merge(status::routes(state))
        .merge(command::routes(CommandState { gate }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::shared_bus_state;
    use crate::write::WriteGate;

    #[test]
    fn router_builds() {
        let gate = WriteGate::new(false, 50, None).shared();
        let _r = router(shared_bus_state(), gate);
    }
}
