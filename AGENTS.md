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
| `pancetta-ft8` | FT8 encoder/decoder/modulator/OSD | Bit-exact with ft8_lib/WSJT-X; crates.io-ready with pancetta-core (CI dry-run), not yet published |
| `pancetta-audio` | Real-time audio I/O (cpal + ringbuf) | Functional |
| `pancetta-dsp` | DSP pipeline (FFT, filtering, resampling) | Functional |
| `pancetta-config` | Configuration loading, validation; file watch live-applies selected autonomous policy, TX timing, receive gain presets and remote-access consent/revocations (operational reload unreleased) | Production-ready |
| `pancetta-qso` | QSO management, priority scoring, frequency allocation, autonomous operator | Core logic |
| `pancetta-dx` | DX cluster + PSKReporter + per-QSO logbook upload (ClubLog/QRZ/cqdx/LoTW/eQSL) + QRZ XML lookup | Live; eQSL awaiting live-account check; LoTW web-scrape module is unused scaffolding |
| `pancetta-hamlib` | Hamlib CAT/PTT via rigctld TCP client (native FFI removed) | Live; CAT/PTT on-air validated (FTdx10); opt-in receive overload ATT guard, configured data-mode control and TX power/ALC meter reads are hardware-unverified |
| `pancetta-cqdx` | cqdx.io HTTP client, cache, types | Live; spots envelope unverified against live API |
| `pancetta-tui` | Terminal UI | Default UI (`--headless` to disable) |
| `pancetta-core` | Shared types, error handling, offline DXCC entity resolver | Stable; crates.io-ready with pancetta-ft8, not yet published |
| `pancetta-agent` | Remote-TX security: arm gating, session binding | Live |
| `pancetta-protocol` | Remote-operation wire protocol | Live |
| `pancetta-research` | Local-only decoder-iteration harness | Local-only; excluded from CI except a non-required compile-only check lane (PAN-372) |
| `pancetta` | Main binary, coordinator, message bus, runtime | Integration point |

LoTW outbox retries, optional certificate passwords and duplicate tqsl exits 8/9 are integration-tested with fake tqsl, unreleased; live-account qualification remains pending.

Operator-facing capability state: README.md § Capability status — keep both consistent.
ADIF entity enrichment (PAN-306) is implemented, unreleased: offline COUNTRY/DXCC/CQZ/ITUZ with token zone overrides, stored-field precedence and unknown omission, including export-time enrichment. Automated coverage only; live qualification pending.
Silent audio-output stall recovery (PAN-295) is implemented, unreleased: output-watchdog forced reopens capped at 3 per rolling 10 min (input-stale and processing-error recovery unchanged), TX muted until callbacks resume, limit shown on the readiness strip; hardware qualification pending.
N1MM contactinfo UDP (PAN-382) is implemented, unreleased: opt-in `[logging] n1mm_udp`, one datagram per logged QSO built from the ADIF record, suppressed under `--replay`; docs/INTEROP.md holds the logger compatibility table, every row unverified.
POTA/SOTA log references (PAN-359) are implemented, unreleased: `[station] my_sig`/`my_sig_info` become ADIF MY_SIG/MY_SIG_INFO on every logged QSO, and a partner that cqdx.io's `GET /api/v1/pota` feed listed within 30 minutes gets SIG/SIG_INFO, stamped once in `QsoManager::emit_event`; automated coverage only, no live-service or upload-acceptance qualification.
TUI QRZ station lookup (PAN-343) is implemented, unreleased: Shift+L uses configured XML credentials, a two-second deadline and a session cache; live-account qualification is pending.
Tier-based pouncing, decode alerts (bell/ntfy/webhook), persistent operator watchlist pins, the local logbook needed/ATNO/confirmed fallback with LoTW file import and opt-in confirmation-based need, and per-target upload state in Recent QSOs (row glyphs, selected-contact detail, completion toast) are implemented, unreleased; live notification-service and on-air qualification are pending.
CQ DX and regional CQ modifier gating in autonomous pouncing (PAN-292) is implemented, unreleased: offline continent data, fail-open on unresolved calls; automated coverage only, on-air qualification pending.
The autonomous band scheduler (PAN-385) is implemented, unreleased: `[[autonomous.band_plan]]` UTC windows acting on entry, needed-activity hops from cqdx.io spots and a grey-line tie-break, all under `band_hopping.enabled`; automated coverage only, on-air and live cqdx qualification pending.
WSJT-X decode ingest (PAN-381) is implemented, unreleased: a loopback `decode_listen` socket shows live WSJT-X Decode(2) datagrams, banded by WSJT-X's Status dial, in Band Activity and DX Hunter tagged `EXT`; automated coverage only, not yet checked against a real WSJT-X.
TUI themes (PAN-387) are implemented, unreleased: Dark (unchanged), Light and HighContrast palettes selected by `[ui] theme` or `[ui.accessibility] high_contrast`, restart required; contrast is checked by snapshot tests on the xterm default palette, not yet on a real terminal.
TX audio drive (PAN-438/439) is implemented, unreleased: completed FT bursts with valid CAT ALC adjust the next burst within configured bounds. F7/F8 select Manual for the current band; F9 toggles Auto and Manual. Per-band gain and mode persist across restarts; hardware qualification pending.
One-shot TX drive calibration (PAN-440) is implemented, unreleased: Shift+C sweeps one bounded Tune carrier from the floor, stores a per-band Auto seed below ALC onset, and refuses without known ALC capability or TX permission; hardware qualification pending.
FT8 Fox slots and type 0.1 multiplexing (PAN-386) are implemented, unreleased: Fox CQ and Hound streams on 60 Hz slots in 300-900 Hz, RR73 and a report shared in one type 0.1 frame, contact logged when RR73 airs; decoded by pancetta's own decoders only, WSJT-X Hound and on-air qualification pending.
Separate data directories (PAN-376) are implemented, unreleased: a global `--data-dir <DIR>` moves config, logbook, logs, keys, the upload outbox and all other station state from `~/.pancetta` to `DIR`, and a live station holds `<data-dir>/pancetta.lock` so a second one on the same directory refuses with `another pancetta is running (pid N)`; Linux-tested only, Windows compile-checked, macOS relaunch with the flag unverified.
Serial RTS PTT (PAN-357) is implemented, unreleased: `rig.ptt.method = "serial_rts"` keys RTS on `rig.ptt.device`; VOX, plain serial and other unimplemented methods fail validation while rig control is on; hardware qualification pending.

## Building and Testing

```bash
# Full workspace build
cargo build

# Run all workspace tests
cargo test --workspace --features transmit

# FT8 tests: default features are the standalone (manta) build and include the
# encoder; the `transmit` feature adds the audio/PTT transmitter (and turns on `serde`)
cargo test -p pancetta-ft8                         # standalone build; skips test files still gated on `transmit`
cargo test --features transmit -p pancetta-ft8    # all FT8 tests

# Loopback integration tests (end-to-end QSO through encode→modulate→decode)
cargo test -p pancetta --test loopback_qso

# pancetta-hamlib: per-test current_thread tokio runtime; run single-threaded for deterministic mock-rig tests
cargo test -p pancetta-hamlib --lib -- --test-threads=1
```

New unit tests go in the same crate, `src/<module>/tests/<behaviour>_tests.rs`, declared with `#[cfg(test)] #[path = "..."] mod <behaviour>_tests;` (CONTRIBUTING.md § Unit Tests); renaming a test module renames its tests.

## Domain Context

- **Ham radio / FT8**: Digital mode protocol — 15-second slots, 8-GFSK modulation, LDPC+CRC coding, structured message exchange (CQ → grid → report → RR73)
- **Hardware target**: Yaesu FTdx10 via USB on Windows 11 MiniPC; Mac for development
- **cqdx.io**: First-party web service (owned by the developer) providing rarity scoring, needed DXCC/grid lookups, and live spots. Custom API endpoints can be built specifically for pancetta. API requirements doc: `docs/cqdx-api-requirements.md`

## Key Invariants

- macOS responsibility relaunch (PAN-264): only commands that open the microphone relaunch, and only when the responsible process is neither an `.app` nor pancetta itself. The thin parent never initialises logging, config, audio or rig; it forwards SIGTERM/SIGHUP, ignores SIGINT/SIGQUIT, and exits with the child's status. A missing private symbol, lookup failure, missing usage description, failed spawn, or `PANCETTA_MACOS_RELAUNCH=0` falls back to in-process; no other OS ever relaunches. Implementation is unreleased and not yet verified on a Mac.

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
- A TX-worker frame, bundle or tune whose PTT-on `send_ptt_command` returns `Dropped` while Hamlib's route is registered is never played or reported as aired. It unwinds like a pre-PTT refusal; a frame or bundle item reports through `report_unaired_frame`, and a tune is abandoned (PAN-278).
- Every TX-worker unkey after a key-down (Step 9, multi-TX, tune, clock-step hold guard, supersede) goes through `PttGuard::release`; the guard stands down only on `PttRelease::Delivered` (or `NoRig`, no Hamlib channel), never after a best-effort `send_message`, and `disarm()` is only for frames whose PTT-on was never enqueued; the next `PttGuard::new` waits for an armed guard's outstanding `Drop` fallback PTT-off before keying (PAN-273).
- Shutdown unkeys through the Hamlib loop (PAN-277): the shutdown flag starts an unkey-only drain. It discards key-downs and other commands without CAT I/O, and exits when empty after `hamlib_drain_stop`, with a 30 s ceiling otherwise. Shutdown queues PTT-off, stops the TX worker and settles its fallback before setting the drain stop. Hamlib gets a 10 s join budget; the independent direct rigctld unkey runs concurrently with a 6 s budget and 3000 ms command timeout.
- The Hamlib command loop clears the PTT watchdog tracker only after the rig acknowledges a PTT-off. A failed unkey keeps the original key-down time, latches `ptt_off_unconfirmed`, and makes the watchdog retry every tick; a later key-down starts a fresh timer (PAN-272).
- PTT method (PAN-357): while `rig.interface.enabled` is true, validation refuses every `rig.ptt.method` with no keying path. Under `serial_rts`, the Hamlib component's rig is `ptt_line::LineKeyedRig`, which keys only the RTS line of `rig.ptt.device` (never CAT `T 1`), reports On from `get_ptt` while the line may be asserted, and forwards every other `RigControl` method. The coordinator's single `PttLine` is opened once and reconciled at each Hamlib start, and shutdown lowers and latches it alongside the drain.
- The TX worker revalidates its snapshotted relocation epoch after acquiring `ptt_sync_gate` and refuses, never keys, a frame whose QSO `apply_tx_offset_switch` moved between that snapshot and the gate (PAN-145).
- The TX worker never sends into its own `Ft8Transmitter` bus channel: messages it took off the channel and has not handled go back on the front of its coordinator-owned `TxRequeue`, which it reads before the channel, so no concurrent producer can overtake them and a full channel cannot drop them (PAN-73).
- The TX hard mute (`tx_hard_mute_reason`) also refuses while the rig CAT link is `PollingFailed` or the audio output has stopped calling back (PAN-115). Both clear on their own when the link/device recovers, and the Hamlib poll loop must keep redialling rigctld while disconnected.
- Band-plan gate (PAN-118): every PTT-on path — single TX, bundle item, tune, manual PTT, any origin — also refuses, before PTT, when the absolute TX RF (effective split-aware dial + audio offset) falls outside the configured band plan (`edge_warnings` is the master switch; skipped while the dial is the unknown-0 sentinel); bundles drop only the offending items wherever items are still separable, and hold the whole bundle once the waveform is summed. The split input is reconciled from rigs that support readback every poll interval (PAN-206), so front-panel or third-party-CAT split cannot bypass the gate after reconciliation.
- SWR protection (PAN-251): with `swr_protection` on, the Hamlib poll loop reads SWR every tick while keyed (`ptt_active` or `ptt_on_since`, so manual PTT counts). Keyed time with valid readings (finite, at least 1.0) above `max_swr` accumulates across transmissions; when it reaches `swr_trip_delay_ms` (default 5000 ms; `0` trips on the first high reading) the shared `tx_halt` stop runs (abort, PTT-off, autonomous off, `TxPolicy::Disabled`, `CancelAllQsos`) and reports why. A reading at or below `max_swr` resets the accumulator, as does more than two minutes without a high reading; unkeyed time never counts. A rig that cannot report SWR is warned about, never blocked, and `get_swr` never invents a reading.
- TX-offset candidates have one source, `frequency::admissible_candidates`: both `allocate_smart_frequency_in_range` paths (spectral and no-snapshot) filter it by one `TxOffsetConstraints` and share one exhaustion tail (`avoid_hz` returned unchanged), and the placement instrument ranks its unconstrained grid; a new constraint kind goes into `TxOffsetConstraints`, never into one path (PAN-80).
- `mode=FT8` paths must remain byte-identical when FT4/FT2 features are untouched.
- Stable decoder API (PAN-380): the items listed in `pancetta-ft8/README.md` § Stable API and compatibility policy are pinned at compile time by `pancetta-ft8/tests/stable_api_guard.rs`. Changing one needs a minor crate-version bump while below 1.0, the README list, the guard and `CHANGELOG.md` in one PR, proposed in dispensa first.
- Registry packaging (PAN-384): `pancetta-core` and `pancetta-ft8` are the only publishable crates (`[workspace.package] publish` stays `false`), and they publish together. Each ships through an `include` whitelist. `scripts/ci/check-publish-package.sh` keeps `cargo publish --dry-run -p pancetta-core -p pancetta-ft8` green and each `.crate` under 10 MB, and a packaged `pancetta-ft8` without the ft8_lib sources fails its build instead of shipping the stub.
- External WSJT-X decodes (PAN-381) enter only through `ft8_to_tui_tx`: the decode listener never puts a decode on the message bus and never sends a datagram, so the QSO engine, Autonomous, alerts, PSKReporter, the remote gateway and the WSJT-X re-emit never see them; it binds loopback only and drops decodes until that WSJT-X has sent a Status with a dial. Space, directed replies and Shift+H (Hound engage) on an `EXT` decode or `EXT`/`EX+N` row are refused with a status line unless its WSJT-X dial is within 10 Hz (`pancetta_core::types::band::same_dial`) of pancetta's dial (the rig readback when known, else the operating frequency), its mode is pancetta's, and the readback is not a band change (`is_band_change`) from the operating frequency; an `EX+N` row with a network spot dial keeps the PAN-287 spot call on Space (PAN-435, PAN-443, PAN-429).
- Every acting consumer of `MessageType::DecodedMessage` (QSO, autonomous, WSJT-X relay) drops a decode whose `captured_dial_hz` maps to a different band than the live dial (fail-open on unknown dials) — a decode captured before a band switch is never acted on after it (PAN-69).
- `merge_with` must carry every config field, and each field merges by replacement unless its type's rule table in `pancetta-config/src/lib/tests/merge_rules_tests.rs` lists another rule (PAN-371; `docs/DECISIONS/config-and-platform.md`).
- A TX frame that will not air as itself settles through `coordinator::tx_outcome::settle_unaired_frame`: the QSO engine always gets `TxFrameNotSent` with the frame's attempt id, and Autonomous gets a failed `TransmitComplete` only when no same-QSO successor (re-key, requeue, carried bundle item, pivot) will report. A not-viable superseder is requeued, never dropped (PAN-279).
- Every transmitted frame (single or multi-TX bundle item) reflects the freshest `MessageToSend` the QSO engine emitted, and the bus accepted, for that qso_id at key-time, or at the moment of an operator-triggered mid-TX abort+re-key, whichever is later.
- TX outcome reports — `TransmitComplete` (→ Autonomous), `TxFrameScheduled`/`TxFrameNotSent`/`TxFrameSent` (→ Qso) — go through `MessageBus::send_message_durable`, and those components read through a `DurableInbox`; a full channel parks them, never drops them (PAN-185).
- At most one QSO object exists per (callsign, band) among active-or-recently-completed QSOs; repeated manual actions resolve to it, never spawn a sibling.
- A completion that queues a closing frame emits `QsoCompleted` (ADIF, uploads, WSJT-X `QSOLogged`) only when that frame's `TxFrameSent` arrives; a `TxFrameNotSent` for its last pending attempt, or expiry, reverts it to its pre-73 state (exempt from timeouts for one `COMPLETED_QSO_REWORK_GRACE`) or, for an open-at-close, to `Failed{CloseNotAired}` (PAN-280).
- Every write to `qsos.adi` (live append, import, import undo) holds the `qsos.adi.lock` file lock (PAN-214); the live writer waits at most 60 s and then writes anyway rather than drop a QSO, and undo rewrites in place, never by rename.
- Readiness strip: blockers and warnings render from typed state every frame. No status-line write can erase them, decodes never write the status line, and the strip's TX verdict comes from `tx_mute_causes`, which is parity-tested against `tx_hard_mute_reason` (PAN-89).
- The coordinator supervisor auto-restarts a crashed Autonomous/DxCluster/PskReporter/RemoteGateway/Qso/Dsp/Ft8Decoder/Hamlib/StationAgent/Ft8Transmitter task (5 restarts/10min, capped-exponential backoff). Restart teardown guarantees PTT release for Hamlib and definitive disarm for StationAgent; TX stays inhibited throughout a Hamlib restart window. Audio deliberately remains `DegradeOnly`. QSO-engine loss surfaces as `SupervisorRestart`; dependency loss as `ComponentCrash` with component-specific QSO scope.

- TUI keys (PAN-327): every bound key in every dialog and the main view resolves through `keymap::resolve` into exactly one `Action`; a new key is a chord on its documented `KEYBINDINGS` row (or `MODAL_BINDINGS` for dialog-internal keys) plus an `Action` variant and a `match action` arm in `handle_key_event`, Main-scope chords for `Action::starts_tx()` refuse Ctrl/Alt, Shift+Q resolves in Global scope before any dialog chord or text-entry rule, and any change to what a key does must re-bless `pancetta-tui/tests/fixtures/key_effects.golden` (`PANCETTA_BLESS_KEY_EFFECTS=1`).

- TUI colours (PAN-387): only `pancetta-tui/src/theme.rs` names a colour (`pancetta-tui/tests/theme_invariant.rs`); renderers read semantic tokens from `app.theme` or a `&Theme`. A new meaning gets a token with Dark, Light and HighContrast values; any change to what a theme paints must re-bless `pancetta-tui/tests/fixtures/theme_snapshots/` (`PANCETTA_BLESS_THEME_SNAPSHOTS=1`).

- Spot-call QSY (PAN-287): Space on a DX Hunter network spot changes the dial only through `coordinator::spot_call`, never while the rig is keyed (the `rig_is_keyed` recheck and the `SetFrequency` enqueue run under `ptt_sync_gate`, which every key-down passes before its PTT-on enqueue; the autonomous operator is never locked under that gate), never across a QSO the admission snapshot shows as answered, and never without a Hamlib route. A call parked for PTT-off is dropped after 30 s, on any operator frequency change or operating-mode switch, by a newer spot call, or when TX policy no longer allows initiation.

- Every autonomous band hop goes through `coordinator::band_hop::execute_band_hop`: never while the rig is keyed (the `rig_is_keyed` recheck and every send run under `ptt_sync_gate`, with the operator unlocked) and never across an exchange in progress; a hop that waits is re-evaluated next slot (PAN-385). It also never happens while Fox mode is on (the scheduler gate and the executor both refuse); a `BandChanged` that still arrives in Fox mode ends Fox mode with `FoxModeStatus { on: false }` and a status line (PAN-447).

- Fox streams (PAN-386): in FT8 Fox mode every Fox-role QSO (`metadata.fox`: the Fox CQ and each Hound answer) transmits on a 60 Hz slot in 300-900 Hz from `pancetta_qso::fox_slot_offset`, pinned for its life; a type 0.1 frame is formed only inside `encode_and_modulate_multi_tx_with_fox` from one RR73 and one report item of Fox-admitted QSOs, at the report item's slot, and each item keeps its own attempt id and outcome; a Fox-role R-report completes through the PAN-280 hold.

- A station-agent `setFrequency`/`setSplit` is refused, with an `error` frame to that peer, while the rig is keyed or while any active QSO has no `remote_client_key_id`; the Hamlib loop also drops a `StationAgent`-sourced `SetFrequency` while keyed (PAN-252).

- Data directory and instance lock (PAN-376): every default path resolves through `pancetta_core::data_dir`, set only by `main()` from `--data-dir`; a live station (no subcommand or `run`, not `--wav`/`--replay`) holds `<data-dir>/pancetta.lock` from before logging until exit and refuses to start when another process holds it, naming the pid in `pancetta.pid`; subcommands never take it, and `qsos.adi.lock` stays the per-write lock (PAN-214).

- Slot clock (PAN-358): the autonomous slot tick, DSP decode-window cutter, QSO watchdog, queued-call TTL, clock-skew monitor and TX worker read time only through the coordinator's one `pancetta_core::SlotClock`, their only wall-clock step detector (wall against monotonic, 250 ms). A step of 1 s or more is a resync: the autonomous `SlotTimer` re-anchors to the first boundary after the step without firing a pre-step deadline and drops the decodes gathered before it, and the DSP cutter's `SlotTimer` re-anchors the same way and flushes its buffer (also on a lag of more than one slot with no step). The TX worker (PAN-483) drops a frame scheduled under an older resync epoch in the pre-PTT wait, at Step 4d before PTT-on, and before and after the Step 6 hold, where it releases PTT first. `pancetta/tests/slot_clock_invariant.rs` keeps the deleted per-site detectors out, including the TX worker's PAN-114 checks and direct wall-clock reads in its slot-timing files (PAN-484). Decode parity and the A7 seed age come from the slot the DSP cut the window from; the always-answer caller key, keep-call TX slot, replay pre-roll and TUI QSO timing row follow the active period; raw capture and the gain sweep stay FT8-only by decision (PAN-477).

- The startup index rebuild builds `qso.db.new`, checkpoints the old index, keeps it as `qso.db.pre-rebuild[.N]` when it holds contacts ADIF lacks or cannot be read, then renames over. Staleness compares contacts, never mtimes. Legacy migration copies `qso.db` to `qso.db.pre-migration` first, refuses unreadable rows, and keeps the ADIF writer closed on refusal (PAN-282).
- TX-worker aborts (PAN-322): every Step 6/8 `SleepOutcome` goes through `on_sleep_outcome` and every Step 4 `PttWait` through `on_ptt_wait`; any change to what an abort unkeys, settles or reports must re-bless `pancetta/tests/fixtures/tx_abort_transcripts.golden` (`PANCETTA_BLESS_TX_ABORT=1`) from the base commit.

## Where Things Live

- `docs/superpowers/specs/` — design specs
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

When sources conflict: code > curated docs > docs/DECISIONS/ > docs/superpowers/specs/ and implementation plans.
