<!-- Moved from CLAUDE.md 2026-07-17 (docs/claude-md-consolidation) during the CLAUDE.md/AGENTS.md
     consolidation pass. The "Development Phases (End-to-End QSO Initiative)" section referenced by
     the original consolidation task no longer exists in CLAUDE.md as of this move — that initiative's
     phases had already been completed and folded into the subsystem digests below in an earlier
     pass. Only "Known Gaps and TODOs" survived to be moved here; content below is carried over
     verbatim from CLAUDE.md. For current behavior trust the code and specs in
     `docs/superpowers/specs/`. -->

# Known gaps and TODOs

- **A non-completing ladder advance whose frame is dropped keeps its
  advanced state.** Advances record no PAN-75 attempt, so a dropped frame
  from a step that does not complete the QSO is not undone. (Surfaced by
  PAN-138 review on PR #483, 2026-10-20.)

- **A completion whose 73 aired can still go unlogged.** Since PAN-280 a
  completion that queues a closing frame is held until that frame's
  `TxFrameSent`. The held record is not persisted, so a Qso-task restart or
  a process stop between the queue and the air report loses the log entry,
  and a `TxFrameSent` that arrives after the record expired (six slots,
  at least 30 s) does not log it. (Surfaced by PAN-280, 2026-11-08.)

- **A mid-TX pivot-duplicate drop keeps its tombstone and skips the
  replaced-frame rollback.** `IncomingDuringTx::Drop` discards a queued
  request whose payload is already airing through a pivot, but it neither
  clears the `pivoted_once` tombstone nor rolls back the frame the pivot
  replaced, so that frame stays counted. Its Autonomous behaviour already
  matches the ownership rule. (Surfaced by PAN-185, 2026-10-23.)

- **A rejected auto-sequence resend sends Autonomous no completion.** The
  QSO forwarder withdraws the intent and rolls back the engine's ledger
  attempt, but an older accepted request may still own the Autonomous
  outcome. PAN-238 repairs registered opening self-CQs only: their rejected
  enqueue sends a durable failed completion because no predecessor owns an
  outcome. The resend gap remains when the 10,000-message TX channel is
  full. (Surfaced by PAN-185, 2026-10-23.)

- **Supervisor coverage:** the restartable set includes Dsp/Ft8Decoder (PAN-4, using retained
  `DecodePipelineHandles`), Hamlib/StationAgent (PAN-5, with pre-restart teardown) and
  Ft8Transmitter (PAN-119, cheap restart relying on `PttGuard`'s `Drop` for PTT release). Audio
  stays intentionally `DegradeOnly` — its in-place device-recovery path owns rollback semantics a
  task restart cannot reproduce.

- **Silent audio-output stall recovery is bounded and hardware-unverified (PAN-115, PAN-295):** a
  stall where output callbacks stop without a cpal error mutes TX and now requests a forced
  reopen of both streams, retrying every 10 s while silent, at most three times in a rolling ten
  minutes. The cap covers only these output-watchdog reopens; input-stale and processing-error
  recovery keep their own cadence. TX stays muted until callbacks resume. At the limit the
  readiness strip shows `⚠ TX AUDIO AUTO-REOPEN LIMIT: press d` and the operator re-selects the
  device with `d`. Covered by unit tests only; no physical stall has been qualified. A dead
  managed rigctld is still only respawned by re-applying the rig (`i`) or a Hamlib restart.

- **cqdx `GET /api/v1/spots?live=true` response envelope key (`groups`) unverified against live API** — a gated live test exists: `CQDX_TOKEN=pat_xxx cargo test -p pancetta-cqdx test_live_spots_envelope -- --ignored --nocapture`.

- ~~**`pancetta-research` has no CI coverage**~~ — resolved 2026-11-18 by PAN-372.
  Other Rust lanes exclude the crate (`.github/workflows/ci.yml:401,490,495,574,577,687`),
  and it remains outside `default-members` (`Cargo.toml:18-31`).
  The formatting lane also touches its source (`ci.yml:422`). Dependabot PR #211 exposed a
  dependency bump that broke the crate while required CI stayed green. The
  non-required, path-filtered Research Check now runs
  `cargo check -p pancetta-research --all-targets` with default and `research-eval`
  features when the crate or its inputs change: `pancetta-research/**`,
  `pancetta-ft8/**`, `pancetta-qso/**`, `pancetta-dsp/**`, root `Cargo.toml`,
  `Cargo.lock`, `rust-toolchain.toml`, and the ft8_lib gitlink. A failure reports
  red but never gates a merge. Local `scripts/check.sh` remains the primary
  drift guard. (Surfaced by PAN-1, 2026-07-29.)

- ~~**~25 duplicated `gaussian_noise` Box-Muller helpers** in `pancetta-research/examples/`~~ —
  fixed 2026-07-31: consolidated into `pancetta_research::noise::gaussian_noise`, all 25
  call sites updated. (Surfaced by PAN-1, 2026-07-29.)

- **Spot-call QSY follow-ups (PAN-287, 2026-11).** Space on a DX Hunter network spot now QSYs
  the rig and calls at the spot's offset (`docs/DECISIONS/tui.md`). Left open:
  - ~~The rig's data mode is not set on a QSY (A13 / review L4-24).~~ Covered by PAN-276: with
    `[rig] data_mode` set, rigctld reapplies the mode after every commanded frequency change.
  - cqdx spots do not reach DX Hunter yet (`spawn_spot_poller(…, None, None)`, review L4-10);
    once wired they take the same path.
  - DX Hunter rows on another band carry no band tag, so the operator learns that Space will QSY
    only from the keypress status line.
  - An autonomous band hop can undo a manual spot QSY. Since PAN-385 a `[[autonomous.band_plan]]`
    window acts only on entry, so it leaves a manual QSY alone until the next window starts, but a
    needed-activity hop can still move away from it after `needed_idle_secs`.
  - Cluster rows are labelled `RX` (`source = Local`), `DxStation.frequency` holds Hz for cluster
    rows and MHz for cqdx rows, and `add_dx_spot` replaces a locally heard row. The spot path
    reads only `spot_rf_hz`.
  - The TUI's band state does not follow autonomous band hops.
  - The Hamlib loop has no keyed recheck for TUI-sourced `SetFrequency`; the relay's
    `rig_is_keyed` recheck under `ptt_sync_gate` is the guard for spot QSYs, and band keys
    have never had one.
  - The exchange-in-progress check is a snapshot taken before `ptt_sync_gate`, not serialized
    with the QSO task's decode loop. A partner reply the QSO task applies between that read and
    the dial store is still torn down by `BandChanged` (Codex P1 on PR #613). The read cannot
    move under the gate (`apply_tx_offset_switch` takes `qsos` before `ptt_sync_gate`); closing
    it needs the QSO task to arbitrate the QSY, for example a request/ack through its message loop.

## Missing-docs lint status (verified 2026-07-17)

Per-crate `#![...(missing_docs)]` levels, dropped from CLAUDE.md's "Documentation Policy" section
to keep that section to a policy statement rather than a point-in-time status list:

- `pancetta-core`: `#![warn(missing_docs)]`
- `pancetta-hamlib`: `#![deny(missing_docs)]`
- `pancetta-protocol`: no `missing_docs` attribute set (defaults to allow)
- All other crates (`pancetta`, `pancetta-agent`, `pancetta-audio`, `pancetta-dsp`, `pancetta-config`,
  `pancetta-qso`, `pancetta-dx`, `pancetta-cqdx`, `pancetta-tui`, `pancetta-ft8`, `pancetta-research`):
  `#![allow(missing_docs)]` with a `// TODO: documentation pass pending` comment.

Switch each crate to `warn`/`deny` as its doc coverage lands.

## Supervisor crash-to-QSO scope (PAN-5, 2026-07-31)

Component failure and QSO-engine restart are intentionally distinct operator diagnoses:
`SupervisorRestart` means the QSO engine itself died and its in-memory state was lost;
`ComponentCrash(name)` means a dependency died and the affected QSO was explicitly terminated.

| Failed component | QSO scope | Failure reason |
|---|---|---|
| Qso | All active QSOs | `SupervisorRestart` |
| Hamlib | All active QSOs | `ComponentCrash("Hamlib")` |
| Audio | All active QSOs | `ComponentCrash("Audio")` |
| StationAgent | Remote-origin QSOs only | `ComponentCrash("StationAgent")` |
| Autonomous, DxCluster, PskReporter, RemoteGateway, Dsp, Ft8Decoder, Ft8Transmitter, Tui, Config, Coordinator, WsjtxUdp | None | No QSO failure emitted |

The scope is dependency-based, not restart-policy-based: Audio remains deliberately `DegradeOnly`
but its loss still invalidates every active on-air exchange, while a StationAgent crash cannot
invalidate local operation. Component-crash drops also suppress the autonomous engine's normal
"QSO ended, start another" reaction so recovery cannot cascade into fresh transmissions.

- **macOS microphone approval is per binary and unverified on hardware.** Ad hoc
  signatures tie the TCC grant to one build (Apple TN3127). Developer ID signing
  would keep it across versions. The PAN-264 Mac matrix M1-M8 has not run yet,
  including prompt delivery, ssh/tmux/launchd capture, Terminal behavior,
  background fast-user-switch capture and upgrade recovery.
  (Surfaced by PAN-264, 2026-11-04.)
