//! Status and recent-frames HTTP endpoints.
//!
//! Pattern: **Facade** (API surface) — exposes in-memory bus state as JSON
//! compatible with future PHP `Command::fromJson` field names.
//!
//! # Endpoints
//!
//! - `GET /status` — latest [`SystemStatus`](crate::messages::SystemStatus) +
//!   [`TempStatus`](crate::messages::TempStatus) (null until first good frame)
//! - `GET /frames?limit=` — newest-first ring buffer (default 32, max 128)
//!
//! Field names: see [`crate::messages`] module docs (`waterTemp`, `circuitStatus`,
//! `waterSet`, `raw` hex, …).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::state::{FramesResponse, SharedBusState, StatusResponse};

/// Query params for `GET /frames`.
#[derive(Debug, Deserialize)]
pub struct FramesQuery {
    /// Max frames to return (newest first). Default 32; capped at ring size.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    32
}

/// Registers status + frames routes with shared bus state.
pub fn routes(state: SharedBusState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/frames", get(get_frames))
        .with_state(state)
}

/// `GET /status` — latest typed snapshots.
pub async fn get_status(State(state): State<SharedBusState>) -> (StatusCode, Json<StatusResponse>) {
    let guard = state.read().await;
    (StatusCode::OK, Json(guard.status_response()))
}

/// `GET /frames?limit=` — recent decoded / quarantined frames.
pub async fn get_frames(
    State(state): State<SharedBusState>,
    Query(q): Query<FramesQuery>,
) -> (StatusCode, Json<FramesResponse>) {
    let limit = q.limit.clamp(1, 128);
    let guard = state.read().await;
    (StatusCode::OK, Json(guard.recent_frames(limit)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MessageRegistry;
    use crate::state::{apply_raw_a5_for_test, shared_bus_state};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const INFO: &str = "ffffffffffffffff00ffa50c0f10080d4848555a620400000000000000028a";
    const LIGHT_ON: &str = concat!(
        "ff00ffa50c0f10021d13330000000000000021000000043b3b00003c",
        "00000004000085df000d0381"
    );

    fn hx(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    async fn seed_state() -> SharedBusState {
        let state = shared_bus_state();
        let reg = MessageRegistry::with_defaults();
        {
            let mut g = state.write().await;
            apply_raw_a5_for_test(&mut g, &reg, &hx(INFO));
            apply_raw_a5_for_test(&mut g, &reg, &hx(LIGHT_ON));
        }
        state
    }

    #[tokio::test]
    async fn status_returns_php_field_names() {
        let app = routes(seed_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["systemStatus"]["hours"], 0x13);
        assert_eq!(body["systemStatus"]["waterTemp"], 0x3B);
        assert_eq!(body["systemStatus"]["circuitStatus"]["filterPump"], "off");
        assert_eq!(body["tempStatus"]["waterSet"], 0x5A);
        assert_eq!(body["tempStatus"]["spaSet"], 0x62);
        assert!(body["framesSeen"].as_u64().unwrap() >= 2);
    }

    #[tokio::test]
    async fn frames_respects_limit() {
        let app = routes(seed_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/frames?limit=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["count"], 1);
        assert_eq!(body["frames"].as_array().unwrap().len(), 1);
        // Newest is SystemStatus (LIGHT_ON applied last).
        assert_eq!(body["frames"][0]["message"]["command"], 0x02);
    }

    #[tokio::test]
    async fn frames_default_limit() {
        let q: FramesQuery = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(q.limit, 32);
    }

    #[tokio::test]
    async fn empty_status_nulls() {
        let app = routes(shared_bus_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["systemStatus"].is_null());
        assert!(body["tempStatus"].is_null());
    }
}
