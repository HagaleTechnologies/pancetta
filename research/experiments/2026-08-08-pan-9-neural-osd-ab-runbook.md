# PAN-9 neural OSD depth-1 A/B runbook

`eval` preflights every manifest WAV and `research/baselines/ft8/<sha>.json` cache,
exiting nonzero on absence. Build and run it from the same checkout.

Run four paired arms on hard-200, hard-1000, **and noise_1000**:

1. A: `--osd-depth 0 --neural-osd off`
2. B: `--osd-depth 1 --neural-osd off` (depth-only attribution)
3. C: `--osd-depth 1 --neural-osd on` with shipped migrated weights
4. D: `--osd-depth 1 --neural-osd on` with soft-rank candidate weights

`noise_1000` is not optional. `compare`'s false-positive hard gate only
evaluates tiers present in **both** scorecards, so an arm run without the noise
tier makes that gate vacuous — a candidate that newly decodes noise would pass
the documented procedure unchallenged.

Use `compare --bootstrap-n 1000 --bootstrap-seed 0xb007` over full
`per_wav_records`. Ship only when **all** of the following hold:

- **Recall vs the depth-0 baseline**: D vs A has recall `ci_low > 0`.
- **Recall vs the attribution controls**: D vs B *and* D vs C each have recall
  `ci_low > 0`. Without this, a gain produced merely by enabling depth-1
  reprocessing (B) — or one the already-shipped weights (C) match or beat —
  would still read as a win for the soft-rank candidate.
- **Noise false positives**: no regression on the `false_positives_total` /
  `noise_files_decoded` hard gate, which requires the `noise_1000` arm above.
- **Elapsed**: `compare` enforces the elapsed gate from
  `Scorecard.harness.elapsed_seconds`, failing when D's wall-clock exceeds A's
  by more than `--max-elapsed-regression-pct` (default 20 %). The gate reports
  as skipped — and is not enforced — when the two runs differ in host or
  `cores_used`, so run every arm on one machine with a fixed core count.

Offline metrics select checkpoints only. A pass creates a follow-up ticket
with measured values; it never silently changes production `osd_depth = Some(0)`.
