# Batch 99 — hb-253 exact Bessel noncoherent LLR metric (kill-switch probe)

**Date**: 2026-06-12
**Branch**: `iter/2026-06-12-batch-99`
**Hypothesis**: hb-253 (exact Bessel metric vs dual-max approximation); also
tests hb-252's pre-registered re-open condition (sharper rescue LLRs).

## Primary source (fetched, not from memory)

Guillén i Fàbregas & Grant, *Capacity Approaching Codes for Non-Coherent
Orthogonal Modulation* (IEEE Trans. Wireless Commun.), PDF fetched from
upf.edu and read directly. Verbatim equation inventory:

- **System model**: `y[k] = √Es·h[k]·x[k] + n[k]`, `n ~ CN(0, N0)`;
  AWGN is `h = 1`; noncoherent receiver measures per-bin energies.
- **Eq. (1)** exact transition probability:
  `p(y | x = e_b) = K·I0(2·(√Es/N0)·a·|y_b|)` with `K` independent of
  the hypothesis `b`.
- **Eq. (6)** exact ("Bessel") bit metric: `L = ln Σ_{b∈B0} p(y|e_b)·q(b)
  − ln Σ_{b∈B1} p(y|e_b)·q(b)` — true sums over labels, extrinsic priors
  `q_{k,i}(b)` exclude bit `i`.
- **Eq. (7)** "Bessel dual-max": max-log approximation of (6).
- **Eq. (12)/(13)** estimation-free variants; **eq. (13)** — dual-max over
  `log(|y_b|²)` — **is exactly pancetta's production demapper** (dB power
  is 10/ln10 × log power; the constant is absorbed by `normalize_llrs`).
  The paper measures the eq.-(13)→(6) gap at ~0.6 dB (64-FSK, RA code,
  iterative decoding; "the loss for not knowing γ is shown to be around
  0.6 dB"); at most ~1.5 dB for high M. M=8 should sit below that.

A notable paper finding that tempers expectations: the parameter-free
dual-max (13) is *better* than the parameter-free sum metric (12) and
"show[s] performance close to the ideal Bessel metric" — i.e., the
upgrade margin over pancetta's current metric is bounded and was always
going to be sub-dB.

## Implementation (shipped behind default-identical flag)

`Ft8Config::llr_metric: LlrMetric {DualMax (default), Bessel}`:

- **Metric**: per-label `m_j = ln I0(2·√(Es·p_{gray(j)})/N0)` from linear
  tone power `p` (spectrogram stores dB power `10·log10(mag²)` → converted
  via `10^(dB/10)`; fine-FFT path stores linear magnitude → squared).
  Bit LLRs by **exact log-sum-exp** over the 4-label sets (eq. (6) with
  zero priors), pancetta sign convention (positive ⇒ bit 0).
- **Estimator** (simplest defensible, documented per pre-registration):
  block-constant per candidate. `N0` = median of the 7 non-max tone
  powers across all 79 symbols (553 samples), ÷ ln 2 (median of
  Exp(N0) is N0·ln 2). `Es` = mean per-symbol max-tone power − N0,
  floored at 0.05·N0. Scale-invariant in the Bessel argument (gain
  invariance preserved, consistent with hb-117). Known bias: per-symbol
  max adds positive selection bias at marginal SNR. Fallback if too
  noisy: the paper's estimation-free metrics — but eq. (13) is already
  production, so "fallback" = SHELVE.
- **Numerics**: `ln_i0(x)` via Abramowitz & Stegun §9.8.1/9.8.2
  polynomials (|ε| ≲ 2e-7): `x < 3.75` series branch (≈ x²/4 for small
  x), `x ≥ 3.75` asymptotic branch `x − ½·ln x + ln(poly(3.75/x))`
  (overflow-free; the ½·ln(2πx) shape with 1/√2π folded into the
  polynomial). Unit-tested against direct power-series reference at 16
  points across both branches (tol 1e-5) and against the 2-term
  asymptotic expansion at x ∈ {50, 100, 1e3, 1e6} (tol 1e-3).
- **BICM-ID composition** (hb-252 re-open test): `par_bicm_id_rescue`
  threads the metric — Bessel rescues use Bessel label metrics and an
  exact-LSE SOMAP refresh (full eq. (6) with extrinsic priors; the
  label-independent prior-normalization terms cancel in the LLR
  difference). DualMax rescues keep the historical max-log refresh
  byte-identically.
- **Pipeline**: downstream whitening + variance-normalization unchanged
  for both metrics (A/B purity). Confound documented: whitening is a
  per-symbol divisive step that could partially override Bessel's
  per-symbol confidence shaping; harness has `BATCH99_WHITEN_OFF=1`
  for a both-configs-unwhitened secondary diagnostic.
- **Scope**: primary parallel decode paths (spectrogram + fine-FFT) in
  `par_decode_candidate`, FT8 (3 bits/symbol) only; FT4/FT2 and the
  sequential legacy path fall back to dual-max. Same kill-switch scope
  as Batch 97.

## Pre-registered bars

1. **Metric REAL**: Bessel (iters=0) synthetic 50%-threshold shift
   ≥ +0.15 dB vs DualMax (iters=0).
2. **hb-252 RE-OPEN**: Bessel × iters=2 spot ΔFP ≤ 2×ΔTP with ΔTP > 0
   vs DualMax/iters=0 baseline (hard_200/50) → full-corpus graduation
   run for the rescue.
3. **SHELVE hb-253**: Bessel (iters=0) synthetic shift < +0.1 dB —
   estimation noise eats the theoretical gap; record the estimator so
   a future attempt doesn't repeat it.

## Results

### Part A — synthetic SNR sweep (50 trials/point, paired AWGN, −24..−16 dB)

| config       | 50%-threshold | shift vs dualmax/it0 | decode wall |
|--------------|---------------|----------------------|-------------|
| dualmax/it0  | −18.73 dB     | —                    | 135.9 s     |
| bessel/it0   | −19.00 dB     | **+0.273 dB**        | 138.2 s     |
| dualmax/it2  | −19.03 dB     | +0.306 dB            | 214.5 s     |
| bessel/it2   | −19.23 dB     | **+0.506 dB**        | 219.7 s     |

- **Bar 1 (metric REAL, ≥ +0.15 dB): PASSED** at +0.273 dB — inside the
  paper's predicted sub-0.6-dB band for the eq.(13)→(6) upgrade at M=8
  with *estimated* (not known) Es/N0. The estimator works.
- dualmax/it2 +0.306 dB reproduces Batch 97/98's BICM-ID mechanism
  (+0.384/+0.500 dB there; same seed base, now under the Batch-98 gate).
- The two mechanisms **compose additively** on synthetic: +0.506 dB
  combined ≈ 0.273 + 0.306 − ε. At −18.5 dB the decode rate goes
  60% → 88%.
- Bessel wall-clock cost is negligible (+1.7% at iters=0): one
  `ln_i0` per tone per data symbol plus a 553-element median.

### Part B — real-corpus spot (hard_200 first 50 WAVs, ft8_lib truth, hash-normalized)

| config       | TP   | FP  | ΔTP  | ΔFP  | wall   |
|--------------|------|-----|------|------|--------|
| dualmax/it0  | 1167 | 309 | —    | —    | 16.8 s |
| bessel/it0   | 1168 | 323 | +1   | +14  | 17.1 s |
| dualmax/it2  | 1169 | 326 | +2   | +17  | 17.9 s |
| bessel/it2   | 1168 | 336 | +1   | +27  | 19.6 s |

- **Bar 2 (hb-252 re-open, ΔFP ≤ 2×ΔTP with ΔTP > 0): NOT MET** —
  bessel/it2 gives +1/+27. The Bessel metric does not sharpen the
  rescue's true/wrong-codeword discrimination; it makes the rescue's
  FP economics *worse* than dual-max (+2/+17, which exactly reproduces
  Batch 98's reference numbers).
- Bessel alone (+1/+14) is also FP-negative on the real corpus: the
  sharper synthetic metric surfaces ~14 additional CRC-passing wrong
  codewords per 50 WAVs for ~1 extra truth.
- Interpretation: the synthetic plant is single-signal AWGN — exactly
  the paper's channel model, where block-constant (Es, N0) is correct.
  hard_200 is interference-dominated; a block-constant noise estimate
  under-prices interference-hit symbols, and the exact-LSE metric's
  extra confidence becomes extra wrong-codeword BP convergence. This is
  the same FP wall hb-252 hit (Batch 97/98) — another mechanism whose
  synthetic dB-gain fails to clear real-corpus FP pricing.

### Secondary diagnostic — whitening off (BATCH99_WHITEN_OFF=1)

Same sweep with `llr_whitening_enabled = false` in all four configs.
Purpose: bound how much of the metric shift the per-symbol divisive
whitening step absorbs/masks. Result — **confound cleared, whitening
is orthogonal to the metric**:

- Synthetic: dualmax/it0 −18.72 dB, bessel/it0 −19.00 dB (metric shift
  **+0.278 dB**, vs +0.273 with whitening on); dualmax/it2 −19.03,
  bessel/it2 −19.23 (combined +0.511 dB, vs +0.506).
- Spot (baseline TP=1166 FP=312 unwhitened): bessel/it0 +1/+14
  (identical to whitened), dualmax/it2 +4/+21, bessel/it2 +1/+25;
  re-open bar still NOT MET.

## Verdict

- **hb-253 → MECHANISM-CONFIRMED-FP-PENDING, shipped opt-in.** Bar 1
  passed (+0.273 dB synthetic, estimation included), bar 3 (SHELVE)
  not triggered. The exact metric is real on FT8 blocks and composes
  with BICM-ID (+0.506 dB combined) — but bar 2 failed, so the default
  stays `DualMax` (byte-identity test pinned) and no full-corpus
  graduation run is triggered (per pre-registration). Opt-in surface:
  `Ft8Config::llr_metric = LlrMetric::Bessel`.
- **hb-252 stays SHIP-OPT-IN; the Bessel re-open condition is now
  MEASURED-DEAD.** Batch 98's re-open clause named two candidate
  discriminators: hb-253 exact metric (this batch: rescue economics
  got worse, +1/+27 vs +2/+17) and hb-259 per-candidate Es/N0 EM
  re-estimation (still open — and now more interesting: the failure
  mode identified here is exactly the block-constant-N0 assumption
  hb-259 attacks with per-iteration re-estimation).
- Next lever if anyone re-attacks this line: per-symbol or per-tone N0
  (interference-aware) instead of block-constant — i.e., go through
  hb-259, not another demapper variant.
