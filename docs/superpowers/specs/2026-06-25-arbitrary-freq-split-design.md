# Arbitrary-frequency tune + split RX/TX — design

**Date:** 2026-06-25
**Status:** Approved (design); implementation pending
**Feature 3 of the next-session feature agenda** (`project_next_session_features`),
first in the build order `split/arbitrary-freq → Hound → FT4 → Fox → SuperFox → FT2`.

## Goal

Two operator capabilities for off-standard and DXpedition work:

1. **Arbitrary dial entry** — type any dial frequency (MHz) instead of cycling the
   standard FT8 band presets.
2. **True rig-level dual-VFO split** — RX dial ≠ TX dial, for working FT8 DX /
   DXpeditions that announce a separate listening frequency. RX stays on VFO A;
   TX moves to VFO B; the rig routes TX to B while keyed.

Non-goal clarification: classic Fox/Hound QSY is an **audio-offset** move within
one dial passband, *not* rig-level split — so this feature is largely independent
of the upcoming Hound work. Hound will reuse the audio-offset retarget machinery,
not this split plumbing.

## Current state (why this is real plumbing)

Mapped 2026-06-25. The whole station shares **one dial atomic**
`operating_frequency_hz: Arc<AtomicU64>` (`coordinator/mod.rs:410`) for both RX
and TX. There is **no split concept anywhere**: the `RigControl` trait
(`pancetta-hamlib/src/rig.rs:145`) exposes no split methods, and the coordinator
hamlib command handler (`coordinator/hamlib.rs:574`) honors only `SetFrequency`
and `SetPtt` (`_ => {}` for everything else). The logged RF is computed as
`dial + audio_offset` from that single atomic (`qso_manager.rs:1555-1557` and the
completed-on-open mirror `:1205-1207`). There is **no band-edge / band-plan
validation** gating transmit; §97.221 in code is an operator-presence gate only,
not frequency. The unenforced `BandPlanConfig` / `band_limits` / `edge_warnings`
structs live in `pancetta-config/src/rig.rs:302-328`.

## Design

### 1. State model — one new atomic

Add `split_tx_frequency_hz: Arc<AtomicU64>` alongside `operating_frequency_hz` in
`coordinator/mod.rs`.

- Convention: **`0` = simplex (split off)**; nonzero = the split TX *dial* in Hz.
- RX dial remains `operating_frequency_hz`, untouched — so every existing reader
  of the RX dial keeps working with no change.
- **Effective TX dial** = `split != 0 ? split : rx_dial`. Provide a small pure
  helper `effective_tx_dial(rx_dial, split) -> u64` (unit-tested).

### 2. Hamlib split plumbing

**Rig trait** (`pancetta-hamlib/src/rig.rs`): add two methods.

- `set_split(&self, enabled: bool, tx_vfo: Vfo) -> Result<()>` → rigctld
  short-form `S <0|1> <VFO>` (split mode on/off + which VFO transmits).
- `set_split_freq(&self, tx_freq: u64) -> Result<()>` → rigctld short-form
  `I <freq>` (set the TX-VFO frequency while in split).

Mode is left as data/FT8 — no `set_split_mode`.

**rigctld client** (`pancetta-hamlib/src/rigctld.rs`): implement both via the same
short-form command path used by `set_frequency` (`:486`) and `vfo_to_string`
(`:378`). The exact emitted strings carry an `OPERATOR-CONFIRM(split)` marker —
verified bit-for-bit by command-string unit tests but only confirmed correct
on-air against the FTdx10 (mirrors the LoTW `OPERATOR-CONFIRM(lotw)` pattern).

**MockRig**: gain `split_enabled: bool` + `split_tx_freq: u64` state with getters
so tests can assert split was applied.

**Bus message** (`message_bus.rs:344`, `RigControlMessage`): one combined variant
`SetSplit { enabled: bool, tx_frequency: u64 }`.

**Coordinator handler** (`coordinator/hamlib.rs`, replacing part of the `_ => {}`
arm): on `SetSplit{enabled:true, tx_frequency}` → `set_split_freq(tx_frequency)`
then `set_split(true, Vfo::B)`; on `SetSplit{enabled:false, ..}` →
`set_split(false, Vfo::A)`. Errors logged at `warn!` (`target: "rig.split"`),
never fatal.

### 3. TX RF + logging correctness

The substantive correctness fix: the QSO RF stamp (`qso_manager.rs:1555-1557` and
`:1205-1207`) currently does `m.frequency += rx_dial` where `rx_dial` comes from
the injected `dial_frequency_hz` source (`set_dial_frequency_source`, `:457`).
Inject the split atomic as a **second source** on the `QsoManager` and compute
`effective_tx_dial(rx_dial, split)` at stamp time. Without this, split QSOs log
the wrong band.

- **`FREQ`** (ADIF) = our TX RF = `effective_tx_dial + our_audio_offset`.
- **`FREQ_RX`** = RX dial RF when split is active — **included only if cheap**
  (stretch; not load-bearing for the feature).

The TX path (`coordinator/tx.rs`) needs **no change** — it only modulates the
audio offset; the rig routes TX to VFO B once split is enabled.

### 4. TUI — modal entry + clear

- **`f`** opens a modal frequency-entry overlay with two fields: RX dial (MHz) and
  TX split (MHz, blank = simplex). A new `InputMode` in `app.rs` captures
  digits / `.` / backspace and **suppresses the normal single-key handlers**
  while the modal is open. **Enter** applies; **Esc** cancels.
- Apply sends `TuiCommand::SetFrequency { vfo: 0, frequency }` (existing path,
  `tui_relay.rs:810`) **plus** a new `TuiCommand::SetSplit { enabled, tx_frequency }`
  (blank TX field → `enabled:false`).
- **`F`** (Shift+F) instantly clears split (`SetSplit{enabled:false}`).
- Split state renders as a small chip/banner near the existing TX-policy banner,
  e.g. `SPLIT TX 14.090`.

**TUI relay** (`coordinator/tui_relay.rs`): a `SetSplit` handler stores
`tx_frequency` into `split_tx_frequency_hz` (0 when disabled), forwards
`RigControlMessage::SetSplit` onto the bus to the hamlib loop, and emits a
status/banner update to the TUI.

### 5. Out-of-band warning (interim, US-only)

On apply, compute the effective TX RF (effective TX dial + current audio offset)
and classify it with `pancetta_core::Band::from_frequency` (the existing ham-band
range table — used here as the interim proxy for US bands; confirm its ranges at
implementation time and add a US-segment table only if its coverage is too coarse
for the warning to be useful). If the RF is **out of band** AND the session has
not yet warned → emit a message the TUI renders
as a **required acknowledgment modal** ("TX <freq> MHz is outside US ham bands —
press Enter to acknowledge"), then set a once-per-session flag and stay silent on
subsequent out-of-band entries. **This never blocks transmit** — acknowledge to
proceed; the operator remains responsible.

Region-aware global band plans are a deferred TODO
(`project_global_bandplan_todo` in memory): extend the existing unenforced
`BandPlanConfig` / `band_limits` / `edge_warnings` structs with an IARU-region
selector + per-region segment tables, wired into this same validation point.

### 6. Autonomous / band-change interaction

- **Band up/down (`=`/`-`)** and **autonomous `ChangeBand`** clear split (split is
  tied to the band it was set on): zero `split_tx_frequency_hz` + send
  `SetSplit{enabled:false}`.
- The DSP band-flush (`coordinator/dsp.rs` `band_flush_decision`) keys off the
  **RX** dial atomic only — split TX changes never trigger a spurious flush; no
  change needed in `dsp.rs`.
- §97.221 presence gate + tri-state TX policy unchanged — split only moves the TX
  dial, it does not gate transmit.

### 7. Testing

- **Pure units**: `effective_tx_dial` (simplex vs split); US out-of-band boundary
  classification; MHz-string → Hz parse (accepts `14.085`, rejects garbage /
  empty handled as simplex for the TX field); split-clears-on-band-change logic.
- **Hamlib**: `MockRig` split state; rigctld command-string assertions for `S` and
  `I` (mirrors the existing `set_frequency` short-form string tests).
- **coord_sim**: a scenario asserting that with split active, a completed QSO's
  logged RF uses the **TX** dial (not the RX dial), and the mock rig keys PTT with
  split enabled. Extend `CoordSim` to carry the split atomic.
- The live rigctld split invocation stays `OPERATOR-CONFIRM(split)` — on-air
  verification on the FTdx10.

## Scope kept out (YAGNI)

- Region-aware / global band plans (memory TODO).
- Per-band split memory or persistence across restarts.
- Split for autonomous-*initiated* QSOs (split is a manual operator concept here;
  autonomous band-hop clears it).
- `set_split_mode` (mode stays data/FT8).

## Files touched (anticipated)

- `pancetta-hamlib/src/{rig.rs, rigctld.rs, mock.rs}` — trait methods + rigctld
  strings + mock state.
- `pancetta/src/message_bus.rs` — `RigControlMessage::SetSplit`,
  `TuiCommand::SetSplit`.
- `pancetta/src/coordinator/{mod.rs, hamlib.rs, tui_relay.rs}` — split atomic,
  handler, relay.
- `pancetta-qso/src/qso_manager.rs` — RF stamp uses effective TX dial.
- `pancetta-tui/src/{tui_runner.rs, app.rs, ui/mod.rs}` — modal, key handling,
  banner, warning modal.
- `pancetta-core/src/types/band.rs` — reuse `Band::from_frequency` (US-band
  classification; no change expected unless a helper is convenient).
- Tests across the above + `pancetta/tests/coord_sim.rs`.
