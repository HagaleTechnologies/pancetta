# Batch 100 — hb-259 per-iteration EM (Es, N0) re-estimation in the BICM-ID rescue

**Date**: 2026-06-12
**Branch**: `iter/2026-06-12-batch-100`
**Hypothesis**: hb-259 (EM channel re-estimation in the demod-decode
loop); also tests the LAST remaining re-open path for hb-252/253's
rescue economics (Batch 99 named block-constant Es/N0 estimation as
the failure mode on interference-dominated slots and hb-259 as the
sole re-open path).

## Primary source (fetched, not from memory)

Cheng, Valenti & Torrieri, *Turbo-NFSK: Iterative Estimation,
Noncoherent Demodulation, and Decoding for Fast Fading Channels*
(MILCOM 2005). The direct PDF fetch timed out; the same material is
chapter 6 of Cheng's dissertation (*Coded Continuous-phase FSK:
Information Theoretic Limits and Receiver Design*, WVU, advisor
Valenti, co-author credit to Torrieri for ch. 4-7), fetched from
`community.wvu.edu/~mcvalenti/documents/ChengDissertation.pdf` and
read directly (pp. 100-113). Verbatim equation inventory:

- **Parametrization**: `A = N0`, `B = 2a√Es`; block-constant per
  block of N symbols ("blocks of N consecutive FSK symbols are
  attenuated by the same channel gain … noise that is stationary for
  the duration of the block"). Both estimated "because either or both
  could change from block-to-block due to jamming, interference, or
  other environmental conditions" — exactly pancetta's hard_200
  failure mode.
- **Complete-data log-likelihood (6.7)**:
  `L(A,B) ~ −MN·log A − C/A − N·B²/(4A) + Σ_i log I0(B·|y_{q_i,i}|/A)`
  with `C = Σ_{k,i} |y_{k,i}|²` for the orthogonal case.
- **E-step (6.9)–(6.13)**: per-symbol posterior tone probabilities
  under the previous estimates,
  `p_{k,i}^{(ξ−1)} = α_i · I0(B̂|y_{k,i}|/Â) · p(q_i = k)`, where the
  symbol prior comes from the decoder extrinsics:
  `p(q_i|v_i) = Π_j e^{v_{j,i}·b_j(q_i)} / (1 + e^{v_{j,i}})` (6.13).
- **M-step (6.15)/(6.16)**:
  `Â = (C − N·B̂²/4)/(MN)`;
  `B̂ = (2/N)·Σ_i Σ_k p_{k,i}·|y_{k,i}|·F(4MN·B̂·|y_{k,i}| / (4C − N·B̂²))`
  with `F(x) = I1(x)/I0(x)` — **implicit**, "found recursively". The
  paper itself ships two reduced-complexity variants (linear
  approximation of F, hard limiting of p_{k,i}) at +0.05…+0.15 dB
  extra loss.
- **Initialization (6.17)**: `B̂^(0) = (2/N)·Σ_i max_k |y_{k,i}|`
  (mean per-symbol max magnitude) — the same shape as pancetta's
  Batch 99 static estimator.
- **Schedule**: the EM inner loop runs to convergence per BICM-ID
  iteration ("halted … when the estimate of B changed less than 10%
  … or … a maximum of 20 iterations"); after the first BICM-ID
  iteration the priors come from the decoder output via (6.13).
- **Reported performance**: full EM = **0.55 dB loss vs known
  a√Es/N0** (16-FSK, rate-1/2 cdma2000 turbo, Rayleigh block fading
  N=4); EM-H +0.05, EM-L +0.1, EM-H/L +0.15 dB additional.

## Implementation (shipped behind default-identical flag)

`Ft8Config::bicm_id_em_reestimation: bool` (default **false**,
byte-identity test-pinned). Active only when `llr_metric = Bessel`
(where Es/N0 enter the metric) AND `bicm_id_iterations ≥ 1`; EM lives
strictly INSIDE the rescue loop, so it is definitionally inert at
iters=0 — Batch 99's static estimator still produces the iteration-0
seed and all primary-path LLRs.

Per global rescue iteration (`par_bicm_id_rescue`,
`bicm_id_em_reestimate`):

- **E-step** (paper (6.11)/(6.13), log-domain): per data symbol,
  `log w_j = ln I0(2√(Es·p_{gray(j)})/N0) + Σ_p v_p·b_p(j)` over the
  8 binary labels with `v = −extrinsic` (paper convention from
  pancetta's positive⇒bit-0), normalized by log-sum-exp (the
  bit-independent `1/(1+e^v)` factors of (6.13) cancel in α). The 21
  Costas symbols enter as **pilots** (posterior = δ at the known sync
  tone); the 58 data symbols use the extrinsic-prior posterior.
- **M-step** (power-domain moment matching — pancetta's simplification
  of the implicit (6.16), documented deviation): under the model the
  believed-signal tone power has mean `Es + N0` and each
  believed-noise tone power has mean `N0`, so
  `N0 ← Σ_i Σ_j q_{i,j}·(noise-tone power sum) / (79·7)` and
  `Es ← Σ_i Σ_j q_{i,j}·(signal-tone power) / 79 − N0` (floor
  `0.05·N0`, the Batch 99 floor). For the exponential noise tones the
  posterior-weighted mean IS the ML estimate; the signal-tone update
  is method-of-moments rather than the paper's exact amplitude
  recursion — same family of simplification the paper itself ships.
- **Inner schedule**: paper's stopping rule — halt when both
  estimates change <10%, max 20 inner iterations.
- **Re-scaling**: refreshed (Es, N0) rebuild the per-label Bessel
  metrics; the per-candidate least-squares scale `g` is refit against
  the (fixed) normalized seed LLRs each global iteration so the SOMAP
  metric stays commensurate with the extrinsic a-priori units (a
  global rescale is what `normalize_llrs` would absorb anyway — the
  payload of EM is the *relative* re-pricing).
- **Numerics/guards**: degenerate seeds (non-finite/non-positive)
  return unchanged; degenerate LS refit keeps the previous metrics
  instead of aborting the rescue. Scale-invariance in overall power
  preserved (hb-117 gain invariance).
- **Refactors** (identical float ops, pinned by the byte-identity
  tests + Part B em-off reproduction): `bessel_label_metrics` split
  into `bessel_label_metrics_with(es, n0)` core;
  `par_compute_soft_llrs_bessel` marginalization factored into
  `bessel_llrs_from_metrics`.

5 new tests (540 total, was 535): default-false pin; EM recovers a
seed 8× wrong in both directions to within 2× truth from uniform
priors; degenerate-seed identity; contradicting extrinsics inflate
the noise estimate ≥1.5× vs truth-consistent extrinsics (guards a
feedback-ignoring EM); flag-alone byte-identity decode test.

## Pre-registered bars

1. **Part A (synthetic)**: EM-on ≥ EM-off (no regression). EM pays
   under model mismatch; AWGN parity is acceptable — the synthetic
   plant is exactly the channel where block-constant (Es, N0) is
   already correct.
2. **Part B (decisive, hard_200/50)**: hb-252/253 re-open bar —
   EM-on rescue economics ΔTP > 0 AND ΔFP ≤ 2×ΔTP vs dualmax/it0.
   EM-off row must reproduce Batch 99's +1/+27.
3. Spot bar passes → full raw_530_full + hard_1000 graduation run
   (ΔTP ≥ +20, ΔFP ≤ +50 on raw; hard_1000 consistent).
4. Spot bar fails → hb-259 MEASURED-NO-RESCUE-FIX; hb-252/253 stay
   opt-in with the on-air A/B as the only remaining validation path;
   the rescue-FP problem is attributed to the CRC-collision floor
   rather than estimation.

## Results

### Part A — synthetic SNR sweep (50 trials/point, paired AWGN, −24..−16 dB, batch97/99 seed base)

| config            | 50%-threshold | shift vs dualmax/it0 | decode wall |
|-------------------|---------------|----------------------|-------------|
| dualmax/it0       | −18.73 dB     | —                    | 142.2 s     |
| bessel/it0        | −19.00 dB     | +0.273 dB            | 144.4 s     |
| bessel/it2/em-off | −19.23 dB     | +0.506 dB            | 229.2 s     |
| bessel/it2/em-on  | −19.25 dB     | **+0.523 dB**        | 237.6 s     |

- **Refactor fidelity**: the dualmax/it0, bessel/it0, and
  bessel/it2/em-off columns reproduce Batch 99's thresholds to the
  hundredth of a dB (−18.73 / −19.00 / −19.23; shifts +0.273 /
  +0.506) — the `bessel_label_metrics` /
  `par_compute_soft_llrs_bessel` refactors are float-identical, as
  designed.
- **Bar 1 (EM-on ≥ EM-off): PASSED** — EM delta +0.017 dB. As
  pre-registered, AWGN parity is the expected result: the synthetic
  plant is exactly the channel where the static block-constant
  (Es, N0) is already correct, so EM has nothing to fix. EM wall
  cost on top of the it2 rescue: +3.7%.

### Part B — the decisive test (hard_200 first 50 WAVs, ft8_lib truth, hash-normalized)

| config            | TP   | FP  | ΔTP  | ΔFP     | wall   |
|-------------------|------|-----|------|---------|--------|
| dualmax/it0       | 1167 | 309 | —    | —       | 17.6 s |
| bessel/it2/em-off | 1168 | 336 | +1   | +27     | 20.7 s |
| bessel/it2/em-on  | 1168 | 338 | +1   | **+29** | 20.5 s |

- **Batch 99 reproduction check: EXACT** — em-off gives ΔTP +1 /
  ΔFP +27 on the identical baseline (TP=1167/FP=309), byte-matching
  Batch 99's bessel/it2 row. Measurement integrity confirmed.
- **Bar 2 (hb-252/253 re-open, ΔTP > 0 AND ΔFP ≤ 2×ΔTP): NOT MET** —
  EM-on gives +1/+29, marginally WORSE FP economics than the static
  estimator (+1/+27). Per-iteration EM re-estimation of (Es, N0)
  does not separate true rescues from wrong-CRC rescues on
  interference-dominated slots; if anything the refreshed estimates
  hand the wrong-codeword BP fixed points slightly more confidence.
- Per pre-registration, the full raw_530_full + hard_1000 graduation
  run is NOT triggered.

## Verdict (pre-registered)

- **hb-259 → MEASURED-NO-RESCUE-FIX, shipped opt-in (default-false,
  byte-identity pinned).** The mechanism is implemented faithfully to
  the primary source (E-step verbatim (6.11)/(6.13) with Costas
  pilots; M-step a documented power-domain moment-matching
  simplification of the implicit (6.16), the same reduction family
  the paper ships), unit-verified (recovers an 8×-wrong seed; uses
  the decoder feedback), and measured: synthetic +0.017 dB (parity,
  as the model predicts on AWGN), real-corpus rescue economics
  +1/+29 vs +1/+27 static. Block-constant estimation quality was NOT
  the binding constraint on the rescue's FP problem.
- **Attribution shift (the strategic output)**: Batch 99 attributed
  the rescue-FP wall to "block-constant Es/N0 estimation is wrong on
  interference-dominated slots". Batch 100 falsifies that: a
  decoder-informed, EM-refined estimate — within 0.55 dB of perfect
  CSI in the literature for this exact receiver family — moves
  nothing. The wrong-CRC rescue population is a **CRC-collision
  floor**: near-converged wrong codewords that pass CRC-14 by chance
  among the marginal-candidate mass; every extra BP attempt re-rolls
  those dice, and no channel-estimation quality fixes that. (A
  per-symbol/per-tone interference-aware N0 would be a *different*
  hypothesis — but the E-step here already re-weights symbols through
  the posterior and the economics did not budge, so the expected
  value of that variant is low.)
- **hb-252/253: corpus-side re-open path CLOSED.** Three
  discriminators now measured dead: unsat-threshold gating (B98),
  Bessel metric sharpening (B99), EM channel estimation (B100). Both
  stay SHIP-OPT-IN exactly as shipped (+0.506 dB composed synthetic,
  additive); the on-air Phase 5 A/B (llr_metric=Bessel ±
  bicm_id_iterations=2, decode-count comparison across sessions —
  already in the meatspace ledger) is their only remaining validation
  path, where marginal signals are continuously distributed and a
  half-dB can convert. A future corpus of MARGINAL (0-0.5 dB below
  threshold) real signals would also test it — noted for the
  corpus-expansion lane.
