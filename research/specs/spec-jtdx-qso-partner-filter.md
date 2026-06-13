# Algorithm spec: JTDX in-QSO band collapse around partner frequency

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File path (for traceability, NOT to be quoted): `lib/decoder.f90`
  (the `nfilter` band-collapse branch inside the FT8 dispatcher) and
  `lib/ft8_decode.f90` (the QSO-thread predicate `lqsothread =
  (nfqso ∈ [nfa, nfb])`)
- License: GPL-3.0
- Reader date: 2026-06-05

## Purpose

When a QSO is in flight, the decoder does not need to keep searching
the full FT8 audio passband — by definition the only message that
matters is the partner's reply at the partner's known audio
frequency. JTDX exposes this as the "filter" toggle (Settings → FT8 →
Filter). When the toggle is on, the decoder narrows its search band
to ±60 Hz around the partner audio frequency (`nfqso`) for normal
operation, or ±290 Hz in Hound mode (where the DXpedition can
re-launch its reply anywhere within a wide window). It also clamps
the thread count to 8 (or 4 for the "double-click" sub-mode), since
a narrow band cannot saturate more cores.

The CPU win is large and the recall cost is essentially zero — by the
time a QSO is in progress, the partner's frequency is known to within
a few hertz and the partner does not drift more than the half-window.
The freed CPU then enables architectural moves pancetta wants: multi-
stream-per-QSO, three-method sweep at the partner frequency, etc.

## Algorithm description (PROSE ONLY)

### Inputs

- The configured wide search band `[nfa_raw, nfb_raw]` in Hz audio
  (typically the full ~150 Hz to ~3.2 kHz FT8 passband, or whatever
  the operator's slider has narrowed it to).
- The QSO partner audio frequency `nfqso` in Hz.
- A boolean `filter` (the user-facing "Filter" toggle).
- A boolean `hound` (Hound-mode operation; a DXpedition special).
- The number of available threads `numthreads` (derived from CPU
  core count).
- A boolean `nagainfil` for the "Decode → Erase → Again" double-click
  flow, which has its own narrower window.

### Outputs

- The effective search band `[nfa, nfb]` actually passed to the inner
  decoder.
- The effective thread count `numthreads_eff` actually used to split
  the band across worker threads.

### Steps

1. **Sanity check.** If `nfqso` falls outside `[nfa_raw, nfb_raw]`,
   abort the decode with an "nfqso out of bandwidth" diagnostic; no
   filter is applied.
2. **Determine the half-window** based on mode:
   - Normal `filter=true`: half-window = 60 Hz.
   - `filter=true` and `hound=true`: half-window = 290 Hz.
   - "Again" / double-click decode (`nagainfil=true`): half-window
     = 25 Hz (the "50 Hz double-click bandwidth").
3. **Clamp the band:**
   `nfa = max(nfa_raw, nfqso - half_window)`,
   `nfb = min(nfb_raw, nfqso + half_window)`.
   The `max` / `min` clamping ensures the filter never widens the
   user's configured search band — only narrows it.
4. **Clamp the thread count:**
   - Normal filter mode: `numthreads_eff = min(8, numthreads)`.
   - Double-click mode: `numthreads_eff = min(4, numthreads)`.
   - Filter off: no clamp (`numthreads_eff = numthreads`).
5. **Dispatch the inner decoder.** JTDX's main dispatcher splits the
   `[nfa, nfb]` band into `numthreads_eff` equal subbands and runs
   one inner decoder per thread. The "wide" arrays `nfawide`,
   `nfbwide` are passed separately and govern the sync-baseline
   normalisation; they remain at the original wide band so the
   baseline percentile is not biased by the narrow filter window.

### "Wide" vs "narrow" band semantics

A critical detail: the search/accept band is `[nfa, nfb]` (narrow),
but the sync-baseline-normalisation band is `[nfawide, nfbwide]`
(wide). The baseline (40th-percentile sync score) is computed over
the wide band so that the per-bin `red[i] / base` normalisation is
not biased by the small filter window. Otherwise a wholly noise
window would still produce candidates with `red/base ≈ 1.0` because
the baseline itself would be a noise sample.

When the filter is OFF, `nfawide = nfa` and `nfbwide = nfb`.

### Why thread count is clamped

The thread budget is mainly justified by the cost of running the
inner decoder over a wide band: many candidates, each candidate runs
through the full LDPC decoder, etc. When the band is narrowed to
±60 Hz, the candidate count drops by ~30× and there is no longer
enough per-thread work to amortise the per-thread overhead (OpenMP
fork, memory copies of the audio buffer, baseline broadcast). The
clamp to 8 (or 4) is a "diminishing returns" pragma: more threads
would not speed it up.

## Numerical constants (facts, not expression)

- Normal filter half-window: **60 Hz**.
- Hound-mode filter half-window: **290 Hz**.
- "Double-click decode" half-window: **25 Hz** (so a 50 Hz total
  bandwidth around the click point).
- Thread clamp, normal filter mode: **8 threads**.
- Thread clamp, double-click decode: **4 threads**.
- The "wide" band used for sync-baseline normalisation stays at the
  original `[nfa_raw, nfb_raw]`.

## Edge cases

- **nfqso outside [nfa_raw, nfb_raw].** Filter is not applied; the
  decode aborts with a diagnostic line.
- **Filter on, wide band already narrow.** The `max`/`min` clamp is a
  no-op; the effective band equals the input band.
- **QSO partner drifts.** The 60 Hz half-window is generous compared
  to FT8's ~6.25 Hz tone spacing and typical drift over a single 15 s
  slot. Hound mode's 290 Hz accommodates DXpeditions that respond on
  a different frequency than the one they CQ'd on.
- **Switching filter off mid-QSO.** No history dependency; the band
  expands immediately on the next pass.
- **`lqsothread` predicate** in `ft8_decode.f90`: this is set when
  `nfqso ∈ [nfa, nfb]` (after any narrowing). When true, JTDX inserts
  two "virtual" candidates at `nfqso ± 5 s` DT into the list so that
  the inner FT8S decoder always tries to decode at the partner
  frequency even if no sync candidate was found there. This is the
  spiritual cousin of hb-217's "force-emit at partner freq" — it
  ensures coverage at the partner frequency under degraded sync.

## Conflict with pancetta's existing mechanisms

Pancetta already has the relevant plumbing:

- **hb-091 `freq_bin_range`**: the FT8 coarse search accepts an
  optional bin range that constrains where candidates are accepted.
  Currently set by the scoped-fast-path env-var / tier classifier
  (hb-216) and not by QSO state. This is exactly the slot the
  JTDX-style filter needs.
- **QsoManager state**: pancetta-qso's QSO state machine knows the
  active partner's `freq_hz` and the QSO start time. It currently
  uses this for TX scheduling but not for RX band collapse.
- **Coordinator pipeline**: the coordinator (`pancetta/src/
  coordinator/pipeline.rs`) is the join point — it has both the
  QSO state and the FT8 decoder handle.

What's missing:

1. A QSO-state-driven setter for `freq_bin_range` that fires on QSO
   start and clears on QSO end / timeout.
2. The Hound-mode branch (pancetta does not currently support
   Hound mode at all; can be deferred as a no-op).
3. The "wide" vs "narrow" baseline distinction. Pancetta's current
   baseline lives inside the coarse-search; need to confirm whether
   shrinking the search band shrinks the baseline window too. If yes,
   need to track a separate "baseline window" alongside the search
   window.
4. Thread-count clamp. Pancetta's FT8 decoder is single-threaded
   today (parallelism is at the slot level via the message bus, not
   the band-split level), so this constant has no immediate
   counterpart. Tier-classifier (hb-216) already handles "Slow"
   hardware separately, so the JTDX clamp logic is largely moot for
   pancetta until band-split parallelism lands.

## Estimated Rust port effort

- ~80 LOC for a QSO-state observer in the coordinator that sets /
  clears `freq_bin_range` on QSO transitions.
- ~50 LOC for a config knob `qso_partner_filter_half_window_hz`
  (default 60) and a Hound-mode placeholder.
- ~120 LOC if the baseline-window separation needs to be added (TBD
  after reading pancetta's existing baseline code).
- ~50 LOC of tests covering: QSO start sets band, QSO end clears
  band, partner-out-of-band edge case.
- 1 implementation session.

Total: ~250 LOC, 1 session, modest risk.

## Implementation notes for the implementer thread

- The narrowing should happen in the coordinator, not in pancetta-ft8.
  pancetta-ft8 already accepts a `freq_bin_range` parameter; the
  coordinator is the only thing that knows about QSO state.
- Look at `pancetta-qso/src/qso_manager.rs` for state transitions —
  the relevant events are "QSO started" (transition into `In73Wait`
  or earlier active states) and "QSO ended" (terminal states or
  timeout).
- The half-window of 60 Hz maps to roughly **20 frequency bins** at
  pancetta's coarse-search bin spacing (~3.125 Hz, same as JTDX).
  Express the configuration in Hz, convert inside the FT8 layer.
- When this lands, the three-method sweep
  (`spec-jtdx-3method-sweep.md`) becomes much cheaper because the
  spectrogram cost scales with the search band. Pair the two for
  best leverage.
- Test invariant: with the filter active, decoded recall on the
  partner's freq must equal recall without the filter. Use a loopback
  test where one TX stream is at the partner freq and a distractor is
  outside the filter band; assert both decode when filter is off and
  only the partner decodes when filter is on.
- Multi-stream-per-QSO architecture: once the filter is per-QSO,
  multiple in-flight QSOs each contribute their own narrow filter
  window. The decoder can either (a) run them as a union (slightly
  larger band, single decode pass) or (b) run them as separate
  inner-decode invocations. JTDX uses (a) implicitly via the wide
  search band; pancetta could do either. (a) is simpler and almost
  always cheap enough.
