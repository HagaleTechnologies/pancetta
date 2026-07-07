# CLAUDE.md

> 🔗 CROSS-REPO CONTRACTS + COORDINATION — read dispensa before any new work.
> This repo is one of three — cqdx (web), pancetta (rig API/server), panino (RN client). Shared
> decisions + interface contracts live in dispensa
> (https://github.com/HagaleTechnologies/dispensa) — clone it as a sibling dir. Before ANY new
> work: git pull dispensa and scan questions/ (anything for you / newly answered) + contracts/
> (changes). Don't diverge from a shared contract — propose changes in dispensa first (ADR and/or
> contract), then update consumers or bump the version. You may push questions/requirements to
> other projects via dispensa/questions/. When committing to dispensa: git pull --rebase first
> and use the next-free adr/question number. Security model: dispensa/adr/0002 (Proposed).

Project instructions for Claude Code when working in this repository.

## Project Overview

Pancetta is an autonomous FT8 ham radio station written in Rust. The goal is a fully operational on-air system: decode, call, complete QSOs, and log — with priority-based station selection, multi-stream TX, and integration with cqdx.io.

## Workspace Structure

12-crate Cargo workspace:

| Crate | Purpose | Status |
|-------|---------|--------|
| `pancetta-ft8` | FT8 encoder/decoder/modulator/OSD | Bit-exact with ft8_lib/WSJT-X |
| `pancetta-audio` | Real-time audio I/O (cpal + ringbuf) | Functional |
| `pancetta-dsp` | DSP pipeline (FFT, filtering, resampling) | Functional |
| `pancetta-config` | Configuration with hot-reload | Production-ready |
| `pancetta-qso` | QSO management, priority scoring, frequency allocation, autonomous operator | Core logic |
| `pancetta-dx` | DX cluster + PSKReporter + per-QSO logbook upload (ClubLog/QRZ/cqdx/LoTW/eQSL) + QRZ XML lookup | Live + scaffolded |
| `pancetta-hamlib` | Hamlib CAT control FFI | Bindings done, integration stub |
| `pancetta-cqdx` | cqdx.io HTTP client, cache, types | Needs live API validation |
| `pancetta-tui` | Terminal UI | Default UI (`--headless` to disable) |
| `pancetta-core` | Shared types, error handling | Stable |
| `pancetta` | Main binary, coordinator, message bus, runtime | Integration point |
| `pancetta-research` | Local-only decoder-iteration harness | Excluded from CI; never builds in GitHub Actions |

## Building and Testing

```bash
# Full workspace build
cargo build

# Run all workspace tests
cargo test --workspace --features transmit

# FT8 tests (encoder is feature-gated behind `transmit`)
cargo test --features transmit -p pancetta-ft8    # all ~295 FT8 tests
cargo test -p pancetta-ft8                         # LDPC/CRC tests only

# Loopback integration tests (end-to-end QSO through encode→modulate→decode)
cargo test -p pancetta --test loopback_qso

# pancetta-hamlib (single-threaded for deterministic mock-rig tests)
cargo test -p pancetta-hamlib --lib -- --test-threads=1
```

## Domain Context

- **Ham radio / FT8**: Digital mode protocol — 15-second slots, 8-GFSK modulation, LDPC+CRC coding, structured message exchange (CQ → grid → report → RR73)
- **Hardware target**: Yaesu FTdx10 via USB on Windows 11 MiniPC; Mac for development
- **cqdx.io**: First-party web service (owned by the developer) providing rarity scoring, needed DXCC/grid lookups, and live spots. Custom API endpoints can be built specifically for pancetta. API requirements doc: `docs/cqdx-api-requirements.md`

## Key Invariants

- Every concurrent active QSO transmits on the same parity; never TX in sequential windows.
- Single-scorer: the TX-placement display and autonomous decisions share one allocator path.
- Remote gateway bus sends are additive-only; `pancetta-tui` behavior stays byte-identical.
- The armed-TX gate fails CLOSED (poisoned lock ⇒ no remote TX) and ANDs under `TxPolicy`; no remote QSO frame is ever emitted as `TxOrigin::Local`.
- Drop-stale-TX: the worker re-checks QSO liveness at the last instant before PTT.
- `mode=FT8` paths must remain byte-identical when FT4/FT2 features are untouched.
- `merge_with` must carry every config field (see the §5 config-merge guardrail).

## Where Things Live

- `docs/superpowers/specs/` — design specs (authoritative for current behavior)
- `docs/DECISIONS/` — decision digests by subsystem (tx-scheduling, qso-engine, modes, remote-operation, tui, logging-uploads, config-and-platform)
- `docs/ARCHITECTURE.md` — crate relationships and data flows
- `docs/CONFIG.md` — configuration reference
- `docs/fcc-part97-compliance.md` — regulatory / TX-safety notes
- `CHANGELOG.md` — release history

## Known Gaps and TODOs

- **DX Hunter — DXCC entity / ATNO / per-band-needed**: local-only decodes show `---` for the entity name (no prefix→entity resolver exposed to the TUI yet); ATNO (all-time-needed DXCC) is pulled from cqdx and drives the autonomous scorer but is not surfaced in the DX Hunter; per-band-needed is not pulled from cqdx (only the local `worked_on_band` set exists) — surfacing it needs a new cqdx endpoint or local QSO-DB cross-referencing threaded into the TUI.
- **cqdx `GET /api/v1/spots?live=true` response envelope key (`groups`) unverified against live API** — a gated live test exists: `CQDX_TOKEN=pat_xxx cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture`.

## Documentation Policy

After significant work: update inline docs (`///` / `//!` on modified public items) and the relevant `docs/DECISIONS/<subsystem>.md` digest (append a dated entry). CLAUDE.md changes ONLY when build commands, crate structure, invariants, or open gaps change — never append feature narratives here. Missing-docs enforcement: `pancetta-core` is `#![warn(missing_docs)]`, `pancetta-hamlib` is `#![deny(missing_docs)]`, all other crates carry `#![allow(missing_docs)]` with a TODO — switch each to `warn`/`deny` as docs land.

## Build Hygiene

The `target/` directory can balloon to 40-50GB with stale incremental compilation caches. Run periodically:

```bash
cargo sweep --installed          # remove artifacts from unused toolchains
cargo sweep --maxsize 10GB       # cap target/ size
```
