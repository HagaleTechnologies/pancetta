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
| `pancetta-dx` | DX cluster + PSKReporter + per-QSO ClubLog/QRZ upload (opt-in) + scaffolded LoTW | Live + scaffolded |
| `pancetta-hamlib` | Hamlib CAT control FFI | Bindings done, integration stub |
| `pancetta-cqdx` | cqdx.io HTTP client, cache, types | Delta-adapted, needs live API validation |
| `pancetta-tui` | Terminal UI | Wired to pipeline (default UI; `--headless` to disable); live autonomous panel + `a` toggle (Shift+Q recovery) + TX-active badge; QSO-detail panel live (per-QSO state/last TX+RX message/reports via enriched ActiveQsosSnapshot, Batch 94); worked-before flags in Band Activity + DX hunter (same `CachedStationLookup` the autonomous scorer uses) and real rig S-meter via hamlib STRENGTH polling (Batch 95) |
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
- **Manual vs. automated calling semantics** (`pancetta-qso/src/qso_manager.rs`, `pancetta-qso/src/autonomous.rs`): A `CallInitiation::{Manual,Auto}` flag on each QSO (in `QsoMetadata`) drives two policy differences. **Manual calls** (operator Space/CallStation → `respond_to_cq_manual`) bypass the self-duplicate gate (the operator explicitly chose to work/re-work the DX) and **keep-call every TX slot** until the DX answers or a watchdog fires. The watchdog (`check_timeouts_at`) retires a manual `RespondingToCq` QSO after **5 minutes OR 10 calls, whichever first** (`TimeoutConfig::{manual_call_watchdog_minutes, manual_call_max_calls}`, defaults 5/10); on expiry it transitions to `Failed` and clears the callsign mapping. Keep-calling is driven by `rearm_manual_calls_at` in the QSO manager's 5s timeout loop, which re-emits one `QsoEvent::MessageToSend` (CqResponse) per slot — the coordinator's existing QSO event loop forwards it as a `TransmitRequest` with the latched `tx_parity`. **Automated calls** keep the duplicate gate AND additionally yield to a busy DX: the autonomous operator tracks callsigns seen in a non-CQ third-party exchange (report/RR73/73 not directed at us) in `recently_in_qso`, and suppresses an auto-response to any such station that CQs again within `AutonomousConfig::dx_busy_window_secs` (default 90s). Both shipped 2026-06-13 in response to an operator duplicate-QSO bug.
- **Manual CQ as a real `CallingCq` QSO** (`pancetta-qso/src/qso_manager.rs::start_cq_manual`, `pancetta/src/coordinator/{tui_relay.rs,qso.rs}`, `QsoMessage::{StartCq,StopCq}`): pressing **`c`** no longer runs a separate text-only CQ-repeat loop. The TUI's `StartCq` is routed to the QSO component, which calls `start_cq_manual` to create a tracked **manual** `CallingCq` QSO. That QSO **owns the CQ transmission** (single TX source per slot — no double-TX): it emits a `StateChanged` (Idle→CallingCq, so the coordinator keys it into `active_tx_qsos` and the TX worker will key PTT for it) and an opening `Cq` `MessageToSend`, then `rearm_manual_calls_at` re-emits one `Cq` per FT8 slot (its `CallingCq` arm, gated to ~1/slot via `last_call_at`) bounded by the **same manual watchdog** (5 min / 10 calls; `check_timeouts_at` covers `CallingCq` too). When a station answers, the existing `CallingCq + CqResponse → WaitingForReport` arm fires and the `CallInitiation::Manual`-gated auto-reply emitter sequences our report → RR73 through to `Completed` + `QsoCompleted` (ADIF log), latching the caller's grid. `tx_parity` is `None` (calling CQ we pick our own slot; the TX scheduler resolves it via the configured self-parity fallback). If multiple stations answer, the first valid `CqResponse` advances the single QSO; later answers no longer match (state moved past `CallingCq`) — one QSO per CQ. **`s`** (StopCq) sends `QsoMessage::StopCq`, which cancels only QSOs still in `CallingCq` (an already-answered exchange is left to finish); TX-policy-away-from-Full, Shift+Q (CancelAllQsos), `g`→Disabled, and F8 also cancel it. Shipped 2026-06-15, closing the "we can't actually run a CQ-initiated QSO" gap.
- **Auto-resend 73 after manual completion (item-2-auto-73)** (`pancetta/src/coordinator/qso.rs`): when a station we JUST completed a **manual** QSO with keeps re-sending us `RR73`/`RRR` (they didn't copy our 73), the coordinator auto-re-sends our 73 — bounded so a stuck DX can never make us TX forever. On `QsoEvent::QsoCompleted` with `metadata.initiated_by == Manual`, the QSO component stashes `(uppercased callsign → {completed_at, freq, dx_parity, resends, last_resend_at})` in a per-task `RecentManualCompletions` map (autonomous completions are NOT stashed). In the decode loop, a directed-at-us `FinalConfirmation` (RR73/RRR with `to_station == our call`) from a stashed callsign triggers `maybe_auto_resend_73`, which sends our 73 via the same `respond_to_caller(SeventyThree)` path the Callers/Space close uses (the resulting Completed QSO rides the drop-stale-TX grace window — the 73 frame goes out then drops). **Bounds: 3-minute window, max 3 resends per completed QSO, at most one per ~15 s slot.** Gates: respects the global `TxPolicy` (`allows_any_tx()` — RESPOND-ONLY allows, DISABLED blocks), manual-only (enforced at populate time), and skips if a live QSO with the station is already active. The counter is incremented under the map lock before the send so two decodes racing in one slot can't both fire.
- **QSO logging — ADIF + SQLite hybrid**: `~/.pancetta/qsos.adi` is the durable, append-only ADIF source of truth (vendor-neutral; point WSJT-X / N1MM / LoTW / eQSL at this file directly). `~/.pancetta/qso.db` is a sqlx-backed queryable index rebuilt from the ADIF on startup if missing or stale — safe to delete. `AdifLogWriter` (pancetta-qso) writes ADIF records; `AsyncQsoLogger` (pancetta) persists to both stores. Existing operators auto-migrate: first startup after upgrade exports the legacy DB into a fresh ADIF before flipping over.
- **Per-QSO online-logbook upload (opt-in)**: on `QsoEvent::QsoCompleted`, the coordinator (`pancetta/src/coordinator/qso.rs::start_qso_upload_subscriber`) uploads that single QSO as one ADIF record (rendered identically to the source-of-truth `qsos.adi` record via `AdifProcessor::qso_to_adif` → `generate_record`) to **ClubLog** (`https://clublog.org/realtime.php`) and/or **QRZ Logbook** (`https://logbook.qrz.com/api`, `ACTION=INSERT`). Clients live in `pancetta-dx/src/qso_upload.rs` (`ClubLogClient`, `QrzLogbookClient`, `parse_qrz_response`). Both default OFF and require config under `[network.clublog]` / `[network.qrz_logbook]` (see `docs/CONFIG.md`); validation rejects enabled-without-credentials. Uploads are best-effort, spawned per-upload, and never block/fail the QSO pipeline; a QRZ duplicate is a non-fatal outcome. Credentials stay local and are never logged (target `qso.upload`). **LoTW auto-upload remains a TODO** (`TODO(lotw)` in `pancetta-dx/src/lotw.rs`): it needs TQSL digital-signature signing, not a raw ADIF POST.
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
- **Compound-callsign equivalence (catalog C18 / peer D4)** (`pancetta-qso/src/exchange.rs::{base_callsign,callsigns_match}`, applied in `qso_manager.rs::{is_us,is_partner}`): a station may sign a compound call (`EA8/G8BCG`, `G8BCG/P`, `K1ABC/R`, `VK9/W1XYZ/MM`) and later in the SAME QSO sign its bare base (`G8BCG`), or vice versa — same operator. WSJT-X/JTDX stall here because sender-verification compares the *displayed* call to the latched partner; pancetta does not. `callsigns_match` treats two calls as the same station iff they are byte-identical (uppercased) OR share a `base_callsign` — the longest `/`-separated component that is callsign-shaped (≥3 chars, has a digit AND a letter), which picks the home call over a country prefix or a portable suffix. It is conservative: genuinely different calls (`K5ARH`/`K5ARG`, `G8BCG`/`G8BCH`) extract distinct bases and never match, so the sender-mismatch security tests still reject impostors. Every `from == DX` (`is_partner`) and `to == us` (`is_us`) check in `determine_state_transition` + `is_message_relevant` routes through it, so a mid-QSO compound↔base change no longer stalls. `validate_callsign` was widened to accept compound forms (valid base + short prefix/suffix tokens) so compound frames parse instead of erroring out of the pipeline. The logged `their_callsign` is upgraded to the most-complete (longest matching) form seen — the compound carries DX/portable info. Tests: `pancetta-qso/tests/adversarial_compound_calls.rs`.
- **Tri-state global TX policy** (`pancetta_core::TxPolicy` = `Full` | `RespondOnly` | `Disabled`, default `Full`): the operator's master TX switch, stored in the coordinator as `tx_policy: Arc<AtomicU8>` (orthogonal to `autonomous_enabled_runtime`). Operator cycles it with `g` in the TUI (Full → RespondOnly → Disabled → Full); Shift+Q emergency stop forces `Disabled`. Each state is echoed back as `MessageType::TxPolicyStatus` → `TuiMessage::TxPolicyUpdate` and rendered as a **bold, color-coded title-bar banner** (GREEN "TX: FULL", YELLOW "TX: RESPOND-ONLY", RED "TX: DISABLED — RX ONLY"). Gating map: **Disabled** = hard mute at the TX worker (`coordinator/tx.rs`, both `TransmitRequest` and `MultiTransmitRequest` arms) — never keys PTT / plays audio / modulates, consumes the request, reports a failed `TransmitComplete` + clears the TUI TX view. **RespondOnly** = suppress *initiations only*, gated at the sources: `StartCq` + `CallStation` refused in `tui_relay.rs` (with operator warning), repeating-CQ loop stopped, and autonomous initiation items (the `qso_id == None` `OperatorAction::Transmit`s, i.e. CQ-self + hunt/pounce) dropped in `coordinator/autonomous.rs` while QSO-in-progress items (`qso_id == Some`) and `RespondToCaller`/`QsoEvent::MessageToSend` flow through normally. **Full** = everything (legacy behavior). Autonomous initiation additionally requires the `autonomous_enabled_runtime` gate open. A **NOW-SENDING / QUEUED** TX strip (`pancetta-tui/src/ui/mod.rs::render_tx_strip`) shows the keyed message text + audio freq and items dequeued-but-not-yet-on-air, fed by `MessageType::TxQueueStatus { sending, queued }` from the TX worker (lightweight scope: reflects the request the worker is currently scheduling, not a deep channel-backlog scan).
- **Drop-stale-TX gate (ended-QSO TX purge)** (`coordinator/{mod.rs,qso.rs,tx.rs}`): closes the "we keep TXing every cycle after a QSO ended / only a restart fixes it" bug. The coordinator holds `active_tx_qsos: Arc<RwLock<HashSet<String>>>` (uppercased+trimmed qso ids, keyed via `active_tx_qso_key`). The QSO component keeps it in sync from the `QsoEvent` stream: **inserts** on `StateChanged` into any non-terminal active state; **removes immediately** on `StateChanged` into terminal `Failed` (covers Superseded / UserCancelled / Timeout / SignalLost) and on `QsoFailed`; **removes after a ~16s grace** (one slot, spawned delayed task) on `QsoCompleted` so the final 73 still goes out but leftover backlog is dropped next slot. The TX worker (`tx.rs`, helper `tx_qso_is_live`, fails *open* on a poisoned lock) re-checks at the last instant before keying PTT — after the pre-PTT slot sleep and at defer-time — and drops any `TransmitRequest`/`MultiTransmitRequest` item whose `qso_id` is no longer in the set: no PTT, clears the TX strip, sends a failed `TransmitComplete`, logs `target:"tx.policy"` "dropping stale TX for ended QSO …". Requests with `qso_id == None` (manual free-text / tune / test-TX) are never gated. The live-TX indicator is also made unmistakable: bold white-on-red `🔴 TX NOW` chip + frame in `render_tx_strip` for the full ~12.64s the message is keyed, mirrored on the QSO Status panel's "Now:" line (`ui/qso_status.rs::live_tx_for_qso`).
- **Coordinator-level QSO sim harness** (`pancetta/tests/coord_sim.rs`): a durable, reusable `CoordSim` fixture that exercises the *coordinator's* TX gate + mock-rig PTT + multi-stream path, complementing the engine-level state-machine harness in `pancetta-qso/src/sim.rs`. It stands up a real `MessageBus`, a real `QsoManager`, a real `MockRig` behind a hamlib consumer (mirror of `coordinator/hamlib.rs`'s `SetPtt` handling), and the *real* shared `active_tx_qsos` set + `tx_policy` atomic. A scenario starts/advances QSOs via the manager's real entry points (`respond_to_cq_with` / `respond_to_caller` / `start_cq`), calls `pump_qso_events()` (a faithful mirror of `coordinator/qso.rs`'s populater — insert on `StateChanged→active`/`QsoCompleted`, remove on `Failed`/`QsoFailed`, forward `MessageToSend`→`TransmitRequest`), then `drive_slot(pending)` which replicates the worker's keying-decision chain — **policy hard-mute → coalesce (real `coalesce_transmit_requests`) → Step-4b gate (real `tx_qso_is_live` over the real set) → key/audio/unkey** — sending real `RigControl(SetPtt)` over the bus to the mock rig and **asserting at the rig level** (`mock.get_ptt() == On`, offset, release). **Determinism**: no `schedule_tx` UTC math and no slot sleep (that's unit-tested in `tx.rs::schedule_tx_tests`); only bounded ms `await`s — pass/fail never depends on wall-clock slot phase. A `Timeline` (keyed / dropped / per-slot offsets, mirroring `sim::Timeline`'s style + Display) carries the readable assertion helpers. Permanent scenarios: PTT-keys-for-scheduled-QSO (StateChanged-at-start fix), stale-TX-dropped-after-supersede (no PTT), coalesce-backlog (newest wins, older not keyed), two-simultaneous-QSOs on distinct freqs, TX-policy Disabled=silent / RespondOnly=in-progress-keys-but-initiation-suppressed / Full=keys, requested-offset-honored, manual-send-never-gated. Made testable by re-exporting `coalesce_transmit_requests` / `CoalesceEntry` / `resolve_required_parity` and widening `active_tx_qso_key` / `tx_qso_is_live` to `pub` from `coordinator/mod.rs` (visibility-only, behavior-preserving).

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
