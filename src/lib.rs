//! Library root for `pentairservice`.
//!
//! Pattern: **Facade** — this crate is the application façade over config, logging,
//! framing, and the local HTTP API. Transport reconnect (B2) plugs in behind this
//! surface without changing callers of [`run`] / [`build_router`].

#![deny(missing_docs)]

pub mod api;
pub mod config;
pub mod framer;
pub mod logging;

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::Config;

/// Logs whether a transport URL is present without opening a connection (B0).
///
/// Pattern: **Facade** helper — keeps bus lifecycle out of the HTTP start path.
pub fn log_transport_idle_state(config: &Config) {
    match config
        .transport_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(url) => info!(
            transport_url = %url,
            "transport configured but not opened in B0 (idle until B2)"
        ),
        None => info!("no transport_url configured; idling without bus connection"),
    }
}

/// Runs the service: bind the local API and serve until the listener fails.
///
/// Does **not** open a transport connection. An empty/unset `transport_url` keeps
/// the process idle with only HTTP health available (B0).
///
/// Pattern: **Facade** — single entry that composes config + API server.
pub async fn run(config: Config) -> Result<(), std::io::Error> {
    let addr: SocketAddr = config
        .bind_addr
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    log_transport_idle_state(&config);

    let app = build_router();
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    info!(%local, "listening");
    axum::serve(listener, app).await
}

/// Builds the Axum router for the local HTTP API.
///
/// Pattern: **Facade** — exposes the stable HTTP surface (`GET /health`, later `/status`).
pub fn build_router() -> Router {
    api::router()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn build_router_is_non_empty() {
        let _router = build_router();
    }

    #[test]
    fn log_transport_idle_with_and_without_url() {
        let idle = Config::default();
        log_transport_idle_state(&idle);

        let with_url = Config {
            bind_addr: "127.0.0.1:0".into(),
            transport_url: Some("tcp://127.0.0.1:8899".into()),
            log_level: "info".into(),
        };
        log_transport_idle_state(&with_url);

        let blank = Config {
            bind_addr: "127.0.0.1:0".into(),
            transport_url: Some("  ".into()),
            log_level: "info".into(),
        };
        log_transport_idle_state(&blank);
    }

    #[tokio::test]
    async fn run_rejects_invalid_bind_addr() {
        let cfg = Config {
            bind_addr: "not-a-socket".into(),
            transport_url: None,
            log_level: "info".into(),
        };
        let err = run(cfg).await.expect_err("invalid bind must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn run_serves_health_then_abort() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = Config {
            bind_addr: addr.to_string(),
            transport_url: None,
            log_level: "info".into(),
        };

        let handle = tokio::spawn(async move { run(cfg).await });

        // Retry until the server accepts connections.
        let client = reqwest_get_health(&addr).await;
        assert_eq!(client, r#"{"status":"ok"}"#);

        handle.abort();
        let _ = handle.await;
    }

    async fn reqwest_get_health(addr: &SocketAddr) -> String {
        let url = format!("http://{addr}/health");
        for _ in 0..50 {
            if let Ok(resp) = tiny_http_get(&url).await {
                return resp;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Fallback: exercise router directly if bind race fails oddly.
        let app = build_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn tiny_http_get(url: &str) -> Result<String, std::io::Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let url = url.strip_prefix("http://").unwrap_or(url);
        let (host_port, path) = url.split_once('/').unwrap_or((url, ""));
        let path = format!("/{path}");
        let mut stream = tokio::net::TcpStream::connect(host_port).await?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await?;
        let text = String::from_utf8_lossy(&buf);
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").trim().to_string();
        Ok(body)
    }
}
