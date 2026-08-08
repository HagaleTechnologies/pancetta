# PAN-9 neural OSD depth-1 A/B runbook

`eval` preflights every manifest WAV and `research/baselines/ft8/<sha>.json` cache,
exiting nonzero on absence. Build and run it from the same checkout.

Run four paired arms on hard-200 and hard-1000:

1. A: `--osd-depth 0 --neural-osd off`
2. B: `--osd-depth 1 --neural-osd off` (depth-only attribution)
3. C: `--osd-depth 1 --neural-osd on` with shipped migrated weights
4. D: `--osd-depth 1 --neural-osd on` with soft-rank candidate weights

Use `compare --bootstrap-n 1000 --bootstrap-seed 0xb007` over full
`per_wav_records`. Ship only when D vs A has recall `ci_low > 0`, does not
regress the noise-tier `false_positives_total` hard gate, and clears the elapsed
gate. Offline metrics select checkpoints only. A pass creates a follow-up ticket
with measured values; it never silently changes production `osd_depth = Some(0)`.
