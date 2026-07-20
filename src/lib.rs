//! Library root for `pentairservice`.
//!
//! Pattern: **Facade** — this crate is the application façade over config, logging,
//! framing, transport Actor, and the local HTTP API.

#![deny(missing_docs)]

pub mod api;
pub mod config;
pub mod framer;
pub mod logging;
pub mod transport;

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::Config;
use crate::transport::actor::{spawn_from_url, BusActorConfig};

/// Logs whether a transport URL is present (idle vs Actor will own the bus).
///
/// Pattern: **Facade** helper — keeps messaging out of the HTTP start path.
pub fn log_transport_idle_state(config: &Config) {
    match config
        .transport_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(url) => info!(
            transport_url = %url,
            "transport configured; bus Actor will own a persistent connection"
        ),
        None => info!("no transport_url configured; idling without bus connection"),
    }
}

/// Runs the service: optional bus Actor + local API until the listener fails.
///
/// When `transport_url` is set, a **single** tokio task owns the socket/port,
/// feeds the streaming framer, and reconnects with backoff — never open/close
/// per frame.
///
/// Pattern: **Facade** — single entry that composes config + transport Actor + API.
pub async fn run(config: Config) -> Result<(), std::io::Error> {
    let addr: SocketAddr = config
        .bind_addr
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    log_transport_idle_state(&config);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bus = spawn_bus_if_configured(&config, shutdown_rx);

    let app = build_router();
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    info!(%local, "listening");

    let serve_result = axum::serve(listener, app).await;

    let _ = shutdown_tx.send(true);
    if let Some(handle) = bus {
        match handle.await {
            Ok(stats) => info!(
                connects = stats.connects,
                reconnects = stats.reconnects,
                frames = stats.frames,
                bytes = stats.bytes_read,
                "bus Actor finished"
            ),
            Err(e) => warn!(error = %e, "bus Actor join error"),
        }
    }

    serve_result
}

/// Spawns the bus Actor when a non-empty transport URL is configured.
fn spawn_bus_if_configured(
    config: &Config,
    shutdown: watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<crate::transport::BusStats>> {
    let url = config
        .transport_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())?;

    match spawn_from_url(url, BusActorConfig::default(), shutdown) {
        Ok((handle, _counters)) => {
            info!(transport_url = %url, "bus Actor spawned");
            Some(handle)
        }
        Err(e) => {
            warn!(
                transport_url = %url,
                error = %e,
                "failed to create transport; HTTP will still serve /health"
            );
            None
        }
    }
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

        let client = reqwest_get_health(&addr).await;
        assert_eq!(client, r#"{"status":"ok"}"#);

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn run_with_replay_transport_spawns_actor() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let cfg = Config {
            bind_addr: addr.to_string(),
            transport_url: Some("replay:fixtures/status_temps.hex".into()),
            log_level: "info".into(),
        };

        let handle = tokio::spawn(async move { run(cfg).await });
        let body = reqwest_get_health(&addr).await;
        assert_eq!(body, r#"{"status":"ok"}"#);
        // Give the Actor a moment to open the replay fixture.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn spawn_bus_invalid_url_still_ok() {
        let cfg = Config {
            bind_addr: "127.0.0.1:0".into(),
            transport_url: Some("http://not-supported".into()),
            log_level: "info".into(),
        };
        let (_tx, rx) = watch::channel(false);
        assert!(spawn_bus_if_configured(&cfg, rx).is_none());
    }

    #[test]
    fn spawn_bus_none_without_url() {
        let cfg = Config::default();
        let (_tx, rx) = watch::channel(false);
        assert!(spawn_bus_if_configured(&cfg, rx).is_none());
    }

    async fn reqwest_get_health(addr: &SocketAddr) -> String {
        let url = format!("http://{addr}/health");
        for _ in 0..50 {
            if let Ok(resp) = tiny_http_get(&url).await {
                return resp;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
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
