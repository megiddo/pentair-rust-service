//! Write gate: single in-flight bus write with listen-window verdict.
//!
//! # Patterns
//!
//! - **Mutex / Gate** — at most one write in flight. Concurrent attempts are
//!   **rejected** (HTTP 409 / [`WriteGateError::Busy`]), not queued — keeps the
//!   API predictable and avoids unbounded backlog on a half-duplex bus.
//! - **Command** — TX bytes come from [`crate::commands`] builders or raw hex.
//! - **Actor** — when `writes_enabled` is true, TX is sent through the bus Actor
//!   write channel (Actor still owns the socket). Default `writes_enabled=false`
//!   dry-runs from fixture ACK frames so CI never needs a live controller.
//!
//! # Listen window
//!
//! After TX (or simulated TX), RX frames captured for `listen_window_ms` are
//! classified:
//! - `ack` — CMD `0x01` with payload = acked opcode (`0x86` / `0x88`)
//! - `status` — SYSTEM_STATUS (`0x02`) or TempStatus (`0x08`) seen (no matching ACK)
//! - `timeout` — neither within the window

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{info, warn};

use crate::commands::{
    decode_hex, fixtures, CircuitChange, HeatChange, CommandError, CMD_ACK, CMD_CIRCUIT_CHANGE,
    CMD_HEAT_CHANGE,
};
use crate::framer::{Frame, Framer};
use crate::messages::{find_a5_index, hex_encode, DecodeError};
use crate::transport::TransportError;

/// Default listen window after TX (milliseconds).
pub const DEFAULT_LISTEN_WINDOW_MS: u64 = 500;

/// Verdict from the post-TX listen window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteVerdict {
    /// Matching ACK (`0x01` / acked opcode) observed.
    Ack,
    /// Status / temp frame observed without a matching ACK.
    Status,
    /// Nothing useful within the listen window.
    Timeout,
}

/// Result of a gated write (live or dry-run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WriteResult {
    /// Whether the bus was actually written (`false` = dry-run / simulate).
    pub dry_run: bool,
    /// TX framed bytes as lowercase hex.
    pub tx_hex: String,
    /// Command opcode that was sent (`0x86` / `0x88`).
    pub command: u8,
    /// Listen-window classification.
    pub verdict: WriteVerdict,
    /// RX frames (hex) observed / simulated during the window.
    pub rx_hex: Vec<String>,
    /// Listen window duration used (ms).
    pub listen_window_ms: u64,
}

/// Errors from the write gate.
#[derive(Debug, thiserror::Error)]
pub enum WriteGateError {
    /// Another write is already in flight (reject policy).
    #[error("write gate busy: only one in-flight write allowed")]
    Busy,
    /// Command craft / hex parse failure.
    #[error(transparent)]
    Command(#[from] CommandError),
    /// Live TX failed on the transport.
    #[error("transport write failed: {0}")]
    Transport(#[from] TransportError),
    /// Live writes requested but no Actor write handle is available.
    #[error("writes_enabled but no bus write handle (no transport Actor)")]
    NoWriteHandle,
    /// Unsupported / unrecognized write command.
    #[error("unsupported write command: {0}")]
    Unsupported(String),
}

/// Handle for Actor-owned TX (oneshot reply per write).
#[derive(Clone)]
pub struct BusWriteHandle {
    tx: tokio::sync::mpsc::Sender<BusWriteRequest>,
}

/// Request delivered to the bus Actor for a single write.
pub struct BusWriteRequest {
    /// Bytes to write on the open connection.
    pub data: Vec<u8>,
    /// Reply channel for I/O result.
    pub reply: tokio::sync::oneshot::Sender<Result<(), TransportError>>,
}

impl BusWriteHandle {
    /// Creates a handle + the receiver the Actor should poll.
    pub fn channel(buffer: usize) -> (Self, tokio::sync::mpsc::Receiver<BusWriteRequest>) {
        let (tx, rx) = tokio::sync::mpsc::channel(buffer);
        (Self { tx }, rx)
    }

    /// Sends bytes via the Actor (serialized with the read loop).
    pub async fn write(&self, data: Vec<u8>) -> Result<(), TransportError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(BusWriteRequest {
                data,
                reply: reply_tx,
            })
            .await
            .map_err(|_| {
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "bus Actor write channel closed",
                ))
            })?;
        reply_rx.await.map_err(|_| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "bus Actor dropped write reply",
            ))
        })?
    }
}

/// Pattern: **Mutex / Gate** — serializes writes; rejects when busy.
pub struct WriteGate {
    lock: Arc<Mutex<()>>,
    /// When false (default), never touch the live bus — simulate ACK from fixtures.
    writes_enabled: bool,
    listen_window: Duration,
    write_handle: Option<BusWriteHandle>,
}

impl WriteGate {
    /// Creates a gate. `writes_enabled` defaults off for CI safety.
    pub fn new(
        writes_enabled: bool,
        listen_window_ms: u64,
        write_handle: Option<BusWriteHandle>,
    ) -> Self {
        Self {
            lock: Arc::new(Mutex::new(())),
            writes_enabled,
            listen_window: Duration::from_millis(listen_window_ms.max(1)),
            write_handle,
        }
    }

    /// Shared handle for the HTTP API.
    pub fn shared(self) -> SharedWriteGate {
        Arc::new(self)
    }

    /// True when live bus TX is allowed.
    pub fn writes_enabled(&self) -> bool {
        self.writes_enabled
    }

    /// Listen window duration.
    pub fn listen_window(&self) -> Duration {
        self.listen_window
    }

    /// Try to acquire the gate without waiting (reject policy).
    pub fn try_lock(&self) -> Result<OwnedMutexGuard<()>, WriteGateError> {
        self.lock
            .clone()
            .try_lock_owned()
            .map_err(|_| WriteGateError::Busy)
    }

    /// Execute a CircuitChange write (typed).
    pub async fn circuit_change(
        &self,
        circuit: u8,
        on: bool,
    ) -> Result<WriteResult, WriteGateError> {
        let cmd = CircuitChange::build(circuit, on)?;
        self.execute(CMD_CIRCUIT_CHANGE, cmd.raw).await
    }

    /// Execute a HeatChange write (typed).
    pub async fn heat_change(
        &self,
        pool_set: u8,
        spa_set: u8,
        mode: u8,
    ) -> Result<WriteResult, WriteGateError> {
        let cmd = HeatChange::build(pool_set, spa_set, mode)?;
        self.execute(CMD_HEAT_CHANGE, cmd.raw).await
    }

    /// Execute from raw framed hex (must be `0x86` or `0x88`).
    pub async fn raw_hex(&self, hex: &str) -> Result<WriteResult, WriteGateError> {
        let raw = decode_hex(hex)?;
        let cmd = command_byte(&raw)?;
        if cmd != CMD_CIRCUIT_CHANGE && cmd != CMD_HEAT_CHANGE {
            return Err(WriteGateError::Unsupported(format!(
                "0x{cmd:02x} (only 0x86 / 0x88)"
            )));
        }
        self.execute(cmd, raw).await
    }

    async fn execute(
        &self,
        command: u8,
        raw: Vec<u8>,
    ) -> Result<WriteResult, WriteGateError> {
        let _guard = self.try_lock()?;
        let tx_hex = hex_encode(&raw);
        let listen_ms = self.listen_window.as_millis() as u64;

        if !self.writes_enabled {
            let rx_hex = dry_run_rx_hex(command);
            let frames = frame_hex_list(&rx_hex);
            let verdict = classify_listen(command, &frames);
            info!(
                command = format_args!("0x{command:02x}"),
                %tx_hex,
                ?verdict,
                "write gate dry-run (writes_enabled=false)"
            );
            return Ok(WriteResult {
                dry_run: true,
                tx_hex,
                command,
                verdict,
                rx_hex,
                listen_window_ms: listen_ms,
            });
        }

        let handle = self
            .write_handle
            .as_ref()
            .ok_or(WriteGateError::NoWriteHandle)?;

        // Snapshot: live path relies on caller-supplied listen frames via
        // `execute_live_with_rx` in tests; production uses Actor tee — for the
        // default live path we TX then wait and classify empty unless frames
        // arrive through `listen_after_tx`.
        handle.write(raw.clone()).await?;
        let rx_frames = listen_after_tx(self.listen_window, &[]).await;
        let verdict = classify_listen(command, &rx_frames);
        let rx_hex: Vec<String> = rx_frames.iter().map(|f| hex_encode(&f.raw)).collect();
        if matches!(verdict, WriteVerdict::Timeout) {
            warn!(
                command = format_args!("0x{command:02x}"),
                "write gate listen window timed out (no ACK/status)"
            );
        }
        Ok(WriteResult {
            dry_run: false,
            tx_hex,
            command,
            verdict,
            rx_hex,
            listen_window_ms: listen_ms,
        })
    }

    /// Live write with an explicit RX byte stream for the listen window
    /// (tests / Actor tee). Still respects the mutex and `writes_enabled`.
    pub async fn execute_with_listen_bytes(
        &self,
        command: u8,
        raw: Vec<u8>,
        listen_bytes: &[u8],
    ) -> Result<WriteResult, WriteGateError> {
        let _guard = self.try_lock()?;
        let tx_hex = hex_encode(&raw);
        let listen_ms = self.listen_window.as_millis() as u64;

        if self.writes_enabled {
            if let Some(handle) = &self.write_handle {
                handle.write(raw).await?;
            } else {
                return Err(WriteGateError::NoWriteHandle);
            }
        }

        let frames = {
            let mut framer = Framer::new();
            framer.feed(listen_bytes)
        };
        let verdict = classify_listen(command, &frames);
        let rx_hex: Vec<String> = frames.iter().map(|f| hex_encode(&f.raw)).collect();
        Ok(WriteResult {
            dry_run: !self.writes_enabled,
            tx_hex,
            command,
            verdict,
            rx_hex,
            listen_window_ms: listen_ms,
        })
    }
}

/// Shared write gate for the API.
pub type SharedWriteGate = Arc<WriteGate>;

fn dry_run_rx_hex(command: u8) -> Vec<String> {
    match command {
        CMD_HEAT_CHANGE => vec![fixtures::SET_TEMP_ACK.to_string()],
        CMD_CIRCUIT_CHANGE => vec![fixtures::CIRCUIT_ACK.to_string()],
        _ => Vec::new(),
    }
}

fn frame_hex_list(hexes: &[String]) -> Vec<Frame> {
    let mut framer = Framer::new();
    let mut bytes = Vec::new();
    for h in hexes {
        if let Ok(b) = decode_hex(h) {
            bytes.extend_from_slice(&b);
        }
    }
    framer.feed(&bytes)
}

/// Sleep for the listen window (live path placeholder when no tee is wired).
async fn listen_after_tx(window: Duration, preloaded: &[Frame]) -> Vec<Frame> {
    if !preloaded.is_empty() {
        return preloaded.to_vec();
    }
    tokio::time::sleep(window).await;
    Vec::new()
}

/// Classify listen-window frames for the expected acked opcode.
pub fn classify_listen(expected_cmd: u8, frames: &[Frame]) -> WriteVerdict {
    let mut saw_status = false;
    for frame in frames {
        if !frame.checksum_ok {
            continue;
        }
        match frame_command_and_ack_payload(&frame.raw) {
            Ok((CMD_ACK, Some(acked))) if acked == expected_cmd => {
                return WriteVerdict::Ack;
            }
            Ok((0x02, _)) | Ok((0x08, _)) => {
                saw_status = true;
            }
            _ => {}
        }
    }
    if saw_status {
        WriteVerdict::Status
    } else {
        WriteVerdict::Timeout
    }
}

fn command_byte(raw: &[u8]) -> Result<u8, CommandError> {
    let a5 = find_a5_index(raw)?;
    raw.get(a5 + 4)
        .copied()
        .ok_or_else(|| CommandError::Decode(DecodeError::Truncated(4)))
}

fn frame_command_and_ack_payload(raw: &[u8]) -> Result<(u8, Option<u8>), DecodeError> {
    let a5 = find_a5_index(raw)?;
    let command = *raw.get(a5 + 4).ok_or(DecodeError::Truncated(4))?;
    let length = *raw.get(a5 + 5).ok_or(DecodeError::Truncated(5))?;
    let payload0 = if length >= 1 {
        raw.get(a5 + 6).copied()
    } else {
        None
    };
    Ok((command, payload0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CircuitChange, HeatChange};

    #[test]
    fn classify_heat_ack_fixture() {
        let frames = frame_hex_list(&[fixtures::SET_TEMP_ACK.to_string()]);
        assert_eq!(
            classify_listen(CMD_HEAT_CHANGE, &frames),
            WriteVerdict::Ack
        );
        assert_eq!(
            classify_listen(CMD_CIRCUIT_CHANGE, &frames),
            WriteVerdict::Timeout
        );
    }

    #[test]
    fn classify_circuit_ack_fixture() {
        let frames = frame_hex_list(&[fixtures::CIRCUIT_ACK.to_string()]);
        assert_eq!(
            classify_listen(CMD_CIRCUIT_CHANGE, &frames),
            WriteVerdict::Ack
        );
    }

    #[test]
    fn classify_status_without_ack() {
        let info = "ffffffffffffffff00ffa50c0f10080d4848555a620400000000000000028a";
        let frames = frame_hex_list(&[info.to_string()]);
        assert_eq!(
            classify_listen(CMD_HEAT_CHANGE, &frames),
            WriteVerdict::Status
        );
    }

    #[tokio::test]
    async fn dry_run_heat_and_circuit() {
        let gate = WriteGate::new(false, 50, None);
        let heat = gate.heat_change(0x2B, 0x60, 0x05).await.unwrap();
        assert!(heat.dry_run);
        assert_eq!(heat.tx_hex, fixtures::SET_TEMP);
        assert_eq!(heat.verdict, WriteVerdict::Ack);
        assert_eq!(heat.rx_hex, vec![fixtures::SET_TEMP_ACK.to_string()]);

        let circ = gate
            .circuit_change(0x06, true)
            .await
            .unwrap();
        assert!(circ.dry_run);
        assert_eq!(circ.verdict, WriteVerdict::Ack);
        assert_eq!(circ.command, CMD_CIRCUIT_CHANGE);
    }

    #[tokio::test]
    async fn mutex_rejects_second_writer() {
        let gate = Arc::new(WriteGate::new(false, 200, None));
        let g1 = Arc::clone(&gate);
        let g2 = Arc::clone(&gate);

        // Hold the lock across a slow dry-run by acquiring manually then spawning.
        let hold = gate.try_lock().unwrap();
        let h = tokio::spawn(async move { g2.heat_change(40, 90, 0).await });
        tokio::task::yield_now().await;
        let err = h.await.unwrap().unwrap_err();
        assert!(matches!(err, WriteGateError::Busy));
        drop(hold);
        // After release, write succeeds.
        let ok = g1.circuit_change(1, false).await.unwrap();
        assert_eq!(ok.verdict, WriteVerdict::Ack);
    }

    #[tokio::test]
    async fn raw_hex_path() {
        let gate = WriteGate::new(false, 10, None);
        let tx = CircuitChange::build(0x01, true).unwrap().to_hex();
        let r = gate.raw_hex(&tx).await.unwrap();
        assert_eq!(r.command, CMD_CIRCUIT_CHANGE);
        assert_eq!(r.verdict, WriteVerdict::Ack);
    }

    #[tokio::test]
    async fn live_without_handle_errors() {
        let gate = WriteGate::new(true, 10, None);
        let err = gate.heat_change(40, 90, 0).await.unwrap_err();
        assert!(matches!(err, WriteGateError::NoWriteHandle));
    }

    #[tokio::test]
    async fn execute_with_listen_bytes_ack() {
        let gate = WriteGate::new(false, 10, None);
        let cmd = HeatChange::build(0x2B, 0x60, 0x05).unwrap();
        let ack = decode_hex(fixtures::SET_TEMP_ACK).unwrap();
        let r = gate
            .execute_with_listen_bytes(CMD_HEAT_CHANGE, cmd.raw, &ack)
            .await
            .unwrap();
        assert!(r.dry_run);
        assert_eq!(r.verdict, WriteVerdict::Ack);
    }

    #[tokio::test]
    async fn bus_write_handle_roundtrip() {
        let (handle, mut rx) = BusWriteHandle::channel(4);
        let worker = tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                assert_eq!(req.data, vec![1, 2, 3]);
                let _ = req.reply.send(Ok(()));
            }
        });
        handle.write(vec![1, 2, 3]).await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn bus_write_handle_closed_channel() {
        let (handle, rx) = BusWriteHandle::channel(1);
        drop(rx);
        let err = handle.write(vec![9]).await.unwrap_err();
        assert!(err.to_string().contains("closed") || err.to_string().contains("I/O"));
    }

    #[tokio::test]
    async fn live_write_times_out_without_rx() {
        let (handle, mut rx) = BusWriteHandle::channel(4);
        let worker = tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                assert!(!req.data.is_empty());
                let _ = req.reply.send(Ok(()));
            }
        });
        let gate = WriteGate::new(true, 5, Some(handle));
        assert!(gate.writes_enabled());
        assert_eq!(gate.listen_window(), Duration::from_millis(5));
        let r = gate.heat_change(40, 90, 0).await.unwrap();
        assert!(!r.dry_run);
        assert_eq!(r.verdict, WriteVerdict::Timeout);
        assert!(r.rx_hex.is_empty());
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn execute_with_listen_bytes_live_ack() {
        let (handle, mut rx) = BusWriteHandle::channel(4);
        let worker = tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                let _ = req.reply.send(Ok(()));
            }
        });
        let gate = WriteGate::new(true, 10, Some(handle));
        let cmd = HeatChange::build(0x2B, 0x60, 0x05).unwrap();
        let ack = decode_hex(fixtures::SET_TEMP_ACK).unwrap();
        let r = gate
            .execute_with_listen_bytes(CMD_HEAT_CHANGE, cmd.raw, &ack)
            .await
            .unwrap();
        assert!(!r.dry_run);
        assert_eq!(r.verdict, WriteVerdict::Ack);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn execute_with_listen_live_no_handle() {
        let gate = WriteGate::new(true, 10, None);
        let cmd = CircuitChange::build(1, true).unwrap();
        let err = gate
            .execute_with_listen_bytes(CMD_CIRCUIT_CHANGE, cmd.raw, &[])
            .await
            .unwrap_err();
        assert!(matches!(err, WriteGateError::NoWriteHandle));
    }

    #[tokio::test]
    async fn raw_hex_rejects_non_write_cmd() {
        let gate = WriteGate::new(false, 10, None);
        // SYSTEM_STATUS light-on fixture — not 0x86/0x88
        let status = concat!(
            "ff00ffa50c0f10021d13330000000000000021000000043b3b00003c",
            "00000004000085df000d0381"
        );
        let err = gate.raw_hex(status).await.unwrap_err();
        assert!(matches!(err, WriteGateError::Unsupported(_)));
    }

    #[tokio::test]
    async fn listen_after_tx_preloaded_and_sleep() {
        let frames = frame_hex_list(&[fixtures::CIRCUIT_ACK.to_string()]);
        let got = listen_after_tx(Duration::from_millis(1), &frames).await;
        assert_eq!(got.len(), 1);
        let empty = listen_after_tx(Duration::from_millis(1), &[]).await;
        assert!(empty.is_empty());
    }

    #[test]
    fn classify_skips_bad_checksum() {
        let mut frames = frame_hex_list(&[fixtures::SET_TEMP_ACK.to_string()]);
        frames[0].checksum_ok = false;
        assert_eq!(
            classify_listen(CMD_HEAT_CHANGE, &frames),
            WriteVerdict::Timeout
        );
    }

    #[test]
    fn accessors_and_shared() {
        let gate = WriteGate::new(false, 0, None); // clamps to 1ms
        assert!(!gate.writes_enabled());
        assert_eq!(gate.listen_window(), Duration::from_millis(1));
        let _shared = gate.shared();
    }
}
