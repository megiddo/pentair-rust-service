# pentairservice

**pentairservice** owns the pool controller link (RS-485 adapter or EW11 TCP bridge) and exposes a **local HTTP API** for health, decoded status, recent frames, and optional write commands. Develop and run it in Docker; deploy the same binary on a Raspberry Pi or similar Linux host when you need live hardware.

Default HTTP bind: `0.0.0.0:28471`.

---

## 0. Quick start

This section gets a working API on your machine without a live controller.

The service can read a **saved hex recording** (*replay*) instead of a live bus. That lets you practice `GET /health`, `GET /status`, and `GET /frames` before wiring hardware. Replay feeds recorded bytes through the same framer and status decode path as a real transport.

### Build the image

```bash
docker compose build
```

Builds the Debian-based toolchain image (Rust + test/coverage tools) used for develop and run in Docker.

### Start with replay (no hardware)

```bash
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=replay:fixtures/status_temps.hex \
  pentairservice-dev cargo run
```

Starts the service, publishes port **28471**, and replays `fixtures/status_temps.hex` so `/status` can populate temperatures without a panel.

### Poll the API from the host

```bash
curl -s http://127.0.0.1:28471/health
# {"status":"ok"}
```

`GET /health` confirms the HTTP server is up (does not require a bus connection).

```bash
curl -s http://127.0.0.1:28471/status
# {"systemStatus":...,"tempStatus":...,"framesSeen":N,"quarantined":N}
```

`GET /status` returns the latest decoded system/temp snapshot (or null fields when nothing has been decoded yet).

```bash
curl -s 'http://127.0.0.1:28471/frames?limit=5'
```

`GET /frames` returns recent framed messages, newest first; `limit` caps how many rows you get (default 32, max 128).

### Idle start (health only)

```bash
docker compose run --rm --service-ports pentairservice-dev cargo run
```

With no `transport_url`, the service **idles**: HTTP works, but it does not open TCP/serial/replay.

### Interactive shell / tests

```bash
docker compose run --rm pentairservice-dev
docker compose run --rm pentairservice-dev cargo test
docker compose run --rm pentairservice-dev cargo llvm-cov --lib --fail-under-lines 95
```

Open a shell in the image, run the test suite, or enforce the library line-coverage gate.

Equivalent without Compose:

```bash
docker build -t pentairservice-dev .
docker run --rm -it -v "$PWD":/workspace -w /workspace -p 28471:28471 pentairservice-dev cargo test
```

Same image and bind mount; publish **28471** when you will `cargo run` and curl from the host.

---

## 1. Install & configure

### Docker image

Source on the host is bind-mounted to `/workspace` in the container. Edit on the host; compile and run in the container.

```bash
docker compose build
```

Produces the `pentairservice-dev` image from this repo’s `Dockerfile`.

### Config file

Copy the example and edit:

```bash
cp config.example.toml config.toml
```

Creates a local `config.toml` the service loads automatically when present (or set `PENTAIR_CONFIG` to another path).

Point at an explicit file:

```bash
export PENTAIR_CONFIG=/etc/pentairservice/config.toml
```

`PENTAIR_CONFIG` overrides the default discovery path so systemd or containers can load a fixed config.

### Settings (TOML and env)

Resolution order: built-in defaults → TOML file → `PENTAIR_*` environment variables (later wins).

| Key | Env | Example | Explanation |
|-----|-----|---------|-------------|
| `bind_addr` | `PENTAIR_BIND_ADDR` | `0.0.0.0:28471` | Host:port for the local HTTP API; default listens on all interfaces at **28471**. |
| `transport_url` | `PENTAIR_TRANSPORT_URL` | `tcp://10.0.0.11:8899` | How to reach the bus; empty/unset means idle (no connect). |
| `log_level` | `PENTAIR_LOG_LEVEL` | `info` | Tracing filter; `RUST_LOG` can also drive the subscriber. |
| `journal_path` | `PENTAIR_JOURNAL_PATH` | `data/frames.journal` | Optional append-only frame log path (relative to `/workspace` unless absolute). |
| `journal_max_bytes` | `PENTAIR_JOURNAL_MAX_BYTES` | `10485760` | Soft size cap; when exceeded, trailing complete lines are kept. |
| `journal_max_age_secs` | `PENTAIR_JOURNAL_MAX_AGE_SECS` | `604800` | Soft age cap accepted in config; age vacuum is currently a no-op stub. |
| `writes_enabled` | `PENTAIR_WRITES_ENABLED` | `false` | When false (default), `POST /command` dry-runs and never TX on the live bus. |
| `listen_window_ms` | `PENTAIR_LISTEN_WINDOW_MS` | `500` | Milliseconds to watch RX after a write for ACK/status classification. |

Example env for a published API + EW11:

```bash
export PENTAIR_BIND_ADDR=0.0.0.0:28471
export PENTAIR_TRANSPORT_URL=tcp://10.0.0.11:8899
```

Binds the HTTP API on the default operator port and opens a persistent TCP session to the EW11.

---

## 2. Operation

Day-to-day use, from idle health checks through optional writes.

### Start idle

```bash
docker compose run --rm --service-ports pentairservice-dev cargo run
# or with config.toml present and transport_url omitted
```

Useful when you only need the HTTP process up, or before attaching a transport via env/config.

### Attach a transport

A **transport** is the byte path to the panel bus. Set `transport_url` / `PENTAIR_TRANSPORT_URL` to one of:

| Mode | Example | Explanation |
|------|---------|-------------|
| **TCP** | `tcp://10.0.0.11:8899` | Client connection to an EW11 or other RS-485↔TCP bridge. |
| **Serial** | `/dev/ttyPentair` or `serial:/dev/ttyPentair?baud=9600` | Direct USB/HAT RS-485; default **9600 8N1** when baud is omitted. |
| **Replay** | `replay:fixtures/status_temps.hex` | Plays a recorded hex file once through the framer (lab/CI, no live bus). |

```bash
# EW11
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=tcp://10.0.0.11:8899 \
  pentairservice-dev cargo run

# Serial (Pi; pass the device into the container for lab soaks)
docker compose run --rm --service-ports \
  --device=/dev/ttyPentair \
  -e PENTAIR_TRANSPORT_URL=/dev/ttyPentair \
  pentairservice-dev cargo run

# Replay
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=replay:fixtures/status_temps.hex \
  pentairservice-dev cargo run
```

Each command starts the service with one transport; pick only one live path to the same bus (see §4).

### Poll health, status, and frames

```bash
curl -s http://127.0.0.1:28471/health
```

Liveness probe for process supervisors and quick smoke checks.

```bash
curl -s http://127.0.0.1:28471/status
```

Latest `systemStatus` / `tempStatus` objects (or null), plus `framesSeen` and `quarantined` counters.

```bash
curl -s 'http://127.0.0.1:28471/frames?limit=5'
```

Newest-first ring of recent frames; raise/lower `limit` to control response size (max 128).

### Optional journal

A **journal** is an append-only on-disk log of framed bus messages (timestamp, checksum flag, kind, hex). Use it when you want a durable capture on the bind-mounted volume for later offline review.

```bash
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=replay:fixtures/status_temps.hex \
  -e PENTAIR_JOURNAL_PATH=data/frames.journal \
  pentairservice-dev cargo run
```

Writes framed lines to `data/frames.journal` while also serving the HTTP API.

### Optional write / command (dry-run by default)

Sending a command can change circuits or heat setpoints on a live panel. By default **`writes_enabled` is false**: the service builds and checksum-verifies TX hex, simulates a listen-window verdict from fixture ACK frames, and **does not** transmit on the bus. Enable live TX only when you intend lab/production writes.

```bash
# Dry-run HeatChange (default)
curl -s -X POST http://127.0.0.1:28471/command \
  -H 'content-type: application/json' \
  -d '{"type":"HeatChange","poolSet":43,"spaSet":96,"mode":5}'
# {"dry_run":true,"tx_hex":"...","command":136,"verdict":"ack",...}
```

Crafts a HeatChange (`0x88`) frame and returns dry-run results without touching hardware.

```bash
curl -s -X POST http://127.0.0.1:28471/command \
  -H 'content-type: application/json' \
  -d '{"type":"CircuitChange","circuit":6,"on":true}'
```

Crafts a CircuitChange (`0x86`); `"circuit"` may be a number or a name such as `"pool_light"`.

```bash
curl -s -X POST http://127.0.0.1:28471/command \
  -H 'content-type: application/json' \
  -d '{"hex":"ff00ffa507102088042b60050001f8"}'
```

Accepts a pre-framed lowercase hex payload instead of a typed object.

Live TX (lab only):

```bash
docker compose run --rm --service-ports \
  -e PENTAIR_WRITES_ENABLED=true \
  -e PENTAIR_TRANSPORT_URL=tcp://10.0.0.11:8899 \
  pentairservice-dev cargo run
```

Turns on real bus transmits for `POST /command`; keep dry-run for routine CI and local practice.

### HTTP API summary

| Method | Path | Example | Explanation |
|--------|------|---------|-------------|
| `GET` | `/health` | `curl -s http://127.0.0.1:28471/health` | Process liveness; `{"status":"ok"}`. |
| `GET` | `/status` | `curl -s http://127.0.0.1:28471/status` | Latest decoded status snapshot + counters. |
| `GET` | `/frames?limit=` | `curl -s 'http://127.0.0.1:28471/frames?limit=5'` | Recent frames, newest first. |
| `POST` | `/command` | see examples above | Typed CircuitChange / HeatChange or raw hex via the write gate. |

**Status JSON fields** (selected):

- Shared: `raw` (hex), `protocol`, `destination`, `source`, `command`, `length`
- SystemStatus (`0x02`): `hours`, `minutes`, `circuits`, `circuitStatus` (`filterPump`, `cleanerPump`, `waterFeature`, `spaLight`, `poolLight` as `"on"`/`"off"`), `waterTemp`, `heaterTemp`, `airTemp`
- TempStatus (`0x08`): `water`, `air`, `waterSet`, `spaSet`, `info`

---

## 3. Low-level operation & operational details

### Connection ownership and reconnect

One long-lived task owns the transport: connect once, stream bytes into the framer, and on failure reconnect with exponential backoff and jitter. The HTTP API keeps serving while reconnect runs; you do not restart the process for a brief link drop.

Watch structured logs for connect / session-end / reconnect messages during soaks.

### Write mutex (HTTP 409)

Only one write may be in flight. A second concurrent `POST /command` is **rejected** with HTTP **409** (`busy`), not queued.

### Journal format and retention

Format: comment header, then TSV lines:

```text
ts_ms<TAB>checksum_ok<TAB>kind<TAB>hex
```

`hex` is lowercase framed bytes. Soft size retention (`journal_max_bytes`) trims older complete lines when over the cap; `journal_max_age_secs` is accepted but age vacuum is a stub.

### Coverage and soak tests

```bash
docker compose run --rm pentairservice-dev cargo llvm-cov --lib --fail-under-lines 95
```

Fails the run if library line coverage drops below 95%.

```bash
docker compose run --rm pentairservice-dev cargo test short_automated_soak -- --nocapture
```

Runs the short automated reconnect soak (local TCP flap + fixture replay) in seconds.

### Longer live soak (optional)

Against a real EW11, leave the process running and confirm reconnect after a brief network interruption:

```bash
docker compose run --rm --service-ports \
  -e PENTAIR_TRANSPORT_URL=tcp://192.168.1.50:8899 \
  pentairservice-dev cargo run
```

Use the device’s LAN IP from the container network. On Linux hosts, `--network=host` avoids NAT surprises:

```bash
docker compose run --rm --network=host \
  -e PENTAIR_BIND_ADDR=0.0.0.0:28471 \
  -e PENTAIR_TRANSPORT_URL=tcp://192.168.1.50:8899 \
  pentairservice-dev cargo run
```

### Device / systemd notes

For production serial on a Pi (udev symlink, `dialout`, host systemd vs Docker `--device=`), see **[`docs/rpi-ops.md`](docs/rpi-ops.md)**. Optional dual-path lab worksheet: **[`docs/lab-NOTES.md`](docs/lab-NOTES.md)**.

### Layout (quick map)

| Path | Role |
|------|------|
| `src/lib.rs` / `src/main.rs` | Application entry and HTTP router |
| `src/config.rs` | Settings (file + env) |
| `src/framer.rs` | Streaming A5 / IntelliChlor framing |
| `src/transport/` | TCP, serial, replay + reconnect |
| `src/api/` | `/health`, `/status`, `/frames`, `/command` |
| `src/journal.rs` | Append-only frame journal |
| `src/write.rs` / `src/commands.rs` | Write gate and CircuitChange / HeatChange craft |
| `fixtures/` | Hex samples for replay and tests |
| `docs/rpi-ops.md` | Pi udev / dialout / systemd runbook |
| `docs/lab-NOTES.md` | Optional dual-mode lab notes template |

---

## 4. EW11 vs local RS-485

Two common physical paths reach the same panel conversation. Configure **one** at a time.

| | **EW11 (TCP)** | **Local RS-485 (serial)** |
|--|----------------|---------------------------|
| **What it is** | Network serial bridge (e.g. Elfin EW11) on the LAN | USB adapter or HAT wired to the bus |
| **When to use** | Service can sit anywhere that routes to the EW11; no tty on the host | Service runs next to the adapter (typical Pi deploy) |
| **Example URL** | `tcp://10.0.0.11:8899` | `/dev/ttyPentair` or `serial:/dev/ttyPentair?baud=9600` |
| **Ops focus** | Reachable IP/port; firewall; one TCP client | Stable udev name, `dialout`, systemd device access |

**One-owner rule:** at most one process may actively open a given EW11 endpoint or serial device. Do not run EW11 TCP and RS-485 as two active masters on the same bus.

Pi-oriented install steps (udev → `/dev/ttyPentair`, systemd unit, Docker `--device=` tradeoffs): **[`docs/rpi-ops.md`](docs/rpi-ops.md)**.
