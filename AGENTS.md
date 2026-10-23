# AGENTS.md

Project instructions for AI coding agents (Claude Code, Codex, and others) working in this repository.

## Cross-repo Contracts

Clone `dispensa` as a sibling directory. `git pull --rebase` it and scan `questions/` + `contracts/` before any cross-cutting work. Propose changes there first.

Interface changes with other repos are proposed in `dispensa` first (contracts/ + questions/) — check dispensa's contracts/questions before any cross-cutting change.

## Project Overview

Pancetta is an autonomous FT8 ham radio station written in Rust. The goal is a fully operational on-air system: decode, call, complete QSOs, and log — with priority-based station selection, multi-stream TX, and integration with cqdx.io.

## Workspace Structure (14-crate Cargo workspace)

| Crate | Purpose | Status |
|-------|---------|--------|
| `pancetta-ft8` | FT8 encoder/decoder/modulator/OSD | Bit-exact with ft8_lib/WSJT-X |
| `pancetta-audio` | Real-time audio I/O (cpal + ringbuf) | Functional |
| `pancetta-dsp` | DSP pipeline (FFT, filtering, resampling) | Functional |
| `pancetta-config` | Configuration loading, validation; file watch live-applies remote-access consent and revocations only | Production-ready |
| `pancetta-qso` | QSO management, priority scoring, frequency allocation, autonomous operator | Core logic |
| `pancetta-dx` | DX cluster + PSKReporter + per-QSO logbook upload (ClubLog/QRZ/cqdx/LoTW/eQSL) + QRZ XML lookup | Live; eQSL awaiting live-account check; LoTW web-scrape module is unused scaffolding |
| `pancetta-hamlib` | Hamlib CAT/PTT via rigctld TCP client (native FFI removed) | Live; on-air validated (FTdx10) |
| `pancetta-cqdx` | cqdx.io HTTP client, cache, types | Live; spots envelope unverified against live API |
| `pancetta-tui` | Terminal UI | Default UI (`--headless` to disable) |
| `pancetta-core` | Shared types, error handling | Stable |
| `pancetta-agent` | Remote-TX security: arm gating, session binding | Live |
| `pancetta-protocol` | Remote-operation wire protocol | Live |
| `pancetta-research` | Local-only decoder-iteration harness | Excluded from CI; never builds in GitHub Actions |
| `pancetta` | Main binary, coordinator, message bus, runtime | Integration point |

Operator-facing capability state: README.md § Capability status — keep both consistent.

## Building and Testing

```bash
# Full workspace build
cargo build

# Run all workspace tests
cargo test --workspace --features transmit

# FT8 tests (encoder is feature-gated behind `transmit`)
cargo test --features transmit -p pancetta-ft8    # all FT8 tests
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
- Remote peers are identified only by the keyId proven in their Noise msg1 (relay `src` is routing metadata), and nothing is sent to a peer until it holds a currently-valid `status`+ capability from a verified `hello`.
- A remote peer's authority (admission, token expiry, revoked jti, scope) is re-checked on every action and every outbound frame; an authorization refresh that drops a peer evicts its live session, deny-lists its hello token, and disarms it if it held control (PAN-96).
- The local gateway sends nothing before a `hello` whose `gatewayToken` matches `[network.remote_gateway].auth_token`; it refuses browser Origins not in `allowed_origins` and binds a non-loopback address only with `allow_network_exposure = true` (PAN-99).
- Drop-stale-TX: the worker re-checks QSO liveness and slot-timing headroom (`tx_late_max_ms`
  elapsed in the target slot) at the last instant before PTT; an over-cap frame is dropped,
  never keyed (PAN-117).
- Key-down never waits on the bus channel table: every PTT-on (single TX, bundle, tune, manual PTT) goes through `tx::send_ptt_command` → `MessageBus::send_rig_command_if`, which is synchronous and lock-free on the route `create_channel(Hamlib)` publishes, so nothing can suspend between the last pre-PTT gate and the enqueue (PAN-202).
- The TX worker revalidates its snapshotted relocation epoch after acquiring `ptt_sync_gate` and refuses, never keys, a frame whose QSO `apply_tx_offset_switch` moved between that snapshot and the gate (PAN-145).
- The TX worker never sends into its own `Ft8Transmitter` bus channel: messages it took off the channel and has not handled go back on the front of its coordinator-owned `TxRequeue`, which it reads before the channel, so no concurrent producer can overtake them and a full channel cannot drop them (PAN-73).
- The TX hard mute (`tx_hard_mute_reason`) also refuses while the rig CAT link is `PollingFailed` or the audio output has stopped calling back (PAN-115). Both clear on their own when the link/device recovers, and the Hamlib poll loop must keep redialling rigctld while disconnected.
- Band-plan gate (PAN-118): every PTT-on path — single TX, bundle item, tune, manual PTT, any origin — also refuses, before PTT, when the absolute TX RF (effective split-aware dial + audio offset) falls outside the configured band plan (`edge_warnings` is the master switch; skipped while the dial is the unknown-0 sentinel); bundles drop only the offending items wherever items are still separable, and hold the whole bundle once the waveform is summed.
- `mode=FT8` paths must remain byte-identical when FT4/FT2 features are untouched.
- `merge_with` must carry every config field (see the §5 config-merge guardrail).
- Every transmitted frame (single or multi-TX bundle item) reflects the freshest `MessageToSend` the QSO engine emitted, and the bus accepted, for that qso_id at key-time, or at the moment of an operator-triggered mid-TX abort+re-key, whichever is later.
- TX outcome reports — `TransmitComplete` (→ Autonomous), `TxFrameNotSent`/`TxFrameSent` (→ Qso) — go through `MessageBus::send_message_durable`, and those components read through a `DurableInbox`; a full channel parks them, never drops them (PAN-185).
- At most one QSO object exists per (callsign, band) among active-or-recently-completed QSOs; repeated manual actions resolve to it, never spawn a sibling.
- Readiness strip: blockers and warnings render from typed state every frame. No status-line write can erase them, decodes never write the status line, and the strip's TX verdict comes from `tx_mute_causes`, which is parity-tested against `tx_hard_mute_reason` (PAN-89).
- The coordinator supervisor auto-restarts a crashed Autonomous/DxCluster/PskReporter/RemoteGateway/Qso/Dsp/Ft8Decoder/Hamlib/StationAgent/Ft8Transmitter task (5 restarts/10min, capped-exponential backoff). Restart teardown guarantees PTT release for Hamlib and definitive disarm for StationAgent; TX stays inhibited throughout a Hamlib restart window. Audio deliberately remains `DegradeOnly`. QSO-engine loss surfaces as `SupervisorRestart`; dependency loss as `ComponentCrash` with component-specific QSO scope.

## Where Things Live

- `docs/superpowers/specs/` — design specs (authoritative for current behavior)
- `docs/DECISIONS/` — decision digests by subsystem (tx-scheduling, qso-engine, modes, remote-operation, tui, logging-uploads, config-and-platform); known gaps/TODOs live in `2026-07-development-phases-and-gaps.md`
- `docs/ARCHITECTURE.md` — crate relationships and data flows
- `docs/CONFIG.md` — configuration reference
- `docs/fcc-part97-compliance.md` — regulatory / TX-safety notes
- `CHANGELOG.md` — release history

**Documentation policy:** keep AGENTS.md focused as the standing brief — decision narratives, feature history, and gap tracking go to `docs/DECISIONS/`, never appended here.

## Build Hygiene

The `target/` directory can balloon to 40-50GB with stale incremental compilation caches. Run periodically:

```bash
cargo sweep --installed && cargo sweep --maxsize 10GB   # unused toolchains, then cap target/ size
```

## Knowledge Wiki and Multi-Agent Hygiene

`wiki/INDEX.md` maps accumulated knowledge — read it before deep exploration; run the wiki-update process after substantive work to distill gotchas/decisions into it (or docs/ if normative). The wiki is descriptive; code and docs/ win any conflict.

You are never alone in this repo — other agents may work concurrently in other clones, branches, or worktrees.

- **Start fresh:** `git fetch` and rebase onto `origin/main` before deciding anything; a stale clone can diverge silently.
- **Claim before work:** search open PRs/issues first, then open a draft PR early — the draft PR *is* the claim; don't duplicate in-flight work.
- **Isolate:** always a branch (worktree preferred), never a shared checkout's main; use per-session scratch dirs and don't bind fixed ports.
- **Flush at the end:** push (`--force-with-lease` only) and open/update your PR before finishing — unpushed work is invisible work.
- **Main moves only by PR merge.**


## Code review convergence

PR review convergence policy (round-based, per-reviewer): see the
`resolve-review-feedback` and `land-pr` Claude Code skills (credenza,
symlinked into `~/.claude/skills/`). Canonical bot-reviewer-facing text:
docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md — synced from
credenza; edit
`credenza/claude/skills/resolve-review-feedback/references/convergence-policy.md`
instead of hand-editing this copy.
