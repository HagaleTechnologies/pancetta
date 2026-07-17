# Pancetta Configuration Reference

Pancetta's configuration lives in a single TOML file at
`~/.pancetta/pancetta.toml`. The file is loaded at startup and watched for
changes; most keys hot-reload without a restart.

This document covers the keys you'll actually touch, with explanations.
The complete schema — every section, every key, every default — is
[`pancetta-config/defaults.toml`](../pancetta-config/defaults.toml),
which is **generated from the code's `Config::default()`** and
drift-tested in CI, so it can't lie. Any key you don't set in your user
config keeps its default value from there.

> **Security:** the config file is plaintext on disk. If you set any
> integration password (LoTW, eQSL, Clublog, QRZ), `chmod 600` the file
> and don't commit it. See [`SECURITY.md`](../SECURITY.md) for the full
> threat model.

---

## Minimum viable config

The fields you must set for Pancetta to do anything useful:

```toml
[station]
callsign = "YOURCALL"        # Your FCC/ITU-issued callsign
grid_square = "FN42"         # 4-character Maidenhead grid

[audio]
input_device = "USB Audio CODEC"
output_device = "USB Audio CODEC"

[rig.interface]
enabled = true
port = "/dev/tty.usbserial-A1"
baud_rate = 38400

[rig]
model = "FTdx10"
```

That's enough to decode and work stations manually. Hands-off operation
additionally needs `[autonomous] enabled = true` (see below).

---

## `[station]` — your identity

| Key | Type | Default | Notes |
|---|---|---|---|
| `callsign` | string | `"N0CALL"` | The license under which Pancetta will TX. **Required.** |
| `grid_square` | string | `"AA00aa"` | Your Maidenhead grid (4 or 6 chars). Used in `CQ` and grid-report exchanges. |
| `power_watts` | integer | `100` | Reported in spots; not used for actual rig power level. |
| `qth` | string | `"Unknown"` | Free-text location label, surfaced in the TUI. |
| `dxcc_entity` | integer | `291` | DXCC entity number (e.g. 291 = United States). |
| `itu_zone` | integer | `8` | Used by some contest exchanges. |
| `cq_zone` | integer | `5` | Same. |
| `operator_name` | string | `""` | Your name (optional, for log export). |
| `tx_late_max_ms` | integer | `8000` | Maximum latency past the slot boundary at which the TX scheduler will still attempt a late-start TX via audio cursor skip-ahead. Beyond this, defers to the next opposite-parity slot (30s later). 8s leaves ~5s of audio on the air, which is enough for the receiver to lock onto the middle and end Costas sync arrays. |
| `tx_self_parity` | string | `"auto"` | When calling CQ (no DX heard), pick TX slot parity by this rule. `"auto"` picks whichever next slot is closer; `"even"` / `"odd"` lock to the named parity. |
| `ptt_lead_ms` | integer | `80` | PTT engage lead time before the slot boundary. Drop to 50ms for fast solid-state keying; bump up to 150–200ms for slow mechanical relays. |

`station.antennas` is an array-of-tables; you can describe each antenna
on the station and Pancetta will surface them in the TUI.

```toml
[[station.antennas]]
id            = "20m_yagi"
name          = "20m 5-element Yagi"
antenna_type  = "yagi"
bands         = ["20m"]
gain_dbi      = 9.5
pattern       = "directional"
height_meters = 18.0
active        = true
```

---

## `[audio]` — the link to the radio

| Key | Type | Default | Notes |
|---|---|---|---|
| `input_device` | string | `"default"` | Exact cpal device name. Run `pancetta test-audio --list` to enumerate. |
| `output_device` | string | `"default"` | Same. Most ham USB CODECs present input and output under the same name. |
| `sample_rate` | integer | `48000` | Pancetta resamples internally to 12 kHz; 48 kHz is the recommended capture rate. |
| `buffer_size` | integer | `512` | cpal frame size. 512 trades latency for stability. |
| `input_channels` | integer | `2` | Most CODECs are 2-channel; Pancetta downmixes to mono. |
| `output_channels` | integer | `2` | TX path will write mono into both channels. |

The `[audio.processing]` block controls the DSP chain (bandpass filter,
compression, AGC). The defaults are tuned for FT8 and most users won't
need to touch them; see `defaults.toml` for the full key list.

`[audio.levels].input_gain_db` applies a fixed gain at the resampler
input. Negative values attenuate; useful when a hot CODEC saturates the
ADC even with the rig's audio output turned all the way down.

---

## `[rig]` — CAT control

```toml
[rig]
model = "FTdx10"            # Display name; Pancetta maps to a hamlib model ID

[rig.interface]
enabled = true              # false → mock rig, no real PTT or freq readback
port = "/dev/tty.usbserial-A1"
baud_rate = 38400
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `model` | string | `""` | Set to a name Pancetta knows (`FTdx10`, `IC-7300`, etc.) so it can resolve the hamlib model number. |
| `interface.enabled` | bool | `false` | Master switch. When false, all CAT calls go to a mock rig and PTT is a no-op. |
| `interface.port` | string | `""` | Serial device path. `/dev/tty.*` (macOS), `/dev/ttyUSB*` (Linux), `COM<N>` (Windows). `host:port` is also accepted (rigctld network rig syntax). |
| `interface.baud_rate` | integer | `38400` | Must match the rig's CAT port setting. |

> **Network mode:** setting environment variable `RIGCTLD_HOST` to a
> non-loopback address tells Pancetta to talk to a remote `rigctld`.
> The TCP port is unauthenticated; if you do this on anything other
> than a trusted LAN, anyone who can reach the port can drive your rig.

---

## `[autonomous]` — the brain

```toml
[autonomous]
enabled = false            # Master enable. Off by default; opt-in to TX.
slot_parity = "auto"       # "even", "odd", or "auto"
cq_after_idle_cycles = 10  # Idle TX cycles before calling CQ (~150 s at 10)
max_concurrent_qsos = 1    # Cap on simultaneous in-flight QSOs
tx_offset_hz = 1500.0      # Preferred TX audio offset (100–3000 Hz)
min_dx_score = 0.3         # Minimum DX score (0–1) to answer a CQ
min_multi_slot_score = 0.7 # Higher bar (0–1) for opening a 2nd+ concurrent QSO
cq_direction = ""          # Directed CQ text ("DX", "NA", …); empty = general CQ
dry_run = false            # Log autonomous TX decisions without keying the rig
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | When false, the autonomous engine never initiates TX. |
| `slot_parity` | enum | `"auto"` | FT8 alternates even/odd 15 s slots; `auto` picks per conditions. |
| `cq_after_idle_cycles` | integer | `10` | TX cycles with nothing to do before calling CQ. Must be ≥ 1. |
| `max_concurrent_qsos` | integer | `1` | Simultaneous in-flight QSOs (multi-stream TX). Must be ≥ 1. |
| `tx_offset_hz` | float | `1500.0` | Validated to 100–3000 Hz. |
| `min_dx_score` | float | `0.3` | 0.0–1.0. Decoded CQs scoring below this are not answered. |
| `min_multi_slot_score` | float | `0.7` | 0.0–1.0. Applies only to second-and-later concurrent QSOs. |
| `cq_direction` | string | `""` | Appended to CQ (`CQ DX <call> <grid>`). |
| `dry_run` | bool | `false` | Autonomous TransmitRequests are logged, not sent. Manual TX unaffected. |

> **There is no `mode` key.** Earlier revisions of this document described
> `[autonomous_operator]` with `mode = "hunt" / "cq" / "hybrid"`,
> `slot_parity_preference`, and a top-level `[priority_weights]`. Those keys
> never existed in the code and were silently ignored. The real behavior is
> always both: answer scored CQs above `min_dx_score`, and fall back to
> calling CQ after `cq_after_idle_cycles` idle cycles. Startup now warns
> about unknown top-level sections, so a stale config will tell you.

### `[autonomous.priorities]` — what to prioritize

Each decoded CQ is scored against these weights (each validated to
−1.0…1.0; positive attracts, negative penalizes; the final score is
clamped to 0.0–1.0 and compared against `min_dx_score`).

```toml
[autonomous.priorities]
needed_dxcc            = 0.35
needed_grid            = 0.20
pota_sota              = 0.15
rarity                 = 0.10
signal_strength        = 0.05   # SNR weight — stronger = more likely to complete
duplicate_penalty      = -0.40  # already worked on this band
recent_failure_penalty = -0.15  # recently called, QSO didn't complete
atno_bonus             = 0.15   # extra premium on top of needed_dxcc for an
                                # all-time-new-one; inert unless cqdx.io flags it
```

### Sub-tables you'll rarely touch

- `[autonomous.frequency]` — the `SmartFrequencyAllocator` knobs (center
  bias, DX-proximity window, own-QSO separation, neighbor guard).
- `[autonomous.listen_cycle]` — adaptive forced-listen-slot cadence for
  collision detection.
- `[autonomous.band_hopping]` — off by default; ordered band list with a
  low-activity hop threshold.

Defaults for all three live in `Config::default()`; see
`pancetta-config/src/autonomous.rs` for every field with doc comments.

### `[duplicate_checking]` — don't call the same station twice

```toml
[duplicate_checking]
enabled = true
time_window_hours = 24
check_frequency = true
```

The duplicate check is what makes Space-to-call return `Call X failed:
duplicate QSO ...` for stations you've already worked. With the default
`check_frequency = true`, a prior QSO only blocks a re-call when it was
within 50 Hz of the same RF frequency — so the same station on a
different band can be worked again. Set `check_frequency = false` for
strict one-QSO-per-callsign inside the window, or `enabled = false` to
turn duplicate checking off entirely.

`time_window_hours` is a rolling window from each prior QSO's start
time (not a UTC-day boundary): a QSO started 23 hours ago still blocks;
one started 25 hours ago does not.

Note: this 50 Hz frequency scoping applies to the in-memory recent-QSO
check. The persistent-database fallback (used after a restart, once a
completed QSO has aged out of memory) always frequency-scopes at a
wider ±100 Hz regardless of `check_frequency` — a corner case, not
something an operator normally needs to think about, but noted here
for completeness.

---

## `[network]` — external services

QRZ.com, LoTW, eQSL, Clublog, PSKReporter all live under `[network]`.
Each has an `enabled` flag and a credentials block.

> **All passwords are stored in plaintext on disk.** If you don't need
> the integration, leave `enabled = false`. The fields used to be named
> `password_encrypted`; despite the name no encryption was ever
> implemented, so they have been renamed to `password` to be honest
> about what's on disk.

```toml
[network.qrz]
enabled  = false
username = ""
password = ""        # plaintext on disk

[network.lotw]
enabled          = false
tqsl_path        = ""   # path to your installed tqsl binary
station_location = ""   # must match a "Station Location" name in TQSL

[network.psk_reporter]
enabled        = true   # Local-only spotter; no credentials
report_decodes = true
```

`pskreporter` doesn't require credentials and is the only network
integration enabled by default — your local copy contributes spots
back to the global PSKReporter database, which makes you reciprocally
visible for spot lookups.

LoTW has no username/password/`base_url` fields — unlike the other
integrations, pancetta never talks to LoTW's servers directly. It shells
out to your locally-installed `tqsl` binary, which signs the QSO record
with your TQSL certificate and handles the actual upload itself.

### Per-QSO log upload — ClubLog, QRZ Logbook, and LoTW

When a QSO completes, pancetta can upload that single QSO (as one ADIF
record) straight to your online logbooks. Both integrations are
**opt-in and default `enabled = false`**. They run best-effort and never
block or fail the QSO pipeline; results are logged under the
`qso.upload` target. **Credentials stay local** — they are read from
this file and never logged. Keep the file readable only by you:
`chmod 600 ~/.pancetta/pancetta.toml`.

> **LoTW auto-upload works differently from ClubLog/QRZ.** Each record
> must be digitally signed with your TQSL certificate, not a raw ADIF
> POST — so instead of an HTTP client, pancetta shells out to your
> locally-installed `tqsl` binary per completed QSO (`[network.lotw]`
> above). If `tqsl` is missing or fails, the upload is skipped
> best-effort, same as ClubLog/QRZ. `~/.pancetta/qsos.adi` remains
> available if you'd rather point WSJT-X / TQSL at it manually.

```toml
[network.clublog]
enabled  = false
email    = ""        # your ClubLog account email (NOT a callsign), plaintext on disk
password = ""        # ClubLog password (an Application Password is recommended), plaintext
callsign = ""        # station call the log uploads into; empty = use the QSO's own call
api_key  = ""        # ClubLog application API key

[network.qrz_logbook]
enabled = false
api_key = ""         # per-logbook API access key, plaintext on disk

[network.cqdx]
enabled = false      # also gates the spot-discovery integration; when true with a
                     # token set, each completed QSO is ALSO logged to your cqdx.io logbook
token   = ""         # cqdx.io Personal Access Token (pat_…), plaintext on disk
# base_url = "https://cqdx.io"   # optional; defaults to https://cqdx.io
```

| Key | Service | Notes |
|---|---|---|
| `clublog.enabled` | ClubLog | Master switch. When `true`, `email`, `password`, and `api_key` are all required (validation fails otherwise). |
| `clublog.email` | ClubLog | The email registered with your ClubLog account. |
| `clublog.password` | ClubLog | Account password. Plaintext on disk. |
| `clublog.callsign` | ClubLog | The station callsign the log is filed under. Leave empty to use each QSO's own callsign. |
| `clublog.api_key` | ClubLog | Application API key. |
| `qrz_logbook.enabled` | QRZ | Master switch. When `true`, `api_key` is required. |
| `qrz_logbook.api_key` | QRZ | Per-logbook API access key. |
| `cqdx.enabled` | cqdx.io | Master switch for the cqdx.io integration. When `true` **and** `cqdx.token` is non-empty, each completed QSO is uploaded to your cqdx.io logbook (in addition to the spot-discovery features the same flag enables). |
| `cqdx.token` | cqdx.io | Personal Access Token (`pat_…`). Plaintext on disk; never logged. |

**Getting the keys:**

- **ClubLog:** create a free account at <https://clublog.org>, then
  request an application API key on the ClubLog API page
  (<https://clublog.org/need_api.php>). The realtime upload POSTs to
  `https://clublog.org/realtime.php` with your email + password +
  callsign + API key. A duplicate QSO is accepted (HTTP 200) and is
  harmless.
- **QRZ Logbook:** open your logbook on <https://logbook.qrz.com>, go to
  the logbook's **Settings**, and copy the **API access key** (this is a
  per-logbook key, distinct from your QRZ XML subscription). Uploads POST
  to `https://logbook.qrz.com/api` with `ACTION=INSERT`. A QSO that QRZ
  already has is reported as a duplicate and skipped (non-fatal).
- **cqdx.io:** cqdx.io is the operator's own first-party logbook service.
  Create a Personal Access Token (`pat_…`) and set `cqdx.token`. Each
  completed QSO is POSTed as structured JSON to `POST /api/v1/qsos`
  (documented in `docs/cqdx-api-requirements.md`) with the dial+offset RF
  frequency and both grids. A QSO cqdx already has is reported as a
  duplicate and skipped (non-fatal). The same `[network.cqdx]` block also
  drives live spot discovery; enabling it turns on both.

### `[network.wsjtx_udp]` — GridTracker / JTAlert / logger interop

Pancetta can speak the WSJT-X-compatible UDP protocol that GridTracker,
JTAlert, and most loggers already consume — decodes, status, and logged
QSOs show up live with zero work on the companion app's side. It can
also *consume* the protocol's Reply/HaltTx requests, which is what makes
a GridTracker double-click call a station on pancetta's behalf. Default
**off** end to end: `enabled = false` binds no socket and emits nothing.
See `docs/GUIDE.md`'s "…use GridTracker with pancetta?" section for the
same-host and cross-machine setup recipes.

```toml
[network.wsjtx_udp]
enabled = false                     # master: emit nothing, bind nothing
destination = "127.0.0.1:2237"      # unicast host:port or multicast group:port
multicast_interface = ""            # LAN IP to send multicast from ("" = OS default)
multicast_ttl = 3                   # only used for multicast destinations
instance_id = "WSJT-X - pancetta"   # the protocol Id field
accept_udp_requests = false         # master switch for ALL inbound processing
allow_tx_initiation = false         # Reply(4) may start a QSO (arm-gated, see below)
allowed_request_hosts = []          # required for multicast inbound; unicast infers the peer
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. `false` ⇒ no socket is ever bound and nothing is emitted. |
| `destination` | string | `"127.0.0.1:2237"` | Where pancetta sends. A unicast `host:port` for a same-host or point-to-point companion, or a multicast group `address:port` (e.g. `"224.0.0.73:2237"`) to reach multiple companions / another machine. |
| `multicast_interface` | string | `""` | Local LAN interface IP to send multicast from. `""` = OS default (which is typically loopback-only — cross-host multicast needs this set to your real NIC's IP). Only used when `destination` is a multicast group. |
| `multicast_ttl` | integer (`u32`) | `3` | IP multicast TTL/hop-limit. Only used for multicast destinations; GridTracker uses `3` for its own sends, so `3` is a safe match. |
| `instance_id` | string | `"WSJT-X - pancetta"` | The protocol's `Id` field. Companion apps display and route by it; a second pancetta instance should get a distinct value. Must be non-empty when `enabled = true`. |
| `accept_udp_requests` | bool | `false` | Master switch for processing **any** inbound request (Reply, HaltTx, Replay, …). `false` ⇒ every inbound datagram is ignored (with an audit entry) — the same "Accept UDP requests" checkbox WSJT-X itself has. |
| `allow_tx_initiation` | bool | `false` | Whether an inbound `Reply(4)` (a GridTracker double-click on a CQ) may start a QSO. Also seeds the shared remote-TX arm gate — see `docs/DECISIONS/remote-operation.md`. Requires `enabled = true` to seed the arm; requires `accept_udp_requests = true` (in addition) for an inbound `Reply` to actually be dispatched. Fail-closed: `false` means neither effect occurs. |
| `allowed_request_hosts` | list of strings (IPs) | `[]` (empty) | For a **multicast** `destination`: the IP addresses allowed to send inbound requests. Empty ⇒ every request is refused (and logged), even with `accept_udp_requests = true` — you must opt a host in explicitly. For a **unicast** `destination`, the peer is inferred (only the configured destination host is accepted) and this list is unused. |

`destination` must parse as a valid `host:port` socket address, and
`instance_id` must be non-empty, whenever `enabled = true` — otherwise
config validation rejects the file.

---

## `[ui]` — TUI behaviour

```toml
[ui]
theme = "dark"   # "dark" or "light"
```

The remaining `[ui]` keys are in `defaults.toml`; `theme` above is the
one with practical effect (`pancetta-tui/src/app.rs` reads `config.ui.theme`
directly). Keybindings are not configurable — the full
map is [`docs/KEYBINDINGS.md`](KEYBINDINGS.md) (or `?` in the TUI). A
`[ui.keyboard]` block is present in `defaults.toml` (it mirrors a real
`KeyboardConfig` struct in the Rust schema) but nothing in the runtime
reads it; it's inert scaffolding, not a way to remap keys.

---

## Environment variables

A small set of environment variables override config keys:

| Variable | Effect |
|---|---|
| `PANCETTA_STUB_AUDIO=1` | Replace the cpal audio thread with a synthetic 1500 Hz tone generator. Useful for offline testing. |
| `PANCETTA_MOCK_RIG=1` | Force `[rig.interface].enabled = false` regardless of config. |
| `RIGCTLD_HOST` | Override the rigctld bind host. Default `127.0.0.1`. |
| `RIGCTLD_PORT` | Override the rigctld TCP port. Default `4532`. |
| `RUST_LOG` | Standard `tracing` filter. `info` is recommended; `debug` for triage. |

CLI flags (e.g. `--audio-device`, `--no-rig`, `--no-audio`) take final
priority over both config and environment.

---

## Hot reload

Pancetta watches `~/.pancetta/pancetta.toml` for changes. Most keys take
effect within a second of save. Exceptions:

- `[audio]` device names — require a TUI restart (cpal streams are bound
  at startup).
- `[rig.interface]` — same; rigctld is spawned once.
- `[station].callsign` — never hot-reloaded (active QSOs would mid-flight
  contradict their own metadata).

When a hot-reload succeeds you'll see a TUI status line like
`Config reloaded: 12 keys updated`. When it fails (typo, schema
violation), the previous config stays active and the parse error shows
in the TUI error log.

---

## Pancetta data files

All persistent state lives under `~/.pancetta/`.

### QSO log files

| File | Role | Recoverable? |
|---|---|---|
| `~/.pancetta/qsos.adi` | Durable, append-only ADIF source of truth. Point WSJT-X / N1MM / LoTW / eQSL at this file directly. | No — back this up. |
| `~/.pancetta/qso.db` | sqlx-backed query index. Rebuilt from ADIF on startup if missing or stale. | Yes — safe to delete; the next run will replay ADIF into a fresh index. |

**Migration note:** if you are upgrading from an earlier release that wrote only
`qso.db`, the first startup will automatically export every row from the old database
into a fresh `qsos.adi` before switching over. No manual action required.

---

## Where to look next

- The complete generated schema is
  [`pancetta-config/defaults.toml`](../pancetta-config/defaults.toml); the
  annotated source of truth is the Rust structs under
  `pancetta-config/src/`.
- Rust types and validation logic live under `pancetta-config/src/`.
- See [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) for how config flows
  through the coordinator.
