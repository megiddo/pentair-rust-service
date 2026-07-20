//! `POST /command` — gated CircuitChange / HeatChange writes.
//!
//! Pattern: **Facade** (API) + **Command** (typed / raw JSON) + **Mutex / Gate**.
//!
//! Accepts typed JSON or raw hex. When `writes_enabled=false` (default), returns
//! a dry-run verdict simulated from fixture ACK frames (CI-safe). Concurrent
//! in-flight writes are **rejected** with HTTP 409 (not queued).

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::commands::parse_circuit_id;
use crate::write::{SharedWriteGate, WriteGateError, WriteResult, WriteVerdict};

/// Application state for command routes.
#[derive(Clone)]
pub struct CommandState {
    /// Write gate (Mutex + dry-run / live TX).
    pub gate: SharedWriteGate,
}

/// `POST /command` body — typed fields and/or raw hex.
///
/// Examples:
/// ```json
/// {"type":"CircuitChange","circuit":6,"on":true}
/// {"type":"CircuitChange","circuit":"pool_light","on":true}
/// {"type":"HeatChange","poolSet":43,"spaSet":96,"mode":5}
/// {"hex":"ff00ffa507102088042b60050001f8"}
/// ```
#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    /// Discriminator: `CircuitChange` / `HeatChange` (optional when `hex` set).
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// Raw framed hex (alternative to typed fields).
    pub hex: Option<String>,
    /// Circuit id (u8 or name string) for CircuitChange.
    #[serde(default)]
    pub circuit: Option<serde_json::Value>,
    /// On/off for CircuitChange.
    pub on: Option<bool>,
    /// Alias for `on` (`status`: 0/1).
    pub status: Option<u8>,
    /// Pool setpoint for HeatChange.
    #[serde(rename = "poolSet", alias = "pool_set")]
    pub pool_set: Option<u8>,
    /// Spa setpoint for HeatChange.
    #[serde(rename = "spaSet", alias = "spa_set")]
    pub spa_set: Option<u8>,
    /// Heat mode byte.
    pub mode: Option<u8>,
}

/// Error JSON body.
#[derive(Debug, Serialize)]
pub struct CommandErrorBody {
    /// Machine-readable error.
    pub error: String,
    /// Human detail.
    pub message: String,
}

/// Registers `POST /command`.
pub fn routes(state: CommandState) -> Router {
    Router::new()
        .route("/command", post(post_command))
        .with_state(state)
}

/// `POST /command` — build + gated send/dry-run + listen verdict.
pub async fn post_command(
    State(state): State<CommandState>,
    Json(req): Json<CommandRequest>,
) -> Result<(StatusCode, Json<WriteResult>), (StatusCode, Json<CommandErrorBody>)> {
    let result = dispatch(&state.gate, req).await.map_err(map_err)?;
    let status = match result.verdict {
        WriteVerdict::Ack | WriteVerdict::Status => StatusCode::OK,
        WriteVerdict::Timeout => StatusCode::OK, // still 200; verdict in body
    };
    Ok((status, Json(result)))
}

async fn dispatch(
    gate: &SharedWriteGate,
    req: CommandRequest,
) -> Result<WriteResult, WriteGateError> {
    if let Some(hex) = req.hex.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return gate.raw_hex(hex).await;
    }

    let type_name = req
        .type_name
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match type_name.as_str() {
        "circuitchange" | "circuit" | "0x86" => {
            let circuit = resolve_circuit(&req)?;
            let on = resolve_on(&req)?;
            gate.circuit_change(circuit, on).await
        }
        "heatchange" | "heat" | "0x88" => {
            let pool = req.pool_set.ok_or_else(|| {
                WriteGateError::Unsupported("HeatChange requires poolSet".into())
            })?;
            let spa = req.spa_set.ok_or_else(|| {
                WriteGateError::Unsupported("HeatChange requires spaSet".into())
            })?;
            let mode = req.mode.unwrap_or(0);
            gate.heat_change(pool, spa, mode).await
        }
        "" => Err(WriteGateError::Unsupported(
            "provide type (CircuitChange|HeatChange) or hex".into(),
        )),
        other => Err(WriteGateError::Unsupported(format!(
            "{other} (only CircuitChange / HeatChange / hex)"
        ))),
    }
}

fn resolve_circuit(req: &CommandRequest) -> Result<u8, WriteGateError> {
    let v = req
        .circuit
        .as_ref()
        .ok_or_else(|| WriteGateError::Unsupported("CircuitChange requires circuit".into()))?;
    if let Some(n) = v.as_u64() {
        if n > 255 {
            return Err(WriteGateError::Unsupported("circuit id out of range".into()));
        }
        return Ok(n as u8);
    }
    if let Some(s) = v.as_str() {
        return parse_circuit_id(s).map_err(WriteGateError::from);
    }
    Err(WriteGateError::Unsupported(
        "circuit must be number or name string".into(),
    ))
}

fn resolve_on(req: &CommandRequest) -> Result<bool, WriteGateError> {
    if let Some(on) = req.on {
        return Ok(on);
    }
    if let Some(s) = req.status {
        return Ok(s != 0);
    }
    Err(WriteGateError::Unsupported(
        "CircuitChange requires on or status".into(),
    ))
}

fn map_err(e: WriteGateError) -> (StatusCode, Json<CommandErrorBody>) {
    let (status, error) = match &e {
        WriteGateError::Busy => (StatusCode::CONFLICT, "busy"),
        WriteGateError::Command(_) => (StatusCode::BAD_REQUEST, "bad_command"),
        WriteGateError::Unsupported(_) => (StatusCode::BAD_REQUEST, "unsupported"),
        WriteGateError::NoWriteHandle => (StatusCode::SERVICE_UNAVAILABLE, "no_write_handle"),
        WriteGateError::Transport(_) => (StatusCode::BAD_GATEWAY, "transport"),
    };
    (
        status,
        Json(CommandErrorBody {
            error: error.into(),
            message: e.to_string(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{fixtures, CMD_CIRCUIT_CHANGE, CMD_HEAT_CHANGE};
    use crate::write::WriteGate;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn app() -> Router {
        let gate = WriteGate::new(false, 50, None).shared();
        routes(CommandState { gate })
    }

    async fn post_json(body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/command")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (status, v)
    }

    #[tokio::test]
    async fn post_heat_typed_dry_run() {
        let (status, body) = post_json(serde_json::json!({
            "type": "HeatChange",
            "poolSet": 43,
            "spaSet": 96,
            "mode": 5
        }))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["dry_run"], true);
        assert_eq!(body["tx_hex"], fixtures::SET_TEMP);
        assert_eq!(body["verdict"], "ack");
        assert_eq!(body["command"], CMD_HEAT_CHANGE);
    }

    #[tokio::test]
    async fn post_circuit_by_name() {
        let (status, body) = post_json(serde_json::json!({
            "type": "CircuitChange",
            "circuit": "pool_light",
            "on": true
        }))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["command"], CMD_CIRCUIT_CHANGE);
        assert_eq!(body["verdict"], "ack");
        assert!(body["dry_run"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn post_raw_hex_heat() {
        let (status, body) = post_json(serde_json::json!({
            "hex": fixtures::SET_TEMP
        }))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tx_hex"], fixtures::SET_TEMP);
        assert_eq!(body["verdict"], "ack");
    }

    #[tokio::test]
    async fn post_missing_fields_400() {
        let (status, body) = post_json(serde_json::json!({"type": "HeatChange"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "unsupported");
    }

    #[tokio::test]
    async fn busy_returns_409() {
        let gate = WriteGate::new(false, 500, None).shared();
        let hold = gate.try_lock().unwrap();
        let app = routes(CommandState {
            gate: Arc::clone(&gate),
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/command")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "type": "CircuitChange",
                            "circuit": 1,
                            "on": false
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        drop(hold);
    }

    #[tokio::test]
    async fn post_heat_missing_spa_and_unknown_type() {
        let (s, body) = post_json(serde_json::json!({
            "type": "HeatChange",
            "poolSet": 40
        }))
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(body["message"].as_str().unwrap().contains("spaSet"));

        let (s2, body2) = post_json(serde_json::json!({"type": "PumpChange"})).await;
        assert_eq!(s2, StatusCode::BAD_REQUEST);
        assert_eq!(body2["error"], "unsupported");

        let (s3, _) = post_json(serde_json::json!({})).await;
        assert_eq!(s3, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_circuit_status_and_bad_circuit() {
        let (s, body) = post_json(serde_json::json!({
            "type": "circuit",
            "circuit": 1,
            "status": 1
        }))
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["command"], CMD_CIRCUIT_CHANGE);

        let (s2, _) = post_json(serde_json::json!({
            "type": "CircuitChange",
            "circuit": true,
            "on": true
        }))
        .await;
        assert_eq!(s2, StatusCode::BAD_REQUEST);

        let (s3, _) = post_json(serde_json::json!({
            "type": "CircuitChange",
            "circuit": 300,
            "on": true
        }))
        .await;
        assert_eq!(s3, StatusCode::BAD_REQUEST);

        let (s4, _) = post_json(serde_json::json!({
            "type": "CircuitChange",
            "circuit": 1
        }))
        .await;
        assert_eq!(s4, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_no_write_handle_503() {
        let gate = WriteGate::new(true, 10, None).shared();
        let app = routes(CommandState { gate });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/command")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "type": "HeatChange",
                            "poolSet": 40,
                            "spaSet": 90,
                            "mode": 0
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn post_bad_hex_400() {
        let (s, body) = post_json(serde_json::json!({"hex": "zz"})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "bad_command");
    }
}
