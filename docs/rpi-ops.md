# Raspberry Pi / ops runbook (EW11 TCP vs RS-485)

Operator guide for deploying **`pentairservice`** as the production bus owner on a Raspberry Pi (or similar Debian host). Switch between **Elfin EW11 (TCP)** and **direct USB-RS485 / HAT (serial)** with config alone — no code change.

**Related:** Track E design [`07-transport-di-ew11-rs485.md`](../../agents/plans/07-transport-di-ew11-rs485.md) (parent repo), Debian Docker validation [`06-pentairservice-docker-dev.md`](../../agents/plans/06-pentairservice-docker-dev.md), service [`README.md`](../README.md).

---

## Canonical vs live hardware

| Role | Environment | Notes |
|------|-------------|--------|
| **Build / test / coverage** | **Debian Docker** (`docker compose` in this tree) | Authoritative DoD. No real tty required. |
| **Production / lab I/O** | Host on the Pi (or Docker with device pass-through) | Live `tcp://` or `/dev/tty*` only here. |

Host `cargo` on macOS is non-authoritative. Live serial is **lab/Pi only**; do not treat “passes on Darwin” as deploy proof.

---

## Transport URL forms (canonical)

Same vocabulary as the service `transport_url` / `PENTAIR_TRANSPORT_URL`:

| URL | Mode |
|-----|------|
| `tcp://10.0.0.11:8899` | EW11 / transparent TCP serial bridge |
| `/dev/ttyPentair` | Serial device, default **9600 8N1** |
| `serial:/dev/ttyPentair?baud=9600` | Serial with explicit baud (parity/stop fixed 8N1) |
| `replay:fixtures/status_temps.hex` | Lab / CI only (no live bus) |

**Defaults:** baud **9600**, data **8**, parity **none**, stop **1**, no hardware flow control. Confirm per adapter if TX fails.

**Ownership:** at most **one** process opens a given EW11 endpoint or serial device. Stop snoop / PHP-native before starting the service on the same URL.

---

## Example configs

### EW11 (TCP)

`config.toml`:

```toml
bind_addr = "0.0.0.0:8080"
transport_url = "tcp://10.0.0.11:8899"
log_level = "info"
# writes_enabled = false   # keep false until lab TX is intentional
```

Env equivalent:

```bash
export PENTAIR_TRANSPORT_URL=tcp://10.0.0.11:8899
export PENTAIR_BIND_ADDR=0.0.0.0:8080
# optional: PENTAIR_CONFIG=/etc/pentairservice/config.toml
```

### Direct RS-485 (`/dev/ttyPentair`)

`config.toml`:

```toml
bind_addr = "0.0.0.0:8080"
transport_url = "/dev/ttyPentair"
# or: transport_url = "serial:/dev/ttyPentair?baud=9600"
log_level = "info"
```

Env equivalent:

```bash
export PENTAIR_TRANSPORT_URL=/dev/ttyPentair
# or: export PENTAIR_TRANSPORT_URL='serial:/dev/ttyPentair?baud=9600'
```

Copy from [`config.example.toml`](../config.example.toml) and uncomment the mode you need.

---

## Stable device path (udev → `/dev/ttyPentair`)

USB adapters often renumber (`ttyUSB0` → `ttyUSB1`). Prefer a **udev symlink** by vendor/product (or serial):

1. Plug in the adapter and identify it:

```bash
lsusb
ls -l /dev/ttyUSB* /dev/ttyACM* 2>/dev/null
udevadm info -a -n /dev/ttyUSB0 | head -80
```

2. Install a rule (replace `idVendor` / `idProduct` from your adapter):

```text
# /etc/udev/rules.d/99-pentair-rs485.rules
# Example FTDI — replace ATTRS from `udevadm info`
SUBSYSTEM=="tty", ATTRS{idVendor}=="0403", ATTRS{idProduct}=="6001", SYMLINK+="ttyPentair", GROUP="dialout", MODE="0660"
```

3. Reload and verify:

```bash
sudo udevadm control --reload-rules
sudo udevadm trigger
ls -l /dev/ttyPentair
```

Config then uses `transport_url = "/dev/ttyPentair"` (or `serial:/dev/ttyPentair?baud=9600`).

**HAT / onboard UART:** common paths are `/dev/ttyAMA0` or `/dev/serial0`. Disable login getty on that UART and enable UART in `raspi-config` / device tree as required. You can still symlink to `ttyPentair` if desired.

---

## Permissions (`dialout`)

On Debian / Raspberry Pi OS, serial devices are typically group **`dialout`**:

```bash
sudo usermod -aG dialout pentair   # or the service user
# log out/in (or reboot) so the new group applies for interactive shells
```

udev `GROUP="dialout"` + `MODE="0660"` (as above) matches this model.

---

## Host systemd (preferred for serial)

For **production serial**, run the binary on the **host** with systemd. Fewer device-mapping footguns than Docker `--device=`.

Example unit (`/etc/systemd/system/pentairservice.service`):

```ini
[Unit]
Description=Pentair bus owner (pentairservice)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=pentair
Group=pentair
SupplementaryGroups=dialout
WorkingDirectory=/opt/pentairservice
ExecStart=/opt/pentairservice/pentairservice
Environment=PENTAIR_CONFIG=/etc/pentairservice/config.toml
# Or inline:
# Environment=PENTAIR_TRANSPORT_URL=/dev/ttyPentair
Restart=on-failure
RestartSec=5

# Optional hardening when using a named device:
# DeviceAllow=/dev/ttyPentair rw

[Install]
WantedBy=multi-user.target
```

EW11 TCP mode can use the same unit with `transport_url = "tcp://…"`.

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now pentairservice
curl -s http://127.0.0.1:8080/health
```

---

## Docker `--device=` (lab / optional)

Debian Docker remains the **build/test** canonical environment. For a **live serial soak inside a container** on the Pi:

```bash
docker compose run --rm --service-ports \
  --device=/dev/ttyPentair \
  -e PENTAIR_TRANSPORT_URL=/dev/ttyPentair \
  pentairservice-dev cargo run
```

### Tradeoffs: systemd host vs Docker device

| | Host systemd | Docker `--device=` |
|--|--------------|-------------------|
| Serial production | **Preferred** — direct `/dev` access, `SupplementaryGroups=dialout` | Works but needs device + group/cgroup mapping; path must exist at start |
| EW11 TCP | Fine either way | Often easier (no tty; publish `8080`, reach LAN IP) |
| Rebuild / CI | N/A | **Canonical** for `cargo test` / `llvm-cov` |
| Symlink renames | udev updates host path immediately | May need container restart if the node appears after start |

**Recommendation:** develop and gate coverage in Debian Docker; deploy serial production under **host systemd**; use Docker `--device=` for short lab soaks if convenient.

---

## EW11 and RS-485 on the same Pi

Pick **one** physical path to the panel bus:

- Service with `tcp://ew11-host:8899`, **or**
- Service with `/dev/ttyPentair`

Do not run both as active masters on the same A5 conversation. Two USB adapters on the same RS-485 wiring is also invalid unless one is a true passive RX-only tap (advanced; out of default ops).

---

## Quick validation checklist

1. `curl -s http://127.0.0.1:8080/health` → `{"status":"ok"}`
2. With transport configured: logs show bus connect; `GET /status` eventually shows non-null temps/status when the panel is talking.
3. Serial: `ls -l /dev/ttyPentair` and confirm the service user can open it (`groups` includes `dialout`).
4. Dual-mode lab proof (same status via tcp **and** serial) is Track **E5** — record notes separately; do not claim RS-485 first-class until E5 is done.

Optional lab template: [`lab-e5-NOTES.md`](lab-e5-NOTES.md).
