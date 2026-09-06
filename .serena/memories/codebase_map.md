# pancetta — codebase map

Autonomous FT8 ham radio station in Rust: decode, call, complete QSOs, and log —
with priority-based station selection, multi-stream TX, and cqdx.io integration.
14-crate Cargo workspace, pure Rust (no REST API, web UI, or mobile layer).
Hardware target is a Yaesu FTdx10 over USB on a Windows 11 MiniPC; development
happens on Mac. Tickets are `PAN-<n>`.

## Layer 0 — no internal deps

- **pancetta-core** — shared types, error handling. Stable.
- **pancetta-audio** — real-time audio I/O (cpal + ringbuf).
- **pancetta-ft8** — FT8 encoder/decoder/modulator/OSD; bit-exact with
  ft8_lib/WSJT-X. The encoder is feature-gated behind `transmit`. Vendors the
  upstream C reference decoder as a git submodule at `pancetta-ft8/vendor/ft8_lib`
  — usually uninitialized, and outside this project's index.
- **pancetta-dsp** — DSP pipeline (FFT, filtering, resampling).
- **pancetta-config** — configuration with hot-reload. Production-ready.

## Layer 1 — build on core/ft8

- **pancetta-qso** — QSO management, priority scoring, frequency allocation,
  autonomous operator. The core logic crate.
- **pancetta-hamlib** — Hamlib CAT control FFI. Bindings done, integration stub.
- **pancetta-dx** — DX cluster + PSKReporter + per-QSO logbook upload
  (ClubLog/QRZ/cqdx/LoTW/eQSL) + QRZ XML lookup.
- **pancetta-cqdx** — cqdx.io HTTP client, cache, types. cqdx.io is a first-party
  service; custom endpoints can be built for pancetta.
- **pancetta-tui** — terminal UI; the default UI (`--headless` disables it).
- **pancetta-agent** — remote-TX security: arm gating, session binding.
- **pancetta-protocol** — remote-operation wire protocol.
- **pancetta-research** — local-only decoder-iteration harness. Excluded from CI
  and from `default-members`; never builds in GitHub Actions.

## Layer 2 — orchestrator

- **pancetta** — main binary, coordinator, message bus, runtime. Depends on all
  of the above; the integration point.

## Non-crate areas

- **docs/DECISIONS/** — decision digests by subsystem (tx-scheduling, qso-engine,
  modes, remote-operation, tui, logging-uploads, config-and-platform). Known
  gaps/TODOs live in `2026-07-development-phases-and-gaps.md`.
- **docs/superpowers/specs/** — design specs, authoritative for current behavior.
- **docs/ARCHITECTURE.md**, **docs/CONFIG.md**, **docs/RUNBOOK.md** — crate
  relationships/data flows, config reference, operations.
- **wiki/** — accumulated cross-cutting gotchas and decisions; read
  `wiki/INDEX.md` first. Descriptive: code and `docs/` win any conflict.
- **research/** — decoder research history (experiment logs, specs, scorecards).
  Real project history, kept searchable; its heavy binary artifacts are
  gitignored.
- **training/neural_osd/**, **scripts/** — this repo's Python: the neural-OSD
  training harness and one-off research batch scripts. Not part of the Rust
  symbol graph; reachable via `search_for_pattern`, not `find_symbol`.
- **assets/** — binary demo media only (WAV/PNG/GIF/SVG). Excluded from the index.

## Conventions

- Tests: `cargo test --workspace --features transmit`. The FT8 encoder is
  feature-gated behind `transmit`, so `-p pancetta-ft8` without it runs only the
  LDPC/CRC tests. `pancetta-hamlib` needs `--test-threads=1` (per-test
  current_thread tokio runtime).
- `scripts/check.sh` is the local pre-flight; `.github/workflows/ci.yml` is the
  authoritative gate. CI's `changes` job only considers `**/*.rs`,
  `**/Cargo.toml`, `**/Cargo.lock`, and `ci.yml` — a diff touching none of those
  runs no Rust job at all.
- Read `AGENTS.md`'s "Key Invariants" section before touching TX scheduling, the
  armed-TX gate, the QSO engine, or the coordinator supervisor. Several of those
  invariants are safety- and regulation-shaped (see
  `docs/fcc-part97-compliance.md`), not merely stylistic.
- Cross-repo interface changes are proposed in the sibling `dispensa` repo first
  (`contracts/`, `questions/`).
- Serena here is **read_only**: navigation only, never edits.
