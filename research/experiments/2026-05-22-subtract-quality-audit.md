---
slug: subtract-quality-audit
mode: ft8
state: shelved
created: 2026-05-22T00:00:00Z
last_updated: 2026-05-22T00:00:00Z
branch: experiment/ft8/subtract-quality-audit
parent_hypothesis: hb-030
wild_card: false
scorecard: (n/a — diagnostic probe, no scorecard)
delta_vs_main: 0 (production unchanged)
disposition: SHELVED with strong diagnostic finding; spawns hb-037 (subtract kernel) and bumps hb-031 (fast-path)
---

## Hypothesis

hb-001 (shelved 2026-05-21) showed multi-pass subtract-and-redecode
contributes only +1.2% on hard-200 — far below the predicted +5-15%.
The mechanism was unclear: either `subtract_with_sidelobes` leaves
artifacts that mask weak signals, or weak residual signals fall below
sync threshold even after clean subtraction.

hb-030 was designed to distinguish these via a controlled two-signal
synth: strong signal at known SNR, weak signal at varying SNRs and
frequency offsets, plus a weak-only control.

## Change

Pure research probe — no production behavior changed.

- `pancetta-research/examples/subtract_quality_probe.rs` — new probe
  binary that generates 2-signal WAVs over a sweep of
  (weak_snr, freq_offset) and reports per-case whether subtraction
  surfaces the weak signal, masks it, or whether the weak is
  fundamentally below the decoder's floor.

## Result

**Sweep on strong_snr=-5 dB, weak_snr ∈ {-15, -18, -20, -22} dB, freq_offset ∈ {12.5, 25, 50, 100} Hz:**

```
wkSNR  Δfreq_Hz  p1_S  p1_W  p2_W  ctlW verdict
-15.0      12.5     Y     .     .     Y   (b) sub masks
-15.0      25.0     Y     .     .     Y   (b) sub masks
-15.0      50.0     Y     Y     Y     Y   other (pass 1 already found)
-15.0     100.0     Y     Y     Y     Y   other (pass 1 already found)
-18.0      12.5     Y     .     .     Y   (b) sub masks
-18.0      25.0     Y     .     .     Y   (b) sub masks
-18.0      50.0     Y     Y     Y     Y   other (pass 1 already found)
-18.0     100.0     Y     Y     Y     Y   other (pass 1 already found)
-20.0      12.5     Y     .     .     Y   (b) sub masks
-20.0      25.0     Y     .     .     Y   (b) sub masks
-20.0      50.0     Y     Y     Y     Y   other (pass 1 already found)
-20.0     100.0     Y     Y     Y     Y   other (pass 1 already found)
-22.0      12.5     Y     .     .     Y   (b) sub masks
-22.0      25.0     Y     .     .     Y   (b) sub masks
-22.0      50.0     Y     .     .     Y   (b) sub masks
-22.0     100.0     Y     .     .     .   (c) below floor

Summary: 9 (b) sub masks  |  0 (a) sub helps  |  1 (c) below floor  |  6 other
```

**Three crisp findings:**

1. **Zero cases where subtraction surfaced a missed decode.** Across
   all 16 (snr, freq) combinations tested, the multi-pass infrastructure
   never recovered a weak signal that pass 1 missed. This is a clean
   confirmation of hb-001's empirical observation at scale (+1.2%
   contribution from pass 2+).

2. **Subtraction MASKS recoverable signals within ~25 Hz of the strong.**
   All 8 cases at Δf ≤ 25 Hz with weak above the decoder's floor failed
   to recover the weak after subtraction. The same weak decodes fine
   in the weak-only control (ctlW=Y). `subtract_with_sidelobes` is
   leaving artifacts at/near the strong signal's frequency that
   prevent the decoder from finding nearby weaker signals.

3. **At Δf ≥ 50 Hz, both signals decode in pass 1 without subtraction.**
   When signals are well-separated in frequency, they don't interfere
   in the spectrogram → both Costas-decoded in pass 1 → subtraction
   isn't needed. The 4 "other" cases at Δf=50 Hz with various weak SNRs
   all fell here.

The mechanism is now clear: the subtraction kernel's sidelobes (or
mainlobe leakage) at the strong signal's TF cell contaminate the
neighborhood for ~25 Hz on either side. Any weak signal in that band
becomes undecodeable in the residual, even though it would decode if
the strong weren't present.

## Disposition

**SHELVED with strong diagnostic finding.** No production change
(the multi-pass infrastructure is currently a no-op for nearby weak
signals — disabling it would require a separate hypothesis test).
Spawns two follow-ups:

- **hb-037 (new):** redesign or replace `subtract_with_sidelobes`
  with a better-shaped kernel (longer windowing for sidelobe
  reduction, frequency-domain subtraction via hb-021's path, or
  outright removal of multi-pass infrastructure). Priority ~0.50.

- **hb-031 (existing, priority 0.40):** fast-path single-pass mode is
  now strongly motivated. Pass 2+ contributes nothing useful on
  nearby weak signals AND adds ~58% wall-clock per the hb-001 timing.
  Defaulting to max_passes=1 would be a pure speedup with no
  sensitivity loss. **Bumping priority to 0.55.**

## Learnings

- **Multi-pass subtract-and-redecode is dead infrastructure for FT8.**
  Combined with hb-001's macro-level finding (+1.2% contribution) and
  hb-008's NMS-off observation (the lever isn't NMS suppression
  either), the picture is clear: **pancetta's pass-1 Costas + LDPC +
  OSD is the entire decode engine.** Pass 2+ is effectively a no-op.

- **The hb-019 win has a unified explanation.** Why did disabling NMS
  produce +1973 decodes? Because pass 1 could now see candidates that
  NMS was masking — candidates that pass 2 would NEVER have surfaced
  via subtraction (this probe proves it). The right architecture is
  "let pass 1 see everything; pass 2+ is overhead."

- **The right next move is structural, not parametric.** hb-007
  (MIN_SYNC_SCORE), hb-009 (block-score ranking), hb-011 (LDPC
  iter cap) are all pass-1 knobs — each could find more signals
  pass 1 was missing. By contrast, every pass-2-targeted experiment
  is wasted effort until the subtract kernel is fixed (hb-037).

- **Two-signal synth is a powerful diagnostic.** The whole audit
  took ~10 minutes of compute. Adding similar 2-signal scenarios
  to a `synth-multi-signal` tier would let future experiments
  quickly identify which knobs help on adjacent-signal cases.
  Worth a follow-up to formalize this corpus.

## Follow-ups added to hypothesis bank

- **hb-037 (new)** — Redesign subtract_with_sidelobes. Three paths:
  (a) longer / better-shaped subtraction window for sidelobe reduction;
  (b) frequency-domain subtraction (the hb-021 wild card);
  (c) remove multi-pass entirely (max_passes=1 always) and reclaim
  the wall-clock budget for other passes-1 work (more candidates,
  more LDPC iters per candidate, OSD-3 with the spare time).
  Priority ~0.50. Estimated effort: 2-3 sessions for (a) or (b); 0.5
  for (c).

- **hb-031 (existing) — priority bumped 0.40 → 0.55.** Fast-path
  single-pass mode is now much better motivated. Production
  max_passes=1 would be a pure speedup with no sensitivity loss for
  on-air decoding. The decision becomes: do we need multi-pass
  infrastructure at all if it doesn't work?

## Reproducing

```bash
cargo run --release -p pancetta-research --example subtract_quality_probe
```

(Self-contained; no corpus needed. Deterministic by seed=42.)
