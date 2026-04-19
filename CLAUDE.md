# CLAUDE.md

Project instructions for Claude Code when working in this repository.

## Project Overview

Pancetta is an autonomous FT8 ham radio station written in Rust. The goal is a fully operational on-air system: decode, call, complete QSOs, and log — with priority-based station selection, multi-stream TX, and integration with cqdx.io.

## Workspace Structure

11-crate Cargo workspace:

| Crate | Purpose | Status |
|-------|---------|--------|
| `pancetta-ft8` | FT8 encoder/decoder/modulator/OSD | Production-grade, ~200 tests, bit-exact with ft8_lib/WSJT-X |
| `pancetta-audio` | Real-time audio I/O (cpal + ringbuf) | Functional |
| `pancetta-dsp` | DSP pipeline (FFT, filtering, resampling) | Functional |
| `pancetta-config` | Configuration with hot-reload | Production-ready, ~59 tests |
| `pancetta-qso` | QSO management, priority scoring, frequency allocation, autonomous operator | Core logic, ~81 tests |
| `pancetta-dx` | DX hunting, DXCC, PSKReporter | Partial implementation |
| `pancetta-hamlib` | Hamlib CAT control FFI | Bindings done, integration stub |
| `pancetta-cqdx` | cqdx.io HTTP client, cache, types | Delta-adapted, needs live API validation |
| `pancetta-tui` | Terminal UI | Scaffold, not wired to pipeline |
| `pancetta-core` | Shared types, error handling | Stable |
| `pancetta` | Main binary, coordinator, message bus, runtime | Integration point |

## Building and Testing

```bash
# Full workspace build
cargo build

# Run all workspace tests (excludes pancetta-hamlib by default — see note)
cargo test

# FT8 tests (encoder is feature-gated behind `transmit`)
cargo test --features transmit -p pancetta-ft8    # all ~200 FT8 tests
cargo test -p pancetta-ft8                         # LDPC/CRC tests only

# Loopback integration tests (end-to-end QSO through encode→modulate→decode)
cargo test -p pancetta --test loopback_qso

# pancetta-hamlib hangs in workspace due to tokio runtime conflicts — test separately:
cargo test -p pancetta-hamlib --lib -- --test-threads=1
```

## Domain Context

- **Ham radio / FT8**: Digital mode protocol — 15-second slots, 8-GFSK modulation, LDPC+CRC coding, structured message exchange (CQ → grid → report → RR73)
- **Hardware target**: Yaesu FTdx10 via USB on Windows 11 MiniPC; Mac for development
- **cqdx.io**: First-party web service (owned by the developer) providing rarity scoring, needed DXCC/grid lookups, and live spots. Custom API endpoints can be built specifically for pancetta. API requirements doc: `docs/cqdx-api-requirements.md`

## Architecture Highlights

- **Coordinator** (`pancetta/src/coordinator.rs`): Central orchestrator, manages decode→decide→transmit pipeline. Large file (~2,700 lines), decomposition planned.
- **Autonomous operator** (`pancetta-qso/src/autonomous.rs`): Decision engine — hunt mode (pounce on rare stations), CQ mode (answer callers), hybrid mode. Configurable priority weights.
- **Priority scoring** (`pancetta-qso/src/priority.rs`): Weighted scoring — needed DXCC > needed grid > POTA/SOTA > rarity. Duplicate suppression and failure backoff.
- **SmartFrequencyAllocator** (`pancetta-qso/src/frequency.rs`): 7 soft-scored criteria for TX frequency selection. Enables parallel QSOs at different audio frequencies.
- **Multi-stream TX**: Supports N simultaneous FT8 signals in a single 15-second slot.

## Development Phases (End-to-End QSO Initiative)

Design spec: `docs/superpowers/specs/2026-04-02-end-to-end-qso-design.md`

- **Phase 1** (complete): Loopback QSO — CQ-to-73 exchange through full pipeline, state machine tests
- **Phase 2** (complete): Autonomous operator + priority engine — configurable weighted scoring, POTA/SOTA detection
- **Phase 3** (complete): Multi-stream TX — SmartFrequencyAllocator, multi-slot decision logic, dual QSO loopback test
- **Phase 4** (next): Hardware integration — hamlib CAT control, real rig TX, on-air testing

## Known Gaps and TODOs

- Grid "needed" set never populated (DXCC needed works via cqdx.io, grid `update_needed_grids()` never called)
- POTA/SOTA detection has false positives on callsigns with `/` suffix (`contains('/')` too broad)
- `is_duplicate` ignores freq_hz (no band-aware dedup yet)
- cqdx.io `GET /spots?live=true` response envelope key (`groups`) unverified against live API

## Documentation Maintenance

After completing significant work, update affected documentation:

- **Inline docs**: Update `///` and `//!` comments on modified public items
- **CLAUDE.md**: Update known gaps, build instructions, or project phases
- **docs/ARCHITECTURE.md**: Update if crate relationships or data flows changed
- **README.md / FEATURES.md**: Update if user-facing capabilities changed

All crates have `#![warn(missing_docs)]` enabled — the compiler will flag undocumented public items.

## Build Hygiene

The `target/` directory can balloon to 40-50GB with stale incremental compilation caches. Run periodically:

```bash
cargo sweep --installed          # remove artifacts from unused toolchains
cargo sweep --maxsize 10GB       # cap target/ size
```
