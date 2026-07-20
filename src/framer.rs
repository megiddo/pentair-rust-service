//! Streaming bus framer: sync seek, length-delimited A5 frames, IntelliChlor short frames.
//!
//! Pattern: **Parser** / **State Machine** — maintains a reassembly buffer and advances
//! through sync-seek → header-complete → payload+checksum complete states as bytes arrive.
//! Does not own transport I/O (B2); callers feed raw bytes via [`Framer::feed`].

use std::sync::atomic::{AtomicU64, Ordering};

/// Standard record separator before `A5` (research: idle `FF*` then this).
const SYNC_00_FF_A5: &[u8] = &[0x00, 0xFF, 0xA5];

/// Full four-byte preamble including a leading idle `FF`.
const SYNC_FF_00_FF_A5: &[u8] = &[0xFF, 0x00, 0xFF, 0xA5];

/// IntelliChlor DLE start (`10 02`).
const IC_STX: &[u8] = &[0x10, 0x02];

/// IntelliChlor DLE end (`10 03`).
const IC_ETX: &[u8] = &[0x10, 0x03];

/// Practical upper bound for chlor payload + CS before abandoning STX.
const IC_MAX_FRAME: usize = 64;

/// Bytes after `A5` before payload: PROTO DST SRC CMD LEN.
const A5_HEADER_AFTER: usize = 5;

/// Framed message family on the RS485 bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Standard `A5` length-delimited frame with u16 BE checksum.
    A5,
    /// IntelliChlor DLE frame `10 02 … CS8 10 03`.
    IntelliChlor,
}

/// One extracted bus frame with checksum verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Frame family.
    pub kind: FrameKind,
    /// Raw bytes including preamble (for A5) or full DLE envelope (for IntelliChlor).
    pub raw: Vec<u8>,
    /// Whether the frame checksum matched the expected algorithm.
    pub checksum_ok: bool,
}

/// Pattern: **Parser** / **State Machine** — extract A5 and IntelliChlor frames from a stream.
///
/// Standard layout after sync:
///
/// ```text
/// A5 PROTO DST SRC CMD LEN PAYLOAD[LEN] CS_HI CS_LO
/// ```
///
/// IntelliChlor (second framer; not truncated A5):
///
/// ```text
/// 10 02 | data… | CS8 | 10 03
/// ```
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
    /// Count of frames emitted with a failed checksum (A5 u16 or IntelliChlor CS8).
    checksum_failures: AtomicU64,
}

impl Framer {
    /// Creates an empty framer (idle state of the parse state machine).
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears the internal reassembly buffer (does not reset failure counters).
    pub fn reset(&mut self) {
        self.buf.clear();
    }

    /// Number of frames whose checksum did not verify since construction.
    pub fn checksum_failures(&self) -> u64 {
        self.checksum_failures.load(Ordering::Relaxed)
    }

    /// Accepts raw bus bytes; returns zero or more complete frames.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Frame> {
        if !data.is_empty() {
            self.buf.extend_from_slice(data);
        }
        self.drain()
    }

    fn drain(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        loop {
            match earliest_sync(&self.buf) {
                None => {
                    // Keep a short tail in case a sync straddles the next feed.
                    if self.buf.len() > 4 {
                        let keep = self.buf.len() - 4;
                        self.buf.drain(..keep);
                    }
                    break;
                }
                Some(sync) => {
                    if sync.start > 0 {
                        self.buf.drain(..sync.start);
                        continue;
                    }
                    let taken = match sync.kind {
                        SyncKind::A5 => self.try_take_a5(sync.mark),
                        SyncKind::IntelliChlor => self.try_take_intellichlor(sync.mark),
                    };
                    match taken {
                        TakeResult::NeedMore => break,
                        TakeResult::Advanced => continue,
                        TakeResult::Frame(frame) => {
                            if !frame.checksum_ok {
                                self.checksum_failures.fetch_add(1, Ordering::Relaxed);
                            }
                            out.push(frame);
                        }
                    }
                }
            }
        }
        out
    }

    fn try_take_a5(&mut self, a5_index: usize) -> TakeResult {
        let need_hdr = a5_index + 1 + A5_HEADER_AFTER;
        if self.buf.len() < need_hdr {
            return TakeResult::NeedMore;
        }
        let length = self.buf[a5_index + 5] as usize;
        let total = need_hdr + length + 2;
        if self.buf.len() < total {
            return TakeResult::NeedMore;
        }

        let raw = self.buf[..total].to_vec();
        self.buf.drain(..total);
        let checksum_ok = verify_a5_frame(&raw[a5_index..]);
        TakeResult::Frame(Frame {
            kind: FrameKind::A5,
            raw,
            checksum_ok,
        })
    }

    fn try_take_intellichlor(&mut self, stx_index: usize) -> TakeResult {
        let search_from = stx_index + 2;
        let window_end = (stx_index + IC_MAX_FRAME).min(self.buf.len());
        let etx = find_subslice(&self.buf, IC_ETX, search_from, window_end);
        match etx {
            None => {
                if self.buf.len() - stx_index >= IC_MAX_FRAME {
                    self.buf.drain(..1);
                    TakeResult::Advanced
                } else {
                    TakeResult::NeedMore
                }
            }
            Some(etx) => {
                // Require STX + ≥1 data byte + CS + ETX (minimum 6 bytes).
                if etx < stx_index + 5 {
                    self.buf.drain(..1);
                    return TakeResult::Advanced;
                }
                let end = etx + 2;
                let raw = self.buf[stx_index..end].to_vec();
                self.buf.drain(..end);
                let checksum_ok = verify_intellichlor_frame(&raw);
                TakeResult::Frame(Frame {
                    kind: FrameKind::IntelliChlor,
                    raw,
                    checksum_ok,
                })
            }
        }
    }
}

enum TakeResult {
    NeedMore,
    Advanced,
    Frame(Frame),
}

#[derive(Clone, Copy)]
enum SyncKind {
    A5,
    IntelliChlor,
}

struct SyncHit {
    kind: SyncKind,
    /// Bytes to discard before the frame start in the buffer.
    start: usize,
    /// Index of `A5` or IntelliChlor STX.
    mark: usize,
}

fn earliest_sync(buf: &[u8]) -> Option<SyncHit> {
    let a5 = find_a5_sync(buf);
    let ic = find_subslice(buf, IC_STX, 0, buf.len());

    let mut best: Option<SyncHit> = None;
    if let Some((discard, a5_idx)) = a5 {
        best = Some(SyncHit {
            kind: SyncKind::A5,
            start: discard,
            mark: a5_idx,
        });
    }
    if let Some(ic_idx) = ic {
        let cand = SyncHit {
            kind: SyncKind::IntelliChlor,
            start: ic_idx,
            mark: ic_idx,
        };
        match &best {
            None => best = Some(cand),
            Some(b) if cand.mark < b.mark => best = Some(cand),
            _ => {}
        }
    }
    best
}

/// Returns `(discard_before, a5_index)` for the earliest A5 sync in `buf`.
fn find_a5_sync(buf: &[u8]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;

    let mut idx = 0;
    while let Some(i) = find_subslice(buf, SYNC_FF_00_FF_A5, idx, buf.len()) {
        let a5 = i + 3;
        let cand = (i, a5);
        if best.map_or(true, |b| a5 < b.1) {
            best = Some(cand);
        }
        idx = i + 1;
    }

    idx = 0;
    while let Some(i) = find_subslice(buf, SYNC_00_FF_A5, idx, buf.len()) {
        let a5 = i + 2;
        let cand = (i, a5);
        if best.map_or(true, |b| a5 < b.1) {
            best = Some(cand);
        }
        idx = i + 1;
    }

    best
}

fn find_subslice(hay: &[u8], needle: &[u8], from: usize, to: usize) -> Option<usize> {
    if needle.is_empty() || from >= to || to > hay.len() {
        return None;
    }
    hay[from..to]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// PHP `Command::pentaircs`: sum of bytes mod 65536.
///
/// For standard frames, `data` is `A5 .. last payload byte` (exclude the two-byte trailer).
pub fn pentair_checksum(data: &[u8]) -> u16 {
    let sum: u32 = data.iter().map(|&b| u32::from(b)).sum();
    (sum % 65536) as u16
}

/// Two-byte big-endian trailer for `body_from_a5` (includes `A5`).
pub fn a5_checksum_bytes(body_from_a5: &[u8]) -> [u8; 2] {
    let cs = pentair_checksum(body_from_a5);
    [(cs >> 8) as u8, (cs & 0xFF) as u8]
}

/// True if trailing u16 BE matches [`pentair_checksum`] over `A5..payload`.
pub fn verify_a5_frame(frame_from_a5: &[u8]) -> bool {
    if frame_from_a5.len() < 1 + A5_HEADER_AFTER + 2 {
        return false;
    }
    let (body, trailer) = frame_from_a5.split_at(frame_from_a5.len() - 2);
    let expected = pentair_checksum(body);
    let actual = u16::from_be_bytes([trailer[0], trailer[1]]);
    expected == actual
}

/// IntelliChlor CS8: `(sum(data) + 18) % 256` (PACKET_SPEC / njsPC-equivalent).
///
/// `data_between_stx_and_cs` is bytes after `10 02` and before the CS8 byte.
pub fn intellichlor_checksum(data_between_stx_and_cs: &[u8]) -> u8 {
    let sum: u32 = data_between_stx_and_cs.iter().map(|&b| u32::from(b)).sum();
    ((sum + 18) % 256) as u8
}

/// True if frame is `10 02 | data | CS8 | 10 03` with a matching CS8.
pub fn verify_intellichlor_frame(frame: &[u8]) -> bool {
    if frame.len() < 5 || &frame[..2] != IC_STX || &frame[frame.len() - 2..] != IC_ETX {
        return false;
    }
    let cs = frame[frame.len() - 3];
    let data = &frame[2..frame.len() - 3];
    cs == intellichlor_checksum(data)
}

/// Decode contiguous hex or whitespace/dump-formatted hex into bytes.
///
/// Accepts pure hex strings and fixture dumps such as `< 0x0\\t ff ff 00 ff a5 ...`.
pub fn parse_hex_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') || stripped.starts_with("/*") {
            continue;
        }
        if stripped.starts_with("*/") || stripped.ends_with("*/") {
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

/// Drop logic-analyzer / dump address columns (`< 0x0`, `0x10:`, `0000:`).
fn strip_dump_prefix(line: &str) -> &str {
    let s = line.trim_start();

    // Pattern: optional `<`, optional `0x`, hex digits, `:`, then payload.
    if let Some(rest) = try_strip_offset_colon(s) {
        return rest;
    }

    // Pattern: `< 0xN` (no colon) then whitespace + payload — sample fixture style.
    if let Some(after_lt) = s.strip_prefix('<') {
        let after_lt = after_lt.trim_start();
        if let Some(after_0x) = after_lt.strip_prefix("0x").or_else(|| after_lt.strip_prefix("0X"))
        {
            let hex_end = after_0x
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(after_0x.len());
            let after_hex = &after_0x[hex_end..];
            if after_hex.starts_with(|c: char| c.is_ascii_whitespace()) {
                return after_hex.trim_start();
            }
        }
        // `<notahex ff aa` — drop `<` token and first word if non-hex-pair junk.
        let mut parts = after_lt.splitn(2, char::is_whitespace);
        let first = parts.next().unwrap_or("");
        if let Some(rest) = parts.next() {
            if !first.is_empty() && !is_all_hex_pairs(first) {
                return rest;
            }
        }
        return after_lt;
    }

    s
}

fn try_strip_offset_colon(s: &str) -> Option<&str> {
    let mut rest = s;
    if let Some(r) = rest.strip_prefix('<') {
        rest = r.trim_start();
    }
    if let Some(r) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        rest = r;
    }
    let hex_end = rest
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(rest.len());
    if hex_end == 0 {
        return None;
    }
    let after_hex = &rest[hex_end..];
    after_hex.strip_prefix(':').map(|r| r.trim_start())
}

fn is_all_hex_pairs(s: &str) -> bool {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    !chars.is_empty()
        && chars.len() % 2 == 0
        && chars.iter().all(|c| c.is_ascii_hexdigit())
}

fn push_hex_pairs(text: &str, out: &mut Vec<u8>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let a = bytes[i];
        let b = bytes[i + 1];
        if is_hex_digit(a) && is_hex_digit(b) {
            out.push((hex_val_byte(a) << 4) | hex_val_byte(b));
            i += 2;
        } else {
            i += 1;
        }
    }
}

fn is_hex_digit(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn hex_val_byte(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    fn load_fixture(rel: &str) -> Vec<u8> {
        let path = fixtures_dir().join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        parse_hex_bytes(&text)
    }

    // Known-good vectors from parent lib/index.php (contiguous hex).
    const CMD_X05: &str = "ff00ffa5070f10050813160217060e0000012e";
    const INFO: &str = "ffffffffffffffff00ffa50c0f10080d4848555a620400000000000000028a";
    const SET_TEMP: &str = "ff00ffa507102088042b60050001f8";
    const SET_TEMP_ACK: &str = "ff00ffa50c2010010188016b";
    const LIGHT_ON: &str = concat!(
        "ff00ffa50c0f10021d13330000000000000021000000043b3b00003c",
        "00000004000085df000d0381"
    );

    const IC_PHP: &[u8] = &[0x10, 0x02, 0x50, 0x00, 0x00, 0x62, 0x10, 0x03];
    const IC_BLOCK: &[u8] = &[0x10, 0x02, 0x50, 0x00, 0x00, 0x00, 0x62, 0x10, 0x03];

    fn a5_body_from_hex(hx: &str) -> Vec<u8> {
        let raw = parse_hex_bytes(hx);
        let mut search_from = 0;
        loop {
            let a5 = raw[search_from..]
                .iter()
                .position(|&b| b == 0xA5)
                .map(|p| search_from + p)
                .expect("A5 in vector");
            let ok = (a5 >= 2 && raw[a5 - 2..a5] == [0x00, 0xFF])
                || (a5 >= 3 && raw[a5 - 3..a5] == [0xFF, 0x00, 0xFF]);
            if ok {
                return raw[a5..].to_vec();
            }
            search_from = a5 + 1;
        }
    }

    #[test]
    fn pentair_checksum_matches_php_vectors() {
        for hx in [CMD_X05, INFO, SET_TEMP, SET_TEMP_ACK, LIGHT_ON] {
            let frame = a5_body_from_hex(hx);
            let (body, trailer) = frame.split_at(frame.len() - 2);
            let expected = u16::from_be_bytes([trailer[0], trailer[1]]);
            assert_eq!(pentair_checksum(body), expected);
            assert!(verify_a5_frame(&frame));
            assert_eq!(a5_checksum_bytes(body), [trailer[0], trailer[1]]);
        }
    }

    #[test]
    fn checksum_rejects_corruption() {
        let mut frame = a5_body_from_hex(SET_TEMP);
        *frame.last_mut().unwrap() ^= 0xFF;
        assert!(!verify_a5_frame(&frame));
    }

    #[test]
    fn framer_standard_a5_with_idle_padding() {
        let mut fr = Framer::new();
        let frames = fr.feed(&parse_hex_bytes(INFO));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, FrameKind::A5);
        assert!(frames[0].checksum_ok);
        assert!(frames[0].raw.ends_with(&[0x02, 0x8a]));
    }

    #[test]
    fn framer_sync_00_ff_a5_without_ff00_preamble() {
        let body = a5_body_from_hex(SET_TEMP);
        let mut stream = vec![0x00, 0xFF];
        stream.extend_from_slice(&body);
        let frames = Framer::new().feed(&stream);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].checksum_ok);
    }

    #[test]
    fn framer_streaming_chunks() {
        let raw = parse_hex_bytes(LIGHT_ON);
        let mut fr = Framer::new();
        assert!(fr.feed(&raw[..5]).is_empty());
        assert!(fr.feed(&raw[5..17]).is_empty());
        let frames = fr.feed(&raw[17..]);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].checksum_ok);
    }

    #[test]
    fn intellichlor_checksum_and_verify() {
        for sample in [IC_PHP, IC_BLOCK] {
            let data = &sample[2..sample.len() - 3];
            assert_eq!(sample[sample.len() - 3], intellichlor_checksum(data));
            assert!(verify_intellichlor_frame(sample));
        }
    }

    #[test]
    fn framer_intellichlor_not_treated_as_a5() {
        let mut stream = IC_PHP.to_vec();
        stream.extend_from_slice(IC_BLOCK);
        let frames = Framer::new().feed(&stream);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.kind == FrameKind::IntelliChlor));
        assert!(frames.iter().all(|f| f.checksum_ok));
        assert_eq!(frames[0].raw, IC_PHP);
    }

    #[test]
    fn framer_mixed_a5_and_intellichlor() {
        let mut stream = vec![0xFF, 0xFF];
        stream.extend_from_slice(&parse_hex_bytes(SET_TEMP_ACK));
        stream.extend_from_slice(IC_PHP);
        stream.extend_from_slice(&parse_hex_bytes(CMD_X05));
        let frames = Framer::new().feed(&stream);
        assert_eq!(
            frames.iter().map(|f| f.kind).collect::<Vec<_>>(),
            vec![FrameKind::A5, FrameKind::IntelliChlor, FrameKind::A5]
        );
        assert!(frames.iter().all(|f| f.checksum_ok));
    }

    #[test]
    fn framer_noise_then_frame() {
        let mut stream = vec![0x01, 0x02, 0x03];
        stream.extend_from_slice(&parse_hex_bytes(CMD_X05));
        let frames = Framer::new().feed(&stream);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].checksum_ok);
    }

    #[test]
    fn framer_reset() {
        let mut fr = Framer::new();
        let raw = parse_hex_bytes(SET_TEMP);
        fr.feed(&raw[..8]);
        fr.reset();
        assert!(fr.feed(&raw)[0].checksum_ok);
    }

    #[test]
    fn parse_hex_bytes_contiguous_and_dump() {
        assert_eq!(parse_hex_bytes("ff00ffa5"), vec![0xff, 0x00, 0xff, 0xa5]);
        let dump = "< 0x0\t ff 00 ff a5 01\n< 0x10\t 02 03\n";
        assert_eq!(
            parse_hex_bytes(dump),
            vec![0xff, 0x00, 0xff, 0xa5, 0x01, 0x02, 0x03]
        );
    }

    #[test]
    fn frame_status_temps_fixture() {
        let data = load_fixture("status_temps.hex");
        let frames = Framer::new().feed(&data);
        assert!(!frames.is_empty());
        assert_eq!(frames[0].kind, FrameKind::A5);
        assert!(frames[0].checksum_ok);
    }

    #[test]
    fn frame_log_breakdown_fixture() {
        let data = load_fixture("log_breakdown/001_baseline_filter_on.hex");
        let frames = Framer::new().feed(&data);
        assert!(!frames.is_empty());
        assert_eq!(frames[0].kind, FrameKind::A5);
        assert!(frames[0].checksum_ok);
    }

    #[test]
    fn frame_pool_light_fixture() {
        let data = load_fixture("log_breakdown/005_pool_light_on.hex");
        let frames = Framer::new().feed(&data);
        assert!(!frames.is_empty());
        assert!(frames.iter().all(|f| f.checksum_ok));
    }

    #[test]
    fn frame_intellichlor_fixtures() {
        let php = load_fixture("intellichlor_php.hex");
        let block = load_fixture("intellichlor_block.hex");
        assert_eq!(php, IC_PHP);
        assert_eq!(block, IC_BLOCK);
        let frames = Framer::new().feed(&[php.as_slice(), block.as_slice()].concat());
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.kind == FrameKind::IntelliChlor && f.checksum_ok));
    }

    #[test]
    fn verify_helpers_reject_short_or_bad() {
        assert!(!verify_a5_frame(&[0xa5, 0x01]));
        assert!(!verify_intellichlor_frame(&[0x10, 0x02, 0x10, 0x03]));
        assert!(!verify_intellichlor_frame(&[0x00, 0x01, 0x02, 0x03, 0x04]));
    }

    #[test]
    fn framer_malformed_intellichlor_recovers() {
        let mut junk = vec![0x10, 0x02, 0x10, 0x03];
        junk.extend_from_slice(IC_PHP);
        let frames = Framer::new().feed(&junk);
        assert!(frames.iter().any(|f| f.raw == IC_PHP));
    }

    #[test]
    fn parse_hex_comments_and_colon_offset() {
        let text = "# comment\nff 00\n/* skip\n*/\n0000: a5\n";
        assert_eq!(parse_hex_bytes(text), vec![0xff, 0x00, 0xa5]);
    }

    #[test]
    fn parse_hex_angle_prefix_and_empty_fallback() {
        assert_eq!(parse_hex_bytes("<notahex ff aa"), vec![0xff, 0xaa]);
        assert_eq!(parse_hex_bytes("<"), Vec::<u8>::new());
        assert_eq!(parse_hex_bytes("hello\nworld"), Vec::<u8>::new());
    }

    #[test]
    fn framer_intellichlor_runaway_without_etx() {
        let mut fr = Framer::new();
        let mut long = vec![0x10, 0x02];
        long.extend(std::iter::repeat(0x00u8).take(70));
        assert!(fr.feed(&long).is_empty());
        let frames = fr.feed(IC_PHP);
        assert!(frames
            .iter()
            .any(|f| f.kind == FrameKind::IntelliChlor && f.raw == IC_PHP));
    }

    #[test]
    fn checksum_failure_increments_counter() {
        let mut frame = a5_body_from_hex(SET_TEMP);
        *frame.last_mut().unwrap() ^= 0xFF;
        let mut stream = vec![0xFF, 0x00, 0xFF];
        stream.extend_from_slice(&frame);
        let mut fr = Framer::new();
        let frames = fr.feed(&stream);
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].checksum_ok);
        assert_eq!(fr.checksum_failures(), 1);

        // Bad IntelliChlor CS also counts.
        let mut bad_ic = IC_PHP.to_vec();
        let cs_idx = bad_ic.len() - 3;
        bad_ic[cs_idx] ^= 0xFF;
        let frames = fr.feed(&bad_ic);
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].checksum_ok);
        assert_eq!(fr.checksum_failures(), 2);
    }

    #[test]
    fn feed_empty_returns_empty() {
        assert!(Framer::new().feed(&[]).is_empty());
    }

    #[test]
    fn parse_hex_0x_colon_style() {
        assert_eq!(
            parse_hex_bytes("0x10: aa bb\n"),
            vec![0xaa, 0xbb]
        );
    }
}
