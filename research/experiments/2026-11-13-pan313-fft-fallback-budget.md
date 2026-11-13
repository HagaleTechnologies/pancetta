# PAN-313: budget-gate the legacy per-symbol FFT fallback — fixture A/B and qualification status

Date: 2026-11-13. Ticket: PAN-313. Change: `pancetta-ft8/src/decoder.rs` gates
the 21-trial time/frequency FFT sweep on the remaining `DecodeBudget` (sweep
admission estimate 20 ms, per-trial estimate 2 ms, FT8-only, kept strictly
after the existing `sync_score < 3.5` rejection) and publishes one
`fft-fallback` telemetry row per decode pass through
`DecodeBudgetReport.stages`. Parent revision: `2c60caf` (pre-gate). Candidate:
this branch.

## Fixture A/B (this container, 13 committed WAVs)

Method: `pancetta-ft8/examples/pan313_fallback_eval.rs --mode results`, parent
and candidate binaries built from the same collector source, fixed two-thread
Rayon pool (CI-class approximation), fresh decoder per window, one warmup plus
one measured repetition per arm (reduced from the plan's five for the cloud
session deadline — medians are from 3 lengths × 1 repetition). Budgets:
unlimited and 250 ms Standard. Lengths: 151,680 / 162,000 / full (180,000).

Host: AMD Ryzen 9 5950X (32 hardware threads; pool pinned to 2), no cgroup
CPU limit, rustc per `rustc --version` in the results files. NOT a CI-class
host despite the pool width — absolute milliseconds are indicative only.

Checker: `scripts/pan313_compare.py /tmp/pan313/parent_results.json
/tmp/pan313/candidate_results.json` — **PASS**:

- Unlimited message sets: identical on all 13 recordings at all 3 lengths
  (aggregate 217 messages both arms). This is the committed-fixture half of
  the ticket's "decode output unchanged under unlimited" scenario; the frozen
  regression guard is `wav_decode_tests::unlimited_fft_fallback_matches_frozen_baseline`
  against `tests/fixtures/fft_fallback_baseline.json` (generated at the
  parent revision before the gate existed).
- Standard-budget reference recall (proxy vs parent-unlimited sets; no jt9
  truth in the cloud, so these are NOT true positives): aggregate
  172 → 179; no recording lost a message (`basicft8/live_now.wav` +6,
  `jtdx/190227_155815.wav` +1, all others unchanged).
- Median per-recording S1-floor wall time at Standard: 379 ms → 223 ms
  (−41%). At the full-recording shape the gate recorded e.g.
  284–297 extraction attempts with 34–38 budget refusals per window; the
  parent publishes no `fft-fallback` row at all (recorded as unavailable,
  not zero).

## Qualification status

- **Hard-200 corpus scenarios: NOT RUN — missing corpus/truth.** The 200
  recordings (`research/corpus/curated/ft8/hard_200.manifest.json`) and the
  jt9 baseline cache live only on the operator's host. Neither implement nor
  validate may treat the fixture results above as proving the ticket's
  hard-200 scenarios.
- Operator procedure (unshipped here): on a host with the corpus and truth
  caches, run parent and candidate `pancetta-research` eval/compare at
  `--effort unlimited` and `--effort standard`
  (`cargo run --release -p pancetta-research --bin eval -- --tier
  curated-hard-200 --mode ft8 --effort <e> --output <file>`), compare like
  budgets with `bin compare`, and collect per-window sets/stages with the
  `pan313_fallback_eval` collector (it correlates by WAV sha256). Pass
  requires identical unlimited sets, aggregate Standard true positives not
  lower with per-recording TP differences published, false positives not
  increased, and median S1-floor wall time decreased on a CI-class host with
  recorded CPU/quota provenance. If recall drops or timing does not improve,
  PAN-313 stays unqualified: do not raise the sync threshold or weaken
  baselines. Add the synth-clean A/B per the decoder refinement methodology.
- Cost-estimate revisions (20 ms / 2 ms) require paired evidence and updates
  to the policy tests, then both budgets re-run.

## Test sequencing honesty note

The frozen baseline WAS generated from the parent revision before any decoder
behavior changed (`--mode baseline --revision 2c60caf`, output committed at
`pancetta-ft8/tests/fixtures/fft_fallback_baseline.json`). The phase-1 red run
of the new tests against the ungated decoder was NOT executed in this
session: the tests and the gate landed in the same editing pass under the
session deadline, so no pre-change failure output exists. The mechanism tests
still discriminate: `expired_budget_floor_skips_fft_fallback` asserts zero
fallback attempts and positive refusals under an expired budget, which the
ungated sweep (no budget consultation anywhere in the sweep) cannot satisfy.
