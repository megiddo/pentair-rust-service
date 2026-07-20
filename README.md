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

Defaults: bind `0.0.0.0:8080`, no transport. With an empty/unset `transport_url`, the service **idles** and serves `GET /health` without opening TCP/serial (B0). Transport connection is deferred to B2.

`RUST_LOG` overrides the tracing filter when set.

## Layout

| Path | Role | Pattern |
|------|------|---------|
| `src/lib.rs` | Application façade (`run`, `build_router`) | Facade |
| `src/main.rs` | Thin binary entry | Facade |
| `src/config.rs` | Settings load (file + env) | Builder / Configuration Object |
| `src/logging.rs` | Tracing subscriber init | Facade |
| `src/api/` | Local HTTP surface | Facade |
| `src/api/health.rs` | `GET /health` | Facade (API surface) |

## Milestone

Track B **B0** bootstrap only. Framing, transport reconnect, and status decode land in later milestones.
