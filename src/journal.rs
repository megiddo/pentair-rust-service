//! Append-only frame journal for offline replay and future `signal/` replacement.
//!
//! Pattern: **Repository** / **Append-Only Log** — durable write-ahead style log of
//! framed bus messages. The watch/decode path appends records; readers replay
//! offline without a live transport.
//!
//! ## File format (v1)
//!
//! Lines starting with `#` are comments. Each data line is lowercase hex of one
//! framed message (same encoding as PHP `signal/index.php`: `bin2hex` of raw
//! bytes). Optional metadata prefix (tab-separated) is accepted on read:
//!
//! ```text
//! # pentairservice-journal-v1
//! # [ts_ms\tchecksum_ok\tkind\t]hex
//! 1710000000000	1	A5	ff00ffa5...
//! ff00ffa5...
//! ```
//!
//! ## Hook for replacing `signal/` ingest
//!
//! PHP today POSTs raw bytes to `signal/`, which stores `bin2hex($body)` in
//! MySQL (`signals.signal`). Implement [`FrameIngest`] to forward the same
//! hex payload (plus optional timestamp) to HTTP/MySQL without changing the
//! decode drain. [`FileFrameJournal`] is the local Repository; a future
//! `SignalHttpIngest` would be another Repository behind the same trait.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing::{debug, warn};

use crate::framer::{Frame, FrameKind};
use crate::messages::hex_encode;

/// Default relative journal path on the bind-mounted volume (`./data/…`).
pub const DEFAULT_JOURNAL_PATH: &str = "data/frames.journal";

/// One append-only journal record (Repository row / log entry).
///
/// Field `hex` matches PHP `signal/` column `signals.signal` (lowercase hex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// Lowercase hex of framed raw bytes (`bin2hex` parity).
    pub hex: String,
    /// Ingest time (milliseconds since Unix epoch), when known.
    pub ts_ms: Option<u64>,
    /// Framer checksum result.
    pub checksum_ok: bool,
    /// `"A5"` or `"IntelliChlor"`.
    pub kind: String,
}

impl JournalRecord {
    /// Builds a record from a framed bus message.
    pub fn from_frame(frame: &Frame) -> Self {
        Self {
            hex: hex_encode(&frame.raw),
            ts_ms: Some(now_ms()),
            checksum_ok: frame.checksum_ok,
            kind: match frame.kind {
                FrameKind::A5 => "A5".into(),
                FrameKind::IntelliChlor => "IntelliChlor".into(),
            },
        }
    }

    /// Parses a journal data line (hex-only or `ts_ms\tchecksum_ok\tkind\thex`).
    pub fn parse_line(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let parts: Vec<&str> = trimmed.split('\t').collect();
        match parts.as_slice() {
            [hex] if is_hex_string(hex) => Some(Self {
                hex: hex.to_ascii_lowercase(),
                ts_ms: None,
                checksum_ok: true,
                kind: "A5".into(),
            }),
            [ts, ok, kind, hex] if is_hex_string(hex) => Some(Self {
                hex: hex.to_ascii_lowercase(),
                ts_ms: ts.parse().ok(),
                checksum_ok: *ok == "1" || ok.eq_ignore_ascii_case("true"),
                kind: (*kind).to_string(),
            }),
            _ => {
                // Tolerate contiguous hex with incidental whitespace stripped.
                let cleaned: String = trimmed
                    .chars()
                    .filter(|c| c.is_ascii_hexdigit())
                    .collect();
                if cleaned.len() >= 2 && cleaned.len() % 2 == 0 {
                    Some(Self {
                        hex: cleaned.to_ascii_lowercase(),
                        ts_ms: None,
                        checksum_ok: true,
                        kind: "A5".into(),
                    })
                } else {
                    None
                }
            }
        }
    }

    /// Serializes as a TSV data line (with metadata) ending without newline.
    pub fn to_line(&self) -> String {
        let ts = self.ts_ms.unwrap_or(0);
        let ok = if self.checksum_ok { "1" } else { "0" };
        format!("{ts}\t{ok}\t{}\t{}", self.kind, self.hex)
    }

    /// Decodes hex back to raw frame bytes (offline replay).
    pub fn raw_bytes(&self) -> Result<Vec<u8>, JournalError> {
        decode_hex(&self.hex)
    }
}

/// Sink for framed messages — Repository port for journal / `signal/` ingest.
///
/// Pattern: **Repository** — the decode drain depends on this abstraction so a
/// file log or future HTTP/MySQL adapter can be swapped without touching framing.
pub trait FrameIngest: Send {
    /// Appends one framed record (append-only; never rewrite prior entries here).
    fn append(&mut self, record: &JournalRecord) -> Result<(), JournalError>;
}

/// Retention / vacuum knobs (stub policy for B4).
///
/// Pattern: **Policy Object** — config-driven; [`RetentionPolicy::vacuum`] may
/// trim by size. Age-based trim is a documented stub when `max_age_secs` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Soft max file size in bytes; when exceeded, vacuum keeps trailing lines.
    pub max_bytes: Option<u64>,
    /// Soft max age in seconds (stub: recorded for future age trim; vacuum may no-op).
    pub max_age_secs: Option<u64>,
}

/// Result of a vacuum / trim attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VacuumStats {
    /// Bytes before vacuum.
    pub bytes_before: u64,
    /// Bytes after vacuum.
    pub bytes_after: u64,
    /// True when an age-based policy was configured but not applied (stub).
    pub age_stub_skipped: bool,
}

impl RetentionPolicy {
    /// No retention limits (journal grows unbounded).
    pub fn none() -> Self {
        Self::default()
    }

    /// Size-only policy.
    pub fn max_bytes(max: u64) -> Self {
        Self {
            max_bytes: Some(max),
            max_age_secs: None,
        }
    }

    /// Runs vacuum against an open journal file path.
    ///
    /// Size trim: rewrite keeping the trailing complete lines under `max_bytes`.
    /// Age trim: **stub** — when `max_age_secs` is set, sets `age_stub_skipped`
    /// and does not delete by timestamp yet (B4).
    pub fn vacuum(&self, path: &Path) -> Result<VacuumStats, JournalError> {
        let mut stats = VacuumStats {
            age_stub_skipped: self.max_age_secs.is_some(),
            ..VacuumStats::default()
        };
        if !path.exists() {
            return Ok(stats);
        }
        let meta = std::fs::metadata(path)?;
        stats.bytes_before = meta.len();
        stats.bytes_after = stats.bytes_before;

        if let Some(max) = self.max_bytes {
            if stats.bytes_before > max {
                trim_file_to_max_bytes(path, max)?;
                stats.bytes_after = std::fs::metadata(path)?.len();
                debug!(
                    path = %path.display(),
                    before = stats.bytes_before,
                    after = stats.bytes_after,
                    max,
                    "journal size vacuum"
                );
            }
        }

        if stats.age_stub_skipped {
            debug!(
                path = %path.display(),
                max_age_secs = ?self.max_age_secs,
                "journal age vacuum stub (no-op)"
            );
        }

        Ok(stats)
    }
}

/// File-backed append-only frame journal.
///
/// Pattern: **Repository** + **Append-Only Log** — opens (or creates) a path on
/// the mounted volume and appends one line per frame.
#[derive(Debug)]
pub struct FileFrameJournal {
    path: PathBuf,
    file: File,
    retention: RetentionPolicy,
    appends_since_vacuum: u64,
    /// Run size vacuum every N appends when retention is configured (0 = never auto).
    pub vacuum_every: u64,
}

impl FileFrameJournal {
    /// Opens or creates a journal at `path`, writing a header if new/empty.
    pub fn open(path: impl Into<PathBuf>, retention: RetentionPolicy) -> Result<Self, JournalError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let meta = file.metadata()?;
        if meta.len() == 0 {
            writeln!(
                file,
                "# pentairservice-journal-v1\n\
                 # Append-Only Log of framed bus messages.\n\
                 # Compatible with signal/ ingest: final column is lowercase hex (bin2hex).\n\
                 # fields: ts_ms\\tchecksum_ok\\tkind\\thex"
            )?;
            file.flush()?;
        }
        Ok(Self {
            path,
            file,
            retention,
            appends_since_vacuum: 0,
            vacuum_every: 64,
        })
    }

    /// Journal file path (typically under a Docker bind mount, e.g. `data/`).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Retention policy in effect.
    pub fn retention(&self) -> &RetentionPolicy {
        &self.retention
    }

    /// Appends one record and optionally runs the retention stub.
    pub fn append_record(&mut self, record: &JournalRecord) -> Result<(), JournalError> {
        writeln!(self.file, "{}", record.to_line())?;
        self.file.flush()?;
        self.appends_since_vacuum += 1;
        if self.vacuum_every > 0
            && self.appends_since_vacuum >= self.vacuum_every
            && (self.retention.max_bytes.is_some() || self.retention.max_age_secs.is_some())
        {
            self.appends_since_vacuum = 0;
            // Close is not required; vacuum reopens by path. Flush first.
            self.file.flush()?;
            let _ = self.retention.vacuum(&self.path)?;
            // Re-open append handle after possible rewrite.
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.path)?;
        }
        Ok(())
    }

    /// Forces a vacuum pass (tests / admin).
    pub fn vacuum_now(&mut self) -> Result<VacuumStats, JournalError> {
        self.file.flush()?;
        let stats = self.retention.vacuum(&self.path)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)?;
        Ok(stats)
    }

    /// Reads all records for offline replay (skips comments / bad lines).
    pub fn read_all(path: &Path) -> Result<Vec<JournalRecord>, JournalError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if let Some(rec) = JournalRecord::parse_line(&line) {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Offline replay: hex lines as raw frame byte vectors (order preserved).
    pub fn replay_raw(path: &Path) -> Result<Vec<Vec<u8>>, JournalError> {
        let records = Self::read_all(path)?;
        records.into_iter().map(|r| r.raw_bytes()).collect()
    }
}

impl FrameIngest for FileFrameJournal {
    fn append(&mut self, record: &JournalRecord) -> Result<(), JournalError> {
        self.append_record(record)
    }
}

/// No-op ingest (journal disabled).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullIngest;

impl FrameIngest for NullIngest {
    fn append(&mut self, _record: &JournalRecord) -> Result<(), JournalError> {
        Ok(())
    }
}

/// Shared mutable ingest handle for the decode drain.
pub type SharedIngest = Option<std::sync::Arc<std::sync::Mutex<Box<dyn FrameIngest>>>>;

/// Wraps a concrete journal as [`SharedIngest`].
pub fn shared_file_journal(journal: FileFrameJournal) -> SharedIngest {
    Some(std::sync::Arc::new(std::sync::Mutex::new(
        Box::new(journal) as Box<dyn FrameIngest>,
    )))
}

/// Appends a frame to an optional shared ingest (logs and continues on error).
pub fn append_frame_best_effort(ingest: &SharedIngest, frame: &Frame) {
    let Some(slot) = ingest else {
        return;
    };
    let record = JournalRecord::from_frame(frame);
    match slot.lock() {
        Ok(mut guard) => {
            if let Err(e) = guard.append(&record) {
                warn!(error = %e, "journal append failed");
            }
        }
        Err(e) => warn!(error = %e, "journal mutex poisoned"),
    }
}

/// Journal / ingest errors.
#[derive(Debug, Error)]
pub enum JournalError {
    /// Underlying I/O failure.
    #[error("journal I/O: {0}")]
    Io(#[from] io::Error),
    /// Hex decode failure on read-back.
    #[error("invalid journal hex: {0}")]
    BadHex(String),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_hex_string(s: &str) -> bool {
    !s.is_empty() && s.len() % 2 == 0 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn decode_hex(s: &str) -> Result<Vec<u8>, JournalError> {
    if !is_hex_string(s) {
        return Err(JournalError::BadHex(s.to_string()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let a = hex_nibble(bytes[i]).ok_or_else(|| JournalError::BadHex(s.to_string()))?;
        let b = hex_nibble(bytes[i + 1]).ok_or_else(|| JournalError::BadHex(s.to_string()))?;
        out.push((a << 4) | b);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Keeps the trailing portion of a journal file under `max_bytes` (complete lines).
fn trim_file_to_max_bytes(path: &Path, max_bytes: u64) -> Result<(), JournalError> {
    let data = std::fs::read(path)?;
    if (data.len() as u64) <= max_bytes {
        return Ok(());
    }
    let max = max_bytes as usize;
    // Prefer starting at a newline so we keep complete records.
    let start = if max >= data.len() {
        0
    } else {
        let tail = &data[data.len() - max..];
        match tail.iter().position(|&b| b == b'\n') {
            Some(rel) => data.len() - max + rel + 1,
            None => data.len() - max,
        }
    };
    let mut kept = Vec::new();
    kept.extend_from_slice(
        b"# pentairservice-journal-v1 (vacuum trim)\n\
          # Append-Only Log; older lines removed by size retention stub.\n",
    );
    kept.extend_from_slice(&data[start..]);
    let tmp = path.with_extension("journal.tmp");
    std::fs::write(&tmp, &kept)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framer::FrameKind;

    fn sample_frame() -> Frame {
        Frame {
            kind: FrameKind::A5,
            raw: vec![0xff, 0x00, 0xff, 0xa5, 0x01, 0x02],
            checksum_ok: true,
        }
    }

    #[test]
    fn record_round_trip_line() {
        let mut rec = JournalRecord::from_frame(&sample_frame());
        rec.ts_ms = Some(1_700_000_000_000);
        let line = rec.to_line();
        let parsed = JournalRecord::parse_line(&line).unwrap();
        assert_eq!(parsed.hex, "ff00ffa50102");
        assert_eq!(parsed.ts_ms, Some(1_700_000_000_000));
        assert!(parsed.checksum_ok);
        assert_eq!(parsed.kind, "A5");
        assert_eq!(parsed.raw_bytes().unwrap(), sample_frame().raw);
    }

    #[test]
    fn parse_hex_only_line() {
        let rec = JournalRecord::parse_line("aabbcc").unwrap();
        assert_eq!(rec.hex, "aabbcc");
        assert_eq!(rec.raw_bytes().unwrap(), vec![0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn parse_skips_comments() {
        assert!(JournalRecord::parse_line("# hello").is_none());
        assert!(JournalRecord::parse_line("").is_none());
    }

    #[test]
    fn file_journal_write_and_replay_offline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frames.journal");
        let mut journal = FileFrameJournal::open(&path, RetentionPolicy::none()).unwrap();

        let f1 = sample_frame();
        let f2 = Frame {
            kind: FrameKind::IntelliChlor,
            raw: vec![0x10, 0x02, 0x50, 0x00],
            checksum_ok: false,
        };
        journal.append_record(&JournalRecord::from_frame(&f1)).unwrap();
        journal.append_record(&JournalRecord::from_frame(&f2)).unwrap();
        drop(journal);

        let records = FileFrameJournal::read_all(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].raw_bytes().unwrap(), f1.raw);
        assert_eq!(records[1].kind, "IntelliChlor");
        assert!(!records[1].checksum_ok);

        let raws = FileFrameJournal::replay_raw(&path).unwrap();
        assert_eq!(raws, vec![f1.raw, f2.raw]);
    }

    #[test]
    fn frame_ingest_trait_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.hex");
        let mut journal: Box<dyn FrameIngest> =
            Box::new(FileFrameJournal::open(&path, RetentionPolicy::none()).unwrap());
        let rec = JournalRecord::from_frame(&sample_frame());
        journal.append(&rec).unwrap();
        assert_eq!(FileFrameJournal::read_all(&path).unwrap().len(), 1);
    }

    #[test]
    fn null_ingest_ok() {
        let mut n = NullIngest;
        n.append(&JournalRecord::from_frame(&sample_frame())).unwrap();
    }

    #[test]
    fn retention_size_vacuum_trims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.journal");
        let policy = RetentionPolicy::max_bytes(200);
        let mut journal = FileFrameJournal::open(&path, policy.clone()).unwrap();
        for i in 0..40 {
            let mut raw = vec![0xff, 0x00, 0xff, 0xa5, i as u8];
            raw.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
            journal
                .append_record(&JournalRecord::from_frame(&Frame {
                    kind: FrameKind::A5,
                    raw,
                    checksum_ok: true,
                }))
                .unwrap();
        }
        let before = std::fs::metadata(&path).unwrap().len();
        assert!(before > 200, "before={before}");
        let stats = journal.vacuum_now().unwrap();
        assert!(stats.bytes_after <= 200 + 120); // header slack
        assert!(stats.bytes_after < before);
        // Still readable.
        let records = FileFrameJournal::read_all(&path).unwrap();
        assert!(!records.is_empty());
    }

    #[test]
    fn retention_age_stub_flags_skip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("age.journal");
        let policy = RetentionPolicy {
            max_bytes: None,
            max_age_secs: Some(3600),
        };
        let mut j = FileFrameJournal::open(&path, policy).unwrap();
        j.append_record(&JournalRecord::from_frame(&sample_frame()))
            .unwrap();
        let stats = j.vacuum_now().unwrap();
        assert!(stats.age_stub_skipped);
        assert_eq!(FileFrameJournal::read_all(&path).unwrap().len(), 1);
    }

    #[test]
    fn shared_ingest_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.journal");
        let journal = FileFrameJournal::open(&path, RetentionPolicy::none()).unwrap();
        let shared = shared_file_journal(journal);
        append_frame_best_effort(&shared, &sample_frame());
        append_frame_best_effort(&None, &sample_frame());
        assert_eq!(FileFrameJournal::read_all(&path).unwrap().len(), 1);
    }

    #[test]
    fn bad_hex_errors() {
        let err = decode_hex("zz").unwrap_err();
        assert!(matches!(err, JournalError::BadHex(_)));
        let rec = JournalRecord {
            hex: "abc".into(),
            ts_ms: None,
            checksum_ok: true,
            kind: "A5".into(),
        };
        assert!(rec.raw_bytes().is_err());
    }

    #[test]
    fn vacuum_missing_path_ok() {
        let stats = RetentionPolicy::max_bytes(10)
            .vacuum(Path::new("/tmp/pentair-journal-does-not-exist-xyz"))
            .unwrap();
        assert_eq!(stats.bytes_before, 0);
    }

    #[test]
    fn journal_error_display() {
        let e = JournalError::BadHex("x".into());
        assert!(e.to_string().contains("hex"));
    }

    #[test]
    fn auto_vacuum_on_append_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto.journal");
        let mut j = FileFrameJournal::open(&path, RetentionPolicy::max_bytes(150)).unwrap();
        j.vacuum_every = 5;
        for i in 0..12 {
            j.append_record(&JournalRecord::from_frame(&Frame {
                kind: FrameKind::A5,
                raw: vec![0xaa, 0xbb, i as u8],
                checksum_ok: true,
            }))
            .unwrap();
        }
        // File should still exist and be readable after auto vacuum.
        assert!(!FileFrameJournal::read_all(&path).unwrap().is_empty());
    }
}
