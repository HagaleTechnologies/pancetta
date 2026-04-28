# Pancetta

An autonomous FT8 ham radio station, written in Rust.

Pancetta listens on the FT8 sub-band, decodes everything it hears, scores
each station against a configurable priority model (needed DXCC entity,
needed grid, POTA/SOTA, rarity, recent activity), and runs full
CQ → grid → report → RR73 exchanges on its own — including transmitting
multiple simultaneous QSOs in a single 15-second slot when band conditions
permit.

It is designed to run on a small headless host (e.g. a Windows MiniPC)
attached to a transceiver via a USB CODEC, controlled remotely over SSH.

> **Status: pre-1.0 / on-air ready.** The decoder is bit-exact with
> ft8_lib and WSJT-X across ~295 tests. Hardware TX has been validated
> end-to-end on a Yaesu FTdx10 with clean ALC and tail-end PSKReporter
> spots across NA + EU. The autonomous QSO loop is the focus of the
> current development phase.

---

## Prerequisites

| Requirement | Linux | macOS | Windows |
|---|---|---|---|
| Rust toolchain | rustup → stable | rustup → stable | rustup → stable |
| Audio dev headers | `libasound2-dev`, `libudev-dev` | (built in) | (built in) |
| TLS | `libssl-dev`, `pkg-config` | (built in) | (built in) |
| Hamlib (CAT control) | `apt install libhamlib-utils` | `brew install hamlib` | hamlib Windows build |

Pancetta is developed and tested on:
- **macOS** (development host, Apple Silicon)
- **Linux** (CI lane and headless deployment)
- **Windows 11 MiniPC** (production deployment behind the radio)

The project is `MIT OR Apache-2.0` dual-licensed; pick whichever fits
your use case.

---

## Quick Start

### 1. Install Rust and system dependencies

```bash
# Linux (Debian/Ubuntu):
sudo apt update
sudo apt install -y libasound2-dev libudev-dev libssl-dev pkg-config libhamlib-utils

# macOS:
brew install hamlib

curl https://sh.rustup.rs -sSf | sh
```

### 2. Clone and build

```bash
git clone https://github.com/HagaleTechnologies/pancetta.git
cd pancetta
cargo build --release
```

The first build will take 5–10 minutes (workspace is 11 crates and
compiles a vendored copy of `ft8_lib`, which Pancetta uses as its
primary FT8 reference decoder via FFI — see [Acknowledgments](#acknowledgments)).
After that, incremental builds are sub-30s.

### 3. Bootstrap your config

The first time you run `pancetta` it walks you through writing a
`~/.pancetta/config.toml` containing your callsign, grid square, audio
device names, and rig model. You can also write the file by hand —
see [`docs/CONFIG.md`](docs/CONFIG.md) for every supported key.

```bash
# First-run wizard
pancetta

# Or write the config directly, then run:
$EDITOR ~/.pancetta/config.toml
```

The minimum viable config:

```toml
[station]
callsign = "YOURCALL"
grid_square = "FN42"   # 4-character Maidenhead grid

[audio]
input_device = "USB Audio CODEC"   # exact name from `pancetta --list-audio`
output_device = "USB Audio CODEC"

[rig.interface]
enabled = true
port = "/dev/tty.usbserial-A1"     # or "COM3" on Windows
baud_rate = 38400

[rig]
model = "FTdx10"
```

> **Replace `YOURCALL` with your actual callsign before transmitting.**
> Pancetta refuses to call CQ as `NOCALL` / `N0CALL`, but it will
> transmit whatever you put in `station.callsign` — and Part 97 is your
> problem, not the software's.

### 4. Run

```bash
# Decode-only mode (safe to run anywhere — no PTT)
cargo run --release -- --no-rig

# Full pipeline (decode + autonomous TX, requires rig + license)
cargo run --release
```

---

## How to drive the TUI

| Key | Action |
|---|---|
| `Tab` | Cycle active panel (Band Activity, DX Hunter, Waterfall, Station Info) |
| `↑` / `↓` | Move selection within the active panel |
| `Space` | Call the selected station (queues a TX for the next slot) |
| `c` | Start auto-CQ (call CQ every cycle until stopped) |
| `s` | Stop auto-CQ |
| `D` | Open audio device picker |
| `?` / `F1` | Toggle help overlay |
| `q` / `Esc` | Quit |

The status bar at the bottom shows live pipeline state, your TX queue,
and any errors emitted by the audio / QSO components.

---

## Troubleshooting

### "Audio init failed" appears in the TUI status

Most often: cpal can't find the input device named in your config.
Run `pancetta --list-audio` to see the names cpal sees and copy one
verbatim into `[audio].input_device`. Wireless USB CODECs sometimes
present a transient name on first plug-in; unplug, replug, restart.

### No decodes appear, even with strong signals

1. Confirm audio is actually flowing: the audio-level meter on the
   bottom-right of the TUI should bounce when stations are on. If it's
   flat, your input device is wrong or muted at the OS level.
2. Confirm slot timing: FT8 slots are aligned to UTC second `:00` and
   `:15` etc. If the host clock is more than ~1 second off, decodes will
   fail systematically. NTP fixes this; `chrony` is the recommended
   daemon on Linux.
3. Confirm the band — set `[station].operating_frequency_hz` (or use
   the rig CAT, which auto-syncs at startup). Listening on the wrong
   band against a CW segment looks identical to "no signal" from the
   decoder's point of view.

### `Call X failed: duplicate QSO`

Pancetta refuses to call the same station on the same band twice within
the configured `duplicate_checking.time_window_hours`. Adjust the
window in config, or remove the prior QSO from `~/.pancetta/qso.db`
if it was a test. The duplicate check is intentional — it prevents
embarrassing repeat-calls during a contest or grid hunt.

### `rigctld` won't connect

Pancetta spawns `rigctld` automatically when `[rig.interface].enabled`
is true. Check:

- The serial device path in `[rig.interface].port` exists (`ls /dev/tty.*`
  on macOS, `ls /dev/ttyUSB*` on Linux, Device Manager on Windows).
- The hamlib model number matches your radio (`rigctl --list`).
- The baud rate matches the radio's CAT port setting (38400 is correct
  for the Yaesu FTdx10 default).
- No other process holds the serial device (e.g. WSJT-X is not running).

If `rigctld` itself works (`rigctld -m 1042 -r /dev/tty... -s 38400`)
but Pancetta refuses to spawn it, check the log line that begins
`Refusing to spawn rigctld with suspicious port path` — Pancetta now
allow-lists `/dev/tty*`, `/dev/cu.*`, `COM<N>`, and `host:port` only.

---

## Workspace layout

11-crate Cargo workspace. Crates form a clean layering: a leaf crate
never reaches up into an orchestrator.

| Crate | Purpose |
|---|---|
| `pancetta-core` | Shared types, error handling |
| `pancetta-audio` | Real-time audio I/O (cpal + ringbuf) |
| `pancetta-dsp` | FFT, filtering, resampling |
| `pancetta-ft8` | FT8 encoder, decoder, modulator, OSD, AP |
| `pancetta-config` | Configuration loader + hot-reload |
| `pancetta-qso` | QSO state machine, priority scoring, autonomous operator |
| `pancetta-hamlib` | rigctld TCP client (CAT control) |
| `pancetta-cqdx` | cqdx.io HTTP client and cache |
| `pancetta-dx` | DX cluster + PSKReporter + scaffolded LoTW |
| `pancetta-tui` | Terminal UI (ratatui + crossterm) |
| `pancetta` | Coordinator binary, message bus, runtime |

Detailed component diagram and channel topology in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

---

## Building, testing, lint

```bash
# Full workspace build
cargo build --workspace

# Run all tests (excludes pancetta-hamlib due to tokio runtime conflicts)
cargo test --workspace --features transmit

# pancetta-hamlib must run single-threaded
cargo test -p pancetta-hamlib --lib -- --test-threads=1

# Lint and format
cargo clippy --workspace --features transmit
cargo fmt --all -- --check

# Loopback integration: end-to-end QSO through encode → modulate → decode
cargo test -p pancetta --test loopback_qso
```

CI runs all of the above on every PR plus a cross-platform `cargo
check` on macOS and Windows. `cargo audit` and `cargo deny check` run
on every push to catch security advisories and license drift.

---

## Documentation

- [`docs/CONFIG.md`](docs/CONFIG.md) — every config key, with examples and defaults.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — crate dependency graph, data flow, key abstractions.
- [`FEATURES.md`](FEATURES.md) — capabilities and feature status.
- [`SECURITY.md`](SECURITY.md) — vulnerability reporting and known trade-offs.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — coding standards, contribution flow.
- [`CHANGELOG.md`](CHANGELOG.md) — release notes.

API documentation: `cargo doc --workspace --no-deps --open`.

---

## Acknowledgments

Pancetta stands on the shoulders of the FT8 community. In particular:

- **Joe Taylor (K1JT) and Steve Franke (K9AN)** designed the FT8
  protocol — the LDPC code, Costas sync arrays, modulation, and message
  schema that this project implements. The protocol is documented in
  [*The FT4 and FT8 Communication Protocols*](https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf).
- **Kārlis Goba (YL3JG)** authored
  [`ft8_lib`](https://github.com/kgoba/ft8_lib), the MIT-licensed C
  reference implementation that Pancetta vendors at
  `pancetta-ft8/vendor/ft8_lib/` and uses as its primary decoder via
  FFI. Pancetta's native Rust decoder also ports several algorithms
  from `ft8_lib` (CRC-14, LDPC tables, Gray code mapping, sliding
  spectrogram, LLR normalization) — these are attributed in the source
  comments where they appear.
- **The WSJT-X project** (GPL) is the de-facto reference FT8
  application. Pancetta does **not** link or vendor any WSJT-X source;
  it interoperates with WSJT-X through the published protocol only.

What's specifically novel in Pancetta: the neural-OSD bit-flip
re-ordering CNN, active-QSO-aware AP decoding, multi-stream TX
modulation, the autonomous-operator priority engine, and integration
with the cqdx.io spotting/scoring service. See
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md) for full third-party
license text.

---

## License

Dual-licensed under your choice of:

- MIT — see [`LICENSE-MIT`](LICENSE-MIT)
- Apache 2.0 — see [`LICENSE-APACHE`](LICENSE-APACHE)

Contributions are accepted under the same dual-license terms unless
explicitly stated otherwise in the PR.
