//! Persistent bus I/O: Strategy transports + Actor task ownership.
//!
//! # Patterns
//!
//! - **Strategy** — [`ByteTransport`] abstracts TCP (EW11), serial, and recorded
//!   replay backends. Callers select a concrete strategy via [`from_url`].
//! - **Actor** — [`actor::BusActor`] runs as a **single tokio task** that owns the
//!   open socket/port for the process lifetime. It reconnects with exponential
//!   backoff + jitter on failure and **never** tears down the connection per
//!   frame (contrast PHP `PentairComFacade` open/close-per-op).
//!
//! Bytes from the persistent transport feed [`crate::framer::Framer::feed`].

pub mod actor;
pub mod backoff;
pub mod replay;
pub mod serial;
pub mod tcp;

use async_trait::async_trait;
use thiserror::Error;

pub use actor::{BusActor, BusActorConfig, BusStats};
pub use backoff::{Backoff, BackoffConfig};
pub use replay::ReplayTransport;
pub use serial::SerialTransport;
pub use tcp::TcpTransport;

/// Errors from transport URL parsing or I/O.
#[derive(Debug, Error)]
pub enum TransportError {
    /// URL / path could not be interpreted as a transport.
    #[error("invalid transport URL: {0}")]
    InvalidUrl(String),
    /// Underlying I/O failure (connect, read, write, serial open).
    #[error("transport I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Serial configuration rejected by the OS / driver.
    #[error("serial config: {0}")]
    Serial(String),
}

/// Pattern: **Strategy** — pluggable async byte stream to the Pentair bus.
///
/// Lifecycle: [`open`](ByteTransport::open) once, then repeated
/// [`read`](ByteTransport::read) / [`write`](ByteTransport::write) on the **same**
/// connection; [`close`](ByteTransport::close) only on shutdown or before
/// reconnect. Implementations must not open/close around each frame.
#[async_trait]
pub trait ByteTransport: Send {
    /// Establish the underlying connection or load the replay buffer.
    async fn open(&mut self) -> Result<(), TransportError>;

    /// Release the connection (idempotent).
    async fn close(&mut self) -> Result<(), TransportError>;

    /// Read up to `buf.len()` bytes. Returns `Ok(0)` on clean end-of-stream
    /// (replay exhausted, peer half-close). A mid-session disconnect should
    /// return `Err` so the Actor can reconnect.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;

    /// Write bytes to the bus (lab / write-gate paths). Replay may reject writes.
    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError>;

    /// Human-readable backend label for logs.
    fn name(&self) -> &str;
}

/// Parsed transport endpoint from a config URL / path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEndpoint {
    /// EW11 (or other) TCP bridge: `tcp://host:port`.
    Tcp {
        /// Hostname or IP.
        host: String,
        /// TCP port.
        port: u16,
    },
    /// Serial device path (e.g. `/dev/ttyUSB0` or `serial:/dev/ttyUSB0`).
    Serial {
        /// Device node path.
        path: String,
        /// Baud rate (default 9600 for Pentair RS485).
        baud: u32,
    },
    /// Recorded hex fixture replay: `replay:path` or `replay://path`.
    Replay {
        /// Path to a `.hex` fixture (dump or contiguous hex).
        path: String,
    },
}

impl TransportEndpoint {
    /// Parses `tcp://host:port`, `serial:/dev/…`, bare `/dev/…`, or `replay:…`.
    pub fn parse(url: &str) -> Result<Self, TransportError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(TransportError::InvalidUrl(
                "empty transport URL".into(),
            ));
        }

        if let Some(rest) = url.strip_prefix("tcp://") {
            return parse_tcp(rest);
        }
        if let Some(rest) = url
            .strip_prefix("replay://")
            .or_else(|| url.strip_prefix("replay:"))
            .or_else(|| url.strip_prefix("hexfile://"))
            .or_else(|| url.strip_prefix("hexfile:"))
        {
            let path = rest.trim();
            if path.is_empty() {
                return Err(TransportError::InvalidUrl(
                    "replay path is empty".into(),
                ));
            }
            return Ok(Self::Replay {
                path: path.to_string(),
            });
        }
        if let Some(rest) = url
            .strip_prefix("serial://")
            .or_else(|| url.strip_prefix("serial:"))
        {
            return parse_serial(rest);
        }
        if url.starts_with('/') || url.starts_with('.') {
            return parse_serial(url);
        }

        Err(TransportError::InvalidUrl(format!(
            "unsupported scheme (want tcp://, serial:, replay:, or /dev path): {url}"
        )))
    }
}

fn parse_tcp(rest: &str) -> Result<TransportEndpoint, TransportError> {
    let rest = rest.trim();
    // host:port — allow IPv6 in [brackets]
    let (host, port_str) = if let Some(stripped) = rest.strip_prefix('[') {
        let (h, after) = stripped
            .split_once(']')
            .ok_or_else(|| TransportError::InvalidUrl("unclosed IPv6 bracket".into()))?;
        let port_str = after
            .strip_prefix(':')
            .ok_or_else(|| TransportError::InvalidUrl("tcp URL missing :port".into()))?;
        (h, port_str)
    } else {
        rest.rsplit_once(':')
            .ok_or_else(|| TransportError::InvalidUrl("tcp URL missing host:port".into()))?
    };
    if host.is_empty() {
        return Err(TransportError::InvalidUrl("tcp host is empty".into()));
    }
    let port: u16 = port_str.parse().map_err(|_| {
        TransportError::InvalidUrl(format!("invalid tcp port: {port_str}"))
    })?;
    Ok(TransportEndpoint::Tcp {
        host: host.to_string(),
        port,
    })
}

fn parse_serial(spec: &str) -> Result<TransportEndpoint, TransportError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(TransportError::InvalidUrl("serial path is empty".into()));
    }
    // Optional `?baud=115200` or `,115200`
    let (path, baud) = if let Some((path, q)) = spec.split_once('?') {
        let baud = q
            .strip_prefix("baud=")
            .unwrap_or(q)
            .parse::<u32>()
            .map_err(|_| TransportError::InvalidUrl(format!("bad baud in {q}")))?;
        (path, baud)
    } else if let Some((path, baud_str)) = spec.rsplit_once(',') {
        if baud_str.chars().all(|c| c.is_ascii_digit()) && !baud_str.is_empty() {
            let baud = baud_str.parse::<u32>().map_err(|_| {
                TransportError::InvalidUrl(format!("bad baud: {baud_str}"))
            })?;
            (path, baud)
        } else {
            (spec, 9600)
        }
    } else {
        (spec, 9600)
    };
    if path.is_empty() {
        return Err(TransportError::InvalidUrl("serial path is empty".into()));
    }
    Ok(TransportEndpoint::Serial {
        path: path.to_string(),
        baud,
    })
}

/// Builds a concrete Strategy implementation from a transport URL.
pub fn from_url(url: &str) -> Result<Box<dyn ByteTransport>, TransportError> {
    match TransportEndpoint::parse(url)? {
        TransportEndpoint::Tcp { host, port } => Ok(Box::new(TcpTransport::new(host, port))),
        TransportEndpoint::Serial { path, baud } => {
            Ok(Box::new(SerialTransport::new(path, baud)))
        }
        TransportEndpoint::Replay { path } => Ok(Box::new(ReplayTransport::from_path(path))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tcp_host_port() {
        assert_eq!(
            TransportEndpoint::parse("tcp://192.168.1.50:8899").unwrap(),
            TransportEndpoint::Tcp {
                host: "192.168.1.50".into(),
                port: 8899,
            }
        );
    }

    #[test]
    fn parse_tcp_ipv6() {
        assert_eq!(
            TransportEndpoint::parse("tcp://[::1]:8899").unwrap(),
            TransportEndpoint::Tcp {
                host: "::1".into(),
                port: 8899,
            }
        );
    }

    #[test]
    fn parse_serial_path_and_baud() {
        assert_eq!(
            TransportEndpoint::parse("/dev/ttyUSB0").unwrap(),
            TransportEndpoint::Serial {
                path: "/dev/ttyUSB0".into(),
                baud: 9600,
            }
        );
        assert_eq!(
            TransportEndpoint::parse("serial:/dev/ttyUSB0?baud=115200").unwrap(),
            TransportEndpoint::Serial {
                path: "/dev/ttyUSB0".into(),
                baud: 115200,
            }
        );
        assert_eq!(
            TransportEndpoint::parse("serial:///dev/ttyUSB0,19200").unwrap(),
            TransportEndpoint::Serial {
                path: "/dev/ttyUSB0".into(),
                baud: 19200,
            }
        );
    }

    #[test]
    fn parse_replay() {
        assert_eq!(
            TransportEndpoint::parse("replay:fixtures/status_temps.hex").unwrap(),
            TransportEndpoint::Replay {
                path: "fixtures/status_temps.hex".into(),
            }
        );
        assert_eq!(
            TransportEndpoint::parse("hexfile://fixtures/x.hex").unwrap(),
            TransportEndpoint::Replay {
                path: "fixtures/x.hex".into(),
            }
        );
    }

    #[test]
    fn parse_rejects_empty_and_unknown() {
        assert!(TransportEndpoint::parse("").is_err());
        assert!(TransportEndpoint::parse("http://x").is_err());
        assert!(TransportEndpoint::parse("tcp://:8899").is_err());
        assert!(TransportEndpoint::parse("tcp://host").is_err());
        assert!(TransportEndpoint::parse("replay:").is_err());
        assert!(TransportEndpoint::parse("serial:").is_err());
        assert!(TransportEndpoint::parse("tcp://[::1").is_err());
    }

    #[test]
    fn from_url_builds_strategies() {
        let t = from_url("tcp://127.0.0.1:8899").unwrap();
        assert_eq!(t.name(), "tcp");
        let r = from_url("replay:fixtures/status_temps.hex").unwrap();
        assert_eq!(r.name(), "replay");
        let s = from_url("/dev/ttyUSB0").unwrap();
        assert_eq!(s.name(), "serial");
    }

    #[test]
    fn transport_error_display() {
        let e = TransportError::InvalidUrl("nope".into());
        assert!(e.to_string().contains("nope"));
        let e = TransportError::Serial("baud".into());
        assert!(e.to_string().contains("baud"));
    }
}
