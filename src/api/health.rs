//! Health check endpoint.
//!
//! Pattern: **Facade** (API surface) — `GET /health` is the minimal readiness signal
//! for process liveness without requiring a bus transport.

use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

/// JSON body for a successful health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthResponse {
    /// Always `"ok"` when the process is serving HTTP.
    pub status: &'static str,
}

impl HealthResponse {
    /// Creates an OK health payload.
    pub fn ok() -> Self {
        Self { status: "ok" }
    }
}

/// Registers health routes.
pub fn routes() -> Router {
    Router::new().route("/health", get(health))
}

/// `GET /health` — returns JSON `{ "status": "ok" }`.
pub async fn health() -> (StatusCode, Json<HealthResponse>) {
    (StatusCode::OK, Json(HealthResponse::ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn health_response_ok_payload() {
        assert_eq!(HealthResponse::ok().status, "ok");
    }

    #[tokio::test]
    async fn health_handler_returns_ok_json() {
        let app = routes();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "ok");
    }
}
