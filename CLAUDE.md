# CLAUDE.md

Project instructions for Claude Code when working in this repository.

## Project Overview

Pancetta is an autonomous FT8 ham radio station written in Rust. The goal is a fully operational on-air system: decode, call, complete QSOs, and log — with priority-based station selection, multi-stream TX, and integration with cqdx.io.

## Workspace Structure

12-crate Cargo workspace:

| Crate | Purpose | Status |
|-------|---------|--------|
| `pancetta-ft8` | FT8 encoder/decoder/modulator/OSD | Production-grade, ~295 tests, bit-exact with ft8_lib/WSJT-X |
| `pancetta-audio` | Real-time audio I/O (cpal + ringbuf) | Functional |
| `pancetta-dsp` | DSP pipeline (FFT, filtering, resampling) | Functional |
| `pancetta-config` | Configuration with hot-reload | Production-ready, ~59 tests |
| `pancetta-qso` | QSO management, priority scoring, frequency allocation, autonomous operator | Core logic, ~60 tests |
| `pancetta-dx` | DX cluster + PSKReporter + scaffolded LoTW | Live + scaffolded |
| `pancetta-hamlib` | Hamlib CAT control FFI | Bindings done, integration stub |
| `pancetta-cqdx` | cqdx.io HTTP client, cache, types | Delta-adapted, needs live API validation |
| `pancetta-tui` | Terminal UI | Scaffold, not wired to pipeline |
| `pancetta-core` | Shared types, error handling | Stable |
| `pancetta` | Main binary, coordinator, message bus, runtime | Integration point |
| `pancetta-research` | Local-only iteration harness for decoder improvements (scorecards, eval, hypothesis bank). **Excluded from CI; never builds in GitHub Actions.** | Plan 1 of 3 in progress |

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

## Architecture Highlights

- **Coordinator** (`pancetta/src/coordinator/`): Central orchestrator, manages decode→decide→transmit pipeline. Decomposed into submodules: `mod.rs`, `pipeline.rs`, `components.rs`, `health.rs`, `hamlib.rs`, `shutdown.rs`, `wav_playback.rs`, `util.rs`.
- **Autonomous operator** (`pancetta-qso/src/autonomous.rs`): Decision engine — hunt mode (pounce on rare stations), CQ mode (answer callers), hybrid mode. Configurable priority weights.
- **Priority scoring** (`pancetta-qso/src/priority.rs`): Weighted scoring — needed DXCC > needed grid > POTA/SOTA > rarity. Duplicate suppression and failure backoff.
- **SmartFrequencyAllocator** (`pancetta-qso/src/frequency.rs`): 7 soft-scored criteria for TX frequency selection. Enables parallel QSOs at different audio frequencies.
- **Multi-stream TX**: Supports N simultaneous FT8 signals in a single 15-second slot.
- **DX-slot-aware TX scheduling** (`pancetta/src/coordinator/tx.rs`): WSJT-X-style. Every `DecodedMessage` carries `slot_parity`; the QSO state machine latches `tx_parity = opposite_of(dx_parity)` at QSO start; the TX scheduler picks the next slot of that parity, padding silent samples if early or skip-ahead-cursoring into the modulated waveform if late (up to `tx_late_max_ms`, default 8s). Past that, defers 30s. Never collides with the DX's parity. See `docs/superpowers/specs/2026-04-27-dx-slot-aware-tx-design.md`.
- **QSO logging — ADIF + SQLite hybrid**: `~/.pancetta/qsos.adi` is the durable, append-only ADIF source of truth (vendor-neutral; point WSJT-X / N1MM / LoTW / eQSL at this file directly). `~/.pancetta/qso.db` is a sqlx-backed queryable index rebuilt from the ADIF on startup if missing or stale — safe to delete. `AdifLogWriter` (pancetta-qso) writes ADIF records; `AsyncQsoLogger` (pancetta) persists to both stores. Existing operators auto-migrate: first startup after upgrade exports the legacy DB into a fresh ADIF before flipping over.
- **Decoder research harness** (`pancetta-research/`, `research/`,
  `scripts/research-env.sh`): a local-only iteration harness for improving
  the decoder. Excluded from `default-members` and CI by construction.
  Spec: `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.
  Plans 1-3 complete; the loop is operational. Run `./scripts/research-env.sh --status`
  to see active experiments; read `research/hypothesis_bank.md` for the
  current backlog.
- **Hardware-tier auto-classification** (`pancetta/src/coordinator/tier.rs`,
  hb-216 S2): on coordinator startup, the host is classified into
  Fast / Moderate / Slow via a background `probe_hardware_tier(10)` call
  (or a cache hit from `~/.pancetta/tier_cache.json` keyed on
  `(cpu_model, core_count, pancetta_version)`). Moderate/Slow tiers
  flip the `scoped_fast_path: Arc<AtomicBool>` (replaces the old
  env-var read in the FT8 hot loop). Tier-driven `Ft8Config` rewrites:
  Fast and Moderate tiers run plain defaults (the Batch 36/41 Fast
  preset `mp=2, ldpc=200` was retired in Batch 83 — under ft8_lib truth
  it bought +24..+57 TPs for +142..+387 FPs at 2.6-3.9× decode time,
  strictly dominated by the documented `ldpc_iterations=300` recall
  lever); Slow tier rewrites to `max_decode_passes = 1` +
  `max_sync_candidates = 150` (Batch 78; its pre-Batch-72
  `osd_depth = Some(1)` rewrite was dropped — it would now *raise* OSD
  depth above the `Some(0)` default). Operator override: `PANCETTA_SCOPED_FAST_PATH=1` forces
  on, `=0` forces off, both skip the tier-driven preset. Spec:
  `docs/superpowers/specs/2026-06-04-hb-216-s2-tier-wiring-design.md`.
- **QSO sender verification**: The QSO state machine (`pancetta-qso/src/qso_manager.rs::determine_state_transition` and `is_message_relevant`) verifies `from_station == expected DX callsign` on every state-advance. Mismatches are logged at `warn!` level (`target: "qso.security"`) and discarded. Frequency tolerance is 15 Hz. The autonomous responder (`autonomous.rs`) tracks per-callsign response timestamps in `recently_responded_to` and skips CQs from callsigns it responded to within the last 60s. Both defenses landed 2026-04-29 in response to Security Review C-1 and I-1; see `docs/security-review-2026-04-29.md`.

## Development Phases (End-to-End QSO Initiative)

Design spec: `docs/superpowers/specs/2026-04-02-end-to-end-qso-design.md`

- **Phase 1** (complete): Loopback QSO — CQ-to-73 exchange through full pipeline, state machine tests
- **Phase 2** (complete): Autonomous operator + priority engine — configurable weighted scoring, POTA/SOTA detection
- **Phase 3** (complete): Multi-stream TX — SmartFrequencyAllocator, multi-slot decision logic, dual QSO loopback test
- **Phase 4** (complete, 2026-04-26): Hardware integration — hamlib CAT control via rigctld short-form commands, real rig TX validated on FTdx10 (DT 0.2, ALC clean), tail-end message decoded on PSKReporter across NA + EU
- **Phase 5** (next): Full autonomous QSO loop — enable autonomous operator with antenna, complete a CQ→grid→report→RR73 exchange end-to-end

## Known Gaps and TODOs

- Grid "needed" set never populated (cqdx.io has no grid-needed endpoint yet); `is_needed_grid` returns `false` when empty to avoid inflating scores
- cqdx.io `GET /api/v1/spots?live=true` response envelope key (`groups`) unverified against live API — a gated live test exists: `CQDX_TOKEN=pat_xxx cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture`
- ~~`auto_sequencer::evaluate_cq_call` slot_parity gap~~ — RESOLVED-AS-STALE (2026-06-11 audit): the autonomous CQ-response path threads `tx_parity = cq.slot_parity.opposite()` (`autonomous.rs` RespondToCq build), and live mid-QSO TX flows through `QsoManager::send_message` → `QsoEvent::MessageToSend`, which carries the parity latched at QSO start. The only `tx_parity: None` site (`autonomous.rs` pending_sequencer_messages drain) has no production caller; documented in-code.

## Documentation Maintenance

After completing significant work, update affected documentation:

- **Inline docs**: Update `///` and `//!` comments on modified public items
- **CLAUDE.md**: Update known gaps, build instructions, or project phases
- **docs/ARCHITECTURE.md**: Update if crate relationships or data flows changed
- **README.md / FEATURES.md**: Update if user-facing capabilities changed

Documentation policy:

- `pancetta-core` enforces `#![warn(missing_docs)]` (zero warnings as of
  the last documentation pass).
- `pancetta-hamlib` enforces `#![deny(missing_docs)]`.
- All other crates carry `#![allow(missing_docs)] // TODO: documentation
  pass pending — see CONTRIBUTING.md`. As docs land, switch each crate
  to `warn` (and eventually `deny`) and clear the TODO.

## Build Hygiene

The `target/` directory can balloon to 40-50GB with stale incremental compilation caches. Run periodically:

```bash
cargo sweep --installed          # remove artifacts from unused toolchains
cargo sweep --maxsize 10GB       # cap target/ size
```
