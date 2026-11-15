# PAN-313: optional FFT fallback budget gate

The FT8 legacy time/frequency FFT sweep now requires 20 ms of estimated
headroom at entry and 2 ms before each extraction. The existing 3.5 sync
cutoff and search grid are unchanged. Floor candidates retain coarse
spectrogram decoding after expiry. Checks between trials cannot stop an
already running trial or reserve budget across workers.

Each pass publishes `fft-fallback` in `DecodeBudgetReport.stages`.
Its tuple contains summed worker elapsed milliseconds, extraction calls,
and budget-refused sweeps. This nested elapsed time is not additive to
S1/S2 wall time. A refusal counts once per sweep, including a sweep cut
short between trials. Sync rejection and invalid positions do not count.
Any refusal sets budget exhaustion, even with positive headroom remaining.

## Compatibility and discriminating tests

The inherited frozen baseline names parent
`2c60caff243122e82b9cc8f235703df2e2b5465a`. This session rebuilt the same
collector against that parent and reproduced its complete recording
identities and message sets exactly. All 13 WAVs have 180,000 samples.
The comparison covers 151,680, 162,000 and full-recording lengths.
The live regression test also checks the fixture inventory and hashes.
No expected message set was regenerated from the candidate.

The interrupted WIP already contained tests and implementation together.
This session ran `expired_budget_floor_skips_fft_fallback` against the parent.
It exited 101 with `left: 0, right: 1` for the expected fallback stage row.
The candidate passes it at full and 162,000-sample lengths, preserving the
minimum-three-message floor assertion and reporting zero FFT attempts with
positive refusals. This is a discriminating red check, not a claim that the
original WIP followed test-first sequencing.

Targeted results in this session:

- Default and FT2-enabled fallback unit suites: six tests pass each.
- WAV integration suite: 23 tests pass, including all 39 frozen-set comparisons.
- Stable API guard: six tests pass.
- Transmit-enabled refinement suite: 22 tests pass.
- Bessel fallback unit suite: nine tests pass.
- FT8lib seed suite with `--features transmit`: eight tests pass. The planned
  command without this feature runs zero tests, so it was supplemented.
- Comparator Python suite: 14 tests pass.
- `cargo build --all-targets`: passes.

`cargo clippy -p pancetta-ft8 --all-targets --features transmit,ft2 -- -D warnings`
exits 101 on existing lints. The same command fails on the pinned parent.
Examples include `src/lib.rs:3` (`cfg_attr(clippy, ...)`),
`src/neural_osd_weights.rs:57` (`chunks_exact`) and existing manual slice-copy
test loops in `decoder.rs`. Autofixes outside the planned files were reverted.
The final integration gate is the repository's `bash scripts/check.sh --full`,
whose Clippy warning policy matches CI.

## Fixture measurement procedure

Build `pan313_fallback_eval` in release mode against the parent and candidate
from the same collector source. Save each executable separately. Run five
external alternating pairs, parent then candidate, without concurrent builds
or tests. Each invocation uses `--mode results --threads 2 --warmups 1
--repetitions 1 --out <arm>-<repetition>.json`. Compare each pair with
`python3 scripts/pan313_compare.py <parent>.json <candidate>.json`.
A fresh decoder is constructed before anchoring each window's budget.

One measured repetition per file is deliberate. Keeping only a final set
beside five timing samples could conceal an earlier recall loss. The collector
and comparator reject multiple repetitions per file. All five paired checks
must be inspected, including failures. Unlimited and Standard budgets are
separate arms, each at all three sample lengths. The comparator checks hashes,
lengths, CPU model, quota, compiler, pool width, warmups, repetition count and
budget anchoring. Parent fallback counters are unavailable, never invented zeros.

Without independent truth, decoded counts and intersections with parent unlimited
sets are reference-recall proxies. They are not true-positive measurements.

## Hard-200 qualification

**Not run: missing corpus/truth.** Manifest inventory in this container:
200 entries, zero existing WAVs. Collector `--manifest
research/corpus/curated/ft8/hard_200.manifest.json` exits 101 with
`manifest WAV missing`. No corpus result file or recall success is produced.
The collector verifies each manifest WAV's SHA-256 before starting any decode.

On a host with all hash-matching WAVs and jt9 truth caches:

1. Build parent and candidate evaluation binaries with identical release settings.
2. On each revision run the research evaluator at both budgets:
   `cargo run --release -p pancetta-research --bin eval -- --tier curated-hard-200 --mode ft8 --effort unlimited --output <revision>-unlimited.json`
   and the corresponding `--effort standard` command. Missing truth must fail
   the existing evaluator preflight.
3. Compare like budgets using the research `compare` binary. Publish per-recording
   TP differences and false positives. Run synth-clean A/B as required by the
   decoder refinement methodology.
4. Run the same collector with `--manifest <manifest>` alongside scoring, using
   five external parent/candidate pairs. Match WAV hashes and sample lengths
   with evaluator results. Preserve CPU/quota provenance and identical pool width.

Qualification requires identical unlimited message sets, no aggregate Standard
TP decrease, no FP increase and lower median aggregate S1-floor wall time on a
CI-class host. Publish individual recording losses even if aggregate TP improves.
Failure of any condition leaves PAN-313 unqualified. Fixture results cannot
prove these corpus scenarios. Revise cost estimates only with paired evidence,
updated policy tests and repeat runs at both budgets.

## Measured fixture results, 2026-11-13

Host provenance: `{"cgroup_cpu": {"cpu.max": "600000 100000", "version": 2}, "cpu_model": "AMD Ryzen 9 5950X 16-Core Processor", "rayon_threads": 2, "rustc": "rustc 1.98.0 (88d9e12ae 2026-08-18)"}`.
This workstation has a six-core CPU quota. A two-thread pool is a CI approximation, not CI-class host qualification.

| Pair | Checker | Parent/candidate reference recall | Parent/candidate aggregate S1 ms |
|---|---|---|---|
| 1 | PASS | 181 / 190 | 8125 / 5462 |
| 2 | PASS | 181 / 188 | 8185 / 5497 |
| 3 | FAIL | 181 / 183 | 8025 / 5518 |
| 4 | FAIL | 181 / 184 | 8168 / 5534 |
| 5 | PASS | 180 / 190 | 8464 / 5515 |

Median aggregate S1-floor time across the five pairs falls from 8,168 ms to 5,515 ms across the 39 fixture/length combinations.

Unlimited message sets match in all five pairs on every recording and length. Standard reference recall is not uniformly preserved. These results do not qualify the change for the corpus recall scenario. The cost estimates remain unchanged.

Below, count lists are measured repetitions 1 through 5. S1 values are medians of five measured wall times. Fallback attempts and refusals are candidate medians. `U` is the common unlimited unique-message count.

| Recording | Samples | U | Parent Standard counts | Candidate Standard counts | S1 ms parent/candidate | Attempts/refusals |
|---|---|---|---|---|---|---|
| basicft8/170923_082000.wav | 151680 | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 21 / 21 | 3468 / 0 |
| basicft8/170923_082000.wav | 162000 | 1 | 0,0,0,0,0 | 0,0,0,0,0 | 325 / 234 | 642 / 16 |
| basicft8/170923_082000.wav | full | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 457 / 238 | 452 / 26 |
| basicft8/170923_082015.wav | 151680 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 30 / 28 | 3567 / 0 |
| basicft8/170923_082015.wav | 162000 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 242 / 233 | 927 / 10 |
| basicft8/170923_082015.wav | full | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 504 / 231 | 423 / 42 |
| basicft8/170923_082030.wav | 151680 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 19 / 18 | 2844 / 0 |
| basicft8/170923_082030.wav | 162000 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 300 / 231 | 789 / 22 |
| basicft8/170923_082030.wav | full | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 476 / 231 | 420 / 46 |
| basicft8/170923_082045.wav | 151680 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 24 / 25 | 3411 / 0 |
| basicft8/170923_082045.wav | 162000 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 330 / 228 | 675 / 37 |
| basicft8/170923_082045.wav | full | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 511 / 235 | 431 / 29 |
| basicft8/live_now.wav | 151680 | 26 | 26,26,26,26,26 | 26,26,26,26,26 | 7 / 7 | 2133 / 0 |
| basicft8/live_now.wav | 162000 | 27 | 27,27,27,27,27 | 27,27,27,27,27 | 22 / 25 | 1944 / 0 |
| basicft8/live_now.wav | full | 27 | 23,23,23,23,23 | 26,26,25,26,26 | 156 / 163 | 468 / 18 |
| generated/ft8_cq.wav | 151680 | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 18 / 18 | 1368 / 0 |
| generated/ft8_cq.wav | 162000 | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 130 / 133 | 1086 / 32 |
| generated/ft8_cq.wav | full | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 309 / 232 | 591 / 21 |
| generated/ft8_report.wav | 151680 | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 13 / 12 | 525 / 0 |
| generated/ft8_report.wav | 162000 | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 55 / 53 | 861 / 0 |
| generated/ft8_report.wav | full | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 315 / 231 | 504 / 14 |
| generated/ft8_rr73.wav | 151680 | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 9 / 9 | 369 / 0 |
| generated/ft8_rr73.wav | 162000 | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 208 / 211 | 642 / 14 |
| generated/ft8_rr73.wav | full | 1 | 1,1,1,1,1 | 1,1,1,1,1 | 341 / 231 | 441 / 27 |
| jtdx/000000_000001.wav | 151680 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 25 / 26 | 3598 / 0 |
| jtdx/000000_000001.wav | 162000 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 300 / 229 | 798 / 33 |
| jtdx/000000_000001.wav | full | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 550 / 238 | 420 / 34 |
| jtdx/190227_155815.wav | 151680 | 25 | 25,25,25,25,25 | 25,25,25,25,25 | 9 / 9 | 2313 / 0 |
| jtdx/190227_155815.wav | 162000 | 25 | 23,23,23,23,23 | 23,23,23,23,23 | 72 / 74 | 2175 / 7 |
| jtdx/190227_155815.wav | full | 26 | 17,17,17,17,16 | 18,20,16,16,20 | 207 / 208 | 406 / 16 |
| wsjt/170709_135615.wav | 151680 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 22 / 23 | 3480 / 0 |
| wsjt/170709_135615.wav | 162000 | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 277 / 233 | 862 / 18 |
| wsjt/170709_135615.wav | full | 0 | 0,0,0,0,0 | 0,0,0,0,0 | 491 / 235 | 453 / 30 |
| wsjt/181201_180245.wav | 151680 | 8 | 8,8,8,8,8 | 8,8,8,8,8 | 9 / 10 | 3534 / 0 |
| wsjt/181201_180245.wav | 162000 | 8 | 3,3,3,3,3 | 6,3,4,4,3 | 321 / 232 | 573 / 25 |
| wsjt/181201_180245.wav | full | 8 | 3,3,3,3,3 | 4,3,3,3,6 | 392 / 230 | 468 / 31 |
| wsjt/210703_133430.wav | 151680 | 8 | 8,8,8,8,8 | 8,8,8,8,8 | 17 / 18 | 3375 / 0 |
| wsjt/210703_133430.wav | 162000 | 9 | 4,4,4,4,4 | 5,5,4,4,4 | 303 / 227 | 503 / 32 |
| wsjt/210703_133430.wav | full | 9 | 4,4,4,4,4 | 4,4,4,4,4 | 355 / 231 | 427 / 18 |

Individual Standard reference-message losses, including losses offset by gains elsewhere:

- Pair 3, `jtdx/190227_155815.wav` at full: ['CQ R9AAB MO04'].
- Pair 4, `jtdx/190227_155815.wav` at full: ['CQ R9AAB MO04'].
