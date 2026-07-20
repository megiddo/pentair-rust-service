//! In-memory latest status snapshot + recent-frames ring buffer.
//!
//! Pattern: **Facade** helper state owned by the service and shared with the
//! HTTP API. Updated by a background task that consumes framed bus messages
//! from the transport Actor.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{mpsc, RwLock};
use tracing::debug;

use crate::framer::{Frame, FrameKind};
use crate::messages::{DecodedMessage, SystemStatus, TempStatus, hex_encode};
use crate::registry::{DecodeOutcome, MessageRegistry};

/// Default capacity for the recent-frames ring buffer.
pub const DEFAULT_FRAME_RING: usize = 128;

/// Shared handle used by the API and the decode drain task.
pub type SharedBusState = Arc<RwLock<BusState>>;

/// Creates an empty shared bus state.
pub fn shared_bus_state() -> SharedBusState {
    Arc::new(RwLock::new(BusState::default()))
}

/// Latest decoded status plus a ring of recent frames.
#[derive(Debug, Clone)]
pub struct BusState {
    /// Most recent SYSTEM_STATUS (`0x02`), if any.
    pub system_status: Option<SystemStatus>,
    /// Most recent INFO / TempStatus (`0x08`), if any.
    pub temp_status: Option<TempStatus>,
    /// Recent decoded (or quarantined) frames, newest last.
    frames: VecDeque<FrameEntry>,
    /// Max ring size.
    ring_cap: usize,
    /// Total frames applied (including quarantined).
    pub frames_seen: u64,
    /// Frames dropped due to checksum mismatch.
    pub quarantined: u64,
}

impl Default for BusState {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_FRAME_RING)
    }
}

impl BusState {
    /// Creates state with a fixed ring capacity.
    pub fn with_capacity(ring_cap: usize) -> Self {
        Self {
            system_status: None,
            temp_status: None,
            frames: VecDeque::with_capacity(ring_cap.max(1)),
            ring_cap: ring_cap.max(1),
            frames_seen: 0,
            quarantined: 0,
        }
    }

    /// Applies one decode outcome: updates typed snapshots and pushes the ring.
    pub fn apply(&mut self, outcome: DecodeOutcome, kind: FrameKind, checksum_ok: bool) {
        self.frames_seen += 1;
        let entry = match &outcome {
            DecodeOutcome::Decoded(DecodedMessage::SystemStatus(s)) => {
                self.system_status = Some(s.clone());
                FrameEntry::from_decoded(kind, checksum_ok, DecodedMessage::SystemStatus(s.clone()))
            }
            DecodeOutcome::Decoded(DecodedMessage::TempStatus(t)) => {
                self.temp_status = Some(t.clone());
                FrameEntry::from_decoded(kind, checksum_ok, DecodedMessage::TempStatus(t.clone()))
            }
            DecodeOutcome::Decoded(DecodedMessage::Unknown(u)) => {
                FrameEntry::from_decoded(kind, checksum_ok, DecodedMessage::Unknown(u.clone()))
            }
            DecodeOutcome::Quarantined {
                reason,
                kind: qk,
                raw_hex,
            } => {
                self.quarantined += 1;
                FrameEntry {
                    kind: format!("{qk:?}"),
                    checksum_ok: false,
                    quarantined: true,
                    reason: Some((*reason).to_string()),
                    message: None,
                    raw: raw_hex.clone(),
                }
            }
        };
        if self.frames.len() >= self.ring_cap {
            self.frames.pop_front();
        }
        self.frames.push_back(entry);
    }

    /// Snapshot for `GET /status` (PHP-compatible nested Command JSON).
    pub fn status_response(&self) -> StatusResponse {
        StatusResponse {
            system_status: self.system_status.clone(),
            temp_status: self.temp_status.clone(),
            frames_seen: self.frames_seen,
            quarantined: self.quarantined,
        }
    }

    /// Newest-first slice for `GET /frames?limit=`.
    pub fn recent_frames(&self, limit: usize) -> FramesResponse {
        let limit = limit.min(self.frames.len());
        let frames: Vec<FrameEntry> = self
            .frames
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect();
        FramesResponse {
            count: frames.len(),
            frames,
        }
    }
}

/// `GET /status` body.
///
/// Nested objects use PHP `Command::toJson` field names (`waterTemp`,
/// `circuitStatus.filterPump`, `waterSet`, …). See [`crate::messages`].
#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    /// Latest SystemStatus, or null if none yet.
    #[serde(rename = "systemStatus")]
    pub system_status: Option<SystemStatus>,
    /// Latest TempStatus, or null if none yet.
    #[serde(rename = "tempStatus")]
    pub temp_status: Option<TempStatus>,
    /// Frames processed since start.
    #[serde(rename = "framesSeen")]
    pub frames_seen: u64,
    /// Checksum failures held aside.
    pub quarantined: u64,
}

/// `GET /frames` body.
#[derive(Debug, Clone, Serialize)]
pub struct FramesResponse {
    /// Number of entries returned.
    pub count: usize,
    /// Newest-first frame records.
    pub frames: Vec<FrameEntry>,
}

/// One ring-buffer record (decoded message or quarantine).
#[derive(Debug, Clone, Serialize)]
pub struct FrameEntry {
    /// `"A5"` or `"IntelliChlor"`.
    pub kind: String,
    /// Framer checksum result.
    #[serde(rename = "checksumOk")]
    pub checksum_ok: bool,
    /// True when the frame was not applied to status.
    pub quarantined: bool,
    /// Quarantine reason when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Decoded Command JSON when checksum OK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<DecodedMessage>,
    /// Always-present raw hex (even when message present).
    pub raw: String,
}

impl FrameEntry {
    fn from_decoded(kind: FrameKind, checksum_ok: bool, message: DecodedMessage) -> Self {
        let raw = message.raw_hex().to_string();
        Self {
            kind: format!("{kind:?}"),
            checksum_ok,
            quarantined: false,
            reason: None,
            message: Some(message),
            raw,
        }
    }
}

/// Spawns a task that decodes Actor frames into [`SharedBusState`].
///
/// Pattern: bridges **Actor** framed output → **Factory/registry** → state.
pub fn spawn_decode_drain(
    mut rx: mpsc::Receiver<Frame>,
    state: SharedBusState,
    registry: MessageRegistry,
) -> tokio::task::JoinHandle<u64> {
    tokio::spawn(async move {
        let mut n = 0u64;
        while let Some(frame) = rx.recv().await {
            n += 1;
            let kind = frame.kind;
            let checksum_ok = frame.checksum_ok;
            let outcome = registry.decode_frame(&frame);
            debug!(
                kind = ?kind,
                checksum_ok,
                quarantined = matches!(outcome, DecodeOutcome::Quarantined { .. }),
                "decoded bus frame"
            );
            let mut guard = state.write().await;
            guard.apply(outcome, kind, checksum_ok);
        }
        n
    })
}

/// Convenience for tests: apply raw framed bytes as if checksum OK.
pub fn apply_raw_a5_for_test(state: &mut BusState, registry: &MessageRegistry, raw: &[u8]) {
    let frame = Frame {
        kind: FrameKind::A5,
        raw: raw.to_vec(),
        checksum_ok: true,
    };
    let outcome = registry.decode_frame(&frame);
    state.apply(outcome, FrameKind::A5, true);
}

/// Helper so quarantine entries still expose hex when message is absent.
pub fn raw_hex_or_empty(raw: &[u8]) -> String {
    hex_encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::MessageRegistry;

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

    #[test]
    fn apply_updates_latest_and_ring() {
        let reg = MessageRegistry::with_defaults();
        let mut state = BusState::with_capacity(2);
        apply_raw_a5_for_test(&mut state, &reg, &hx(INFO));
        apply_raw_a5_for_test(&mut state, &reg, &hx(LIGHT_ON));
        assert!(state.temp_status.is_some());
        assert!(state.system_status.is_some());
        assert_eq!(state.frames_seen, 2);
        assert_eq!(state.recent_frames(10).count, 2);
        // Ring capacity 2 — third drops oldest.
        apply_raw_a5_for_test(&mut state, &reg, &hx(INFO));
        assert_eq!(state.recent_frames(10).count, 2);
        let status = state.status_response();
        assert_eq!(status.system_status.as_ref().unwrap().hours, 0x13);
        assert_eq!(status.temp_status.as_ref().unwrap().water, 0x48);
    }

    #[test]
    fn quarantine_increments_counter() {
        let mut state = BusState::default();
        let outcome = DecodeOutcome::Quarantined {
            reason: "checksum_mismatch",
            kind: FrameKind::A5,
            raw_hex: "dead".into(),
        };
        state.apply(outcome, FrameKind::A5, false);
        assert_eq!(state.quarantined, 1);
        assert!(state.system_status.is_none());
        let frames = state.recent_frames(1);
        assert!(frames.frames[0].quarantined);
    }

    #[tokio::test]
    async fn decode_drain_updates_shared_state() {
        let state = shared_bus_state();
        let (tx, rx) = mpsc::channel(8);
        let h = spawn_decode_drain(rx, Arc::clone(&state), MessageRegistry::with_defaults());
        tx.send(Frame {
            kind: FrameKind::A5,
            raw: hx(INFO),
            checksum_ok: true,
        })
        .await
        .unwrap();
        drop(tx);
        assert_eq!(h.await.unwrap(), 1);
        let guard = state.read().await;
        assert!(guard.temp_status.is_some());
    }

    #[test]
    fn raw_hex_helper() {
        assert_eq!(raw_hex_or_empty(&[0xAB]), "ab");
    }
}
