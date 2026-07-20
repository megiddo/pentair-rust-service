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

# Run the service (health API on published port 8080)
docker compose run --rm --service-ports pentairservice-dev cargo run
```

Then from the host:

```bash
curl -s http://127.0.0.1:8080/health
# {"status":"ok"}
```

Equivalent without compose:

```bash
docker build -t pentairservice-dev .
docker run --rm -it -v "$PWD":/workspace -w /workspace -p 8080:8080 pentairservice-dev cargo test
```

## Configuration

| Source | Keys |
|--------|------|
| Env | `PENTAIR_BIND_ADDR`, `PENTAIR_TRANSPORT_URL`, `PENTAIR_LOG_LEVEL`, `PENTAIR_CONFIG` |
| TOML | `bind_addr`, `transport_url`, `log_level` (see `config.example.toml`) |

Defaults: bind `0.0.0.0:8080`, no transport. With an empty/unset `transport_url`, the service **idles** and serves `GET /health` without opening TCP/serial.

### Transport URL schemes (B2)

| URL | Backend |
|-----|---------|
| `tcp://host:port` | Async TCP client (EW11 / RS485↔TCP bridge) |
| `/dev/ttyUSB0` or `serial:/dev/ttyUSB0?baud=9600` | Async serial |
| `replay:fixtures/status_temps.hex` | Recorded hex replay (tests / lab without live bus) |

A **single tokio task (Actor)** owns the connection: connect once, stream bytes into the framer buffer, reconnect with exponential backoff + jitter on failure. The connection is **never** torn down per frame (unlike PHP `PentairComFacade`).

## Recorded replay (automated soak)

Unit tests include an accelerated reconnect soak (local TCP flap + fixture replay) that finishes in seconds. Run them via:

```bash
docker compose run --rm pentairservice-dev cargo test short_automated_soak -- --nocapture
```

One-shot replay while serving health:

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
4. Serial lab: pass the device into the container, e.g. `--device=/dev/ttyUSB0` and `PENTAIR_TRANSPORT_URL=/dev/ttyUSB0`.

## Layout

| Path | Role | Pattern |
|------|------|---------|
| `src/lib.rs` | Application façade (`run`, `build_router`) | Facade |
| `src/main.rs` | Thin binary entry | Facade |
| `src/config.rs` | Settings load (file + env) | Builder / Configuration Object |
| `src/framer.rs` | Streaming A5 + IntelliChlor sync/seek | Parser / State Machine |
| `src/transport/` | TCP / serial / replay + reconnect Actor | Strategy + Actor |
| `src/logging.rs` | Tracing subscriber init | Facade |
| `src/api/` | Local HTTP surface | Facade |
| `src/api/health.rs` | `GET /health` | Facade (API surface) |
| `fixtures/` | Hex samples for framer + replay tests | — |

## Milestone

Track B **B2** persistent transport + reconnect (on B1 framer). Status decode API is B3.
