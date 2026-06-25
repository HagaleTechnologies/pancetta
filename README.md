# Pancetta

A full-featured FT8 ham radio station, written in Rust — decode, log, and
work QSOs, with optional hands-off operation.

Pancetta listens on the FT8 sub-band, decodes what it hears, and scores each
station against a configurable priority model (needed DXCC entity, needed
grid, POTA/SOTA, rarity, recent activity) so you can work the ones that matter
— with a keystroke from the terminal UI, or hands-off when you choose to
enable it. It can run a full CQ → grid → report → RR73 exchange, and transmit
multiple simultaneous QSOs in a single 15-second slot when conditions allow.

It runs on a normal desktop or a small headless host (e.g. a Windows MiniPC)
attached to a transceiver via a USB CODEC, and is comfortable driven remotely
over SSH.

> **Status: pre-1.0, on-air ready.** Pancetta's FT8 engine pairs the
> MIT-licensed [`ft8_lib`](https://github.com/kgoba/ft8_lib) C decoder (via
> FFI) with a native Rust decoder that adds parallel multi-candidate decoding
> and a-priori-aided recovery. On a 1,201-file real off-air corpus the native
> decoder produced **+11.6% more decodes than ft8_lib on the same audio**
> (recovering 90.7% of ft8_lib's set, plus extras) — see
> [`docs/decoder-comparison.md`](docs/decoder-comparison.md) for methodology,
> the parallel-execution rationale, and honest caveats. Hardware TX has been
> validated end-to-end on a Yaesu FTdx10 (clean ALC, PSKReporter spots across
> NA + EU). ~295 FT8 tests cover encode / decode / LDPC / CRC / OSD. Hands-off
> (automatic) operation respects FCC §97.221 — see
> [`docs/fcc-part97-compliance.md`](docs/fcc-part97-compliance.md).

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

The first build will take 5–10 minutes (workspace is 12 crates and
compiles a vendored copy of `ft8_lib`, which Pancetta uses as a decoder
via FFI alongside its own native Rust decoder — see
[Acknowledgments](#acknowledgments)). After that, incremental builds are
sub-30s.

### 3. Bootstrap your config

The first time you run `pancetta` it walks you through writing a
`~/.pancetta/pancetta.toml` containing your callsign, grid square, audio
device names, and rig model. You can also write the file by hand —
see [`docs/CONFIG.md`](docs/CONFIG.md) for every supported key.

```bash
# First-run wizard
pancetta

# Or write the config directly, then run:
$EDITOR ~/.pancetta/pancetta.toml
```

The minimum viable config:

```toml
[station]
callsign = "YOURCALL"
grid_square = "FN42"   # 4-character Maidenhead grid

[audio]
input_device = "USB Audio CODEC"   # exact name from `pancetta test-audio --list`
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
# Decode-only mode (safe — no PTT). Achieved by leaving the rig
# interface disabled in config (the default).
cargo run --release

# Full pipeline (decode + manual / autonomous TX). Requires:
#   [rig.interface] enabled = true   in ~/.pancetta/pancetta.toml
#   [autonomous]    enabled = true   for hands-off operation
# and an actual antenna + license. See docs/RUNBOOK.md for the
# Phase 5 (autonomous QSO loop) procedure.
cargo run --release
```

---

## How to drive the TUI

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Cycle active panel |
| `↑` / `↓` | Move selection within active panel |
| `Home` / `End` (or `<` / `>`) | Jump to newest (realtime) / oldest in the focused list |
| `←` / `→` or `[` / `]` | TX offset −/+ 50 Hz |
| `=` / `-` | Band up / down |
| `Space` | Call selected station |
| `Enter` | Send the TX text in the input buffer |
| `c` / `s` | Start / stop repeating CQ |
| `t` | **Find clear TX offset** — auto-picks a 25 Hz candidate clear in your TX parity. |
| `Shift+T` | **Tune** — 12 s single tone at TX offset (PTT engages). |
| `h` | **Halt current TX** (drops PTT within ~150 ms) |
| `p` | Toggle PTT manually |
| `g` | Cycle TX policy: Full → Respond-only → Disabled |
| `f` | TX-frequency mode: **Hold** (pin your offset) / **Auto** (Pancetta picks) |
| `a` | Toggle autonomous mode |
| `Shift+P` | Pause / resume autonomous |
| `m` | Toggle audio monitoring |
| `d` | Open audio device picker (also reclaims a hijacked device) |
| `x` | Clear decoded messages |
| `?` | Toggle help overlay |
| `q` | Quit (with `[y/N]` confirm) |
| `Esc` | Dismiss any overlay / cancel modal |

The status bar at the bottom shows live pipeline state, your TX queue,
and any errors emitted by the audio / QSO components.

---

## Troubleshooting

### "Audio init failed" appears in the TUI status

Most often: cpal can't find the input device named in your config.
Run `pancetta test-audio --list` to see the names cpal sees and copy one
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
3. Confirm the band — set the dial on your rig (CAT auto-syncs at
   startup), or use the `=` / `-` band keys in the TUI. Listening on the
   wrong band against a CW segment looks identical to "no signal" from
   the decoder's point of view.

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

# Run all tests
cargo test --workspace --features transmit

# pancetta-hamlib (single-threaded for deterministic mock-rig tests)
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
- [`docs/decoder-comparison.md`](docs/decoder-comparison.md) — native decoder vs. ft8_lib: measured decode yield on a 1,201-file corpus + the parallel-execution approach.
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

## Provenance & clean-room methodology

Pancetta is MIT/Apache-2.0. Its FT8 engine is built from three clearly
separated sources, and we are careful about the boundary so the codebase
stays free of copyleft contamination:

1. **MIT code we use directly.** [`kgoba/ft8_lib`](https://github.com/kgoba/ft8_lib)
   (MIT, © Kārlis Goba) is vendored and called via FFI, and re-implemented
   in places in native Rust. ft8_lib's MIT license permits this; every
   ft8_lib-derived algorithm or constant is attributed at its call site
   (search `ft8_lib` in `pancetta-ft8/src/`).

2. **The published FT8 protocol.** The Costas arrays, LDPC(174,91)
   generator/parity matrices, CRC-14 polynomial, Gray code, and message
   schema are defined by Joe Taylor (K1JT) and Steve Franke (K9AN) in the
   [QEX paper](https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf). These values are
   **identical in every conformant decoder** (WSJT-X, ft8_lib, JTDX, MSHV, …)
   because the protocol requires them — matching them is interoperability,
   not derivation.

3. **GPL peer decoders — algorithm *ideas* only, never code.** Where Pancetta
   adopts a *technique* from a GPL-licensed project (WSJT-X, JTDX,
   JS8Call-Improved, ft8mon, MSHV), it follows a strict **clean-room
   firewall**: one contributor reads the peer and writes a *prose-only*
   algorithm spec under `research/specs/` that explicitly does not quote
   source; a separate implementer writes the Rust from that spec alone. No
   GPL source is read, ported, copied, or paraphrased into Pancetta's code,
   and the modules written this way carry a `clean-room` affirmation in their
   header comments. Pancetta does **not** link, vendor, or copy any GPL
   source, and does **not** shell out to any GPL binary at runtime.

So: yes, the encoder/decoder *will* resemble the MIT `ft8_lib` (by design and
by license), and the protocol constants *will* match every other FT8 decoder
(by necessity) — but no GPL-licensed source has been incorporated. If you spot
anything that looks like a copyleft-source copy, please open an issue; we treat
that as a bug.

---

## License

Dual-licensed under your choice of:

- MIT — see [`LICENSE-MIT`](LICENSE-MIT)
- Apache 2.0 — see [`LICENSE-APACHE`](LICENSE-APACHE)

Contributions are accepted under the same dual-license terms unless
explicitly stated otherwise in the PR.
