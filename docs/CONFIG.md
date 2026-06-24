# Pancetta Configuration Reference

Pancetta's configuration lives in a single TOML file at
`~/.pancetta/pancetta.toml`. The file is loaded at startup and watched for
changes; most keys hot-reload without a restart.

This document covers the keys you'll actually touch. The full schema —
including hundreds of advanced knobs (DSP filter coefficients, contest
categories, multi-antenna routing, etc.) — lives in
[`pancetta-config/defaults.toml`](../pancetta-config/defaults.toml). Any
key you don't set in your user config inherits the value from there.

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

That's enough to run the autonomous operator. Everything else has a
sensible default in `defaults.toml`.

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

## `[autonomous_operator]` — the brain

```toml
[autonomous_operator]
enabled = false                 # Master enable. Off by default; opt-in to TX.
mode = "hybrid"                 # "hunt", "cq", or "hybrid"
slot_parity_preference = "auto" # "even", "odd", or "auto"
max_concurrent_qsos = 4         # Cap on simultaneous in-flight QSOs
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | When false, Pancetta runs decode-only — no TX. |
| `mode` | enum | `"hybrid"` | `hunt` = chase rare CQs only. `cq` = call CQ and answer callers. `hybrid` = hunt when a rare target is on; CQ otherwise. |
| `slot_parity_preference` | enum | `"auto"` | FT8 alternates even/odd 15s slots; `auto` picks the parity with less local QRM. |
| `max_concurrent_qsos` | integer | `4` | The `SmartFrequencyAllocator` caps simultaneous TX streams here. |

### `[priority_weights]` — what to prioritize

Each decoded CQ is scored against these criteria, weighted, and sorted.
Tuning these is how you specialize Pancetta for DX chasing vs. grid
hunting vs. contesting.

```toml
[priority_weights]
needed_dxcc = 0.35
needed_grid = 0.20
pota_sota   = 0.15
rarity      = 0.10
snr         = 0.05
```

Weights need not sum to 1.0; they're combined linearly with the
duplicate-and-failure penalty applied on top. Set any weight to `0.0`
to disable that signal entirely.

### `[duplicate_checking]` — don't call the same station twice

```toml
[duplicate_checking]
enabled = true
time_window_hours = 24
check_frequency = false
```

`check_frequency = true` allows the same station to be called again on
a different band. The default (`false`) is one-and-done per UTC day.
The duplicate check is what makes Space-to-call return `Call X failed:
duplicate QSO ...` for stations you've already worked.

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
enabled  = false
username = ""
password = ""        # plaintext on disk

[network.psk_reporter]
enabled        = true   # Local-only spotter; no credentials
report_decodes = true
```

`pskreporter` doesn't require credentials and is the only network
integration enabled by default — your local copy contributes spots
back to the global PSKReporter database, which makes you reciprocally
visible for spot lookups.

LoTW credential handling refuses to send the username/password unless
`base_url` is `https://`. This matches the real LoTW endpoint
(`https://lotw.arrl.org`) and protects you from a typo or hostile
config override that would otherwise transmit credentials in cleartext.

### Per-QSO log upload — ClubLog and QRZ Logbook

When a QSO completes, pancetta can upload that single QSO (as one ADIF
record) straight to your online logbooks. Both integrations are
**opt-in and default `enabled = false`**. They run best-effort and never
block or fail the QSO pipeline; results are logged under the
`qso.upload` target. **Credentials stay local** — they are read from
this file and never logged. Keep the file readable only by you:
`chmod 600 ~/.pancetta/pancetta.toml`.

> **LoTW auto-upload is deferred.** Unlike ClubLog/QRZ, LoTW requires
> each record to be digitally signed with your TQSL certificate, not a
> raw ADIF POST, so per-QSO LoTW upload is not yet wired. Point WSJT-X /
> TQSL at `~/.pancetta/qsos.adi` for LoTW in the meantime.

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

---

## `[ui]` — TUI behaviour

```toml
[ui]
theme       = "dark"   # "dark" or "light"
time_format = "utc"    # "utc" or "local" — UTC strongly recommended for FT8
target_fps  = 30       # Refresh rate; lower this on slow SSH links
```

The TUI also reads its layout, key bindings, and color scheme details
from `[ui]`. The full set is in `defaults.toml`; the keys above are the
ones with practical effect.

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

- The annotated source of truth is
  [`pancetta-config/defaults.toml`](../pancetta-config/defaults.toml).
- Rust types and validation logic live under `pancetta-config/src/`.
- See [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) for how config flows
  through the coordinator.
