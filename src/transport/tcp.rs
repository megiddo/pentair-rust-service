//! Async TCP client transport for EW11 (and similar RS485↔TCP bridges).
//!
//! Pattern: **Strategy** implementation of [`crate::transport::ByteTransport`].

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use super::{ByteTransport, TransportError};

/// Persistent TCP connection to an EW11-style bridge.
#[derive(Debug)]
pub struct TcpTransport {
    host: String,
    port: u16,
    stream: Option<TcpStream>,
}

impl TcpTransport {
    /// Creates a disconnected TCP transport for `host:port`.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            stream: None,
        }
    }

    /// Target address string (`host:port`).
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[async_trait]
impl ByteTransport for TcpTransport {
    async fn open(&mut self) -> Result<(), TransportError> {
        let addr = self.addr();
        info!(%addr, "tcp transport connecting");
        let stream = TcpStream::connect(&addr).await?;
        stream.set_nodelay(true).ok();
        self.stream = Some(stream);
        info!(%addr, "tcp transport connected");
        Ok(())
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        if let Some(mut s) = self.stream.take() {
            debug!(addr = %self.addr(), "tcp transport closing");
            let _ = s.shutdown().await;
        }
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| {
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "tcp transport not open",
                ))
            })?;
        let n = stream.read(buf).await?;
        Ok(n)
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| {
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "tcp transport not open",
                ))
            })?;
        stream.write_all(data).await?;
        stream.flush().await?;
        Ok(())
    }

    fn name(&self) -> &str {
        "tcp"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tcp_connect_read_write_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(b"hello").await.unwrap();
            let mut buf = [0u8; 8];
            let n = sock.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ping");
        });

        let mut t = TcpTransport::new("127.0.0.1", addr.port());
        assert_eq!(t.name(), "tcp");
        t.open().await.unwrap();
        let mut buf = [0u8; 16];
        let n = t.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello");
        t.write(b"ping").await.unwrap();
        t.close().await.unwrap();
        // Idempotent close
        t.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn read_write_without_open_errors() {
        let mut t = TcpTransport::new("127.0.0.1", 1);
        assert!(t.read(&mut [0u8; 4]).await.is_err());
        assert!(t.write(b"x").await.is_err());
    }

    #[tokio::test]
    async fn connect_refused() {
        // Port 1 is typically closed on loopback.
        let mut t = TcpTransport::new("127.0.0.1", 1);
        let err = t.open().await.expect_err("should refuse");
        assert!(matches!(err, TransportError::Io(_)));
    }
}
