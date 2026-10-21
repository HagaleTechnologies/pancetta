# Pancetta Configuration Reference

Pancetta's configuration lives in a single TOML file at
`~/.pancetta/pancetta.toml`. The file is loaded at startup. It is also
watched for changes, but only the remote-access revocations listed under
[Hot reload](#hot-reload) apply to a running station; every other key
needs a restart.

This document covers the keys you'll actually touch, with explanations.
The complete schema — every section, every key, every default — is
[`pancetta-config/defaults.toml`](../pancetta-config/defaults.toml),
which is **generated from the code's `Config::default()`** and
drift-tested in CI, so it can't lie. Every table only needs the keys you
change; anything you leave out keeps its default. A misspelled key is
reported as a warning at startup and by `pancetta config --validate`.

> **Security:** the config file is plaintext on disk. If you set any
> integration password (LoTW, eQSL, Clublog, QRZ), `chmod 600` the file
> and don't commit it. See [`SECURITY.md`](../SECURITY.md) for the full
> threat model.

---

## Where pancetta looks for its config

With `--config <path>`, pancetta reads exactly that file and nothing else.
Without it, pancetta checks these directories in order, and in each one it
uses the first of `pancetta.toml`, `config.toml`, `pancetta.json`,
`config.json` that exists:

| # | Linux | macOS | Windows |
|---|---|---|---|
| 1 | current directory | current directory | current directory |
| 2 | `$XDG_CONFIG_HOME/pancetta` (only when set to something other than `~/.config`) | `~/Library/Application Support/pancetta` | `%APPDATA%\pancetta` |
| 3 | `/etc/pancetta` | `/etc/pancetta` | `%APPDATA%\Pancetta` |
| 4 | `/usr/local/etc/pancetta` | `/usr/local/etc/pancetta` | — |
| 5 | `~/.pancetta` | `~/.pancetta` | `%USERPROFILE%\.pancetta` |
| 6 | `~/.config/pancetta` | `~/.config/pancetta` | `%USERPROFILE%\.config\pancetta` |

Every file found is loaded, and later rows override earlier ones. The
override is per file, not per key: a later file's defaults also replace
most values an earlier file set, even in sections the later file never
mentions. A file that exists but fails to load (syntax error, wrong
value type, unreadable) stops pancetta with an error naming it; it is
never silently skipped. Keep
a single file at `~/.pancetta/pancetta.toml`, and run
`pancetta config --validate` to see which file(s) were read. A table only
needs the keys you change; every key you leave out keeps its default.

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
| `tx_late_max_ms` | integer | `3000` | Hard cap on elapsed slot time at keying: a frame that would key past it is dropped at the final pre-PTT gate, never keyed undecodably. The queue-time same-slot acceptance reserves the unconditional coalesce window + safety margin below the cap — at the default, a request within roughly the first 1.7s of the slot still keys the current slot; later arrivals defer 30s to the next opposite-parity slot. A multi-TX bundle the worker receives already assembled skips that coalesce wait, so it reserves only the 0.5s margin and still keys the current slot if it arrives within roughly the first 2.5s at the default. 3s keeps both the middle and end Costas sync arrays and 49 of 58 data symbols (147 coded bits vs the 91 information bits LDPC needs) on the air. |
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

### `[audio.recording]` — save decode windows as WAV (off by default)

Pancetta can save every live decode window it hands the decoder as a WAV
file, for offline decoder validation and corpus building. Recording is off
unless you turn it on:

```toml
[audio.recording]
auto_record = true
directory = "~/.pancetta/recordings"
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `auto_record` | bool | `false` | `true` writes one WAV per live decode window. |
| `directory` | string | `"~/Documents/Pancetta/Recordings"` | Where recordings go; created if missing. A leading `~` is your home directory (`~user` is not expanded). Empty means `~/.pancetta/recordings`. |

- Each file is a 12 kHz, mono, 16-bit WAV of one decode window (about
  300 KB for FT8), named `ft8_<UTC date>_<time>[_<band>].wav`. Windows of
  digital silence (rig off or unplugged) are skipped. A full day of FT8 is
  about 1.7 GB.
- The directory is capped at 10 GB of top-level `*.wav` files. Past that,
  the oldest recording (by filename) is deleted for each new one.
- With recording on, pancetta also saves the first 90 s of raw capture once
  per start as `raw_48khz_diagnostic.wav` (about 17 MB) in the same directory.
- `--replay` and `--no-audio` runs never record, whatever this says.
- The other keys in this table (`format`, `bit_depth`, `sample_rate`,
  `max_length_minutes`, `filename_pattern`, `compression_level`) are not
  read. Recordings always use the format above.
- Changes take effect after a restart.
- The startup log says which way it went: `WAV recording enabled: dir=…`
  or `WAV recording disabled -- set audio.recording.auto_record = true …`.

**Station decoder corpus.** The corpus offload job (magazzino
`clients/pancetta-wav-offload.sh`) and the decoder research tools
(`curate --source-dir ~/.pancetta/recordings`, see docs/RUNBOOK.md) read
`~/.pancetta/recordings`. A station that feeds them must set both keys as
shown above. `auto_record = true` on its own records into the default
`~/Documents/Pancetta/Recordings`, which nothing collects.

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

### `[rig.frequency.band_plan]` — amateur band privileges (TX gate + edge warning)

```toml
[rig.frequency.band_plan]
region = 2            # ITU/IARU region 1, 2 or 3 (2 = Americas; default 2)
edge_warnings = true  # master switch: the band-plan TX gate AND the TUI edge
                      # warning. false disables BOTH — the operator's opt-out
                      # (e.g. a CW/SSB station parked outside the FT8 tables).
                      # Default true.

[rig.frequency.band_plan.custom_bands]   # optional per-band override, keyed by Band::Display
"40m" = { name = "40m", ranges = [{ start = 7_000_000, end = 7_200_000, modes = [] }], default_mode = "FT8", band_type = "HF" }
# e.g. a national allocation narrower than the IARU region table
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `region` | integer | `2` | Selects the IARU region tables (1, 2 or 3; they diverge on 40/60/80 m, 2 m, 70 cm). An invalid number falls back to the global table — never panics. |
| `edge_warnings` | bool | `true` | Master switch for both the hard TX refusal and the TUI's manual-frequency-entry warning. `false` disables both: nothing is refused, nothing warns. |
| `custom_bands."<band>"` | table | — | Overrides the region table for that band (in-band iff the RF falls within ANY of the entry's `ranges`, OR across ranges). Applies to frequencies the band table resolves to that band — the escape hatch for a national allocation narrower than your region's default. The TUI's entry warning checks one frequency (OR across ranges); a transmission must fit entirely inside ONE of the entry's `ranges` (PAN-208), so a frame straddling the gap between two ranges is refused before PTT. |

Since PAN-118 the band plan is enforced, not just displayed: every path that
asserts PTT (scheduled FT8 transmissions and multi-TX bundle items, F4 tune,
manual Shift+T PTT, remote-originated frames) is **refused before keying**
whenever the absolute TX RF it would transmit on — the effective dial (split
TX dial when split is active, else the RX dial) plus the frame's audio
offset — falls outside the plan, with a `TX blocked (TX frequency … Hz is
outside the configured amateur band plan)` diagnostic. A multi-TX bundle
drops only its out-of-privilege items; in-privilege siblings still key. The
check is skipped while the dial is not yet known (mock rig / CAT-disabled
modes). Edits take effect on restart (the band plan is not among the
[live-applied settings](#hot-reload)).

---

## `[autonomous]` — the brain

```toml
[autonomous]
enabled = false            # Master enable. Off by default; opt-in to TX.
slot_parity = "auto"       # "even", "odd", or "auto"
cq_after_idle_cycles = 10  # Idle TX cycles before calling CQ (~150 s at 10)
cq_no_response_switch_after = 4 # Consecutive no-response self-CQs before switching TX frequency (Auto mode only)
qso_stall_switch_after = 4 # Consecutive stalled mid-QSO cycles before switching/reverting TX offset (Auto mode only)
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
| `cq_no_response_switch_after` | integer | `4` | Consecutive self-CQs with zero responses before switching TX frequency. Auto mode only (Hold mode tracks but never acts). Must be ≥ 1. |
| `qso_stall_switch_after` | integer | `4` | Consecutive stalled mid-QSO cycles before switching/reverting the QSO's TX offset. Auto mode only. Threaded into `pancetta_qso::QsoManagerConfig::TimeoutConfig::qso_stall_switch_after` by the coordinator. Must be ≥ 1. |
| `max_concurrent_qsos` | integer | `1` | Simultaneous active QSOs (multi-stream TX). Also caps manual opens (PAN-78): a new manual call, caller reply, Hound engage or CQ is refused while this many QSOs are active (Fox mode uses `fox_max_streams`). Must be ≥ 1. |
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

### `[database]` — QSO-log persistence

```toml
[database]
persist_qso_timeline = false
```

Off by default. When `true`, a completed or failed QSO's full
state-transition + message timeline (`QsoProgress.state_history` /
`.messages` — every rung the QSO climbed and every frame sent/received)
is persisted so it can be reconstructed offline, keyed by QSO id
(`QsoDatabase::get_qso_timeline`). Completed QSOs also get the real
timeline written into the existing `qsos.progress_data` column; failed
QSOs (which are never logged as contacts) get a row in a separate
`qso_events` table instead. Leave this off unless you're diagnosing why
QSOs are failing — the timeline is unbounded per-QSO JSON that
accumulates for the life of the SQLite index, so enabling it is a
deliberate storage/IO tradeoff, not a default an operator should pay
without asking for it. See `docs/observability-diagnostics-plan.md`
§"Persist the timeline" for the full design rationale.

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
[network.qrz_xml]
enabled  = false
username = ""
password = ""        # plaintext on disk

[network.lotw]
enabled          = false
tqsl_path        = ""   # path to your installed tqsl binary
station_location = ""   # must match a "Station Location" name in TQSL

[network.eqsl]
enabled      = false
username     = ""
password     = ""        # plaintext on disk
qth_nickname = ""         # only needed if your eQSL account has >1 location
```

`psk_reporter` doesn't require credentials, but — unlike its historical
reputation as "the default-on spotter" — it is **off by default**
(`PskReporterConfig::default().enabled == false`, verified by
`test_default_network_config`). Set `enabled = true` to have your local
copy contribute spots back to the global PSKReporter database, which
makes you reciprocally visible for spot lookups. As with every section,
a partial block is enough:
```toml
[network.psk_reporter]
enabled = true
```
See "Customizing PSKReporter" below if you want to see or change the
other defaults.

LoTW has no username/password/`base_url` fields — unlike the other
integrations, pancetta never talks to LoTW's servers directly. It shells
out to your locally-installed `tqsl` binary, which signs the QSO record
with your TQSL certificate and handles the actual upload itself.

### Per-QSO log upload — ClubLog, QRZ Logbook, LoTW, and eQSL

When a QSO completes, pancetta can upload that single QSO (as one ADIF
record) straight to your online logbooks. All four integrations are
**opt-in and default `enabled = false`**. They never block or fail the
QSO pipeline. ClubLog, QRZ Logbook, eQSL and cqdx.io uploads go through a
durable pending-upload queue (`~/.pancetta/upload_outbox.json`) with
retries and readiness-strip warnings — see
[Upload retries and the pending-upload queue](#upload-retries-and-the-pending-upload-queue)
below. Results are logged under the `qso.upload` target. **Credentials stay local** — they are read from
this file and never logged. Keep the file readable only by you:
`chmod 600 ~/.pancetta/pancetta.toml`.

> **LoTW auto-upload works differently from ClubLog/QRZ.** Each record
> must be digitally signed with your TQSL certificate, not a raw ADIF
> POST — so instead of an HTTP client, pancetta shells out to your
> locally-installed `tqsl` binary per completed QSO (`[network.lotw]`
> above). If `tqsl` is missing or fails, the upload is skipped after
> that single attempt: LoTW does not use the retry queue the HTTP
> services use. `~/.pancetta/qsos.adi` remains
> available if you'd rather point WSJT-X / TQSL at it manually.

```toml
[network.clublog]
enabled  = false
email    = ""        # your ClubLog account email (NOT a callsign), plaintext on disk
password = ""        # ClubLog password (an Application Password is recommended), plaintext
callsign = ""        # station call the log uploads into; empty = use the QSO's own call
# api_key: OMIT this line on an official release binary — it falls back to
# Pancetta's own baked-in ClubLog application key. Only set it if you
# registered your own ClubLog application, or you're building from source.

[network.qrz_logbook]
enabled = false
api_key = ""         # per-logbook API access key, plaintext on disk

[network.cqdx]
enabled            = false   # also gates the spot-discovery integration; when true with a
                              # token set, each completed QSO is ALSO logged to your cqdx.io logbook
token              = ""      # cqdx.io Personal Access Token (pat_…), plaintext on disk
base_url           = "https://cqdx.io"   # not actually optional to omit — see note below
poll_interval_secs = 30      # how often to poll for new priority spots
```

> As with every section, a partial table (just `enabled`/`token`) works
> fine. The values shown above ARE the real defaults, so copying this
> block verbatim and only changing `token` is equally safe if you'd
> rather see everything explicitly.

| Key | Service | Notes |
|---|---|---|
| `clublog.enabled` | ClubLog | Master switch. When `true`, `email`, `password`, and `api_key` are all required (validation fails otherwise). |
| `clublog.email` | ClubLog | The email registered with your ClubLog account. |
| `clublog.password` | ClubLog | Account password. Plaintext on disk. |
| `clublog.callsign` | ClubLog | The station callsign the log is filed under. Leave empty to use each QSO's own callsign. |
| `clublog.api_key` | ClubLog | Application API key. **Omit this key entirely** (don't set it to `""`) on an official release binary — it falls back to Pancetta's own application key, baked in at build time. Only set it if you registered your own ClubLog application, or you're building from source (which never has the baked-in key, see below). |
| `qrz_logbook.enabled` | QRZ | Master switch. When `true`, `api_key` is required. |
| `qrz_logbook.api_key` | QRZ | Per-logbook API access key. |
| `eqsl.enabled` | eQSL.cc | Master switch. When `true`, `username` and `password` are required. |
| `eqsl.username` | eQSL.cc | Account username. |
| `eqsl.password` | eQSL.cc | Account password. Plaintext on disk. |
| `eqsl.qth_nickname` | eQSL.cc | Optional "Profile" name — only needed if your account has more than one QTH location configured. |
| `cqdx.enabled` | cqdx.io | Master switch for the cqdx.io integration. When `true` **and** `cqdx.token` is non-empty, each completed QSO is uploaded to your cqdx.io logbook (in addition to the spot-discovery features the same flag enables). |
| `cqdx.token` | cqdx.io | Personal Access Token (`pat_…`). Plaintext on disk; never logged. |
| `cqdx.base_url` | cqdx.io | API base URL. Only change this for local development against a non-production cqdx.io instance. |
| `cqdx.poll_interval_secs` | cqdx.io | How often to poll for new priority spots. |

**Getting the keys:**

- **ClubLog:** create a free account at <https://clublog.org> and set
  `email`/`password` (an Application Password is recommended) —
  that's it for an official release binary. ClubLog issues its API key
  per *application*, not per operator, so Pancetta's own key is baked
  into official release binaries at build time
  (`pancetta-config/src/network.rs`'s `option_env!("CLUBLOG_API_KEY")`,
  set only by `release.yml`'s `CLUBLOG_API_KEY` repo secret) — leave
  `api_key` out of your config file entirely to use it. Only request
  your own key on the ClubLog API page
  (<https://clublog.org/need_api.php>) if you're building from source
  (source builds never have the baked-in key) or want your own
  application identity. The realtime upload POSTs to
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
- **eQSL.cc:** use your existing eQSL.cc account username and password —
  there's no separate API key. Uploads POST a single-record ADIF file to
  `https://www.eqsl.cc/qslcard/importADIF.cfm`. If your account has more
  than one "Profile" (QTH) configured on eQSL, set `qth_nickname` to the
  one uploads should file under; leave it empty for a single-location
  account.

### Upload retries and the pending-upload queue

Every ClubLog, QRZ Logbook, eQSL and cqdx.io upload for a completed QSO
is written to `~/.pancetta/upload_outbox.json` **before** its first
attempt, and leaves it only when the service accepts it (or reports it as
a duplicate). The queue is sent again at every start, so a service
outage or a restart no longer loses an upload. LoTW (tqsl) keeps its
single attempt.

`[network.reliability.retry]` sets the quick retries:

```toml
[network.reliability.retry]
max_attempts       = 3       # tries in the quick burst, counting the first (>= 1)
base_delay_ms      = 1000    # wait after the first failure
max_delay_ms       = 10000   # cap on any quick-retry wait (>= base_delay_ms)
backoff_multiplier = 2.0     # each wait = previous × this (>= 1.0)
jitter_factor      = 0.1     # each wait varies by ± this fraction (0.0–1.0)
```

| Key | Meaning |
|---|---|
| `max_attempts` | Tries in the quick burst, counting the first. Must be at least 1. |
| `base_delay_ms` | Wait after the first failure. Must not exceed `max_delay_ms`. |
| `max_delay_ms` | Cap on any quick-retry wait. Upload retries never wait longer than 24 hours, whatever this says. |
| `backoff_multiplier` | Each wait is the previous one times this. Finite, at least 1.0. |
| `jitter_factor` | Each wait varies by ± this fraction. Finite, 0.0–1.0. |

With the defaults a failing upload is tried at 0 s, about 1 s and about
2 s. After that its service retries every 5 minutes until it accepts an
upload again (and again at every start). Each service is retried on its
own, one upload at a time, with sends spaced 1 s apart; an upload that
keeps failing never blocks the ones behind it. A change to this section
needs a restart.

How failures are treated:

- **Transient** (HTTP 5xx, 408/425/429, a timeout, a refused
  connection, an unrecognised response page): retried as above. After
  the quick retries the readiness strip shows
  `⚠ <SERVICE> UPLOAD RETRYING · N QUEUED` until the service accepts an
  upload again.
- **Login rejected** (HTTP 401/403, QRZ `RESULT=AUTH`, an eQSL bad
  login, an invalid cqdx.io token): pancetta stops sending to that
  service for the session and shows `⚠ <SERVICE> LOGIN REJECTED`. Every
  upload for it stays queued. Fix the credentials and restart; the held
  uploads go out at startup.
- **Record rejected** (any other 4xx, QRZ `RESULT=FAIL`, an oversized
  response): that one upload is held and the strip shows
  `⚠ <SERVICE> REJECTED N QSO(S)`; the others keep going. Held uploads
  are tried again at the next start. Nothing is ever dropped
  automatically.

These are **warnings**: they can be acknowledged with `!`, are listed in
the Shift+S station-status overlay, get one Shift+D diagnostics row per
change, and never hold TX.

The queue holds at most 5,000 uploads. Past that, new uploads are not
queued (the strip shows `⚠ UPLOAD QUEUE FULL`); those QSOs are still in
`~/.pancetta/qsos.adi`.

The queue file (mode 0600 on Unix) is pretty-printed JSON holding QSO
data only — credentials are added at send time and are never stored.
Each entry has a `destination` (`clublog`, `qrz_logbook`, `eqsl`,
`cqdx`), the `payload` that is sent, the attempt count, the last error
and a `state` (`pending` or `held`). To discard queued uploads, stop
pancetta and delete the file, or delete single entries from it. A file
pancetta cannot read is never deleted: it is renamed to
`upload_outbox.json.corrupt-<unix-seconds>` and pancetta starts with an
empty queue. `--replay` never opens the queue.

### Customizing PSKReporter

`psk_reporter` ships off (`enabled = false`) with otherwise-sane
defaults — most operators only need to flip `enabled` to `true` and
nothing else (see the note above). This block is the verified-complete
set of current defaults, shown here so you can see and tweak any of
them; nothing here needs to be copied verbatim unless you're changing
it:

```toml
[network.psk_reporter]
enabled                  = false
server_url               = "https://pskreporter.info/cgi-bin/pskdata.pl"
upload_interval_seconds  = 300
batch_size               = 50
include_receives         = true
include_transmits        = true
min_snr_db               = -20.0
max_age_hours            = 24
frequency_accuracy_hz    = 1

[network.psk_reporter.reporter_info]
software_name    = "Pancetta"
software_version = "0.9.6"   # informational only; not validated against the running binary

[network.psk_reporter.filters]
enabled_modes = ["PSK31", "PSK63", "FT8", "FT4", "JS8"]
enabled_bands = ["40m", "30m", "20m", "17m", "15m", "12m", "10m", "6m"]

[network.psk_reporter.filters.geographic]
include_dxcc = []
exclude_dxcc = []
include_itu_zones = []
exclude_itu_zones = []
include_cq_zones = []
exclude_cq_zones = []

[network.psk_reporter.filters.geographic.distance]
great_circle = true
```

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
| `allow_tx_initiation` | bool | `false` | Whether an inbound `Reply(4)` (a GridTracker double-click on a CQ) may **create** a QSO. Consent is tracked per channel (PAN-98), so this setting gates only the WSJT-X UDP channel's own request path — it never arms anything and never satisfies the station-agent channel's `remote_tx_enabled`. Putting the resulting QSO's frames on the air additionally requires a **live remote-TX arm**, which only the station-agent channel can establish: `[network.station_agent].remote_tx_enabled = true` plus a paired, TX-arm-eligible remote client that has armed and keeps heartbeating (see `docs/DECISIONS/remote-operation.md`). With no live arm the QSO is created but every transmission is dropped and audited `not-armed`. Requires `enabled = true` and `accept_udp_requests = true` (in addition) for an inbound `Reply` to actually be dispatched. Fail-closed: `false` means no Reply-initiated QSOs. |
| `allowed_request_hosts` | list of strings (IPs) | `[]` (empty) | For a **multicast** `destination`: the IP addresses allowed to send inbound requests. Empty ⇒ every request is refused (and logged), even with `accept_udp_requests = true` — you must opt a host in explicitly. For a **unicast** `destination`, the peer is inferred (only the configured destination host is accepted) and this list is unused. |

`destination` must parse as a valid `host:port` socket address, and
`instance_id` must be non-empty, whenever `enabled = true` — otherwise
config validation rejects the file.

### `[network.station_agent]` — remote operation

The station agent's keys (`enabled`, `remote_tx_enabled`, `relay_url`,
`pairing_api_url`, `key_dir`, `tx_allow_list`, …) and their defaults are
in [`defaults.toml`](../pancetta-config/defaults.toml); the design is in
[`docs/DECISIONS/remote-operation.md`](DECISIONS/remote-operation.md).
Revocations — `remote_tx_enabled = false`, removing a `tx_allow_list`
entry, `enabled = false` — apply to a running station when you save;
every other change to this table needs a restart. See
[Hot reload](#hot-reload).

#### The remote-access audit log (`audit_log_path`)

`audit_log_path` defaults to `~/.pancetta/agent-audit.log`. Pancetta
manages that exact file plus three archives beside it, `<path>.1`
(newest), `<path>.2` and `<path>.3` (oldest); don't put other files at
those names. Each file is at most 1 MiB (1,048,576 bytes), so normal
operation keeps at most 4 MiB. When the active file is full it becomes
`.1` and the oldest archive is deleted. These are size limits, not a
fixed number of days.

Each line is one JSON object with `ts_unix_ms`, `kind`,
`operator_callsign` and `detail`. Operator callsigns are capped at 64
bytes and details at 1,024 bytes (cut text ends in `...[truncated]`).
Each event kind may write a burst of 20 records, then 10 per second.
Events over that rate are counted rather than written, and a `Summary`
record (operator `null`) reports them about every five seconds, for
example `audit summary: TxRequested coalesced=80 failed=0 first_ts=1000
last_ts=1000`. `coalesced` counts events summarized by the rate limit;
`failed` counts events whose recording could not be confirmed (some of
them may have reached the file). Treat the log as a bounded diagnostic
history, not a complete ledger, and handle the `Summary` kind in any
tool that reads it.

On upgrade, a managed file already larger than 1 MiB is reset to zero
bytes before anything else is written, and a `Summary` records
`audit retention: reset oversized active file; discarded_bytes=N` (or
`archive=1` … `archive=3`). Copy the old file elsewhere first if you need
it. Counts not yet written are lost if pancetta crashes.

When a write fails (disk full, permissions, missing directory), the
readiness strip shows `⚠ DEGRADED: Remote audit unavailable; events may
be missing`, and headless mode logs the same text at ERROR (first
failure, then at most once a minute). Remote arming keeps working, and
disarm and PTT-off never wait on the log. To recover, check free space
and the permissions of the configured file and its directory. Pancetta
retries about every five seconds, writes the missing counts, clears
the warning and logs `Remote audit recording recovered`.

---

### `[network.remote_gateway]` — local read-only view (Panino)

The default-off gateway serves station and contact data to authenticated clients.
Generate a secret with `openssl rand -hex 32`, put it in `auth_token`, and protect
this file with `chmod 600`.

```toml
[network.remote_gateway]
enabled = true
bind_addr = "127.0.0.1:4080"
auth_token = "<paste the generated secret here>"
allowed_origins = [] # for a browser dev client: ["http://localhost:8081"]
allow_network_exposure = false
```

| Key | Default | Rule |
|-----|---------|------|
| `enabled` | `false` | Enable the read-only WebSocket listener. Disabled sections are not validated. |
| `bind_addr` | `"127.0.0.1:4080"` | An IP literal and port, such as `127.0.0.1:4080` or `[::1]:4080`; hostnames are refused. |
| `auth_token` | absent | Required when enabled; at least 32 characters, with no whitespace or control characters. |
| `allowed_origins` | `[]` | Bare `http://` or `https://` browser origins, with a host and optional port; no path, query, fragment, credentials, whitespace or trailing slash. Each entry must be written exactly as a browser sends it in `Origin` (ASCII case aside): no default port (`http://localhost`, not `http://localhost:80`) and no host the URL parser would rewrite. A non-canonical entry is refused, and the error names the origin a browser would send. |
| `allow_network_exposure` | `false` | Explicitly acknowledge serving station and contact data unencrypted over the network when binding a non-loopback address. A token is still required. |

Connect to `ws://<bind_addr>/ws` and send `hello.gatewayToken` as the first text
frame within 5 seconds. The gateway sends no Welcome, snapshot or live events
until that token matches. Authentication failure closes with WebSocket code
1008 and reason `authentication required`. Inbound messages and frames are
limited to 64 KiB. Authenticated clients can only read; commands are ignored.

Browser requests must carry an Origin matching an entry in `allowed_origins`,
ignoring ASCII case. Unlisted origins and `null` receive HTTP 403 before upgrade.
Native requests without Origin still need the token. There is no implicit
localhost Origin allowance.

Loopback includes 127.0.0.0/8 and ::1. Unspecified addresses, LAN addresses and
IPv4-mapped IPv6 addresses require `allow_network_exposure = true`. A non-loopback
bind is still plain `ws://`, without TLS. The listener logs its actual address
and warns when network-exposed. Gateway settings take effect on restart.

`pancetta config --validate` reports the following exact validation messages:

```text
# enabled = true, no auth_token (or shorter than 32 chars, or containing whitespace)
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.auth_token must be set to a secret of at least 32 characters with no whitespace when the gateway is enabled (generate one with: openssl rand -hex 32)
exit 1

# enabled = true, auth_token set, bind_addr = "0.0.0.0:4080", allow_network_exposure = false
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.bind_addr 0.0.0.0:4080 is not a loopback address, so station and contact data would be served unencrypted to the network; bind to 127.0.0.1 or [::1], or set network.remote_gateway.allow_network_exposure = true to accept that
exit 1

# enabled = true, bind_addr = "localhost:4080"
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.bind_addr "localhost:4080" must be an IP address and port, e.g. "127.0.0.1:4080" or "[::1]:4080"
exit 1

# enabled = true, allowed_origins = ["https://cqdx.io/app"]
Configuration validation: FAIL
Error: Validation error: network.remote_gateway.allowed_origins entry "https://cqdx.io/app" must be a bare browser origin: http:// or https:// followed by a host and optional port, with no path, query or trailing slash
exit 1

# enabled = true, auth_token set, bind_addr = "0.0.0.0:4080", allow_network_exposure = true
Configuration validation: PASS
exit 0
```

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

Pancetta watches its config file for changes while it runs: the file
given with `--config`, otherwise `~/.pancetta/pancetta.toml`. If that
path is a symlink, both edits to the file it points at and repointing
the symlink itself are picked up. The file's directory is watched even
while the file does not exist, so creating it later is picked up too
(when the directory itself does not exist, nothing is watched and every
change needs a restart). A save is picked up within a few seconds. With
`--config`, each save is compared with that file's previous content.
Without `--config`, each save re-loads and validates every config file and
`PANCETTA_*` environment variable the way startup does, and the merged
result is compared with the previous one: a setting a higher-priority file
(for example `~/.config/pancetta/pancetta.toml`) overrides does not change
when you edit it in `~/.pancetta/pancetta.toml`, and an edit to that other
file is applied the next time the watched file is saved. A save made while
pancetta is starting is applied when watching begins.

Deleting the watched file is a change too. Without `--config`, the
remaining config files, environment variables and defaults then apply,
so a remote-TX consent or `tx_allow_list` entry that only the deleted
file granted is revoked live. With `--config`, the file is the whole
configuration: deleting it keeps the previous config active and shows
`Config reload failed: … — previous configuration still active`.
Editors that save by removing the file and writing a new one are read
as a save, not a deletion: while the file is missing, pancetta waits a
moment longer before reloading.

**Applied live — remote-access revocations (security):**

- `[network.station_agent].remote_tx_enabled` — the station-agent
  channel's local consent. Setting it to `false` immediately refuses all
  remote transmission: queued remote frames are dropped and audited
  `not-armed`, and a transmission in progress stops within ~50 ms (PTT
  off). The arm record itself is kept; re-enabling consent re-allows an
  existing arm within its TTL and heartbeat. To kill a client's arm,
  remove it from `tx_allow_list` (below) or disarm from the TUI.
- `[network.wsjtx_udp].enabled` / `allow_tx_initiation` — the WSJT-X UDP
  channel's own consent bit (`enabled && allow_tx_initiation`), same
  mechanics. It never arms anything and never satisfies the
  station-agent channel's consent. Only the consent bit is live: the UDP
  socket itself opens and closes on a restart.
- Removing an entry from `[network.station_agent].tx_allow_list` — the
  client's session is evicted (Noise transport dropped, hello-token JTI
  deny-listed), and it is disarmed and loses control if it held either.
  On cqdx-integrated stations (`[network.cqdx]` enabled with a token)
  the authorization poll stays authoritative: a removed entry is evicted
  immediately but is re-admitted by the next successful poll if cqdx
  still returns its edge — revoke it there too.
- `[network.station_agent].enabled` set to `false` — the relay session
  and authorization poll stop and remote TX is disarmed, live.

**Explicitly restart-required (a diagnostic says so on save):**

- Adding a `tx_allow_list` entry, `station_agent.enabled = true` on a
  disabled station, and the station agent's `relay_url`,
  `pairing_api_url`, `pairing_origin`, `key_dir` and `audit_log_path`.
- `[network.remote_gateway]` changes take effect on restart, including
  `auth_token` and `allowed_origins`: until pancetta restarts, the
  running gateway still accepts the old token and origins and keeps
  clients that are already connected.
- Every other key: `[audio]` device names, `[rig.interface]`,
  `[station]`, `[autonomous]`, `[ui]`, upload/logging settings, …
  Nothing outside the live list above is applied to a running process,
  not even to its in-memory copy of the config; restart pancetta to
  enforce them. (`[station].callsign`, `[station].grid_square` and
  `[autonomous].slot_parity` in particular can never change under an
  in-progress QSO.)

When a reload succeeds you see a `Config reloaded` line in the TUI
diagnostics, plus one line per security action and, when other keys
changed, `Saved changes take effect after a restart: …` naming them. A
misspelled or unrecognized key shows a `Config warning: …` line on the
save that introduces it; that key has no effect. Otherwise a save that
changes nothing shows nothing. When a reload fails (typo, schema violation), the
previous config stays active and
`Config reload failed: … — previous configuration still active` shows
in the TUI diagnostics.

To revoke a suspect remote client step by step, see
[RUNBOOK.md → Revoking a suspect remote client](RUNBOOK.md#revoking-a-suspect-remote-client).

---

## Pancetta data files

All persistent state lives under `~/.pancetta/`.

### QSO log files

| File | Role | Recoverable? |
|---|---|---|
| `~/.pancetta/qsos.adi` | Durable, append-only ADIF source of truth. Point WSJT-X / N1MM / LoTW / eQSL at this file directly. | No — back this up. |
| `~/.pancetta/qso.db` | sqlx-backed query index. Rebuilt from ADIF on startup if missing or stale. | Yes — safe to delete; the next run will replay ADIF into a fresh index. |
| `~/.pancetta/qsos.adi.bak`, `qsos.adi.bak.N` | Your logbook as it was before a `pancetta import` added contacts (one per import). | This *is* the recovery copy. |
| `~/.pancetta/backups/qso_backup_YYYYMMDD_HHMMSS.db` | Automatic backups of `qso.db` (every 24 h, 7 kept), regardless of the directory Pancetta was launched from. Covers the query index only — the `qsos.adi` source of truth is not copied here. | Yes — same recovery path as `qso.db`. |

**Migration note:** if you are upgrading from an earlier release that wrote only
`qso.db`, the first startup will automatically export every row from the old database
into a fresh `qsos.adi` before switching over. No manual action required.

**Bringing in another logger's log:** run `pancetta import <file>` (add
`--dry-run` to preview). It appends only the contacts you don't already have
and backs up `qsos.adi` first. Do not copy another logger's file over
`qsos.adi`: that replaces your own contacts.

### Decode-window recordings

Decode-window WAVs exist only on a station whose config sets
`[audio.recording] auto_record = true`, and land in
`audio.recording.directory` (see the `[audio.recording]` section above).
The station decoder corpus — the magazzino offload job and the
`pancetta-research` curation tools — reads `~/.pancetta/recordings`, so a
corpus station sets `directory = "~/.pancetta/recordings"` explicitly.
Releases before this setting existed recorded every window into
`~/.pancetta/recordings` unconditionally; upgrading does not touch those
files, and deleting them reclaims the space (the directory was capped at
10 GB).

---

## Where to look next

- The complete generated schema is
  [`pancetta-config/defaults.toml`](../pancetta-config/defaults.toml); the
  annotated source of truth is the Rust structs under
  `pancetta-config/src/`.
- Rust types and validation logic live under `pancetta-config/src/`.
- See [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) for how config flows
  through the coordinator.
