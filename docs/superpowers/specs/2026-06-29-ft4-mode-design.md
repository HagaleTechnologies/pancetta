# FT4 Mode (7.5s digital mode) — Design Spec

**Date:** 2026-06-29
**Status:** Proposed (self-approved under operator's standing overnight "do all in sequence, use best judgement" authorization; operator to review on waking)
**Author:** Claude Opus 4.8 (autonomous, under K5ARH standing authorization)

## Goal

Let pancetta operate the **FT4** digital mode — the faster (7.5s T/R period) sibling of FT8 — as a
**station-wide selectable mode**: the operator picks FT8 or FT4, and the whole station (decode window,
slot grid, TX scheduling, QSO sequencing, ADIF logging) runs that mode. FT8 behavior is **byte-identical
to today** when the active mode is FT8 (hard regression invariant).

## Key finding: the codec already works — FT4 is an INTEGRATION job

A read-only readiness audit (2026-06-29) confirmed FT4's **encoder and decoder already function**:

- `pancetta-ft8/src/protocol.rs` has a **complete `ProtocolParams::ft4()` preset** (`Protocol::Ft4`):
  `num_tones: 4` (4-GFSK), `num_symbols: 105`, `symbol_period: 0.048`, `tone_spacing: 20.8333`,
  `cycle_duration: 7.5`, 4 Costas arrays at positions `[1,34,67,100]`, `num_data_symbols: 87`,
  `Gfsk { bt: 1.0 }`, and an FT4 XOR scrambling sequence.
- The **encoder** is parameterized (`Ft8Encoder::with_protocol` / `generate_symbols_protocol` routes
  4-GFSK vs 8-GFSK by `bits_per_symbol`).
- The **decoder** is ~85% parameterized: `Ft8Config.protocol` selects the params; the hot-path Costas
  search/scoring reads `pp.costas_positions/arrays/length/num_tones`.
- **End-to-end FT4 round-trip tests already pass** (`pancetta-ft8/tests/round_trip_tests.rs`:
  `test_ft4_round_trip_cq`, `test_ft4_round_trip_all_message_types`).
- **ADIF is already mode-agnostic** (`AdifQso.mode` from `QsoMetadata.mode`).

So FT4 needs **no new codec, no new message formats** (same 77-bit / LDPC(174,91) / CRC-14 payload).
The work is wiring the rest of the station to the protocol's timing and stamping the mode through.

## What's genuinely NEW

### 1. Parameterized slot timing (the one hard part)
`pancetta-core/src/slot.rs` hardcodes `SLOT_NS = 15s` and every helper reads it. FT4 needs a 7.5s
grid (TX begins at :00, :07.5, :15, :22.5 … — 8 slots/minute vs FT8's 4). Parity still alternates each
period (FT4 Even = :00/:15/:30/:45, Odd = :07.5/:22.5/…), so the **parity concept is unchanged** — just
computed mod the active period.

**Approach (regression-safe):** add `*_with_period(…, slot_ns: i64)` variants of every public fn
(`current_slot_start`, `SlotParity::of`, `next_slot_start`, `next_slot_with_parity`, `next_audio_start`,
`next_phase`) plus `Protocol::slot_ns()` / `ProtocolParams` period accessors. **Keep the existing fns as
thin wrappers that call the `_with_period` variant with `SLOT_NS`** → all current FT8 call sites and the
24 existing slot tests stay byte-identical. The active period is a coordinator-held
`active_slot_ns: Arc<AtomicI64>` (set once at startup from the configured mode; an atomic so a future
runtime toggle is a one-line change). Call sites that must follow the mode (`ft8.rs` parity stamping,
`tx.rs` scheduler, `dsp.rs` decode-phase) read the atomic and call the `_with_period` variant.

### 2. Protocol-derived DSP window + decode phase
`pancetta/src/coordinator/dsp.rs` hardcodes `FT8_WINDOW_SECONDS = 12.64`, a decode trigger at slot+13s,
and `IDEAL_SAMPLES = sample_rate * 15`. These become **derived from the active protocol**:
- window seconds = `num_symbols * symbol_period` (FT8 12.64s, FT4 5.04s).
- decode-phase offset = `cycle_duration - decode_margin` (FT8 15−2=13s; FT4 7.5−1=6.5s).
- retained-overlap samples = `sample_rate * cycle_duration`.
Threaded as values derived once at startup from the active `ProtocolParams` (mirrors `active_slot_ns`).

### 3. Mode selection (config, startup)
- New config knob `[radio] mode = "FT8" | "FT4"` (default `"FT8"`) in `pancetta-config` (parsed to
  `Protocol`; validation rejects unknown strings). Wired at coordinator startup to: the initial
  `Ft8Config.protocol`, the `active_slot_ns` atomic, the DSP window/phase values, and the
  active-mode string.
- **Runtime mode-toggle is v2** (deferred): v1 selects the mode at startup; switching modes is a
  restart. The atomic-based architecture makes the runtime toggle a small follow-up (set the atomic +
  hot-reload the decoder + flush the DSP buffer like a band change). Documented as a non-goal here.

### 4. Mode stamped through QSO + decode views
- `QsoManagerConfig` gains an `active_mode: String` (default `"FT8"`), set from config at startup
  (same threading pattern as Hound regions / Fox). The ~9 hardcoded `mode: "FT8"` sites in
  `qso_manager.rs` read `self.config.active_mode` instead → ADIF `MODE=FT4` flows automatically.
- `DecodedMessageView.mode` is stamped with the active mode string at the coordinator broadcast
  boundary (`ft8.rs`) so the TUI shows FT4 decodes labeled FT4. (No `DecodedMessage` struct change —
  single active mode means the station-global value is authoritative.)

### 5. FT4 dial frequencies (usability)
FT4 lives on different sub-band dial frequencies than FT8 (e.g. 40m 7.0475, 30m 10.140, 20m 14.080,
17m 18.104, 15m 21.140, 12m 24.919, 10m 28.180 MHz; 80m 3.575, 60m n/a, 6m 50.318). Add an FT4 dial
table keyed by `Band` alongside the FT8 one, and have the band-default / autonomous band-hop pick the
table for the active mode. (Manual arbitrary-freq tune already exists; this just makes the defaults
correct so the mode is usable out of the box.)

### 6. TUI mode indicator
A title-bar chip showing the active mode (e.g. "FT4") when not FT8. Additive; reuses the chip pattern
(split / FOX / HOLD chips).

## Design summary (data flow)

```
config.radio.mode ──startup──> Protocol (Ft8|Ft4)
   ├─> Ft8Config.protocol            (decoder: already param)
   ├─> TX encoder protocol           (encode/modulate: already param)
   ├─> active_slot_ns: AtomicI64     (slot grid: NEW _with_period fns)
   ├─> dsp window_secs / decode_phase / overlap_samples  (NEW derived)
   ├─> QsoManagerConfig.active_mode  (ADIF MODE)
   ├─> DecodedMessageView.mode       (TUI label)
   └─> band-default dial table       (FT4 frequencies)
```

## Scope / non-goals (v1)
- **Single active mode, selected at startup.** No simultaneous FT8+FT4 decode (two grids at once is a
  much larger lift — deferred). No runtime mode toggle (v2; architecture is ready for it).
- **No new codec / messages / Fox-Hound-for-FT4** (Hound/Fox are FT8 procedures; FT4 Fox is later).
- **No new QSO sequencing** — the state machine is mode-agnostic (same exchange messages).
- **FT2** is the next mode after FT4 (behind its own feature gate; separate build).

## Risks / careful points
1. **FT8 regression is the #1 risk.** Mitigation: `slot.rs` existing fns become wrappers over
   `_with_period(SLOT_NS)` — the 24 existing slot tests must stay green unchanged; DSP/TX values for
   FT8 mode resolve to today's exact constants (12.64 / 13s / 15×rate). A coord-level FT8-mode test
   asserts unchanged timing. **No behavior change unless `mode = "FT4"`.**
2. **Decode-phase for FT4** (6.5s into a 7.5s slot) must leave PTT/CAT headroom; verify the TX
   scheduler's pre-PTT lead fits the shorter slot (lead < 7.5s − tx_duration 5.04s − pre-roll).
3. **Decoder residual path** (`codeword_to_symbols`, multi-pass subtraction) still hardcodes FT8
   Costas/Gray. v1: verify FT4 decode quality with the parameterized hot path; **if** multi-pass is
   engaged and degrades FT4, parameterize it (task included, gated on a measured need). The basic FT4
   decode path is already validated by the round-trip tests.
4. **Pre-roll for FT4:** keep 0.5s (WSJT-X FT4 uses a short pre-roll); the buffer math derives from
   cycle_duration so a 0.5s pre-roll inside a 7.5s slot is fine.
5. **Atomic load cost** in the hot DSP/decode loop is a single relaxed `i64` load per window — negligible.

## Testing
- **Unit (`slot.rs`):** new `_with_period` fns at `slot_ns = 7.5e9` — boundaries at :00/:07.5/:15/…,
  parity alternates each 7.5s, `next_phase` accepts offsets in `[0, 7.5s)`. Existing 24 FT8 tests
  unchanged (regression).
- **Unit (config):** `mode` parses FT8/FT4, rejects garbage, defaults FT8.
- **Unit (dsp helpers):** window/phase/overlap derive correctly for both protocols.
- **Integration:** keep the existing FT4 round-trip tests; add a coord-level test that with
  `mode=FT4` the parity stamping + decode phase use 7.5s, and with `mode=FT8` they're identical to
  today.
- **ADIF:** a completed QSO in FT4 mode writes `MODE:FT4`.

## Open questions (self-answered for the overnight build; flag for operator review)
1. **Config location:** `[radio] mode` (proposed) vs a new `[mode]` section. Chose `[radio]` —
   mode is a radio-operating concern alongside band/frequency. (Operator may prefer elsewhere.)
2. **Decode margin for FT4:** 1s before slot end (decode at 6.5s). Tunable; FT8's is 2s. If FT4
   decodes feel late/early on-air, adjust the margin constant.
3. **Runtime toggle now or later:** chose later (v2) to bound the core-timing blast radius; v1 ships a
   solid startup-selected mode. Operator can prioritize the runtime toggle next if wanted.
