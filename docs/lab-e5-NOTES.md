# Lab dual-mode proof notes (Track E5 template)

**Status:** Optional template only. Leave E5 unchecked until real hardware trials are recorded here (or linked). Do **not** invent results.

Fill after running `pentairservice` (and/or snoop) against the **same** panel via EW11 TCP **and** via serial, one owner at a time.

---

## Install

| Field | Value |
|-------|--------|
| Host (Pi / lab) | |
| Date | |
| Operator | |
| `pentairservice` commit / version | |

---

## Trial A — EW11 TCP

| Field | Value |
|-------|--------|
| `transport_url` / env | e.g. `tcp://10.0.0.11:8899` |
| EW11 host:port | |
| Other processes holding this endpoint? | none / list |
| `GET /health` | |
| `GET /status` summary (temps / circuits of interest) | |
| Notes | |

---

## Trial B — Direct RS-485

| Field | Value |
|-------|--------|
| Device path | e.g. `/dev/ttyPentair` |
| Baud / framing | 9600 8N1 (or note override) |
| udev symlink used? | yes / no |
| `transport_url` / env | |
| Other processes holding this tty? | none / list |
| `GET /health` | |
| `GET /status` summary (same fields as Trial A) | |
| Notes | |

---

## Equivalence check

- [ ] Same status fields observed in both trials (document any deltas).
- [ ] Confirmed only **one** bus owner open during each trial.
- [ ] No secrets beyond LAN IPs already used in settings.

**Verdict / link to captures:** _(paste path or short soak note)_
