# Batch 97 — hb-252 BICM-ID kill-switch (SOMAP iterative demodulation)

Date: 2026-06-12
Branch: `iter/2026-06-12-batch-97`
Hypothesis: hb-252 (priority 0.70, spawned Batch 96 deep-research sweep)
Primary source: Valenti & Cheng, "Iterative Demodulation and Decoding of
Turbo-Coded M-ary Noncoherent Orthogonal Modulation", IEEE JSAC 23(9)
2005, eq. 8 (PDF verified directly during this batch; eq. 8 max-log
SOMAP and eq. 16 NCSI metric transcribed from the paper, not from
memory).

## Mechanism

Pancetta's per-symbol tone-LLR extraction (max-log over 8 Gray-labeled
tone magnitudes → 3 bit LLRs) is the zero-feedback degenerate case of
the SOMAP demodulator. BICM-ID closes the loop: after a candidate's
standard BP attempt fails CRC, compute the decoder extrinsic
(BP posterior − channel input) per bit, feed it back as a-priori into
the symbol-level LLR computation —

    LLR_i = max over labels with b_i=0 of (metric_j + Σ_{p≠i} v_p·b_p(j))
          − max over labels with b_i=1 of (same)

(pancetta sign convention, positive ⇒ bit 0; v_p = paper-convention
a-priori = −pancetta extrinsic) — then re-run BP on the refreshed
channel LLRs. The `p ≠ i` exclusion makes the demod output extrinsic,
exactly eq. 8 with max-star degraded to max (max-log).

## Implementation

- `Ft8Config::bicm_id_iterations: usize`, **default 0 = byte-identical**
  (guarded by `bicm_id_tests::default_config_keeps_bicm_id_off` and the
  transmit-gated `bicm_id_zero_is_byte_identical_to_default` e2e test).
- Wired into the primary parallel spectrogram decode path
  (`par_decode_candidate`): standard `decode_soft` + CRC first; on
  failure and `bicm_id_iterations > 0`, `par_bicm_id_rescue` runs up to
  N global iterations (BP posterior → extrinsic → SOMAP refresh → BP),
  stopping early on syndrome+CRC pass. The fine-timing FFT path and the
  secondary passes (subtract/repass, a7, cross-cycle) are NOT wired —
  kill-switch scope is the primary pass only.
- BP posteriors: `LdpcDecoder::belief_propagation` already returns
  posterior LLRs; `decode_soft` does not expose them, so the rescue
  re-runs one BP on the same channel LLRs to seed the loop (cost ≈ one
  extra BP per rescued candidate per the seed, plus one BP + one SOMAP
  per global iteration).

## SOMAP scaling decision (units)

The paper's per-label metric f(y|s) must be in log-likelihood units
commensurate with the a-priori LLRs. Pancetta's tone metrics are
dB-spectrogram magnitudes, and the channel LLRs BP consumes have been
(optionally whitened and) variance-normalized (`normalize_llrs`,
target variance 24.0). Decision: fit a single per-candidate
least-squares scale g = Σ raw·chan / Σ raw² mapping the raw dB max-log
LLRs onto the normalized channel LLRs, and evaluate the SOMAP with
per-label metric g·dB. With whitening off this reproduces the
normalized LLRs exactly at zero feedback (normalization is one global
multiplicative factor); with whitening on (production default) it is
the best single-scalar approximation. This is the "fixed multiplicative
LLR scale, calibrated empirically" option from the pre-registration:
max-log outputs are linear in the metric scale, and the existing
`llr_target_variance` machinery already defines what scale BP expects.
No additional free parameter was tuned.

Fairness guard (pre-registered): `somap_nonzero_feedback_changes_llrs`
asserts non-zero LLR deltas under feedback, the extrinsic j≠i
exclusion, and the near-tie-resolution direction (believing bit1 of a
10.0-vs-9.5 ambiguous 101/011 symbol sharpens bit0 from −0.5 to −10) —
a no-op bug cannot masquerade as a null.

## Part A — synthetic SNR sweep

20 distinct standard messages, AWGN at −24..−16 dB (2500 Hz reference
BW) in 0.5 dB steps, N=50 trials/point, paired noise realizations
across configs. Harness:
`pancetta-research/examples/batch97_bicm_id_kill_switch.rs` (plant
reused from `batch30_snr_recall_curve.rs`).

| SNR (dB) | iters=0 | iters=2 | iters=4 |
|---------:|--------:|--------:|--------:|
| −24.0 … −21.0 | 0/50 | 0/50 | 0/50 |
| −20.5 | 1/50 (2%) | 1/50 (2%) | 1/50 (2%) |
| −20.0 | 5/50 (10%) | 8/50 (16%) | 8/50 (16%) |
| −19.5 | 8/50 (16%) | 11/50 (22%) | 11/50 (22%) |
| −19.0 | 19/50 (38%) | 29/50 (58%) | 32/50 (64%) |
| −18.5 | 30/50 (60%) | 37/50 (74%) | 39/50 (78%) |
| −18.0 | 41/50 (82%) | 46/50 (92%) | 46/50 (92%) |
| −17.5 | 48/50 (96%) | 50/50 (100%) | 50/50 (100%) |
| −17.0 | 50/50 | 50/50 | 50/50 |
| −16.5 | 49/50 (98%) | 49/50 (98%) | 49/50 (98%) |
| −16.0 | 50/50 | 50/50 | 50/50 |

50%-decode-rate thresholds (linear interpolation):

| config | threshold | decode wall (850 windows) |
|--------|----------:|--------------------------:|
| iters=0 | −18.73 dB | 139.4 s |
| iters=2 | −19.11 dB | 230.2 s (+65%) |
| iters=4 | −19.17 dB | 292.4 s (+110%) |

**Threshold shift 0→2: +0.384 dB. 0→4: +0.439 dB.** Both clear the
pre-registered 0.2 dB bar. The gain is monotone across the entire
waterfall (every row −20.0 … −17.5 improves or ties; no row regresses),
and consistent with the literature interpolation (~0.4–0.7 dB for M=8;
FT8's 174-bit block realizes the low end, as the bank entry's honesty
caveat predicted). Diminishing returns 2→4 (+0.055 dB) — the loop
mostly converges in 2 global iterations.

## Part B — real-corpus spot check

hard_200 first 50 WAVs, iterations {0, 2}, hash-normalized
ft8_lib-truth scoring (`pancetta_research::metrics::hash_normalize_message`),
all 50 WAVs have truth files.

| config | TP | FP | wall |
|--------|---:|---:|-----:|
| iters=0 | 1167 | 309 | 16.9 s |
| iters=2 | 1170 | 330 | 18.5 s (+9.5%) |

ΔTP = **+3**, ΔFP = **+21**, ΔFP/ΔTP = 7.0 (bar for PROCEED was ≤ 2.0).

## Verdict

**MECHANISM-CONFIRMED-CORPUS-PENDING** (pre-registered bar 2).

- Synthetic shift ≥ 0.2 dB: PASS (+0.384 dB at 2 iterations, +0.439 dB
  at 4, paired noise, N=50/point, monotone across the waterfall).
- Real-corpus PROCEED bar: FAIL — ΔTP +3 is barely above flat and
  ΔFP +21 is 7× ΔTP (bar: ≤ 2×). This is the B91 signal-limited story:
  on hard_200 few BP-failures sit in the 0.4 dB rescue window, while
  every noise candidate that fails BP gets up to 2 extra
  BP+SOMAP+BP attempts, each a fresh CRC-14 lottery ticket (the same
  failure mode that demoted osd_depth=Some(2) in Batch 72).
- The flag stays in (default 0 = byte-identical, double-guarded by
  unit + e2e tests). DO NOT flip default-ON as-is.

Follow-ups journaled for the bank (hb-252 status update):
1. FP-side gating before any graduation path: restrict the rescue to
   near-converged BP failures (few unsatisfied checks — same
   subpopulation hb-254 targets) and/or raise the confidence floor for
   rescue-originated decodes. The synthetic curve says the physics is
   real; the corpus says the trigger is too promiscuous.
2. Composes with hb-253 (exact Bessel metric would replace the g·dB
   max-log metric) and hb-259 (EM channel re-estimation supplies
   per-candidate Es/N0) exactly as the bank's conflict analysis
   predicted.
3. Wall cost: +65% on single-signal synthetic windows at iters=2 (BP
   re-runs on every failing noise candidate), but only +9.5% on busy
   real recordings — acceptable for a gated retry if (1) lands.

## Test counts

`cargo test --features transmit -p pancetta-ft8`: **525 passed, 0
failed, 2 ignored** (393 lib + 132 integration), exit 0. New tests:
`bicm_id_tests::{default_config_keeps_bicm_id_off,
rescue_with_zero_iterations_is_none,
somap_zero_feedback_equals_legacy_maxlog,
somap_nonzero_feedback_changes_llrs,
bicm_id_zero_is_byte_identical_to_default}`. Workspace
`cargo check --workspace --features transmit` clean; clippy adds no
new warnings on the touched code.

Reproduce: `cargo run --release -p pancetta-research --example
batch97_bicm_id_kill_switch` (env: BATCH97_TRIALS, BATCH97_REAL_WAVS,
BATCH97_SKIP_SYNTH, BATCH97_SKIP_REAL).

## Results (controller-finalized from /tmp/batch97_full.log)

### Part A — synthetic SNR sweep (50 trials/point, paired AWGN)

50%-decode-rate thresholds: iters=0 −18.73 dB; iters=2 −19.11 dB;
iters=4 −19.17 dB → **threshold shift +0.384 dB (2 iters), +0.439 dB
(4 iters)** — inside the literature band (M=8 interpolation predicted
0.4-0.7 dB). Largest single point: −19.0 dB decode rate 38% → 58%/64%.
Convergence is fast (4 iters ≈ 2). Wall: +65% on the synthetic harness
(rescue runs only on CRC-failing candidates).

### Part B — hard_200 first 50 WAVs (hash-normalized ft8_lib truth)

| iters | TP | FP | wall |
|---|---:|---:|---:|
| 0 | 1167 | 309 | 16.9s |
| 2 | 1170 | 330 | 18.5s |

ΔTP +3, ΔFP +21 — real decodes ARE rescued, but rescued BP runs
sometimes converge to wrong CRC-passing codewords (the known ~1/16k
per-attempt CRC-collision surface, multiplied by many rescue attempts).

## Verdict (pre-registered bars)

Synthetic ≥ 0.2 dB: **PASS (+0.384)**. Real ΔTP > 0: PASS (+3). Real
ΔFP ≤ 2×ΔTP: **FAIL (21 > 6)** → **MECHANISM-CONFIRMED-FP-PENDING**.
Flag stays default-0. Graduation path (Batch 98): stamp BICM-ID-rescued
decodes with a dedicated decode_origin level so the shipped v3 content
gate + suspicion filters price them; consider restricting rescue to
candidates with few unsatisfied parity checks; re-measure full corpus.
