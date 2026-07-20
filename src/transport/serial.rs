//! Async serial port transport (USB-RS485 adapters).
//!
//! Pattern: **Strategy** implementation of [`crate::transport::ByteTransport`].
//! Baud defaults to 9600 (Pentair RS485). Device path comes from config
//! (`/dev/ttyUSB0` or `serial:/dev/ttyUSB0?baud=9600`).

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tracing::{debug, info};

use super::{ByteTransport, TransportError};

/// Persistent serial connection to an RS485 adapter.
#[derive(Debug)]
pub struct SerialTransport {
    path: String,
    baud: u32,
    stream: Option<SerialStream>,
}

impl SerialTransport {
    /// Creates a disconnected serial transport.
    pub fn new(path: impl Into<String>, baud: u32) -> Self {
        Self {
            path: path.into(),
            baud,
            stream: None,
        }
    }

    /// Device path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Configured baud rate.
    pub fn baud(&self) -> u32 {
        self.baud
    }
}

#[async_trait]
impl ByteTransport for SerialTransport {
    async fn open(&mut self) -> Result<(), TransportError> {
        info!(path = %self.path, baud = self.baud, "serial transport opening");
        let stream = tokio_serial::new(&self.path, self.baud)
            .open_native_async()
            .map_err(|e| TransportError::Serial(e.to_string()))?;
        self.stream = Some(stream);
        info!(path = %self.path, "serial transport opened");
        Ok(())
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        if self.stream.take().is_some() {
            debug!(path = %self.path, "serial transport closing");
        }
        Ok(())
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let stream = self.stream.as_mut().ok_or_else(|| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "serial transport not open",
            ))
        })?;
        let n = stream.read(buf).await?;
        Ok(n)
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let stream = self.stream.as_mut().ok_or_else(|| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "serial transport not open",
            ))
        })?;
        stream.write_all(data).await?;
        stream.flush().await?;
        Ok(())
    }

    fn name(&self) -> &str {
        "serial"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_with_path_and_baud() {
        let t = SerialTransport::new("/dev/ttyUSB0", 9600);
        assert_eq!(t.path(), "/dev/ttyUSB0");
        assert_eq!(t.baud(), 9600);
        assert_eq!(t.name(), "serial");
    }

    #[tokio::test]
    async fn open_missing_device_errors() {
        let mut t = SerialTransport::new("/dev/nonexistent-pentair-serial", 9600);
        let err = t.open().await.expect_err("missing device");
        assert!(matches!(err, TransportError::Serial(_)));
    }

    #[tokio::test]
    async fn read_write_without_open_errors() {
        let mut t = SerialTransport::new("/dev/ttyUSB0", 9600);
        assert!(t.read(&mut [0u8; 4]).await.is_err());
        assert!(t.write(b"x").await.is_err());
        t.close().await.unwrap();
    }
}
