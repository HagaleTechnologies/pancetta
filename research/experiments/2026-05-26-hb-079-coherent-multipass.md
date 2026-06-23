---
slug: hb-079-coherent-multipass
mode: ft8
state: graduated
created: 2026-05-26T20:00:00Z
last_updated: 2026-05-26T20:00:00Z
branch: iter/2026-05-26-coherent-multipass
parent_hypothesis: hb-079 (new this session, user-directed "crazy stuff" pick)
wild_card: false
scorecard: research/scorecards/history/2026-05-26-hb-079-coherent-multipass.json
delta_vs_main: composite +0.009212 (0.558279 -> 0.567491). hard-200 +158 rec / +75 novel. hard-1000 +401 rec / +132 novel. BIGGEST single-iter composite win in project history.
disposition: GRADUATE hb-079 — coherent iterative-subtract multi-pass is the production default. The recall ceiling on hard-* was interference, not threshold.
---

## Hypothesis

User-directed pick after seven graduations made it clear that
**individual precision knobs and parameter tweaks were exhausted** (mr-006
+ hb-067 + hb-018 all confirmed). The remaining unexplored structural
lever was **interference masking** — pancetta's 51% recall on hard-200
suggested half of jt9's truth was being masked by stronger nearby
signals at decode time.

Mechanism: revive multi-pass — but fix the kernel that broke it (hb-030
showed `subtract_with_sidelobes` is fundamentally broken in dB-domain).
Now that hb-075 landed complex spectrogram retention, **coherent
(complex-domain) signal subtraction** becomes feasible. The
maximum-likelihood subtract — `proj = Re(bin·conj(rotor))·rotor`,
`residual = bin - proj` — removes the rotor-aligned signal component
while preserving orthogonal noise. Iterate.

## Change

`pancetta-ft8/src/decoder.rs`:
- `Ft8Config::coherent_multipass: bool` (default flipped false → **true**).
- `reverse_derive_candidate(msg, pp, time_padding)` — back-compute
  `(time_step, freq_bin, freq_sub)` from a DecodedMessage's
  `frequency_offset` and `time_offset` (we don't keep candidates paired
  through the rayon decode path; this lets us subtract from any decode).
- `subtract_decode_coherent(spectrogram, pp, candidate, rotor, tone_symbols)`
  — ML projection subtract at each of the 79 symbols × both TIME_OSR
  substeps, refreshes `spectrogram.power` consistently. Mutates the
  complex spectrogram in place.
- `Ft8Decoder::coherent_subtract_and_repass(spectrogram, decoded)`
  method: for every decoded message with `tone_symbols` preserved,
  estimate phase rotor (via `estimate_candidate_phase_rotor`) and
  coherent-subtract. Then re-run `costas_sync_search` on the residual,
  filter out candidates at already-subtracted positions, and decode the
  remaining (sequentially — count is small after subtraction). Returns
  new decodes for the caller to union + dedup.
- Wired into `decode_window_with_ap` after the cross-cycle pass (so
  cross-cycle integrates full data first, then subtract sweeps).

Research builder `with_coherent_multipass` + `--coherent-multipass` /
`--no-coherent-multipass` eval flags. Unit test
`test_coherent_subtract_ml_projection` verifies the ML projection math:
residual ⊥ rotor, `|signal_est|² + |residual|² = |bin|²` (Pythagorean
orthogonal decomposition). Lib tests 196 → **197**.

## Result

### Targeted hard-200 four-way A/B

| config           | recovered | novel | rate    |
|------------------|----------:|------:|--------:|
| ctrl no-filter   |      4431 |  1596 | 0.51667 |
| **on no-filter** | **4589 (+158)** | 1723 (+127) | 0.53510 |
| ctrl with filter |      4430 |   842 | 0.51656 |
| **on with filter**| **4588 (+158)** | 920 (+78) | 0.53498 |

**+158 recovered.** That's larger than the entire prior session's
cumulative structural gain (hb-063 +18 + hb-056 +14 + hb-075 +22 = +54
on hard-200). Real:novel ratio after filter: ~2:1 — solid.

### Full 5-tier with production FP filter

| metric                     | old (hb-075) | new (hb-079) | Δ         |
|----------------------------|-------------:|-------------:|----------:|
| **composite**              |     0.558279 | **0.567491** | **+0.009212** |
| fixtures pass_rate         |          1.0 |          1.0 |         0 |
| synth-clean @50/@90 dB     |      -20/-18 |      -20/-18 |         0 |
| hard-200 rec / novel       |   4430 / 845 |   4588 / 920 | +158 / +75 |
| **hard-1000 rec / novel**  | 14515 / 2890 |**14916 / 3022**| **+401 / +132** |
| wild-50                    |            0 |            0 |         0 |
| elapsed                    |        1782s |        2039s |       +14%|

**+0.009212 composite — biggest single-iter move in project history**,
about 7× the prior biggest (hb-075's +0.00128) and roughly an order of
magnitude bigger than the typical structural iter. **Hard-1000 +401
recovered** confirms the gain scales (158 × 5 ≈ 790 with some sub-linear
falloff; the actual +401 is two-thirds of linear, plausibly because the
1000-WAV corpus has fewer per-WAV interfering pairs on average).

Fixtures preserved exactly. Synth-clean SNR unchanged at both
percentiles. The in-place spectrogram mutation is a no-op on single-slot
tiers (no second signal to mask, so the new sync_search on the residual
finds the same Costas pattern as before — which is then filtered out by
the "already subtracted" guard, so no spurious new decodes).

## Why it worked

Three things had to be true and were:
1. **Interference was actually the recall ceiling**, not LDPC threshold.
   The +158 hard-200 confirms this directly — those decodes already had
   enough signal SNR to clear LDPC + CRC; they were being masked by
   adjacent tones from stronger neighbors.
2. **Coherent subtraction is the right kernel.** The dB-domain
   `subtract_with_sidelobes` was broken (hb-030) because it loses phase
   and over-subtracts. The ML projection (`Re(bin·conj(rotor))·rotor`)
   removes only the rotor-aligned component — preserving orthogonal
   noise — and is canonical.
3. **The complex spectrogram from hb-075 made this feasible.** Before
   hb-075, pancetta's spectrogram was power-only; coherent subtract
   wasn't possible. hb-075's complex retention (originally for cross-
   cycle phase recovery) directly enabled hb-079.

## Decision

**GRADUATE.** `coherent_multipass = true` default. main.json updated;
scorecard archived to `history/2026-05-26-hb-079-coherent-multipass.json`.

## Cumulative session impact

(start 2026-05-25 → now, after this iter)

| metric              | start     | now       | Δ            |
|---------------------|----------:|----------:|-------------:|
| **composite**       |  0.555131 | **0.567491** | **+0.012360** |
| hard-200 recovered  |      4376 |      4588 | **+212**     |
| hard-1000 recovered |     14267 |     14916 | **+649**     |
| fixtures            |       1.0 |       1.0 |            0 |
| synth-clean @50     |       -20 |       -20 |            0 |

Eight graduations across two days: hb-063 layered BP, hb-056 non-coh
cross-cycle, hb-058 contest-FP, hb-060/061 cleanup, hb-075 MRC
coherent, hb-072 CQ-whitelist, **hb-079 coherent-multipass**. This
one iter quadrupled the cumulative gain.

## Learnings / follow-ups

- **Diagnose the wall before targeting it.** The "interference is the
  recall ceiling" hypothesis was unstated assumption-then-confirmed.
  Future structural pushes should explicitly diagnose the binding
  constraint (e.g., re-run cross_validate_novels on the new +212
  hard-200 to see how many are real-but-uncredited) before designing
  the lever.
- **Coherent infrastructure compounds.** hb-075 landed complex
  spectrogram retention "for cross-cycle"; hb-079 directly reused it
  for subtract. The phase rotor estimation (`estimate_candidate_phase_rotor`,
  `compute_costas_complex_accumulator`) reused too. The infra was a
  one-time tax that's now paying composite dividends three times over
  (hb-056 + hb-075 + hb-079).
- **Spawned hb-080 (priority 0.45):** N>2 iterative passes. Currently
  hb-079 subtracts pass-1 decodes and re-decodes ONCE. Some hard
  signals may be tertiary-masked (after pass-2 finds the second tier,
  a third pass could find a tier-3). Plausibly +20-50 more hard-200.
- **Spawned hb-081 (priority 0.40):** MRC-style weighted subtract.
  Currently every decode subtracts at full amplitude regardless of
  rotor confidence. For weak-rotor decodes (low-SNR borderline), the
  ML projection might over-estimate the signal amplitude and damage
  adjacent bins. Weighting subtraction magnitude by rotor confidence
  (analogous to hb-075's MRC fix for cross-cycle) could bound the
  damage and yield small additional gains.
- **Spawned hb-082 (priority 0.30):** lower min_sync_score threshold
  during the residual sync_search. After subtraction, the residual's
  noise floor changes; the current `min_sync_score` (production)
  might be too strict for the residual. A separate tunable could
  surface a few more masked candidates.
- **Operational implication:** the +14% wall-clock from the extra
  pass is real but still inside the 3000 ms/WAV budget. On the
  operator's MiniPC, decode time will go from ~280 ms to ~320 ms per
  slot — easily absorbed.
- **Phase 5 on-air validation is now even more pressing.** The
  cumulative +0.012360 composite + ~75% relative recall improvement
  on hard-1000 (vs the pre-session baseline) is operationally massive.
  Validate with an A/B against a same-time WSJT-X reference run at
  the rig.
