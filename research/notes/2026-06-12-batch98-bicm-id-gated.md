# Batch 98 — hb-252 BICM-ID FP control: gated, measured, SHIP-OPT-IN

Date: 2026-06-12
Branch: `iter/2026-06-12-batch-98`
Hypothesis: hb-252 (Batch 97 left it MECHANISM-CONFIRMED-FP-PENDING:
synthetic +0.384 dB at `bicm_id_iterations=2`, real spot ΔTP +3 /
ΔFP +21 — rescued BP runs sometimes converge to wrong CRC-passing
codewords).

## Three FP-control changes (all shipped, all inert at default config)

1. **Near-converged gate** — `Ft8Config::bicm_id_max_unsatisfied_checks`
   (default 18, pinned by `default_config_gate_value_is_pinned`).
   Before the SOMAP loop runs, the seed BP re-run's hard decision is
   syndrome-counted (`count_parity_errors`, 0..=83 checks); candidates
   above the threshold skip the rescue.
2. **`decode_origin = Some(7)`** — rescued decodes get a dedicated
   hb-247 ordinal (doc enum extended in `message.rs`). The shipped
   hb-103 v3 content gate derives `lateness_frac = origin/6` and clamps
   to `[0, 1]`, so 7 yields 7/6 → **saturates at the maximum lateness
   penalty 1.0**. Documented in `content_score.rs` and `autonomous.rs`;
   the shipped divisor 6 is unchanged by design.
3. **Unconditional suspicion scrutiny** — rescued decodes must pass
   `suspicion_score() < 2` regardless of sync confidence (the primary
   path only applies it below `SCRUTINY_THRESHOLD = 0.65`). A rescue is
   an aggressive recovery whose wrong-CRC failure mode is exactly the
   CRC-collision shape `suspicion_score` targets.

Instrumentation: `PANCETTA_BICM_ID_INSTRUMENT_FILE=<path>` (OnceLock,
one relaxed check when unset) appends `ok/fail/reject/gated <unsat>`
lines per rescue event; the harness classifies `ok` lines against
ft8_lib truth per WAV.

## Unsatisfied-check distribution (hard_200/50, iters=2, gate OFF)

Totals: **ok-true = 299** (rescue emissions matching ft8_lib truth),
**ok-fp = 104** (wrong-CRC emissions), **fail = 10,734** (rescue ran,
no CRC pass), **reject = 11** (CRC pass but dropped by
parse/plausibility/suspicion — the new unconditional suspicion gate at
work).

| unsat | ok-true | ok-fp | reject | fail |
|------:|--------:|------:|-------:|-----:|
| 0 | 4 | 0 | 0 | 9 |
| 1 | 3 | 0 | 0 | 52 |
| 2 | 2 | 0 | 0 | 34 |
| 3 | 2 | 0 | 0 | 72 |
| 4 | 3 | 2 | 0 | 112 |
| 5 | 8 | 0 | 0 | 160 |
| 6 | 12 | 5 | 0 | 234 |
| 7 | 14 | 3 | 2 | 346 |
| 8 | 15 | 12 | 0 | 414 |
| 9 | 20 | 3 | 0 | 496 |
| 10 | 13 | 7 | 0 | 580 |
| 11 | 25 | 11 | 4 | 641 |
| 12 | 28 | 9 | 0 | 751 |
| 13 | 18 | 4 | 0 | 779 |
| 14 | 25 | 10 | 0 | 776 |
| 15 | 18 | 4 | 3 | 748 |
| 16 | 7 | 6 | 0 | 688 |
| 17 | 18 | 2 | 0 | 658 |
| 18 | 7 | 5 | 0 | 559 |
| 19 | 5 | 1 | 0 | 485 |
| 20 | 7 | 0 | 0 | 409 |
| 21 | 13 | 4 | 1 | 332 |
| 22 | 4 | 0 | 0 | 336 |
| 23 | 6 | 6 | 1 | 228 |
| 24 | 5 | 0 | 0 | 184 |
| 25 | 4 | 2 | 0 | 123 |
| 26 | 2 | 2 | 0 | 121 |
| 27-30 | 7 | 5 | 0 | 211 |
| 31-42 | 4 | 3 | 0 | 173 |
| 43+ | 0 | 0 | 0 | 23 |

Cumulative keep at selected thresholds:

| T | true kept | fp kept | fail kept |
|--:|----------:|--------:|----------:|
| 5 | 22/299 (7.4%) | 2/104 (1.9%) | 4.1% |
| 8 | 63/299 (21.1%) | 22/104 (21.2%) | 13.4% |
| 12 | 149/299 (49.8%) | 52/104 (50.0%) | 36.3% |
| **18** | **242/299 (80.9%)** | **83/104 (79.8%)** | **75.5%** |
| 26 | 288/299 (96.3%) | 98/104 (94.2%) | 96.2% |

**Chosen threshold: T = 18** — the smallest T keeping ≥80% of ok-true,
per the pre-registered rule.

**Key finding (the honest one): the unsat distribution of wrong-CRC
rescues tracks the true-rescue distribution within ~1 pp at every
threshold.** The near-converged-gate premise — that wrong-CRC
conversions come disproportionately from far-from-convergence noise
candidates — is *refuted* on hard_200. Both true and false rescues
draw from the same near-converged population; what differs is only
which CRC-passing codeword BP lands on. The gate survives as a
wall-cost pruner (cuts 24.5% of the 10,734 futile rescue attempts at
T=18), not as an FP discriminator. Caveat: per-rescue emissions
overstate marginal value — most ok-true emissions duplicate decodes
the standard path already produces elsewhere in the window (299
emissions vs ΔTP +2 after dedup), so the gated spot below is the
decisive measurement.

## Gated spot (hard_200 first 50, iters {0, 2}, hash-normalized ft8_lib truth)

| config | TP | FP | wall |
|--------|---:|---:|-----:|
| iters=0 | 1167 | 309 | 16.5s |
| iters=2, T=18 | 1169 | 326 | 18.1s (+9.2%) |
| iters=2, T=8 | 1167 | 317 | (+7.8%) |
| iters=2, T=5 | 1167 | 310 | (+6.5%) |

Frontier: ungated (B97) ΔTP +3/ΔFP +21 → T=18 **+2/+17** → T=8
**+0/+8** → T=5 **+0/+1**. The marginal rescued population is
FP-dominated at every threshold; tightening the gate sheds TPs and FPs
in lockstep, exactly as the overlapping distributions predict. **No
operating point passes the proceed bar (ΔFP ≤ 2×ΔTP with ΔTP > 0).**
The iters=0 row is unchanged from Batch 97 (1167/309) — the three
FP-control changes are byte-identical when the rescue is off, and the
origin-7 + unconditional-suspicion additions only touch rescued
decodes.

## Synthetic re-check at the gated config (10 trials/point, paired AWGN, T=18)

| config | 50% threshold | wall (340 windows) |
|--------|--------------:|-------------------:|
| iters=0 | −18.83 dB | 26.9s |
| iters=2 gated | −19.33 dB | 42.3s |

**Gated shift +0.500 dB — the pre-registered ≥0.2 dB sanity bar
PASSES.** (N=10/point is noisier than Batch 97's N=50 +0.384, but
every waterfall row improves or ties.) The gate does not destroy the
mechanism: true synthetic rescues sit in the near-converged population.

## Verdict (pre-registered bars)

- Spot proceed bar (ΔFP ≤ 2×ΔTP, ΔTP > 0): **FAIL at every threshold**
  (best ΔTP>0 point: +2/+17, ratio 8.5×).
- Full-corpus graduation run (raw_530_full + hard_1000): **not
  triggered** — pre-registered as conditional on the spot passing.
- Synthetic gated sanity (≥0.2 dB): PASS (+0.500 dB).

**→ SHIP-OPT-IN.** `bicm_id_iterations` stays default 0 (byte-identical,
double-guarded). The opt-in config is now substantially safer than
Batch 97's: an operator who flips `bicm_id_iterations=2` gets the
near-converged pruning (default `bicm_id_max_unsatisfied_checks=18`),
origin-7 content-gate pricing, and unconditional suspicion scrutiny.
Measured opt-in frontier on hard_200/50: +2 TPs / +17 FPs / +9.2% wall
at T=18, or +0/+1/+6.5% at T=5.

Re-open condition: a *different* discriminator for the rescue's
acceptance — hb-253 (exact Bessel metric, changes the LLR quality the
rescue feeds back) or hb-259 (per-candidate Es/N0 → calibrated rescue
confidence) — not another unsat-threshold tune; the threshold axis is
measured and flat.

## Test counts

`cargo test --features transmit -p pancetta-ft8`: **527 passed, 0
failed, 2 ignored**, exit 0 (395 lib + 132 integration/doc). New:
`bicm_id_tests::{rescue_gate_zero_blocks_non_converged_candidate,
default_config_gate_value_is_pinned}`; `rescue_with_zero_iterations_is_none`
updated for the new signature; byte-identity e2e
(`bicm_id_zero_is_byte_identical_to_default`) green unchanged.

Reproduce:
```
BATCH98_PART=instrument BATCH98_WAVS=50 cargo run --release -p pancetta-research --example batch98_bicm_id_gated
BATCH98_PART=spot BATCH98_MAX_UNSAT=18 cargo run --release -p pancetta-research --example batch98_bicm_id_gated
BATCH98_PART=synth BATCH98_TRIALS=10 cargo run --release -p pancetta-research --example batch98_bicm_id_gated
BATCH98_PART=full BATCH98_CORPUS=raw_530_full.manifest.json ...   # (not run this batch — spot bar failed)
```

## Deviations from the batch brief

- The brief expected "ΔFP to collapse while keeping most ΔTP" from the
  near-converged gate; the measurement says the gate cannot do that
  (distributions overlap). The brief's own pre-registered SHIP-OPT-IN
  branch covers this outcome.
- Full corpora (raw_530_full + hard_1000) deliberately not run: the
  brief conditions them on the spot passing ΔFP ≤ 2×ΔTP, which it did
  not at any threshold (three thresholds measured, not just the chosen
  one, to map the frontier).
- The default gate value was committed as 18 (data-chosen) after a
  provisional placeholder during development; the pinning test
  enforces it.
