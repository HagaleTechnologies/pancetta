# FP-on-noise tier baseline (Workstream 0, Task W0.1) — 3 FPs / 1000 WAVs

**Date**: 2026-07-07
**Branch**: `worktree-decoder-tp-sensitivity` (commit at measurement time: `a5e83be0` + working-tree
W0.1 changes, later committed)
**Status**: BASELINE MEASUREMENT. Not an A/B — this is the first-ever run of a brand-new eval
tier, establishing the number every later `[A/B]` flip in the decoder-tp-sensitivity plan must
hold at-or-below (hard gate: any *increase* fails `compare`).

## What this is

Workstream 0 (Task W0.1) built the harness's first false-positive measurement: a corpus of
1000 WAVs containing **zero FT8 signal** — pure seeded white Gaussian noise, with 30% of files
additionally carrying birdie interference (1-3 steady sine carriers + one slowly-drifting
carrier). Any decode a decoder under test returns against one of these WAVs is, by construction,
a false positive: there is nothing valid to decode. Before this task, the harness had no way to
measure false-positive decodes at all (design spec §2) — a hallucinating decoder scored
identically to a correct one.

This log records the FIRST measurement: running the current production decoder
(`Ft8Config::default()`, main-equivalent config as of this branch) against the real 1000-file
corpus.

## Corpus

- Generator: `pancetta-research/src/gen_noise.rs` (`generate_noise_corpus_with_manifest`), driven
  by the new `gen-noise` binary.
- Config: `count: 1000, seed: 20260706, birdie_fraction: 0.3`.
- Location: `~/.pancetta/recordings/noise_1000/` (1000 WAVs, 15 s / 12 kHz mono 16-bit PCM each,
  344 MB total) — not committed, matching the existing curated-corpus convention (real WAVs live
  outside the repo; only the manifest is tracked).
- Manifest: `research/corpus/curated/noise/noise_1000.manifest.json` (schema_version 1, label
  `noise_1000`, 1000 entries each with absolute `wav_path` + SHA-256 + `has_birdie` +
  `seed_for_this_wav` — same shape convention as `curated::CuratedManifest`/hard_200).
- Verified: exactly 300/1000 entries have `has_birdie: true` (deterministic selection, not a
  probabilistic approximation of the 0.3 fraction).
- Determinism: `gen_noise::tests::same_seed_produces_byte_identical_wavs` proves re-running the
  same config produces byte-identical WAV files; this corpus is reproducible from the config
  alone (no hidden state).

## Command

```
cargo build --release -p pancetta-research --bin gen-noise
./target/release/gen-noise --count 1000 --seed 20260706 --birdie-fraction 0.3 \
  --output-dir ~/.pancetta/recordings/noise_1000 \
  --manifest research/corpus/curated/noise/noise_1000.manifest.json --label noise_1000

cargo build --release -p pancetta-research --bin eval
./target/release/eval --tier noise_1000 --mode ft8 \
  --output research/scorecards/noise_1000_baseline.json
```

No decoder-config overrides were passed to `eval`, so this measures `Ft8Config::default()` —
the exact production config, including `osd_depth: Some(0)`, `max_decode_passes: 1`,
`max_candidates: 100`, `max_sync_candidates: 200`, `min_sync_score` at its production default.

## Result

```
noise_1000: 3 FALSE POSITIVE decode(s) across 3/1000 noise-only WAVs
false_positives_total: 3
noise_files_decoded: 3
wavs_processed: 1000
elapsed: 726.2s (single eval-tier run; decoder internally parallelizes per-WAV, ~760% CPU)
```

Scorecard: `research/scorecards/noise_1000_baseline.json` (`tiers.noise_1000`).

## Is this "near 0"? Honest assessment

The plan's expectation was: *"Expected near 0 today since OSD is off — that's the point: it must
stay 0 as OSD returns."* The measured number is **3, not 0**. This is a genuine, small, nonzero
baseline — reported honestly rather than rounded down to the expected answer.

Is it a harness bug or a real decoder property? I believe it's a real (if small) property of the
current pipeline, not a bug in the noise-tier code itself, for three reasons:

1. **`osd_depth: Some(0)` is not "OSD fully off"** — per the in-code comment at
   `pancetta-ft8/src/decoder.rs:1150-1157` (Batch 73, 2026-06-11), depth 0 "keeps the depth-0
   trial (a single hard-decision attempt costing one branch) which adds zero FPs **in
   measurement**" — but that measurement was against hard_1000/raw_530_full (real off-air
   recordings with actual signal content in every LLR), not pure noise. It is plausible that a
   hard-decision attempt on pure-noise LLRs occasionally passes CRC-14 by chance in a way that
   never showed up against real-signal corpora.
2. **Trial volume is large enough to expect a handful of CRC-14 chance-passes.** Each WAV tries
   up to `max_sync_candidates: 200` Costas sync candidates and `max_candidates: 100` decode
   attempts, each independently gated by a 14-bit CRC (nominal false-accept rate ≈ 1/16384 per
   independent trial). Over 1000 WAVs this is on the order of 10⁵ CRC gates. A handful of chance
   CRC passes on structured-but-signal-free content (especially the 300 birdie-carrying files,
   which hand the sync/demod path *something* periodic to lock onto) is squarely within the
   statistically expected range for that trial volume — 3 observed successes is, if anything, on
   the low side of a naive 1/16384-per-trial estimate, not an alarming excess.
3. **The gen_noise.rs unit tests pass** (determinism, RMS-within-5%, exact birdie-count
   selection), so the corpus itself is not obviously malformed in a way that would manufacture
   spurious signal content.

I did **not** re-run a targeted scan to identify which 3 of the 1000 WAVs produced the decodes
(would require a second ~12-minute full-corpus pass; `run_noise_tier` records aggregate counts
only, not per-WAV identity, by design — see Known follow-ups). That would be useful confirmation
(birdie vs. clean-noise split, actual decoded message text) but wasn't done in this pass; flagged
below as a natural follow-up rather than blocking this baseline.

## What this means for the standing gate

Per the plan's standing `[A/B]` gate: *"FP-on-noise tier = 0 new decodes."* That gate is about
**deltas**, not the absolute value — `compare`'s hard gate (also shipped in this task) fails on
any *increase* in `false_positives_total` between two scorecards. **This baseline of 3 is now
the reference point**: any future flip (OSD re-enable, acceptance-metric work, etc.) must not
push that number above 3 on this corpus/config, and ideally should drive it toward 0 as
Workstream 2's signal-domain acceptance metric lands.

## Known follow-ups (not blocking, not part of this task)

- Identify which 3 WAVs decoded and whether they're birdie or clean-noise (helps root-cause
  whether it's a birdie-triggered sync lock or a pure-noise CRC coincidence).
- `noise_1000` currently reports only aggregate counts (`false_positives_total`,
  `noise_files_decoded`); no `per_wav_records` (so `compare`'s bootstrap-CI section skips this
  tier, same as every other non-`per_wav_records` tier today). Could be added later if per-WAV
  noise-tier diffing becomes useful.
- This run used the eval binary's default seed value only for reproducibility bookkeeping
  (`--seed` doesn't affect noise-tier generation — the corpus's own `seed: 20260706` is baked
  into the manifest/WAVs already on disk).
