//! Typed bus message DTOs (Command / Message pattern).
//!
//! # Patterns
//!
//! - **Command / Message** — serde DTOs mirroring PHP `Command` subclasses and
//!   Python `pentairsnoop.messages`. Field names match PHP `toJson` /
//!   `Command::fromJson` so a future `RustServiceBackend` can round-trip.
//!
//! # JSON field names (PHP-compatible)
//!
//! Shared header (all messages):
//! - `raw` — hex string of the framed bytes
//! - `protocol`, `destination`, `source`, `command`, `length` — u8 as JSON numbers
//!
//! [`SystemStatus`] (cmd `0x02`) additionally:
//! - `hours`, `minutes`, `circuits`
//! - `circuitStatus` object: `filterPump`, `cleanerPump`, `waterFeature`,
//!   `spaLight`, `poolLight` — each `"on"` or `"off"`
//! - `waterTemp`, `heaterTemp`, `airTemp`
//!
//! [`TempStatus`] (cmd `0x08` / INFO) additionally:
//! - `water`, `air`, `waterSet`, `spaSet`, `info`
//!
//! [`Unknown`] keeps the shared header when A5 is present; otherwise `raw` only
//! (IntelliChlor). Optional `type_name` aids clients (not required by PHP).
//!
//! PHP `Command::fromJson` dispatches on `command` and re-parses `raw` hex.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// CircuitStatus bitmask flags (PHP `Enum\CircuitStatus`).
pub const CIRCUIT_CLEANER_PUMP: u8 = 0x02;
/// Water feature / waterfall bit.
pub const CIRCUIT_WATER_FEATURE: u8 = 0x04;
/// Spa light bit.
pub const CIRCUIT_SPA_LIGHT: u8 = 0x08;
/// Pool light bit.
pub const CIRCUIT_POOL_LIGHT: u8 = 0x10;
/// Filter / pool body pump bit.
pub const CIRCUIT_FILTER_PUMP: u8 = 0x20;

// Relative offsets from the A5 sync byte (PHP *Bytes indices assume A5 at abs 3).
const A5_PROTOCOL: usize = 1;
const A5_DST: usize = 2;
const A5_SRC: usize = 3;
const A5_COMMAND: usize = 4;
const A5_LENGTH: usize = 5;

const SS_HOURS: usize = 6;
const SS_MINUTES: usize = 7;
const SS_CIRCUITS: usize = 8;
const SS_WATER_TEMP: usize = 20;
const SS_HEATER_TEMP: usize = 21;
const SS_AIR_TEMP: usize = 24;

const TS_WATER: usize = 7;
const TS_AIR: usize = 8;
const TS_WATER_SET: usize = 9;
const TS_SPA_SET: usize = 10;
const TS_INFO: usize = 11;

/// Errors while locating fields in a framed buffer.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    /// No A5 sync found in the raw frame.
    #[error("A5 sync byte not found in raw frame")]
    MissingA5,
    /// Raw buffer too short for the requested field.
    #[error("raw too short for field at A5+{0}")]
    Truncated(usize),
}

/// Locates the `A5` byte, tolerating `FF 00 FF A5` / `00 FF A5` / idle `FF*`.
pub fn find_a5_index(raw: &[u8]) -> Result<usize, DecodeError> {
    let mut i = 0;
    while i < raw.len() {
        let Some(rel) = raw[i..].iter().position(|&b| b == 0xA5) else {
            break;
        };
        let a5 = i + rel;
        if a5 >= 3 && raw[a5 - 3..a5] == [0xFF, 0x00, 0xFF] {
            return Ok(a5);
        }
        if a5 >= 2 && raw[a5 - 2..a5] == [0x00, 0xFF] {
            return Ok(a5);
        }
        i = a5 + 1;
    }
    Err(DecodeError::MissingA5)
}

fn byte_from_a5(raw: &[u8], a5: usize, offset: usize) -> Result<u8, DecodeError> {
    let idx = a5
        .checked_add(offset)
        .ok_or(DecodeError::Truncated(offset))?;
    raw.get(idx).copied().ok_or(DecodeError::Truncated(offset))
}

fn on_off(circuits: u8, flag: u8) -> &'static str {
    if circuits & flag != 0 {
        "on"
    } else {
        "off"
    }
}

/// Pattern: **Command** — decoded CircuitStatus bitmask as on/off strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CircuitStatusFlags {
    /// Filter / pool body pump (`0x20`).
    #[serde(rename = "filterPump")]
    pub filter_pump: String,
    /// Cleaner pump (`0x02`).
    #[serde(rename = "cleanerPump")]
    pub cleaner_pump: String,
    /// Water feature (`0x04`).
    #[serde(rename = "waterFeature")]
    pub water_feature: String,
    /// Spa light (`0x08`).
    #[serde(rename = "spaLight")]
    pub spa_light: String,
    /// Pool light (`0x10`).
    #[serde(rename = "poolLight")]
    pub pool_light: String,
}

impl CircuitStatusFlags {
    /// Builds on/off strings from the circuits bitmask byte.
    pub fn from_circuits(circuits: u8) -> Self {
        Self {
            filter_pump: on_off(circuits, CIRCUIT_FILTER_PUMP).to_string(),
            cleaner_pump: on_off(circuits, CIRCUIT_CLEANER_PUMP).to_string(),
            water_feature: on_off(circuits, CIRCUIT_WATER_FEATURE).to_string(),
            spa_light: on_off(circuits, CIRCUIT_SPA_LIGHT).to_string(),
            pool_light: on_off(circuits, CIRCUIT_POOL_LIGHT).to_string(),
        }
    }
}

impl Default for CircuitStatusFlags {
    fn default() -> Self {
        Self::from_circuits(0)
    }
}

/// Pattern: **Command** — shared header fields for a framed Pentair message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHeader {
    /// Framed bytes as lowercase hex (PHP `toJson` converts `raw` to hex).
    pub raw: String,
    /// Protocol / type byte after A5.
    pub protocol: u8,
    /// Destination address.
    pub destination: u8,
    /// Source address.
    pub source: u8,
    /// Command byte.
    pub command: u8,
    /// Payload length byte.
    pub length: u8,
}

impl MessageHeader {
    /// Fills shared header fields from raw, indexing from A5.
    pub fn parse(raw: &[u8], a5: Option<usize>) -> Result<Self, DecodeError> {
        let a5 = match a5 {
            Some(i) => i,
            None => find_a5_index(raw)?,
        };
        Ok(Self {
            raw: hex_encode(raw),
            protocol: byte_from_a5(raw, a5, A5_PROTOCOL)?,
            destination: byte_from_a5(raw, a5, A5_DST)?,
            source: byte_from_a5(raw, a5, A5_SRC)?,
            command: byte_from_a5(raw, a5, A5_COMMAND)?,
            length: byte_from_a5(raw, a5, A5_LENGTH)?,
        })
    }
}

/// Pattern: **Command** — SYSTEM_STATUS (`0x02`) typed fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemStatus {
    /// Framed bytes as lowercase hex.
    pub raw: String,
    /// Protocol byte.
    pub protocol: u8,
    /// Destination.
    pub destination: u8,
    /// Source.
    pub source: u8,
    /// Command (`0x02`).
    pub command: u8,
    /// Payload length.
    pub length: u8,
    /// Clock hours.
    pub hours: u8,
    /// Clock minutes.
    pub minutes: u8,
    /// Circuit bitmask byte 0.
    pub circuits: u8,
    /// Named circuit on/off flags (PHP `circuitStatus`).
    #[serde(rename = "circuitStatus")]
    pub circuit_status: CircuitStatusFlags,
    /// Water temperature (°F as panel reports).
    #[serde(rename = "waterTemp")]
    pub water_temp: u8,
    /// Spa / heater temperature.
    #[serde(rename = "heaterTemp")]
    pub heater_temp: u8,
    /// Air temperature.
    #[serde(rename = "airTemp")]
    pub air_temp: u8,
    /// Discriminator for clients (optional for PHP).
    #[serde(rename = "type_name", default = "system_status_type")]
    pub type_name: String,
}

fn system_status_type() -> String {
    "SystemStatus".into()
}

impl SystemStatus {
    /// Parses SYSTEM_STATUS from framed raw bytes.
    pub fn parse(raw: &[u8]) -> Result<Self, DecodeError> {
        let a5 = find_a5_index(raw)?;
        let hdr = MessageHeader::parse(raw, Some(a5))?;
        let circuits = byte_from_a5(raw, a5, SS_CIRCUITS)?;
        Ok(Self {
            raw: hdr.raw,
            protocol: hdr.protocol,
            destination: hdr.destination,
            source: hdr.source,
            command: hdr.command,
            length: hdr.length,
            hours: byte_from_a5(raw, a5, SS_HOURS)?,
            minutes: byte_from_a5(raw, a5, SS_MINUTES)?,
            circuits,
            circuit_status: CircuitStatusFlags::from_circuits(circuits),
            water_temp: byte_from_a5(raw, a5, SS_WATER_TEMP)?,
            heater_temp: byte_from_a5(raw, a5, SS_HEATER_TEMP)?,
            air_temp: byte_from_a5(raw, a5, SS_AIR_TEMP)?,
            type_name: system_status_type(),
        })
    }
}

/// Pattern: **Command** — INFO / TempStatus (`0x08`) typed fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TempStatus {
    /// Framed bytes as lowercase hex.
    pub raw: String,
    /// Protocol byte.
    pub protocol: u8,
    /// Destination.
    pub destination: u8,
    /// Source.
    pub source: u8,
    /// Command (`0x08`).
    pub command: u8,
    /// Payload length.
    pub length: u8,
    /// Water / spa actual (PHP `TempStatusBytes::WATER_ACTUAL` — payload idx 1).
    pub water: u8,
    /// Air actual.
    pub air: u8,
    /// Pool water setpoint.
    #[serde(rename = "waterSet")]
    pub water_set: u8,
    /// Spa setpoint.
    #[serde(rename = "spaSet")]
    pub spa_set: u8,
    /// Heat mode / info byte.
    pub info: u8,
    /// Discriminator for clients.
    #[serde(rename = "type_name", default = "temp_status_type")]
    pub type_name: String,
}

fn temp_status_type() -> String {
    "TempStatus".into()
}

impl TempStatus {
    /// Parses INFO / TempStatus from framed raw bytes.
    pub fn parse(raw: &[u8]) -> Result<Self, DecodeError> {
        let a5 = find_a5_index(raw)?;
        let hdr = MessageHeader::parse(raw, Some(a5))?;
        Ok(Self {
            raw: hdr.raw,
            protocol: hdr.protocol,
            destination: hdr.destination,
            source: hdr.source,
            command: hdr.command,
            length: hdr.length,
            water: byte_from_a5(raw, a5, TS_WATER)?,
            air: byte_from_a5(raw, a5, TS_AIR)?,
            water_set: byte_from_a5(raw, a5, TS_WATER_SET)?,
            spa_set: byte_from_a5(raw, a5, TS_SPA_SET)?,
            info: byte_from_a5(raw, a5, TS_INFO)?,
            type_name: temp_status_type(),
        })
    }
}

/// Pattern: **Command** — unknown / unregistered command (raw hex preserved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unknown {
    /// Discriminator.
    #[serde(rename = "type_name", default = "unknown_type")]
    pub type_name: String,
    /// Command byte (0 when no A5 header, e.g. IntelliChlor).
    pub command: u8,
    /// Protocol (0 if absent).
    pub protocol: u8,
    /// Destination (0 if absent).
    pub destination: u8,
    /// Source (0 if absent).
    pub source: u8,
    /// Length (0 if absent).
    pub length: u8,
    /// Framed bytes as lowercase hex.
    pub raw: String,
}

fn unknown_type() -> String {
    "Unknown".into()
}

impl Unknown {
    /// Parses A5 header when present; otherwise raw-only (IntelliChlor).
    pub fn parse(raw: &[u8]) -> Self {
        match MessageHeader::parse(raw, None) {
            Ok(hdr) => Self {
                type_name: unknown_type(),
                command: hdr.command,
                protocol: hdr.protocol,
                destination: hdr.destination,
                source: hdr.source,
                length: hdr.length,
                raw: hdr.raw,
            },
            Err(_) => Self {
                type_name: unknown_type(),
                command: 0,
                protocol: 0,
                destination: 0,
                source: 0,
                length: 0,
                raw: hex_encode(raw),
            },
        }
    }

    /// Like [`parse`](Self::parse) but forces `command` when header is absent.
    pub fn parse_with_command(raw: &[u8], command: u8) -> Self {
        let mut u = Self::parse(raw);
        if find_a5_index(raw).is_err() {
            u.command = command;
        }
        u
    }
}

/// Any decoded message for the ring buffer / API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecodedMessage {
    /// SYSTEM_STATUS.
    SystemStatus(SystemStatus),
    /// INFO / TempStatus.
    TempStatus(TempStatus),
    /// Unregistered / short frames.
    Unknown(Unknown),
}

impl DecodedMessage {
    /// Command byte when known.
    pub fn command(&self) -> u8 {
        match self {
            Self::SystemStatus(m) => m.command,
            Self::TempStatus(m) => m.command,
            Self::Unknown(m) => m.command,
        }
    }

    /// Hex raw.
    pub fn raw_hex(&self) -> &str {
        match self {
            Self::SystemStatus(m) => &m.raw,
            Self::TempStatus(m) => &m.raw,
            Self::Unknown(m) => &m.raw,
        }
    }
}

/// Lowercase hex encode (no `0x` prefix).
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // lib/index.php contiguous hex vectors (also used by pentairsnoop).
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
    fn find_a5_preamble_variants() {
        let raw = hx(LIGHT_ON);
        assert_eq!(find_a5_index(&raw).unwrap(), 3);
        let padded = hx(INFO);
        assert_eq!(padded[find_a5_index(&padded).unwrap()], 0xA5);
        let short = [&[0x00, 0xFF][..], &raw[3..]].concat();
        assert_eq!(find_a5_index(&short).unwrap(), 2);
    }

    #[test]
    fn temp_status_parse_info() {
        let raw = hx(INFO);
        let msg = TempStatus::parse(&raw).unwrap();
        assert_eq!(msg.command, 0x08);
        assert_eq!(msg.protocol, 0x0C);
        assert_eq!(msg.destination, 0x0F);
        assert_eq!(msg.source, 0x10);
        assert_eq!(msg.length, 0x0D);
        assert_eq!(msg.water, 0x48);
        assert_eq!(msg.air, 0x55);
        assert_eq!(msg.water_set, 0x5A);
        assert_eq!(msg.spa_set, 0x62);
        assert_eq!(msg.info, 0x04);
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type_name"], "TempStatus");
        assert_eq!(v["waterSet"], 0x5A);
        assert_eq!(v["spaSet"], 0x62);
        assert!(v["raw"].as_str().unwrap().ends_with("028a"));
    }

    #[test]
    fn system_status_parse_light_on() {
        let raw = hx(LIGHT_ON);
        let msg = SystemStatus::parse(&raw).unwrap();
        assert_eq!(msg.command, 0x02);
        assert_eq!(msg.hours, 0x13);
        assert_eq!(msg.minutes, 0x33);
        assert_eq!(msg.circuits, 0x00);
        assert_eq!(msg.circuit_status.filter_pump, "off");
        assert_eq!(msg.water_temp, 0x3B);
        assert_eq!(msg.heater_temp, 0x3B);
        assert_eq!(msg.air_temp, 0x3C);
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["circuitStatus"]["poolLight"], "off");
        assert_eq!(v["waterTemp"], 0x3B);
    }

    #[test]
    fn system_status_circuit_flags() {
        let mut payload = vec![0u8; 29];
        payload[0] = 10;
        payload[1] = 30;
        payload[2] = CIRCUIT_FILTER_PUMP | CIRCUIT_CLEANER_PUMP;
        payload[14] = 80;
        payload[15] = 81;
        payload[18] = 70;
        let mut body = vec![0xA5, 0x01, 0x0F, 0x10, 0x02, 0x1D];
        body.extend_from_slice(&payload);
        let cs = body.iter().map(|&b| u32::from(b)).sum::<u32>() % 65536;
        body.push((cs >> 8) as u8);
        body.push((cs & 0xFF) as u8);
        let raw = [&[0xFF, 0x00, 0xFF][..], &body[..]].concat();
        let msg = SystemStatus::parse(&raw).unwrap();
        assert_eq!(msg.circuit_status.filter_pump, "on");
        assert_eq!(msg.circuit_status.cleaner_pump, "on");
        assert_eq!(msg.circuit_status.water_feature, "off");
        assert_eq!(msg.water_temp, 80);
        assert_eq!(msg.air_temp, 70);
    }

    #[test]
    fn unknown_clock_and_intellichlor() {
        let clock = Unknown::parse(&hx(CMD_X05));
        assert_eq!(clock.command, 0x05);
        assert!(clock.raw.starts_with("ff00ffa5"));
        let ic = Unknown::parse(&hx(IC));
        assert_eq!(ic.command, 0);
        assert_eq!(ic.raw, IC);
        let forced = Unknown::parse_with_command(&hx(IC), 0x00);
        assert_eq!(forced.command, 0);
    }

    #[test]
    fn decode_error_missing_a5() {
        assert_eq!(find_a5_index(&[0x10, 0x02]), Err(DecodeError::MissingA5));
        assert!(SystemStatus::parse(&[0x00, 0x01]).is_err());
    }

    #[test]
    fn truncated_payload_errors() {
        // Valid preamble + A5 but missing typed fields.
        let raw = [0xFF, 0x00, 0xFF, 0xA5, 0x0C, 0x0F, 0x10, 0x02, 0x1D];
        assert!(SystemStatus::parse(&raw).is_err());
        assert!(TempStatus::parse(&raw).is_err());
    }

    #[test]
    fn circuit_flags_default_all_off() {
        let f = CircuitStatusFlags::default();
        assert_eq!(f.filter_pump, "off");
        assert_eq!(f.pool_light, "off");
    }

    #[test]
    fn decoded_message_accessors() {
        let msg = TempStatus::parse(&hx(INFO)).unwrap();
        let d = DecodedMessage::TempStatus(msg.clone());
        assert_eq!(d.command(), 0x08);
        assert_eq!(d.raw_hex(), msg.raw);
        let u = DecodedMessage::Unknown(Unknown::parse(&hx(IC)));
        assert_eq!(u.command(), 0);
    }

    #[test]
    fn hex_encode_lowercase() {
        assert_eq!(hex_encode(&[0x0A, 0xFF]), "0aff");
    }
}
