# pentairservice

Long-running Pentair bus owner (Rust, Linux). Local sibling git repo (ignored by parent phpentair).

## Canonical environment

**Build, test, coverage, and run inside Debian Docker** — not on the Darwin host alone. Host `cargo` is non-authoritative; the production target is Linux.

Source on the host is bind-mounted to `/workspace` in the container. Edit on the host; compile and run in the container.

## Quick start (Docker)

```bash
# Build the toolchain image (Rust stable + llvm-tools + cargo-llvm-cov)
docker compose build

# Interactive shell
docker compose run --rm pentairservice-dev

# Unit / integration tests
docker compose run --rm pentairservice-dev cargo test

# Line coverage (95% gate on library code; thin binary excluded)
docker compose run --rm pentairservice-dev cargo llvm-cov --lib --fail-under-lines 95

# Run the service (health + status API on published port 8080)
docker compose run --rm --service-ports pentairservice-dev cargo run
```

Then from the host:

```bash
curl -s http://127.0.0.1:8080/health
# {"status":"ok"}

curl -s http://127.0.0.1:8080/status
# {"systemStatus":null,"tempStatus":null,"framesSeen":0,"quarantined":0}
```

### Replay + poll `/status`

```bash
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=replay:fixtures/status_temps.hex \
  pentairservice-dev cargo run
```

In another terminal:

```bash
curl -s http://127.0.0.1:8080/status
# tempStatus populated (water, air, waterSet, spaSet, info, raw hex, …)
curl -s 'http://127.0.0.1:8080/frames?limit=5'
```

Equivalent without compose:

```bash
docker build -t pentairservice-dev .
docker run --rm -it -v "$PWD":/workspace -w /workspace -p 8080:8080 pentairservice-dev cargo test
```

## Configuration

| Source | Keys |
|--------|------|
| Env | `PENTAIR_BIND_ADDR`, `PENTAIR_TRANSPORT_URL`, `PENTAIR_LOG_LEVEL`, `PENTAIR_CONFIG`, `PENTAIR_JOURNAL_PATH`, `PENTAIR_JOURNAL_MAX_BYTES`, `PENTAIR_JOURNAL_MAX_AGE_SECS`, `PENTAIR_WRITES_ENABLED`, `PENTAIR_LISTEN_WINDOW_MS` |
| TOML | `bind_addr`, `transport_url`, `log_level`, `journal_path`, `journal_max_bytes`, `journal_max_age_secs`, `writes_enabled`, `listen_window_ms` (see `config.example.toml`) |

Defaults: bind `0.0.0.0:8080`, no transport, no journal. With an empty/unset `transport_url`, the service **idles** and serves `GET /health` / `/status` / `/frames` without opening TCP/serial.

### Frame journal (B4)

Optional **append-only** log of framed bus messages on the bind-mounted volume (e.g. `data/frames.journal` under `/workspace`).

```bash
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=replay:fixtures/status_temps.hex \
  -e PENTAIR_JOURNAL_PATH=data/frames.journal \
  pentairservice-dev cargo run
```

- Format: comment header + TSV lines `ts_ms\tchecksum_ok\tkind\thex` (final column = lowercase hex, same as PHP `signal/` `bin2hex`).
- Pattern: **Repository** / **Append-Only Log**; trait `FrameIngest` is the hook for a future HTTP/MySQL sink replacing `signal/` ingest.
- Retention stub: `journal_max_bytes` trims trailing lines when over size; `journal_max_age_secs` is accepted but age vacuum is a no-op stub.
- Offline replay: unit tests read the journal back via `FileFrameJournal::replay_raw`.

### HTTP API (B3)

| Method | Path | Body |
|--------|------|------|
| `GET` | `/health` | `{"status":"ok"}` |
| `GET` | `/status` | Latest `systemStatus` + `tempStatus` (PHP Command JSON field names) or null |
| `GET` | `/frames?limit=` | Newest-first ring (default `limit=32`, max 128) |
| `POST` | `/command` | Typed CircuitChange / HeatChange or raw hex — write gate (B5) |

### Write gate (B5)

`POST /command` accepts typed JSON or raw framed hex for confirmed write commands:

- **CircuitChange** (`0x86`): `{"type":"CircuitChange","circuit":6,"on":true}` or `"circuit":"pool_light"`
- **HeatChange** (`0x88`): `{"type":"HeatChange","poolSet":43,"spaSet":96,"mode":5}`
- **Raw hex**: `{"hex":"ff00ffa507102088042b60050001f8"}`

**Mutex policy:** only one in-flight write; concurrent requests are **rejected** with HTTP 409 (`busy`), not queued.

**Safety:** `writes_enabled` defaults to **false**. When disabled, the service **dry-runs**: builds TX hex (CS verified), simulates the listen-window verdict from fixture ACK frames (`$set_temp_ack` / crafted circuit ACK), and never touches the live bus — CI passes without a controller.

```bash
# Dry-run (default)
curl -s -X POST http://127.0.0.1:8080/command \
  -H 'content-type: application/json' \
  -d '{"type":"HeatChange","poolSet":43,"spaSet":96,"mode":5}'
# {"dry_run":true,"tx_hex":"ff00ffa507102088042b60050001f8","command":136,"verdict":"ack",...}

# Live TX (lab only)
# -e PENTAIR_WRITES_ENABLED=true -e PENTAIR_TRANSPORT_URL=tcp://ew11:8899
```

Config / env: `writes_enabled` / `PENTAIR_WRITES_ENABLED`, `listen_window_ms` / `PENTAIR_LISTEN_WINDOW_MS` (default 500).

Patterns: **Command** (craft), **Mutex/Gate**, **Actor** owns TX when enabled.

**JSON field names** (compatible with future PHP `Command::fromJson`):

- Shared: `raw` (hex), `protocol`, `destination`, `source`, `command`, `length`
- SystemStatus (`0x02`): `hours`, `minutes`, `circuits`, `circuitStatus` (`filterPump`, `cleanerPump`, `waterFeature`, `spaLight`, `poolLight` as `"on"`/`"off"`), `waterTemp`, `heaterTemp`, `airTemp`
- TempStatus (`0x08`): `water`, `air`, `waterSet`, `spaSet`, `info`

PHP `fromJson` re-parses from `command` + `raw`; typed fields are for status clients.

### Transport URL schemes (B2 / E0)

Canonical forms (same mental model as snoop/PHP when those catch up):

| URL | Backend |
|-----|---------|
| `tcp://host:port` | Async TCP client (EW11 / RS485↔TCP bridge) |
| `/dev/ttyPentair` or `/dev/ttyUSB0` | Async serial, default **9600 8N1** |
| `serial:/dev/ttyPentair?baud=9600` | Async serial with explicit baud |
| `replay:fixtures/status_temps.hex` | Recorded hex replay (tests / lab without live bus) |

Examples:

```bash
# EW11
export PENTAIR_TRANSPORT_URL=tcp://10.0.0.11:8899

# Pi RS-485 (stable udev name — see docs/rpi-ops.md)
export PENTAIR_TRANSPORT_URL=/dev/ttyPentair
# or: export PENTAIR_TRANSPORT_URL='serial:/dev/ttyPentair?baud=9600'
```

A **single tokio task (Actor)** owns the connection: connect once, stream bytes into the framer buffer, reconnect with exponential backoff + jitter on failure. Framed messages are decoded via an explicit command-byte registry into the in-memory snapshot. The connection is **never** torn down per frame (unlike PHP `PentairComFacade`).

### Raspberry Pi / ops (E4)

Deploy on a Pi to either EW11 or USB-RS485 using **config only**. Full runbook:

- **[`docs/rpi-ops.md`](docs/rpi-ops.md)** — udev → `/dev/ttyPentair`, `dialout`, systemd `SupplementaryGroups`, host systemd vs Docker `--device=` tradeoffs, example `config.toml` / env for `tcp://` and serial.
- **Debian Docker** remains build/test/coverage canonical; live tty is lab/Pi only ([`06` plan](../agents/plans/06-pentairservice-docker-dev.md) in the parent tree when checked out together).
- Optional E5 lab template (fill on real hardware only): [`docs/lab-e5-NOTES.md`](docs/lab-e5-NOTES.md).

## Recorded replay (automated soak)

Unit tests include an accelerated reconnect soak (local TCP flap + fixture replay) that finishes in seconds. Run them via:

```bash
docker compose run --rm pentairservice-dev cargo test short_automated_soak -- --nocapture
```

One-shot replay while serving the status API:

```bash
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=replay:fixtures/status_temps.hex \
  pentairservice-dev cargo run
```

## Lab soak against a live EW11 (optional, ≥1 hour)

Automated DoD for B2 is **recorded replay + short soak tests**. For a longer lab soak on real hardware:

1. Note the EW11 IP/port on your LAN (often `8899`).
2. From the **container**, the EW11 must be reachable. On Docker Desktop (macOS), `host.docker.internal` reaches the host; for a LAN device use the device’s LAN IP directly (bridge mode usually routes fine). On Linux, `--network=host` avoids NAT surprises:

```bash
# Example: publish API + reach EW11 at 192.168.1.50:8899
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=tcp://192.168.1.50:8899 \
  pentairservice-dev cargo run

# Linux alternative (host networking)
docker compose run --rm --network=host \
  -e PENTAIR_BIND_ADDR=0.0.0.0:8080 \
  -e PENTAIR_TRANSPORT_URL=tcp://192.168.1.50:8899 \
  pentairservice-dev cargo run
```

3. Leave the process running ≥1 hour. Watch structured logs for `bus connected`, `bus session ended; will reconnect`, and reconnect counts. Kill/restart the EW11 (or unplug Ethernet briefly) and confirm the Actor reconnects without restarting the service.
4. Serial lab: prefer host systemd on the Pi for production serial ([`docs/rpi-ops.md`](docs/rpi-ops.md)). For a short container soak, pass the device through, e.g. `--device=/dev/ttyPentair` and `PENTAIR_TRANSPORT_URL=/dev/ttyPentair`.

## Layout

| Path | Role | Pattern |
|------|------|---------|
| `src/lib.rs` | Application façade (`run`, `build_router`) | Facade |
| `src/main.rs` | Thin binary entry | Facade |
| `src/config.rs` | Settings load (file + env) | Builder / Configuration Object |
| `src/framer.rs` | Streaming A5 + IntelliChlor sync/seek | Parser / State Machine |
| `src/messages.rs` | SystemStatus / TempStatus / Unknown DTOs | Command / Message |
| `src/registry.rs` | cmd-byte → parser map | Factory |
| `src/state.rs` | Latest snapshot + frames ring | Facade (shared state) |
| `src/journal.rs` | Append-only frame log + `FrameIngest` hook | Repository / Append-Only Log |
| `src/commands.rs` | CircuitChange / HeatChange craft (`0x86`/`0x88`) | Command |
| `src/write.rs` | Write gate Mutex + listen-window verdict | Mutex / Gate |
| `src/transport/` | TCP / serial / replay + reconnect Actor | Strategy + Actor |
| `src/logging.rs` | Tracing subscriber init | Facade |
| `src/api/` | Local HTTP surface | Facade |
| `src/api/health.rs` | `GET /health` | Facade (API surface) |
| `src/api/status.rs` | `GET /status`, `GET /frames` | Facade (API surface) |
| `src/api/command.rs` | `POST /command` write gate | Facade (API surface) |
| `fixtures/` | Hex samples for framer + replay + ACK tests | — |
| `docs/rpi-ops.md` | RPi udev / dialout / systemd vs Docker | Runbook |
| `docs/lab-e5-NOTES.md` | Optional dual-mode lab proof template (E5) | — |

## Milestone

Track E **E4** RPi/ops docs (stacked on B5 write gate). Track B **B5** remains the last functional Track B milestone.
