# Batch 101 — hb-256 impulse-robust per-symbol LLR weighting (lightning-static robustness)

**Date**: 2026-06-12
**Branch**: `iter/2026-06-12-batch-101`
**Hypothesis**: hb-256 (bank entry, spawned Batch 96 web scan)
**Primary source**: Clavier, Peters, Septier & Nevat, *Impulsive noise
modeling and robust receiver design*, EURASIP JWCN 2021:13 (open
access; PDF fetched and read directly — eq. (15) and Sect. 3.4.5).

## Applicability check (done FIRST, before any implementation)

The paper's robust LLR is `LLR(y) = sign(y)·min(a|y|, b/|y|)`
(eq. (15)) where `y` is a **time-domain per-sample / matched-filter
observation** in a BPSK-style binary test. The mechanism behind it
(Sect. 3.4.5): under sub-exponential (alpha-stable, Middleton)
impulsive noise the optimal LLR is **non-monotonic** — it rises
linearly for small `|y|`, peaks, then decays toward zero — i.e. large
received amplitudes must be ATTENUATED (∝ 1/|y|), not trusted (∝ |y|).
Parameters `a` (linear slope) and `b` (inverse-branch scale) are fit
by various estimation methods; the paper gives no closed-form setting.

**Decision: the literal form does NOT map — implemented the TRANSLATED
mechanism.** Pancetta's 174 LLRs are dB tone-power differences
(max-vs-max over 8 Gray-mapped tone bins) in the spectrogram domain;
there is no per-bit scalar amplitude `y` whose magnitude carries the
impulse. Where impulsive noise *does* manifest: a lightning crash is
broadband + short-time, so it inflates ALL 8 tone bins of 1-3 symbols
(160-480 ms). The per-symbol **total 8-tone linear power** is the
amplitude statistic analogous to `|y|`. Translation:

- linear branch (`a|y|`): symbols whose total power `P_s` is at or
  below `k×` the candidate's own median symbol power `P_med` keep
  their demapper LLRs unchanged — the existing extraction IS the
  small-amplitude trusted regime;
- inverse branch (`b/|y|`): symbols with `P_s > k·P_med` are
  impulse-suspect; their `bits_per_symbol` LLRs are multiplied by
  `w = k·P_med / P_s < 1` — attenuation ∝ 1/power, continuous at the
  knee (`w → 1`).

The single knob `k` plays the role of the `a`/`b` crossover
(`|y|* = sqrt(b/a)`); the per-candidate median makes it scale-free
(invariant to input gain, dB reference, and whitening order — cf.
hb-117's gain-invariance result).

Relation to LLR whitening (shipped default-on): whitening divides each
symbol's LLRs by `sqrt(n_tone[winner]·n_symbol)` — a soft, always-on,
geometric-mean rescale. The hb-256 weighting is a hard outlier gate
that only acts past the knee and attenuates ∝ 1/P. They compose; the
probe measures hb-256 ON TOP of the production default (whitening on),
per probe-baseline discipline.

## Ship surface

`Ft8Config::impulse_robust_llr: Option<f64>` (default `None` =
byte-identical, tested). Applied at all 10 demapper output sites
(after `maybe_whiten_llrs`, before `normalize_llrs`): the 7 `&self`
paths (AP, averaged-candidate, subtract-repass ×3, spectrogram
decode_candidate, fine-FFT decode_candidate) and the 3 parallel-ctx
paths (par spectrogram, par fine-FFT, par AP). dB-vs-linear-magnitude
units handled per site via `ToneUnits` (fine-FFT paths store `|y|`,
spectrogram paths store dB log-power); a unit test pins that both
unit paths agree on the same physical scene.

## Synthetic noise model (Part A impulsive plant — documented)

Bernoulli-timed broadband bursts on top of the base AWGN (simple
alternative to a symmetric alpha-stable sampler):

- base: AWGN at σ for the target SNR (2500 Hz reference BW; SNR is
  measured against the BASE floor in both plants — impulses are
  additional energy);
- burst process: at each FT8 symbol boundary (1920 samples @ 12 kHz),
  p = 0.02 a burst begins; duration uniform {1, 2, 3} symbols;
  overlapping bursts add in variance;
- burst waveform: extra white Gaussian noise, σ_burst = σ_base ·
  10^(A/20), A ~ Uniform(10, 30) dB;
- expected duty ≈ p·E[dur] = 4% of symbols impacted.

Paired seeds: same base-noise realization across all 3 configs and
both plants at each (SNR, trial).

## Pre-registered bars

- (a) pure AWGN: robust-on must NOT regress (> 0.05 dB threshold
  regression vs off = FAIL);
- (b) impulsive: robust-on threshold improvement ≥ +0.3 dB = PASS;
- (c) real-corpus spot (hard_200/50, {off, k=6}): ΔTP ≥ −2.
- Both synthetic bars pass + no real-corpus regression → SHIP
  default-ON candidate (full raw_530_full + hard_1000 graduation per
  standard rule). AWGN regression or impulsive bar fails → SHELVE
  with the translated mechanism recorded.

Note: the corpus characterization framework (Batch 65) records
decode-derived per-corpus stats only — no per-day impulsiveness /
kurtosis metric exists, so the per-day "most impulsive days" probe
from the bank entry is not accessible; Part B falls back to the
hard_200/50 do-no-harm spot per the probe spec.

## Results

Run: `batch101_impulse_robust_llr_kill_switch` (release, 50
trials/point, 17 SNR points −24..−16 in 0.5 dB steps, paired seeds
`0xB101_0000` base).

### Part A — 50%-decode-rate thresholds

| noise model | off | k=3 | k=6 | shift k=3 | shift k=6 |
|---|---|---|---|---|---|
| pure AWGN | −18.74 dB | −18.74 dB | −18.74 dB | **+0.000** | **+0.000** |
| impulsive | −17.95 dB | −18.17 dB | −18.20 dB | **+0.219** | **+0.253** |

Pure-AWGN decode-rate curves are IDENTICAL at every SNR point for all
three configs (not just equal thresholds — every cell matches): on
clean Gaussian noise the per-symbol totals essentially never exceed
3× the median, so the knee never fires. Zero-cost on the nominal
channel, exactly as designed.

The impulsive plant costs the baseline 0.79 dB of threshold
(−18.74 → −17.95). The robust weighting recovers +0.22 dB (k=3) /
+0.25 dB (k=6) of that — about a third of the impulse-induced loss.
Largest single-point gain: −18.0 dB, off 23/50 → k 27/50 (+8 pp).
Wall cost: ≤ +1.1s on 143s (≤ 0.8%, noise).

### Part A bars

- (a) AWGN no-regression: k=3 +0.000, k=6 +0.000 → **PASS** (both)
- (b) impulsive ≥ +0.3 dB: k=3 +0.219, k=6 +0.253 → **FAIL** (both)

### Part B — hard_200/50 spot ({off, k=6}, ft8_lib truth, hash-normalized)

| config | TP | FP | wall |
|---|---|---|---|
| off | 1167 | 309 | 17.4s |
| k=6 | 1168 | 308 | 17.4s |

ΔTP **+1** / ΔFP **−1** → no-regression bar (ΔTP ≥ −2) **PASS**.

## Verdict: SHELVE (pre-registered — impulsive bar not met)

The pre-registered ship rule required BOTH synthetic bars; the
impulsive improvement landed at +0.22/+0.25 dB against the +0.3 dB
bar. Per pre-registration: **SHELVE**, translated-mechanism
description recorded above.

Honest characterization for the bank: the mechanism is REAL and
correctly signed everywhere measured — zero AWGN cost (byte-level
identical curves), +0.25 dB under impulsive noise, +1/−1 on the real
corpus — it is just *small relative to the bar*. Plausible reasons it
undershoots: (1) the always-on LLR whitening (default since the
graduation) already divides by a per-symbol noise median, absorbing
part of the same effect the knee targets; (2) Costas sync itself
degrades under bursts and no LLR-domain fix recovers a sync miss;
(3) the burst-duty here (4% of symbols) caps the recoverable margin.
A future re-open path is a real storm-day corpus (no impulsiveness
metric exists in the corpus framework today — a per-day
kurtosis/crest-factor characterization sweep would identify candidate
days) or an on-air A/B during summer lightning season (meatspace).
The flag ships opt-in (`impulse_robust_llr: Option<f64>`, default
`None`, byte-identical, 8 new tests) so the mechanism stays available
to operators in QRN-heavy conditions at zero default-path cost.

## Counters

- Tests: 548 passed / 0 failed across 13 suites (was 540; +8 new in
  `impulse_robust_llr_tests`, incl. default-None byte-identity
  end-to-end and dB-vs-linear-mag unit agreement).
- Workspace build with `--features transmit`: clean.
- Production default config unchanged (None = byte-identical).
- hb-256: PROBE-MEASURED → SHELVE-OPT-IN-AVAILABLE.
