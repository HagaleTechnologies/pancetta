# CLAUDE.md

> **Cross-repo contracts:** clone `dispensa` as a sibling directory. `git pull --rebase` it and
> scan `questions/` + `contracts/` before any cross-cutting work. Propose changes there first.

Project instructions for Claude Code when working in this repository.

## Project Overview

Pancetta is an autonomous FT8 ham radio station written in Rust. The goal is a fully operational on-air system: decode, call, complete QSOs, and log — with priority-based station selection, multi-stream TX, and integration with cqdx.io.

## Workspace Structure (14-crate Cargo workspace)

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
| `pancetta-agent` | Remote-TX security: arm gating, session binding | Live |
| `pancetta-protocol` | Remote-operation wire protocol | Live |
| `pancetta-research` | Local-only decoder-iteration harness | Excluded from CI; never builds in GitHub Actions |
| `pancetta` | Main binary, coordinator, message bus, runtime | Integration point |

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

# pancetta-hamlib: per-test current_thread tokio runtime; run single-threaded for deterministic mock-rig tests
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
- Every transmitted frame (single or multi-TX bundle item) reflects the freshest `MessageToSend` the QSO engine emitted for that qso_id at key-time, or at the moment of an operator-triggered mid-TX abort+re-key, whichever is later.
- At most one QSO object exists per (callsign, band) among active-or-recently-completed QSOs; repeated manual actions resolve to it, never spawn a sibling.

## Where Things Live

- `docs/superpowers/specs/` — design specs (authoritative for current behavior)
- `docs/DECISIONS/` — decision digests by subsystem (tx-scheduling, qso-engine, modes, remote-operation, tui, logging-uploads, config-and-platform); known gaps/TODOs live in `2026-07-development-phases-and-gaps.md`
- `docs/ARCHITECTURE.md` — crate relationships and data flows
- `docs/CONFIG.md` — configuration reference
- `docs/fcc-part97-compliance.md` — regulatory / TX-safety notes
- `CHANGELOG.md` — release history

**Documentation policy:** keep CLAUDE.md under ~100 lines as the standing brief — decision narratives, feature history, and gap tracking go to `docs/DECISIONS/`, never appended here.

## Build Hygiene

The `target/` directory can balloon to 40-50GB with stale incremental compilation caches. Run periodically:

```bash
cargo sweep --installed          # remove artifacts from unused toolchains
cargo sweep --maxsize 10GB       # cap target/ size
```

## Knowledge Wiki and Multi-Agent Hygiene

`wiki/INDEX.md` maps accumulated knowledge — read it before deep exploration; run /wiki-update after substantive work to distill gotchas/decisions into it (or docs/ if normative). The wiki is descriptive; code and docs/ win any conflict.

You are never alone in this repo — other agents may work concurrently in other clones, branches, or worktrees.

- **Start fresh:** `git fetch` and rebase onto `origin/main` before deciding anything; a stale clone can diverge silently.
- **Claim before work:** search open PRs/issues first, then open a draft PR early — the draft PR *is* the claim; don't duplicate in-flight work.
- **Isolate:** always a branch (worktree preferred), never a shared checkout's main; use per-session scratch dirs and don't bind fixed ports.
- **Flush at the end:** push (`--force-with-lease` only) and open/update your PR before finishing — unpushed work is invisible work.
- **Main moves only by PR merge.**
