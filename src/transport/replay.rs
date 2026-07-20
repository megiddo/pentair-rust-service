//! Recorded hex-fixture replay transport (lab / automated soak without live EW11).
//!
//! Pattern: **Strategy** — feeds fixture bytes as if from the wire. Optional
//! disconnect injection lets reconnect tests exercise the Actor without hardware.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::debug;

use super::{ByteTransport, TransportError};

/// Parses dump-style or contiguous hex text into bytes (parity with pentairsnoop).
pub fn parse_hex_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty()
            || stripped.starts_with('#')
            || stripped.starts_with("/*")
            || stripped.starts_with("*/")
        {
            continue;
        }
        let cleaned = strip_dump_prefix(stripped);
        push_hex_pairs(&cleaned, &mut out);
    }
    if out.is_empty() {
        push_hex_pairs(text, &mut out);
    }
    out
}

fn strip_dump_prefix(line: &str) -> String {
    let mut s = line.to_string();
    // "< 0x0\t ..." or "0000: ..."
    if let Some(rest) = s.strip_prefix('<') {
        let rest = rest.trim_start();
        if let Some(idx) = rest.find(|c: char| c.is_whitespace()) {
            s = rest[idx..].trim_start().to_string();
        } else {
            s = rest.to_string();
        }
    }
    if let Some(idx) = s.find(':') {
        let prefix = &s[..idx];
        if prefix.chars().all(|c| c.is_ascii_hexdigit() || c == 'x' || c == 'X') {
            s = s[idx + 1..].trim_start().to_string();
        }
    }
    s
}

fn push_hex_pairs(s: &str, out: &mut Vec<u8>) {
    let mut chars: Vec<char> = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if chars.len() % 2 == 1 {
        chars.pop();
    }
    for chunk in chars.chunks(2) {
        let a = chunk[0].to_digit(16).unwrap() as u8;
        let b = chunk[1].to_digit(16).unwrap() as u8;
        out.push((a << 4) | b);
    }
}

/// In-memory / file-backed byte stream for tests and recorded soak.
#[derive(Debug)]
pub struct ReplayTransport {
    label: String,
    path: Option<PathBuf>,
    data: Vec<u8>,
    pos: usize,
    opened: bool,
    /// After this many successful `read` calls that returned data, next read
    /// returns a disconnect error (then clears). `None` = never.
    disconnect_after_reads: Option<u32>,
    reads_with_data: u32,
    /// When true, `open` rewinds to the start (looping soak). Default true.
    rewind_on_open: bool,
    write_allowed: bool,
}

impl ReplayTransport {
    /// Replay from an already-decoded byte buffer.
    pub fn from_bytes(data: impl Into<Vec<u8>>) -> Self {
        Self {
            label: "replay:memory".into(),
            path: None,
            data: data.into(),
            pos: 0,
            opened: false,
            disconnect_after_reads: None,
            reads_with_data: 0,
            rewind_on_open: true,
            write_allowed: false,
        }
    }

    /// Replay from a hex fixture path (loaded on [`ByteTransport::open`]).
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let label = format!("replay:{}", path.display());
        Self {
            label,
            path: Some(path),
            data: Vec::new(),
            pos: 0,
            opened: false,
            disconnect_after_reads: None,
            reads_with_data: 0,
            rewind_on_open: true,
            write_allowed: false,
        }
    }

    /// Inject a disconnect after `n` non-empty reads (reconnect test hook).
    pub fn disconnect_after_reads(mut self, n: u32) -> Self {
        self.disconnect_after_reads = Some(n);
        self
    }

    /// When false, subsequent `open` does not reload/rewind (one-shot EOF).
    pub fn rewind_on_open(mut self, rewind: bool) -> Self {
        self.rewind_on_open = rewind;
        self
    }

    /// Allow writes (default rejects — fixtures are read-only).
    pub fn allow_write(mut self, allow: bool) -> Self {
        self.write_allowed = allow;
        self
    }

    /// Bytes remaining in the current buffer.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Total loaded payload length.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True when no bytes are loaded.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    fn load_from_path(path: &Path) -> Result<Vec<u8>, TransportError> {
        let text = std::fs::read_to_string(path).map_err(TransportError::Io)?;
        Ok(parse_hex_bytes(&text))
    }
}

#[async_trait]
impl ByteTransport for ReplayTransport {
    async fn open(&mut self) -> Result<(), TransportError> {
        if let Some(path) = &self.path {
            if self.data.is_empty() || self.rewind_on_open {
                self.data = Self::load_from_path(path)?;
            }
        }
        if self.rewind_on_open {
            self.pos = 0;
            self.reads_with_data = 0;
        }
        self.opened = true;
        debug!(
            label = %self.label,
            bytes = self.data.len(),
            "replay transport opened"
        );
        Ok(())
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.opened = false;
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if !self.opened {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "replay transport not open",
            )));
        }
        if let Some(limit) = self.disconnect_after_reads {
            if self.reads_with_data >= limit {
                self.disconnect_after_reads = None;
                self.opened = false;
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected replay disconnect",
                )));
            }
        }
        if self.pos >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        if n > 0 {
            self.reads_with_data = self.reads_with_data.saturating_add(1);
        }
        Ok(n)
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        if !self.write_allowed {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "replay transport is read-only",
            )));
        }
        let _ = data;
        Ok(())
    }

    fn name(&self) -> &str {
        "replay"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dump_fixture_style() {
        let text = "< 0x0\t ff 00 ff a5 01\n< 0x5\t 02 03\n";
        assert_eq!(
            parse_hex_bytes(text),
            vec![0xff, 0x00, 0xff, 0xa5, 0x01, 0x02, 0x03]
        );
    }

    #[test]
    fn parse_contiguous_and_comments() {
        let text = "# comment\nAABB\n/* c */\nCC\n";
        assert_eq!(parse_hex_bytes(text), vec![0xaa, 0xbb, 0xcc]);
    }

    #[tokio::test]
    async fn replay_from_bytes_chunked() {
        let mut t = ReplayTransport::from_bytes(vec![1, 2, 3, 4, 5]);
        t.open().await.unwrap();
        let mut buf = [0u8; 2];
        assert_eq!(t.read(&mut buf).await.unwrap(), 2);
        assert_eq!(&buf, &[1, 2]);
        assert_eq!(t.read(&mut buf).await.unwrap(), 2);
        assert_eq!(t.remaining(), 1);
        assert_eq!(t.read(&mut buf).await.unwrap(), 1);
        assert_eq!(t.read(&mut buf).await.unwrap(), 0);
        t.close().await.unwrap();
    }

    #[tokio::test]
    async fn replay_from_fixture_file() {
        let mut t = ReplayTransport::from_path("fixtures/status_temps.hex");
        t.open().await.unwrap();
        assert!(!t.is_empty());
        assert!(t.len() > 10);
        let mut buf = [0u8; 64];
        let n = t.read(&mut buf).await.unwrap();
        assert!(n > 0);
        // Leading idle FF from fixture
        assert_eq!(buf[0], 0xff);
    }

    #[tokio::test]
    async fn injected_disconnect() {
        let mut t = ReplayTransport::from_bytes(vec![9, 8, 7, 6]).disconnect_after_reads(1);
        t.open().await.unwrap();
        let mut buf = [0u8; 2];
        assert_eq!(t.read(&mut buf).await.unwrap(), 2);
        let err = t.read(&mut buf).await.expect_err("disconnect");
        assert!(matches!(err, TransportError::Io(_)));
    }

    #[tokio::test]
    async fn write_rejected_unless_allowed() {
        let mut t = ReplayTransport::from_bytes(vec![1]);
        t.open().await.unwrap();
        assert!(t.write(b"x").await.is_err());
        let mut t = ReplayTransport::from_bytes(vec![1]).allow_write(true);
        t.open().await.unwrap();
        t.write(b"x").await.unwrap();
    }

    #[tokio::test]
    async fn not_open_errors() {
        let mut t = ReplayTransport::from_bytes(vec![1]);
        assert!(t.read(&mut [0u8; 1]).await.is_err());
    }

    #[tokio::test]
    async fn missing_file_on_open() {
        let mut t = ReplayTransport::from_path("/nonexistent/pentair-replay.hex");
        assert!(t.open().await.is_err());
    }

    #[test]
    fn colon_address_prefix() {
        let text = "0000: aa bb\n0010: cc\n";
        assert_eq!(parse_hex_bytes(text), vec![0xaa, 0xbb, 0xcc]);
    }
}
