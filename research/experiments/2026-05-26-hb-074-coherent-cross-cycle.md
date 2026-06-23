---
slug: hb-074-coherent-cross-cycle
mode: ft8
state: shelved
created: 2026-05-26T15:00:00Z
last_updated: 2026-05-26T15:00:00Z
branch: iter/2026-05-26-hb-074-coherent-cross-cycle
parent_hypothesis: hb-074
wild_card: false
scorecard: research/scorecards/coh-{ctrl,on}-{nofilter,filter}.json (transient, removed)
delta_vs_main: -10 hard-200 recovered vs non-coherent baseline (both no-filter and filtered). NEGATIVE.
disposition: SHELVE hb-074 — coherent integration with Costas-based phase recovery is net-negative on real operator audio. Infrastructure kept flag-gated.
---

## Hypothesis

hb-074 (priority 0.50 plan-sized, spawned 2026-05-25 from hb-056):
extend hb-056's non-coherent cross-cycle averaging to *coherent*
integration. The math predicts ~2× the non-coherent gain (3 dB vs 1.5 dB
at N=2) — and the bank entry projected ~+0.0016-0.0024 composite if
JTDX-class coherent integration carried over.

The known unknown was **phase recovery**: pancetta's spectrogram discards
phase, and even with complex retention there's no guarantee the
inter-slot phase is preserved in real-world TX/RX/propagation chains.
The spec's plan: recover phase per-candidate from the 21 known Costas
symbols, rotate each member to a common phase, then complex-sum.

## Change

`pancetta-ft8/src/decoder.rs`:
- `Spectrogram` gains an optional `complex: Option<Vec<Vec<Vec<Complex<f64>>>>>`
  field. Populated only when `Ft8Config::cross_cycle_coherent` is true at
  decode start — roughly doubles spectrogram memory when used, zero cost
  otherwise.
- `compute_spectrogram` retains the complex FFT bins alongside `power`
  when the flag is set.
- `par_extract_complex_symbols_from_spectrogram` — complex sibling of
  `par_extract_symbols_from_spectrogram`. Returns `None` when the
  spectrogram lacks `.complex`.
- `estimate_candidate_phase_rotor` — sums `complex_symbols[costas_sym]
  [expected_tone]` across all 21 Costas positions and returns the
  unit-magnitude phase rotor `exp(jφ_cand)`.
- `coherent_sum_complex_to_db` — complex sum across phase-aligned
  members; `|sum|²` → dB → existing LLR pipeline.
- `cross_cycle_averaging_pass` now branches: when `cross_cycle_coherent`
  is set AND the spectrogram has `.complex`, take the coherent path; else
  the existing non-coherent (hb-056) path.

Research builder `with_cross_cycle_coherent` + `--cross-cycle-coherent`
eval flag. Unit test `test_coherent_phase_rotor_and_gain` verifies the
math on synthetic data: rotor recovery exact, exactly 3 dB coherent gain
at N=2. Lib tests 194 → **195**.

## Result

| config              | recovered | novel | rate    |
|---------------------|----------:|------:|--------:|
| noncoh no-filter    |      4409 |  1613 | 0.51411 |
| **coh no-filter**   | **4399 (-10)** |  1608 | 0.51294 |
| noncoh with filter  |      4408 |   844 | 0.51399 |
| **coh with filter** | **4398 (-10)** |   836 | 0.51283 |

**−10 recovered both no-filter and filtered.** Coherent is *worse* than
non-coherent. Unit test math is right; the loss is empirical, not a
bug.

## Diagnosis

Two compounding effects, both inherent to the marginal-recovery regime:

### 1. Phase-estimate noise on the candidates we're trying to rescue

Phase recovery quality scales with `1/√(N_costas · SNR_per_sample)`.
With 21 Costas samples, even at SNR 0 dB the estimate has ~0.22 rad
precision. But the candidates hb-056 is rescuing are *marginal* — sync
scores 4-6 (≈ -3 to +3 dB SNR) — and there the phase estimate is
unstable.

A coherent sum of one well-phased candidate plus one noisily-phased
candidate is
`|cs_strong + cs_weak · exp(jε)|² = |cs_strong|² + |cs_weak|² +
2·Re(cs_strong · conj(cs_weak) · exp(jε))`
The cross-term has random sign (ε noisy) — same *mean* as the
non-coherent power sum, but **higher variance**, occasionally pushing
the result below the LDPC threshold where the non-coherent sum would
have cleared it. That's the −10 mechanism.

### 2. Inter-slot phase is not actually preserved in real-world audio

Even with a perfect phase estimate per slot, the *expected phase
shift* between slots `Δφ = 2π·f·15s` is only deterministic if the
whole TX→propagation→RX chain is phase-coherent across 15 s. In
practice:
- Propagation Doppler accumulates ~mHz·15 = ~milliradians per cycle —
  small but unmasked at the phase-estimate noise floor.
- Operator TX rigs with PLL synthesizers don't always preserve phase
  across keyings.
- The receiver's local oscillator + ADC clock generally *do* run
  continuously, but the captured signal still has the cumulative TX-side
  drift.

So even the "good" phase estimates per slot align candidates to slightly
different references, breaking the coherent integration assumption.

### Why the unit test passed but the eval lost

The unit test used unit-amplitude tones with zero-noise Costas symbols
and a deterministic phase difference — the regime where coherent
integration's 3 dB gain is exact. The hard-corpus regime has noisy
phase estimates AND no guarantee of inter-slot phase preservation —
both fatal to coherent gain.

## Decision

**SHELVE.** Default `cross_cycle_coherent = false`. main.json unchanged
(stays at hb-056's 0.556996). Infrastructure kept flag-gated since the
math is correct and three real follow-up variants might rescue the
approach — see Learnings.

## Learnings / follow-ups

- **The non-coherent variant (hb-056) wins for a reason.** Power
  summation doesn't rely on phase, so it cleanly survives both
  failure modes above. The +0.000816 it shipped is the right
  measurement of what's recoverable from cross-cycle integration *on
  real operator audio* — not a non-coherent floor that coherent can
  beat, but the actual ceiling for this corpus/architecture without
  more sophisticated phase modeling.
- **hb-075 (NEW, priority 0.30):** phase-magnitude-weighted coherent
  sum. Weight each member's contribution by the magnitude of its
  Costas-derived rotor (`|acc|` before normalisation). Strong rotors
  contribute fully, weak rotors weakly — reduces variance from
  noisily-phased members without losing the coherent gain when phases
  are good. Plausibly bounds the loss; uncertain whether it wins.
- **hb-076 (NEW, priority 0.30):** per-symbol-block phase recovery.
  Use each of the 3 Costas blocks (start/middle/end) to estimate
  phase locally, then drift-correct symbols within that block.
  Robust against per-slot phase drift (which the per-candidate global
  estimate averages over). Risk: 7-sample estimates per block are
  noisier than 21-sample, may compound.
- **hb-077 (NEW, priority 0.25):** acquire a phase-coherent eval
  corpus — direct SDR-IQ captures where TX→RX phase coherence is
  *guaranteed* (e.g., a known transmitter on a stable PA + SDR
  receiver). Tests whether the corpus is the binding constraint vs
  the algorithm. Requires hardware effort similar to hb-073.
- **Methodological note:** the unit test's `+3 dB at N=2` was
  necessary but not sufficient — it confirmed the math, not the
  applicability. Future phase-coherent experiments should include an
  *on-corpus phase-stability probe* (estimate inter-slot phase drift
  on a known signal) before claiming the math carries over.
- Infrastructure (`Spectrogram::complex`, complex extraction, phase
  rotor, coherent sum) lands behind the flag — all four hb-075/076/077
  + any future variant reuse it.
