//! Explicit command-byte → parser registry (Factory pattern).
//!
//! Pattern: **Factory** — maps command bytes to typed parsers. Prefer this
//! deterministic registry over PHP-style `scandir` of a Command directory.

use crate::framer::{Frame, FrameKind};
use crate::messages::{
    DecodedMessage, DecodeError, SystemStatus, TempStatus, Unknown, find_a5_index,
};

/// Commands enum values (PHP `Enum\Commands`).
pub const CMD_UNKNOWN: u8 = 0x00;
/// Circuit / heat change ACK.
pub const CMD_CIRCUIT_CHANGE_ACK: u8 = 0x01;
/// System / equipment status broadcast.
pub const CMD_SYSTEM_STATUS: u8 = 0x02;
/// Clock / calendar broadcast.
pub const CMD_CLOCK_BROADCAST: u8 = 0x05;
/// Pump status (pump protocol conversation).
pub const CMD_PUMP_STATUS_REQUEST: u8 = 0x07;
/// INFO / TempStatus.
pub const CMD_INFO: u8 = 0x08;
/// Remote layout ACK.
pub const CMD_REMOTE_LAYOUT_ACK: u8 = 0x21;
/// Set circuit request.
pub const CMD_CIRCUIT_CHANGE_REQUEST: u8 = 0x86;
/// Set heat request.
pub const CMD_TEMP_CHANGE_REQUEST: u8 = 0x88;
/// Remote layout request.
pub const CMD_REMOTE_LAYOUT_REQUEST: u8 = 0xE1;

/// Parser function stored in the registry.
pub type MessageParser = fn(&[u8]) -> Result<DecodedMessage, DecodeError>;

/// Pattern: **Factory** — explicit cmd-byte → parser map (no scandir).
#[derive(Debug, Clone)]
pub struct MessageRegistry {
    parsers: [Option<MessageParser>; 256],
}

impl Default for MessageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageRegistry {
    /// Empty registry (everything → [`Unknown`]).
    pub fn new() -> Self {
        Self {
            parsers: [None; 256],
        }
    }

    /// Explicit default: SystemStatus + TempStatus; remaining Commands enum → Unknown.
    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        reg.register(CMD_SYSTEM_STATUS, parse_system_status);
        reg.register(CMD_INFO, parse_temp_status);
        for byte in [
            CMD_UNKNOWN,
            CMD_CIRCUIT_CHANGE_ACK,
            CMD_CLOCK_BROADCAST,
            CMD_PUMP_STATUS_REQUEST,
            CMD_REMOTE_LAYOUT_ACK,
            CMD_CIRCUIT_CHANGE_REQUEST,
            CMD_TEMP_CHANGE_REQUEST,
            CMD_REMOTE_LAYOUT_REQUEST,
        ] {
            reg.register(byte, parse_unknown);
        }
        reg
    }

    /// Binds a command byte to a parser.
    pub fn register(&mut self, command_byte: u8, parser: MessageParser) {
        self.parsers[command_byte as usize] = Some(parser);
    }

    /// Builds a typed message; unregistered bytes become [`Unknown`].
    pub fn parse(&self, command_byte: u8, raw: &[u8]) -> DecodedMessage {
        match self.parsers[command_byte as usize] {
            Some(parser) => parser(raw).unwrap_or_else(|_| {
                DecodedMessage::Unknown(Unknown::parse_with_command(raw, command_byte))
            }),
            None => DecodedMessage::Unknown(Unknown::parse_with_command(raw, command_byte)),
        }
    }

    /// Decode a standard A5 frame: read command from A5, then dispatch.
    pub fn decode_a5(&self, raw: &[u8]) -> DecodedMessage {
        match find_a5_index(raw) {
            Ok(a5) if a5 + 4 < raw.len() => {
                let command = raw[a5 + 4];
                self.parse(command, raw)
            }
            _ => DecodedMessage::Unknown(Unknown::parse(raw)),
        }
    }

    /// Decode a framer [`Frame`]: quarantine bad checksums; IntelliChlor → Unknown.
    pub fn decode_frame(&self, frame: &Frame) -> DecodeOutcome {
        if !frame.checksum_ok {
            return DecodeOutcome::Quarantined {
                reason: "checksum_mismatch",
                kind: frame.kind,
                raw_hex: crate::messages::hex_encode(&frame.raw),
            };
        }
        let message = match frame.kind {
            FrameKind::A5 => self.decode_a5(&frame.raw),
            FrameKind::IntelliChlor => DecodedMessage::Unknown(Unknown::parse(&frame.raw)),
        };
        DecodeOutcome::Decoded(message)
    }
}

fn parse_system_status(raw: &[u8]) -> Result<DecodedMessage, DecodeError> {
    Ok(DecodedMessage::SystemStatus(SystemStatus::parse(raw)?))
}

fn parse_temp_status(raw: &[u8]) -> Result<DecodedMessage, DecodeError> {
    Ok(DecodedMessage::TempStatus(TempStatus::parse(raw)?))
}

fn parse_unknown(raw: &[u8]) -> Result<DecodedMessage, DecodeError> {
    Ok(DecodedMessage::Unknown(Unknown::parse(raw)))
}

/// Result of decoding one framed bus message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeOutcome {
    /// Typed or Unknown message (checksum OK).
    Decoded(DecodedMessage),
    /// Bad checksum — do not apply to status snapshot.
    Quarantined {
        /// Why the frame was held aside.
        reason: &'static str,
        /// Frame family.
        kind: FrameKind,
        /// Raw hex.
        raw_hex: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framer::FrameKind;

    const INFO: &str = "ffffffffffffffff00ffa50c0f10080d4848555a620400000000000000028a";
    const LIGHT_ON: &str = concat!(
        "ff00ffa50c0f10021d13330000000000000021000000043b3b00003c",
        "00000004000085df000d0381"
    );
    const CMD_X05: &str = "ff00ffa5070f10050813160217060e0000012e";
    const IC: &str = "1002500000621003";

    fn hx(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn default_dispatches_status_and_temps() {
        let reg = MessageRegistry::with_defaults();
        match reg.decode_a5(&hx(INFO)) {
            DecodedMessage::TempStatus(t) => assert_eq!(t.water, 0x48),
            other => panic!("expected TempStatus, got {other:?}"),
        }
        match reg.decode_a5(&hx(LIGHT_ON)) {
            DecodedMessage::SystemStatus(s) => assert_eq!(s.hours, 0x13),
            other => panic!("expected SystemStatus, got {other:?}"),
        }
        match reg.decode_a5(&hx(CMD_X05)) {
            DecodedMessage::Unknown(u) => assert_eq!(u.command, CMD_CLOCK_BROADCAST),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn decode_frame_quarantines_bad_checksum() {
        let reg = MessageRegistry::with_defaults();
        let mut bad = hx(LIGHT_ON);
        *bad.last_mut().unwrap() ^= 0xFF;
        let outcome = reg.decode_frame(&Frame {
            kind: FrameKind::A5,
            raw: bad,
            checksum_ok: false,
        });
        assert!(matches!(outcome, DecodeOutcome::Quarantined { .. }));
    }

    #[test]
    fn decode_frame_intellichlor() {
        let reg = MessageRegistry::with_defaults();
        let outcome = reg.decode_frame(&Frame {
            kind: FrameKind::IntelliChlor,
            raw: hx(IC),
            checksum_ok: true,
        });
        match outcome {
            DecodeOutcome::Decoded(DecodedMessage::Unknown(u)) => {
                assert_eq!(u.raw, IC);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn empty_registry_falls_back_to_unknown() {
        let reg = MessageRegistry::new();
        match reg.parse(0x02, &hx(LIGHT_ON)) {
            DecodedMessage::Unknown(u) => assert_eq!(u.command, 0x02),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn decode_a5_without_header_is_unknown() {
        let reg = MessageRegistry::with_defaults();
        match reg.decode_a5(&[0x01, 0x02]) {
            DecodedMessage::Unknown(_) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn fixture_status_temps_and_pool_light() {
        let reg = MessageRegistry::with_defaults();
        let temps = std::fs::read_to_string("fixtures/status_temps.hex").unwrap();
        let temps_bytes = crate::transport::replay::parse_hex_bytes(&temps);
        let frames = crate::framer::Framer::new().feed(&temps_bytes);
        assert!(!frames.is_empty());
        match reg.decode_frame(&frames[0]) {
            DecodeOutcome::Decoded(DecodedMessage::TempStatus(t)) => {
                assert_eq!(t.command, CMD_INFO);
                assert_eq!(t.water, 0x48);
                assert_eq!(t.water_set, 0x5A);
            }
            other => panic!("expected TempStatus from fixture, got {other:?}"),
        }

        let light = std::fs::read_to_string("fixtures/log_breakdown/005_pool_light_on.hex").unwrap();
        let light_bytes = crate::transport::replay::parse_hex_bytes(&light);
        let frames = crate::framer::Framer::new().feed(&light_bytes);
        let status = frames
            .iter()
            .find_map(|f| match reg.decode_frame(f) {
                DecodeOutcome::Decoded(DecodedMessage::SystemStatus(s)) => Some(s),
                _ => None,
            })
            .expect("SystemStatus in pool light fixture");
        assert_eq!(status.command, CMD_SYSTEM_STATUS);
        assert!(status.circuit_status.pool_light == "on" || status.circuits != 0);
    }
}
