//! Bus Actor: single task owns the transport for the process lifetime.
//!
//! Pattern: **Actor** / task ownership — all bus I/O is serialized through one
//! tokio task. The task opens the Strategy transport, feeds a
//! [`crate::framer::Framer`], emits frames on a channel, and on disconnect
//! reconnects with [`super::Backoff`]. It does **not** open/close per frame.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::framer::{Frame, Framer};

use super::backoff::{Backoff, BackoffConfig};
use super::{ByteTransport, TransportError};

/// Tunables for the bus actor loop.
#[derive(Debug, Clone)]
pub struct BusActorConfig {
    /// Reconnect backoff schedule.
    pub backoff: BackoffConfig,
    /// Read buffer size (bytes per `read` syscall).
    pub read_buf_size: usize,
    /// When true, EOF (`read` → 0) triggers reconnect (TCP half-close / replay end).
    /// When false, EOF ends the actor cleanly (one-shot replay).
    pub reconnect_on_eof: bool,
    /// Optional cap on reconnect attempts (`None` = unlimited).
    pub max_reconnects: Option<u64>,
    /// Use deterministic backoff (tests).
    pub deterministic_backoff: bool,
}

impl Default for BusActorConfig {
    fn default() -> Self {
        Self {
            backoff: BackoffConfig::default(),
            read_buf_size: 512,
            reconnect_on_eof: true,
            max_reconnects: None,
            deterministic_backoff: false,
        }
    }
}

impl BusActorConfig {
    /// Accelerated reconnect for automated soak tests (no live EW11 wait).
    pub fn accelerated_for_tests() -> Self {
        Self {
            backoff: BackoffConfig::accelerated_for_tests(),
            read_buf_size: 64,
            reconnect_on_eof: true,
            max_reconnects: None,
            deterministic_backoff: true,
        }
    }
}

/// Counters from a bus actor session (soak / reconnect evidence).
#[derive(Debug, Default, Clone)]
pub struct BusStats {
    /// Successful `open` calls.
    pub connects: u64,
    /// Disconnect / I/O error / EOF-triggered reconnects.
    pub reconnects: u64,
    /// Frames extracted by the framer (checksum ok or not).
    pub frames: u64,
    /// Frames with failed checksum (from framer counter delta).
    pub checksum_failures: u64,
    /// Bytes read from the transport.
    pub bytes_read: u64,
    /// True if the actor stopped because `max_reconnects` was hit.
    pub stopped_on_max_reconnects: bool,
}

/// Shared live counters (updated while the actor runs).
#[derive(Debug, Default)]
pub struct BusCounters {
    /// Successful connects.
    pub connects: AtomicU64,
    /// Reconnect events.
    pub reconnects: AtomicU64,
    /// Frames emitted.
    pub frames: AtomicU64,
    /// Bytes read.
    pub bytes_read: AtomicU64,
    /// Actor is currently connected (open succeeded, not yet closed).
    pub connected: AtomicBool,
}

impl BusCounters {
    /// Snapshot into a plain [`BusStats`] (checksum_failures filled by caller if known).
    pub fn snapshot(&self) -> BusStats {
        BusStats {
            connects: self.connects.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            frames: self.frames.load(Ordering::Relaxed),
            checksum_failures: 0,
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            stopped_on_max_reconnects: false,
        }
    }
}

/// Pattern: **Actor** — owns one [`ByteTransport`] Strategy for the service lifetime.
pub struct BusActor {
    transport: Box<dyn ByteTransport>,
    config: BusActorConfig,
    counters: Arc<BusCounters>,
}

impl BusActor {
    /// Creates an actor around a Strategy transport.
    pub fn new(transport: Box<dyn ByteTransport>, config: BusActorConfig) -> Self {
        Self {
            transport,
            config,
            counters: Arc::new(BusCounters::default()),
        }
    }

    /// Shared counters for external observation during soak.
    pub fn counters(&self) -> Arc<BusCounters> {
        Arc::clone(&self.counters)
    }

    /// Spawns the actor task. Frames are sent on `frame_tx`.
    /// Set `shutdown` to `true` to stop. Returns a join handle with final stats.
    pub fn spawn(
        self,
        frame_tx: mpsc::Sender<Frame>,
        shutdown: watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<BusStats> {
        tokio::spawn(async move { self.run(frame_tx, shutdown).await })
    }

    /// Runs the connect → read → frame → reconnect loop until shutdown.
    pub async fn run(
        mut self,
        frame_tx: mpsc::Sender<Frame>,
        mut shutdown: watch::Receiver<bool>,
    ) -> BusStats {
        let mut framer = Framer::new();
        let mut backoff = if self.config.deterministic_backoff {
            Backoff::deterministic(self.config.backoff)
        } else {
            Backoff::new(self.config.backoff)
        };
        let mut stats = BusStats::default();
        let mut read_buf = vec![0u8; self.config.read_buf_size.max(1)];
        let name = self.transport.name().to_string();

        info!(transport = %name, "bus actor starting (persistent connection; never open/close per frame)");

        loop {
            if *shutdown.borrow() {
                break;
            }

            // --- connect ---
            match self.transport.open().await {
                Ok(()) => {
                    stats.connects += 1;
                    self.counters.connects.fetch_add(1, Ordering::Relaxed);
                    self.counters.connected.store(true, Ordering::Relaxed);
                    backoff.reset();
                    framer.reset();
                    info!(transport = %name, connects = stats.connects, "bus connected");
                }
                Err(e) => {
                    warn!(transport = %name, error = %e, "bus connect failed");
                    if self.hit_max_reconnects(&mut stats, &name) {
                        break;
                    }
                    stats.reconnects += 1;
                    self.counters.reconnects.fetch_add(1, Ordering::Relaxed);
                    if !sleep_backoff_or_shutdown(&mut backoff, &mut shutdown).await {
                        break;
                    }
                    continue;
                }
            }

            // --- read loop (same connection until error/EOF/shutdown) ---
            let session = self
                .read_session(
                    &mut framer,
                    &mut read_buf,
                    &frame_tx,
                    &mut shutdown,
                    &mut stats,
                )
                .await;

            let _ = self.transport.close().await;
            self.counters.connected.store(false, Ordering::Relaxed);

            match session {
                SessionEnd::Shutdown => break,
                SessionEnd::Eof | SessionEnd::Error(_) => {
                    if matches!(session, SessionEnd::Eof) && !self.config.reconnect_on_eof {
                        info!(transport = %name, "bus EOF (one-shot; not reconnecting)");
                        break;
                    }
                    if let SessionEnd::Error(ref e) = session {
                        warn!(transport = %name, error = %e, "bus session ended; will reconnect");
                    } else {
                        info!(transport = %name, "bus EOF; will reconnect");
                    }
                    if self.hit_max_reconnects(&mut stats, &name) {
                        break;
                    }
                    stats.reconnects += 1;
                    self.counters.reconnects.fetch_add(1, Ordering::Relaxed);
                    if !sleep_backoff_or_shutdown(&mut backoff, &mut shutdown).await {
                        break;
                    }
                }
            }
        }

        let _ = self.transport.close().await;
        self.counters.connected.store(false, Ordering::Relaxed);
        stats.checksum_failures = framer.checksum_failures();
        info!(
            transport = %name,
            connects = stats.connects,
            reconnects = stats.reconnects,
            frames = stats.frames,
            bytes = stats.bytes_read,
            checksum_failures = stats.checksum_failures,
            "bus actor stopped"
        );
        stats
    }

    /// Returns true when `max_reconnects` is configured and already reached.
    fn hit_max_reconnects(&self, stats: &mut BusStats, name: &str) -> bool {
        if let Some(max) = self.config.max_reconnects {
            if stats.reconnects >= max {
                warn!(
                    transport = %name,
                    reconnects = stats.reconnects,
                    max,
                    "max reconnects reached; stopping actor"
                );
                stats.stopped_on_max_reconnects = true;
                return true;
            }
        }
        false
    }

    async fn read_session(
        &mut self,
        framer: &mut Framer,
        read_buf: &mut [u8],
        frame_tx: &mpsc::Sender<Frame>,
        shutdown: &mut watch::Receiver<bool>,
        stats: &mut BusStats,
    ) -> SessionEnd {
        loop {
            if *shutdown.borrow() {
                return SessionEnd::Shutdown;
            }

            let read_result = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return SessionEnd::Shutdown;
                    }
                    continue;
                }
                result = self.transport.read(read_buf) => result,
            };

            match read_result {
                Ok(0) => return SessionEnd::Eof,
                Ok(n) => {
                    stats.bytes_read += n as u64;
                    self.counters
                        .bytes_read
                        .fetch_add(n as u64, Ordering::Relaxed);
                    let frames = framer.feed(&read_buf[..n]);
                    for frame in frames {
                        stats.frames += 1;
                        self.counters.frames.fetch_add(1, Ordering::Relaxed);
                        debug!(
                            kind = ?frame.kind,
                            len = frame.raw.len(),
                            checksum_ok = frame.checksum_ok,
                            "framed bus message"
                        );
                        if frame_tx.send(frame).await.is_err() {
                            // Receiver dropped — treat as shutdown.
                            return SessionEnd::Shutdown;
                        }
                    }
                }
                Err(e) => return SessionEnd::Error(e),
            }
        }
    }

}

/// Sleeps backoff delay unless shutdown is signaled. Returns `false` if shutting down.
async fn sleep_backoff_or_shutdown(
    backoff: &mut Backoff,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    let delay = backoff.next_delay();
    debug!(
        attempt = backoff.attempt(),
        delay_ms = delay.as_millis() as u64,
        "reconnect backoff"
    );
    tokio::select! {
        _ = shutdown.changed() => !*shutdown.borrow(),
        _ = tokio::time::sleep(delay) => true,
    }
}

enum SessionEnd {
    Shutdown,
    Eof,
    Error(TransportError),
}

/// Spawns a fire-and-forget frame drain task (B2: log/count only; B3 adds `/status`).
pub fn spawn_frame_drain(mut rx: mpsc::Receiver<Frame>) -> tokio::task::JoinHandle<u64> {
    tokio::spawn(async move {
        let mut n = 0u64;
        while let Some(frame) = rx.recv().await {
            n += 1;
            debug!(
                kind = ?frame.kind,
                checksum_ok = frame.checksum_ok,
                "drained frame (no status API until B3)"
            );
        }
        n
    })
}

/// Convenience: build actor from URL and spawn with default channel capacity.
pub fn spawn_from_url(
    url: &str,
    config: BusActorConfig,
    shutdown: watch::Receiver<bool>,
) -> Result<(tokio::task::JoinHandle<BusStats>, Arc<BusCounters>), TransportError> {
    let transport = super::from_url(url)?;
    let actor = BusActor::new(transport, config);
    let counters = actor.counters();
    let (tx, rx) = mpsc::channel(256);
    let _drain = spawn_frame_drain(rx);
    let handle = actor.spawn(tx, shutdown);
    Ok((handle, counters))
}

/// Tiny helper so tests can wait without spinning forever.
pub async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    pred()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::replay::ReplayTransport;
    use crate::transport::tcp::TcpTransport;
    use crate::transport::ByteTransport;
    use async_trait::async_trait;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Scripted Strategy for reconnect tests.
    struct ScriptedTransport {
        opens: u32,
        max_opens: u32,
        payload: Vec<u8>,
        pos: usize,
        fail_open_times: u32,
        opened: bool,
        disconnect_after: Option<u32>,
        reads: u32,
    }

    impl ScriptedTransport {
        fn new(payload: Vec<u8>) -> Self {
            Self {
                opens: 0,
                max_opens: u32::MAX,
                payload,
                pos: 0,
                fail_open_times: 0,
                opened: false,
                disconnect_after: None,
                reads: 0,
            }
        }
    }

    #[async_trait]
    impl ByteTransport for ScriptedTransport {
        async fn open(&mut self) -> Result<(), TransportError> {
            self.opens += 1;
            if self.opens <= self.fail_open_times {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "scripted refuse",
                )));
            }
            if self.opens > self.max_opens {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "max opens",
                )));
            }
            self.pos = 0;
            self.reads = 0;
            self.opened = true;
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
                    "not open",
                )));
            }
            if let Some(after) = self.disconnect_after {
                if self.reads >= after {
                    self.opened = false;
                    return Err(TransportError::Io(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "scripted reset",
                    )));
                }
            }
            if self.pos >= self.payload.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.payload.len() - self.pos);
            buf[..n].copy_from_slice(&self.payload[self.pos..self.pos + n]);
            self.pos += n;
            self.reads += 1;
            Ok(n)
        }

        async fn write(&mut self, _data: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    fn a5_idle_frame() -> Vec<u8> {
        // Minimal valid-looking A5 from fixtures pattern: idle + sync + short header.
        // Use real fixture bytes for checksum-correct framing.
        let text = std::fs::read_to_string("fixtures/status_temps.hex").unwrap();
        crate::transport::replay::parse_hex_bytes(&text)
    }

    #[tokio::test]
    async fn actor_frames_replay_fixture() {
        let data = a5_idle_frame();
        let transport = Box::new(ReplayTransport::from_bytes(data).rewind_on_open(false));
        let mut cfg = BusActorConfig::accelerated_for_tests();
        cfg.reconnect_on_eof = false;

        let (tx, mut rx) = mpsc::channel(32);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let actor = BusActor::new(transport, cfg);
        let handle = actor.spawn(tx, shutdown_rx);

        let mut frames = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                biased;
                Some(f) = rx.recv() => frames.push(f),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if handle.is_finished() {
                        break;
                    }
                }
            }
        }
        let _ = shutdown_tx.send(true);
        let stats = handle.await.unwrap();
        assert!(stats.frames >= 1, "expected framed messages, got {}", stats.frames);
        assert!(stats.connects >= 1);
        assert!(!frames.is_empty());
    }

    #[tokio::test]
    async fn actor_reconnects_after_disconnect() {
        let data = a5_idle_frame();
        let mut scripted = ScriptedTransport::new(data);
        scripted.disconnect_after = Some(1);
        scripted.max_opens = 3;

        let mut cfg = BusActorConfig::accelerated_for_tests();
        cfg.max_reconnects = Some(2);
        cfg.reconnect_on_eof = true;

        let (tx, _rx) = mpsc::channel(64);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let actor = BusActor::new(Box::new(scripted), cfg);
        let counters = actor.counters();
        let handle = actor.spawn(tx, shutdown_rx);

        assert!(
            wait_until(Duration::from_secs(3), || {
                counters.reconnects.load(Ordering::Relaxed) >= 1
            })
            .await,
            "expected at least one reconnect"
        );
        // Allow actor to hit max reconnects and stop.
        let stats = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("actor should finish")
            .unwrap();
        assert!(stats.connects >= 2, "connects={}", stats.connects);
        assert!(stats.reconnects >= 1);
        assert!(stats.stopped_on_max_reconnects || stats.reconnects >= 1);
    }

    #[tokio::test]
    async fn actor_retries_failed_connect() {
        let mut scripted = ScriptedTransport::new(vec![0xff, 0x00, 0xff, 0xa5]);
        scripted.fail_open_times = 2;
        scripted.max_opens = 3;

        let mut cfg = BusActorConfig::accelerated_for_tests();
        cfg.reconnect_on_eof = false;
        cfg.max_reconnects = Some(5);

        let (tx, _rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let actor = BusActor::new(Box::new(scripted), cfg);
        let counters = actor.counters();
        let handle = actor.spawn(tx, shutdown_rx);

        assert!(
            wait_until(Duration::from_secs(2), || {
                counters.connects.load(Ordering::Relaxed) >= 1
            })
            .await
        );
        let _ = shutdown_tx.send(true);
        let stats = handle.await.unwrap();
        assert!(stats.connects >= 1);
    }

    #[tokio::test]
    async fn short_automated_soak_with_tcp_flapping() {
        // Accelerated soak: local TCP server sends fixture chunks then closes;
        // client reconnects several times within seconds (not a 1-hour EW11 wait).
        let payload = a5_idle_frame();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sessions = Arc::new(AtomicU64::new(0));
        let sessions_c = Arc::clone(&sessions);
        let payload_c = payload.clone();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                sessions_c.fetch_add(1, Ordering::Relaxed);
                let _ = sock.write_all(&payload_c).await;
                // Brief pause then close → client sees EOF and reconnects.
                tokio::time::sleep(Duration::from_millis(5)).await;
                drop(sock);
            }
        });

        let mut cfg = BusActorConfig::accelerated_for_tests();
        cfg.reconnect_on_eof = true;
        cfg.max_reconnects = Some(6);

        let transport = Box::new(TcpTransport::new("127.0.0.1", addr.port()));
        let (tx, mut rx) = mpsc::channel(128);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let actor = BusActor::new(transport, cfg);
        let counters = actor.counters();
        let handle = actor.spawn(tx, shutdown_rx);

        let mut framed = 0u64;
        let soak_deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        while tokio::time::Instant::now() < soak_deadline {
            tokio::select! {
                Some(_f) = rx.recv() => { framed += 1; }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if counters.reconnects.load(Ordering::Relaxed) >= 3
                        && framed >= 3
                    {
                        break;
                    }
                }
            }
        }

        let _ = shutdown_tx.send(true);
        let stats = tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("soak actor join")
            .unwrap();
        server.abort();
        let _ = server.await;

        assert!(
            stats.connects >= 2,
            "soak connects={} reconnects={} frames={} drained={}",
            stats.connects,
            stats.reconnects,
            stats.frames,
            framed
        );
        assert!(
            stats.frames + framed >= 2,
            "expected frames during soak; stats.frames={} drained={}",
            stats.frames,
            framed
        );
        assert!(stats.bytes_read > 0);
        // Evidence: multi-session soak completed in seconds with reconnects.
        info!(
            connects = stats.connects,
            reconnects = stats.reconnects,
            frames = stats.frames,
            bytes = stats.bytes_read,
            server_sessions = sessions.load(Ordering::Relaxed),
            "short automated soak complete"
        );
    }

    #[tokio::test]
    async fn spawn_from_url_replay() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut cfg = BusActorConfig::accelerated_for_tests();
        cfg.reconnect_on_eof = false;
        cfg.max_reconnects = Some(0);
        let (handle, counters) =
            spawn_from_url("replay:fixtures/status_temps.hex", cfg, shutdown_rx).unwrap();
        assert!(
            wait_until(Duration::from_secs(2), || {
                counters.frames.load(Ordering::Relaxed) >= 1
                    || counters.connects.load(Ordering::Relaxed) >= 1
            })
            .await
        );
        let _ = shutdown_tx.send(true);
        let stats = handle.await.unwrap();
        assert!(stats.connects >= 1);
        assert!(stats.frames >= 1);
    }

    #[tokio::test]
    async fn shutdown_during_backoff() {
        let mut scripted = ScriptedTransport::new(vec![]);
        scripted.fail_open_times = 100;
        let mut cfg = BusActorConfig::accelerated_for_tests();
        cfg.backoff.initial = Duration::from_secs(30); // long — interrupt via shutdown

        let (tx, _rx) = mpsc::channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = BusActor::new(Box::new(scripted), cfg).spawn(tx, shutdown_rx);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = shutdown_tx.send(true);
        let stats = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("should stop on shutdown")
            .unwrap();
        assert_eq!(stats.connects, 0);
    }

    #[tokio::test]
    async fn counters_snapshot() {
        let c = BusCounters::default();
        c.connects.store(3, Ordering::Relaxed);
        c.frames.store(9, Ordering::Relaxed);
        let s = c.snapshot();
        assert_eq!(s.connects, 3);
        assert_eq!(s.frames, 9);
    }

    #[tokio::test]
    async fn drain_counts_frames() {
        let (tx, rx) = mpsc::channel(8);
        let h = spawn_frame_drain(rx);
        tx.send(Frame {
            kind: crate::framer::FrameKind::A5,
            raw: vec![1, 2, 3],
            checksum_ok: true,
        })
        .await
        .unwrap();
        drop(tx);
        assert_eq!(h.await.unwrap(), 1);
    }
}
