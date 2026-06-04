# hb-117 — AGC-diversity: re-decode at multiple synthetic gain settings — SHELVED

**Date**: 2026-06-04
**Branch**: iter/2026-06-04-hb-117-agc-diversity
**Status**: **SHELVED — definitive at the ±12 dB scale**
**Effort**: ~20 minutes (diagnostic + run + journal)

## Question

Pancetta-ft8's float32 internal pipeline is supposed to be gain-invariant.
Does real-world quantization, log-domain threshold behavior, or
SNR-estimator nonlinearity produce a decode-set diversity across gain
settings large enough to mine for new true-positives via an
AGC-ensemble?

Per the bank entry hb-117 kill-switch:

> 20 hard-200, rescale ±12 dB, count NEW true-positives at off-baseline
> gains. < 1.0 per WAV mean → too weak to mine.

## Setup

`pancetta-research/examples/hb117_agc_diversity_diagnostic.rs`. For each
of the top 20 hard-200 WAVs:

1. Load original audio.
2. For each gain g ∈ {-12 dB, 0 dB, +12 dB} (linear factors ≈ 0.251,
   1.0, 3.981): rescale, decode with `Ft8Config::default()`, collect
   text-set.
3. Truth = jt9 baseline messages for the WAV.
4. "New TP at off-baseline" = decoded(g) ∩ truth \ decoded(0 dB) for
   g ∈ {-12, +12}; summed across off-baseline gains.

PROCEED if mean(new TP / WAV) ≥ 1.0. SHELVE otherwise.

## Result

**0 novel TPs across 20 WAVs (mean 0.000/WAV).**

| Statistic | Value |
|---|---:|
| WAVs scored | 20 |
| Total truth across WAVs | 861 |
| Total novel TPs at off-baseline | **0** |
| Mean novel TPs per WAV | **0.000** |
| Threshold for PROCEED | ≥ 1.0 |

Per-WAV TP counts were essentially identical at all three gains
(differences ≤ 1 TP and always at the boundary between decoded
"recovered" and decoded "novel", never as a NEW true-positive). Total
decode counts wobble slightly (e.g. `ac493417`: -12=36, 0=39, +12=41
total) — gain DOES perturb FP rate — but the truth-matched subset is
byte-identical across the gain range.

## Mechanism finding

Pancetta-ft8's hot path uses `dB = 10 * log10(power)` spectrograms.
Pure-linear scaling of the input audio adds a constant offset to every
spectrogram cell in dB (`scaled_db = original_db + 20 * log10(gain)`).
Costas scoring, max-log LLR extraction, and LDPC BP are all
**invariant to constant offsets** (Costas takes signal-minus-neighbor
differences; LLRs take max-vs-max differences; BP converges to the
same fixed points). The TP set being byte-identical across ±12 dB is
the predicted result.

Where gain DOES affect behavior (and thus where the small total-count
wobble comes from): SNR estimation absolute-floor and the
plausibility / FP gates downstream of decode. Those run on absolute
dB values and shift accordingly. But none of those gates are
"discover a new TP at off-baseline" — they're "accept-or-reject what
LDPC already found," which is gain-INDEPENDENT.

The hb-069 SHELVE (which the bank entry cited as evidence-for) found
that log-domain vs linear-domain interpolation perturbs spectrogram
values at fractional bin levels — that effect is ~0.1 dB scale.
Constant ±12 dB scaling is structurally different and doesn't
trigger the same effect.

## What this closes

- The hb-117 AGC-diversity ensemble idea: dead lever on hard-200.
- Implicitly: any "re-decode at different gains and vote" ensemble is
  unproductive on this corpus.
- Strengthens the case that pancetta's FP-rate dependence on gain
  (the total-count wobble) is downstream of decode and would not be
  recovered by gain ensembling.

## What this doesn't close

- **Very extreme gain scaling** (e.g., ±48 dB) could push individual
  samples below f32 precision or into f32 inf — not tested. Such
  ranges aren't physically meaningful in practice.
- **Time-varying gain** (e.g., AGC compression artifacts in field
  recordings) is the more realistic concern. hb-117 is about
  ensembling, not detecting AGC-corrupted WAVs.

## Substance-check notes

- Decisive zero result on 20 WAVs is structurally explained by the
  dB-domain invariance argument above; no need to retest at larger N.
- This SHELVE is in the same family as hb-092 (codeword-dedup SHELVED
  2026-06-04 at 0/6912): the mechanism the bank entry proposed
  doesn't *exist* in pancetta on this corpus, not just "doesn't pay
  off." Both pre-empt entire downstream branches of the bank.

## Artifacts

- `pancetta-research/examples/hb117_agc_diversity_diagnostic.rs` —
  the diagnostic (kept; tunable via HB117_TOP_N, HB117_GAINS_DB).
- `research/hypothesis_bank.md` — hb-117 marked SHELVED.
- This journal.

## Production impact

None. Research-only diagnostic.

## Counters

- hb-117 status: pending → SHELVED.
- Bank counters: +1 shelf.
