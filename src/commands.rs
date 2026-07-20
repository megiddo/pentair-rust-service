//! Write-path Command builders (CircuitChange `0x86`, HeatChange `0x88`).
//!
//! Pattern: **Command** — clean encode/decode from research + known hex. Does
//! **not** port broken PHP (`HeatChange` imports / `CircuitChange::parse` →
//! TempStatus). Defaults match local PHP intent + `lib/index.php` `$set_temp`.

use thiserror::Error;

use crate::framer::a5_checksum_bytes;
use crate::messages::{find_a5_index, hex_encode, DecodeError};

/// CIRCUIT_CHANGE_REQUEST.
pub const CMD_CIRCUIT_CHANGE: u8 = 0x86;
/// TEMP_CHANGE_REQUEST / HeatChange.
pub const CMD_HEAT_CHANGE: u8 = 0x88;
/// General ACK (payload = acked opcode).
pub const CMD_ACK: u8 = 0x01;

/// Leading idle sync used on encode (PHP `Command::Header`).
pub const DEFAULT_PREAMBLE: [u8; 3] = [0xFF, 0x00, 0xFF];

/// Default write destination (panel).
pub const DEFAULT_WRITE_DST: u8 = 0x10;
/// Default write source (local PHP / sample remote).
pub const DEFAULT_WRITE_SRC: u8 = 0x20;
/// Default protocol byte (local samples use `0x07` on TX).
pub const DEFAULT_WRITE_PROTOCOL: u8 = 0x07;

const A5_PAYLOAD0: usize = 6;

/// Local PHP `Enum\CircuitChange` wire IDs (naming **unconfirmed** on panel).
///
/// External sources often call body POOL `0x06` (local name `POOL_LIGHT`) and
/// treat local `POOL=0x02` as AUX1. Prefer numeric IDs until captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CircuitId {
    /// Spa.
    Spa = 0x01,
    /// Local PHP label; external often AUX1.
    Pool = 0x02,
    /// Cleaner.
    Cleaner = 0x03,
    /// Water feature.
    WaterFeature = 0x04,
    /// Spa light.
    SpaLight = 0x05,
    /// Local PHP; external often POOL body circuit.
    PoolLight = 0x06,
    /// Heat boost.
    HeatBoost = 0x85,
}

impl CircuitId {
    /// Wire byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// PHP HeatChange OR-style: pool heater bit.
pub const HEAT_POOL_MODE: u8 = 0x01;
/// PHP HeatChange OR-style: spa heater bit.
pub const HEAT_SPA_MODE: u8 = 0x04;

/// Errors building or parsing write commands.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CommandError {
    /// Payload too long for the length byte.
    #[error("payload longer than 255 bytes")]
    PayloadTooLong,
    /// Hex string is not valid even-length hex.
    #[error("invalid hex: {0}")]
    InvalidHex(String),
    /// Frame decode / header problem.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// Unexpected command byte.
    #[error("expected cmd 0x{expected:02x}, got 0x{got:02x}")]
    WrongCommand {
        /// Expected opcode.
        expected: u8,
        /// Actual opcode.
        got: u8,
    },
    /// Payload length too short for the command shape.
    #[error("payload length {got} too short (need >= {need})")]
    TruncatedPayload {
        /// Required minimum length.
        need: u8,
        /// Actual length.
        got: u8,
    },
    /// Unknown circuit name token.
    #[error("unknown circuit {0:?}; use 0xNN or spa/pool/cleaner/…")]
    UnknownCircuit(String),
    /// Heat mode nibble out of range.
    #[error("pool_mode and spa_mode must be in 0..3")]
    BadHeatMode,
}

/// Pack EasyTouch heat modes: `(spa_mode << 2) | pool_mode` (each 0–3).
pub fn pack_heat_mode(pool_mode: u8, spa_mode: u8) -> Result<u8, CommandError> {
    if pool_mode > 3 || spa_mode > 3 {
        return Err(CommandError::BadHeatMode);
    }
    Ok(((spa_mode & 0x03) << 2) | (pool_mode & 0x03))
}

/// Resolve a circuit id from int / `0xNN` / local PHP enum name.
pub fn parse_circuit_id(token: &str) -> Result<u8, CommandError> {
    let raw = token.trim().to_ascii_lowercase().replace('-', "_");
    if let Some(hex) = raw.strip_prefix("0x") {
        return u8::from_str_radix(hex, 16)
            .map_err(|_| CommandError::UnknownCircuit(token.to_string()));
    }
    if raw.chars().all(|c| c.is_ascii_digit()) && !raw.is_empty() {
        return raw
            .parse::<u8>()
            .map_err(|_| CommandError::UnknownCircuit(token.to_string()));
    }
    let id = match raw.as_str() {
        "spa" => CircuitId::Spa,
        "pool" => CircuitId::Pool,
        "cleaner" => CircuitId::Cleaner,
        "water_feature" | "waterfall" => CircuitId::WaterFeature,
        "spa_light" => CircuitId::SpaLight,
        "pool_light" => CircuitId::PoolLight,
        "heat_boost" | "boost" => CircuitId::HeatBoost,
        _ => return Err(CommandError::UnknownCircuit(token.to_string())),
    };
    Ok(id.as_u8())
}

/// Pattern: **Command** — PHP `buildHex`: preamble+A5+header+payload+u16 BE CS.
///
/// Checksum is `sum(A5 .. last payload byte) % 65536` (same as B1 framer).
pub fn build_a5_frame(
    protocol: u8,
    destination: u8,
    source: u8,
    command: u8,
    payload: &[u8],
    preamble: &[u8],
) -> Result<Vec<u8>, CommandError> {
    if payload.len() > 255 {
        return Err(CommandError::PayloadTooLong);
    }
    let length = payload.len() as u8;
    let mut from_a5 = Vec::with_capacity(1 + 5 + payload.len() + 2);
    from_a5.push(0xA5);
    from_a5.push(protocol);
    from_a5.push(destination);
    from_a5.push(source);
    from_a5.push(command);
    from_a5.push(length);
    from_a5.extend_from_slice(payload);
    let cs = a5_checksum_bytes(&from_a5);
    from_a5.push(cs[0]);
    from_a5.push(cs[1]);
    let mut out = Vec::with_capacity(preamble.len() + from_a5.len());
    out.extend_from_slice(preamble);
    out.extend_from_slice(&from_a5);
    Ok(out)
}

/// Hex form of [`build_a5_frame`].
pub fn build_a5_hex(
    protocol: u8,
    destination: u8,
    source: u8,
    command: u8,
    payload: &[u8],
) -> Result<String, CommandError> {
    Ok(hex_encode(&build_a5_frame(
        protocol,
        destination,
        source,
        command,
        payload,
        &DEFAULT_PREAMBLE,
    )?))
}

/// Decode even-length lowercase/uppercase hex into bytes.
pub fn decode_hex(s: &str) -> Result<Vec<u8>, CommandError> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.len() % 2 != 0 || cleaned.is_empty() {
        return Err(CommandError::InvalidHex(s.to_string()));
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for i in (0..cleaned.len()).step_by(2) {
        let b = u8::from_str_radix(&cleaned[i..i + 2], 16)
            .map_err(|_| CommandError::InvalidHex(s.to_string()))?;
        out.push(b);
    }
    Ok(out)
}

/// Pattern: **Command** — CIRCUIT_CHANGE_REQUEST (`0x86`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitChange {
    /// Full framed bytes.
    pub raw: Vec<u8>,
    /// Protocol byte.
    pub protocol: u8,
    /// Destination.
    pub destination: u8,
    /// Source.
    pub source: u8,
    /// Circuit wire id.
    pub circuit: u8,
    /// `1` = on, `0` = off.
    pub status: u8,
}

impl CircuitChange {
    /// Build a write request (does not send).
    pub fn build(circuit: u8, on: bool) -> Result<Self, CommandError> {
        Self::build_with(
            circuit,
            on,
            DEFAULT_WRITE_PROTOCOL,
            DEFAULT_WRITE_DST,
            DEFAULT_WRITE_SRC,
        )
    }

    /// Build with explicit header fields.
    pub fn build_with(
        circuit: u8,
        on: bool,
        protocol: u8,
        destination: u8,
        source: u8,
    ) -> Result<Self, CommandError> {
        let status = if on { 1 } else { 0 };
        let raw = build_a5_frame(
            protocol,
            destination,
            source,
            CMD_CIRCUIT_CHANGE,
            &[circuit, status],
            &DEFAULT_PREAMBLE,
        )?;
        Ok(Self {
            raw,
            protocol,
            destination,
            source,
            circuit,
            status,
        })
    }

    /// Lowercase framed hex.
    pub fn to_hex(&self) -> String {
        hex_encode(&self.raw)
    }

    /// Parse a `0x86` frame (does **not** return TempStatus).
    pub fn parse(raw: &[u8]) -> Result<Self, CommandError> {
        let a5 = find_a5_index(raw)?;
        let protocol = raw.get(a5 + 1).copied().ok_or(DecodeError::Truncated(1))?;
        let destination = raw.get(a5 + 2).copied().ok_or(DecodeError::Truncated(2))?;
        let source = raw.get(a5 + 3).copied().ok_or(DecodeError::Truncated(3))?;
        let command = raw.get(a5 + 4).copied().ok_or(DecodeError::Truncated(4))?;
        let length = raw.get(a5 + 5).copied().ok_or(DecodeError::Truncated(5))?;
        if command != CMD_CIRCUIT_CHANGE {
            return Err(CommandError::WrongCommand {
                expected: CMD_CIRCUIT_CHANGE,
                got: command,
            });
        }
        if length < 2 {
            return Err(CommandError::TruncatedPayload {
                need: 2,
                got: length,
            });
        }
        let circuit = *raw
            .get(a5 + A5_PAYLOAD0)
            .ok_or(DecodeError::Truncated(A5_PAYLOAD0))?;
        let status = *raw
            .get(a5 + A5_PAYLOAD0 + 1)
            .ok_or(DecodeError::Truncated(A5_PAYLOAD0 + 1))?;
        Ok(Self {
            raw: raw.to_vec(),
            protocol,
            destination,
            source,
            circuit,
            status,
        })
    }
}

/// Pattern: **Command** — TEMP_CHANGE_REQUEST (`0x88`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeatChange {
    /// Full framed bytes.
    pub raw: Vec<u8>,
    /// Protocol byte.
    pub protocol: u8,
    /// Destination.
    pub destination: u8,
    /// Source.
    pub source: u8,
    /// Pool water setpoint.
    pub pool_set: u8,
    /// Spa setpoint.
    pub spa_set: u8,
    /// Mode byte (`(spa<<2)|pool` or OR-style bits).
    pub mode: u8,
}

impl HeatChange {
    /// Build a heat setpoint write request (does not send).
    pub fn build(pool_set: u8, spa_set: u8, mode: u8) -> Result<Self, CommandError> {
        Self::build_with(
            pool_set,
            spa_set,
            mode,
            DEFAULT_WRITE_PROTOCOL,
            DEFAULT_WRITE_DST,
            DEFAULT_WRITE_SRC,
        )
    }

    /// Build with explicit header fields.
    pub fn build_with(
        pool_set: u8,
        spa_set: u8,
        mode: u8,
        protocol: u8,
        destination: u8,
        source: u8,
    ) -> Result<Self, CommandError> {
        let payload = [pool_set, spa_set, mode, 0x00];
        let raw = build_a5_frame(
            protocol,
            destination,
            source,
            CMD_HEAT_CHANGE,
            &payload,
            &DEFAULT_PREAMBLE,
        )?;
        Ok(Self {
            raw,
            protocol,
            destination,
            source,
            pool_set,
            spa_set,
            mode,
        })
    }

    /// Lowercase framed hex.
    pub fn to_hex(&self) -> String {
        hex_encode(&self.raw)
    }

    /// Parse a `0x88` frame.
    pub fn parse(raw: &[u8]) -> Result<Self, CommandError> {
        let a5 = find_a5_index(raw)?;
        let protocol = raw.get(a5 + 1).copied().ok_or(DecodeError::Truncated(1))?;
        let destination = raw.get(a5 + 2).copied().ok_or(DecodeError::Truncated(2))?;
        let source = raw.get(a5 + 3).copied().ok_or(DecodeError::Truncated(3))?;
        let command = raw.get(a5 + 4).copied().ok_or(DecodeError::Truncated(4))?;
        let length = raw.get(a5 + 5).copied().ok_or(DecodeError::Truncated(5))?;
        if command != CMD_HEAT_CHANGE {
            return Err(CommandError::WrongCommand {
                expected: CMD_HEAT_CHANGE,
                got: command,
            });
        }
        if length < 4 {
            return Err(CommandError::TruncatedPayload {
                need: 4,
                got: length,
            });
        }
        let pool_set = *raw
            .get(a5 + A5_PAYLOAD0)
            .ok_or(DecodeError::Truncated(A5_PAYLOAD0))?;
        let spa_set = *raw
            .get(a5 + A5_PAYLOAD0 + 1)
            .ok_or(DecodeError::Truncated(A5_PAYLOAD0 + 1))?;
        let mode = *raw
            .get(a5 + A5_PAYLOAD0 + 2)
            .ok_or(DecodeError::Truncated(A5_PAYLOAD0 + 2))?;
        Ok(Self {
            raw: raw.to_vec(),
            protocol,
            destination,
            source,
            pool_set,
            spa_set,
            mode,
        })
    }
}

/// Known research fixtures (local PHP `lib/index.php`).
pub mod fixtures {
    /// HeatChange TX matching `$set_temp`.
    pub const SET_TEMP: &str = "ff00ffa507102088042b60050001f8";
    /// Heat ACK `$set_temp_ack` — CMD `0x01` payload `0x88`.
    pub const SET_TEMP_ACK: &str = "ff00ffa50c2010010188016b";
    /// Crafted CircuitChange ACK — CMD `0x01` payload `0x86` (CS verified).
    pub const CIRCUIT_ACK: &str = "ff00ffa50c20100101860169";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framer::verify_a5_frame;
    use crate::messages::find_a5_index;

    #[test]
    fn heat_change_matches_set_temp_fixture() {
        let msg = HeatChange::build(0x2B, 0x60, 0x05).unwrap();
        assert_eq!(msg.to_hex(), fixtures::SET_TEMP);
        let a5 = find_a5_index(&msg.raw).unwrap();
        assert!(verify_a5_frame(&msg.raw[a5..]));
        let parsed = HeatChange::parse(&msg.raw).unwrap();
        assert_eq!(parsed.pool_set, 43);
        assert_eq!(parsed.spa_set, 96);
        assert_eq!(parsed.mode, 0x05);
    }

    #[test]
    fn heat_ack_fixture_checksum() {
        let raw = decode_hex(fixtures::SET_TEMP_ACK).unwrap();
        let a5 = find_a5_index(&raw).unwrap();
        assert!(verify_a5_frame(&raw[a5..]));
        assert_eq!(raw[a5 + 4], CMD_ACK);
        assert_eq!(raw[a5 + 6], CMD_HEAT_CHANGE);
    }

    #[test]
    fn circuit_change_build_and_cs() {
        let msg = CircuitChange::build(CircuitId::PoolLight.as_u8(), true).unwrap();
        let a5 = find_a5_index(&msg.raw).unwrap();
        assert!(verify_a5_frame(&msg.raw[a5..]));
        assert_eq!(msg.circuit, 0x06);
        assert_eq!(msg.status, 1);
        // DST=0x10, SRC=0x20, CMD=0x86, LEN=2
        assert_eq!(msg.raw[a5 + 2], 0x10);
        assert_eq!(msg.raw[a5 + 3], 0x20);
        assert_eq!(msg.raw[a5 + 4], 0x86);
        assert_eq!(msg.raw[a5 + 5], 2);
        let parsed = CircuitChange::parse(&msg.raw).unwrap();
        assert_eq!(parsed.circuit, 0x06);
        assert!(parsed.status != 0);
    }

    #[test]
    fn circuit_ack_fixture_checksum() {
        let raw = decode_hex(fixtures::CIRCUIT_ACK).unwrap();
        let a5 = find_a5_index(&raw).unwrap();
        assert!(verify_a5_frame(&raw[a5..]));
        assert_eq!(raw[a5 + 6], CMD_CIRCUIT_CHANGE);
        // Rebuild must match fixture.
        let rebuilt = build_a5_hex(0x0C, 0x20, 0x10, CMD_ACK, &[CMD_CIRCUIT_CHANGE]).unwrap();
        assert_eq!(rebuilt, fixtures::CIRCUIT_ACK);
    }

    #[test]
    fn pack_heat_mode_dual_heater() {
        assert_eq!(pack_heat_mode(1, 1).unwrap(), 0x05);
        assert!(pack_heat_mode(4, 0).is_err());
    }

    #[test]
    fn parse_circuit_names() {
        assert_eq!(parse_circuit_id("pool_light").unwrap(), 0x06);
        assert_eq!(parse_circuit_id("0x06").unwrap(), 0x06);
        assert_eq!(parse_circuit_id("6").unwrap(), 6);
        assert!(parse_circuit_id("nope").is_err());
    }

    #[test]
    fn wrong_command_parse() {
        let heat = HeatChange::build(40, 90, 0).unwrap();
        assert!(matches!(
            CircuitChange::parse(&heat.raw),
            Err(CommandError::WrongCommand { .. })
        ));
    }

    #[test]
    fn decode_hex_rejects_odd() {
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("").is_err());
    }

    #[test]
    fn payload_too_long_and_truncated() {
        let long = vec![0u8; 256];
        assert!(matches!(
            build_a5_frame(1, 2, 3, 4, &long, &DEFAULT_PREAMBLE),
            Err(CommandError::PayloadTooLong)
        ));
        // Header claims LEN=0 for circuit shape.
        let short = build_a5_frame(0x07, 0x10, 0x20, CMD_CIRCUIT_CHANGE, &[], &DEFAULT_PREAMBLE)
            .unwrap();
        assert!(matches!(
            CircuitChange::parse(&short),
            Err(CommandError::TruncatedPayload { need: 2, .. })
        ));
        let short_heat =
            build_a5_frame(0x07, 0x10, 0x20, CMD_HEAT_CHANGE, &[1, 2], &DEFAULT_PREAMBLE).unwrap();
        assert!(matches!(
            HeatChange::parse(&short_heat),
            Err(CommandError::TruncatedPayload { need: 4, .. })
        ));
        let circ = CircuitChange::build(1, true).unwrap();
        assert!(matches!(
            HeatChange::parse(&circ.raw),
            Err(CommandError::WrongCommand { .. })
        ));
    }

    #[test]
    fn circuit_id_aliases_and_build_with() {
        assert_eq!(parse_circuit_id("waterfall").unwrap(), 0x04);
        assert_eq!(parse_circuit_id("boost").unwrap(), 0x85);
        let msg = CircuitChange::build_with(0x02, false, 0x07, 0x10, 0x20).unwrap();
        assert_eq!(msg.status, 0);
        assert_eq!(msg.to_hex().len() > 10, true);
    }
}
