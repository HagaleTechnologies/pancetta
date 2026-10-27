# PAN-161: crowded temporal-edge test of partial Costas recovery

## Result

The final corpus contains 48 crowded edge windows, eight in-window controls, and eight noise windows. At the production cap of 200, 34 edge targets have no nearby pass-0 candidate although the paired cap-600 run has one. Thirty of those 34 have a CRC-valid target in the cap-600 run. The predeclared qualification threshold of 20 is met. The partial B+C metric does **not** earn a default change: against production at cap 200 it loses two total true positives (one edge target and one control), gains no false positives, and its paired bootstrap intervals include zero. `Ft8Config::default().costas_partial_metric_enabled` remains `false`.

This is a pass-0 candidate-displacement test. A displaced pass-0 target can still decode through a later pass at cap 200. The candidate evidence and the final output recall are reported separately for that reason.

## Corpus and reproduction

- Frozen recipe: [`research/corpus/synth/manifests/pan161.config.json`](../corpus/synth/manifests/pan161.config.json), seed `16120261027`, generator version 1.
- Frozen manifest: [`research/corpus/synth/manifests/pan161.manifest.json`](../corpus/synth/manifests/pan161.manifest.json), SHA-256 `1c200df52bd9739028617fe1e9dc36a202ace779d36ef0a0786f5ef53044a450`.
- Full per-WAV evidence: [`research/experiments/2026-10-26-pan-161-crowded-slot-edge.scorecard.json`](2026-10-26-pan-161-crowded-slot-edge.scorecard.json). Every WAV hash is checked before decoding.
- Grid: 24 repetitions × target SNR `[-17, -18]` dB × target start `-0.65` s relative to the nominal boundary; 40 distinct distractors at 0 dB SNR, spaced 65 Hz from 200 through 2735 Hz; target at 2900 Hz; 2500-Hz-reference AWGN at -16 dB; eight control and eight noise windows. All 41 message truths in each edge window are distinct. Each WAV contains 13.5 s of 12-kHz mono PCM.
- The target begins 0.15 s before the capture start. About one 0.16-s symbol of Costas block A is cut; blocks B and C remain. The decoder's negative-time search does not support the originally proposed geometry with all of block A missing. This result therefore tests the reachable temporal-edge geometry, not a fully absent block A.

From the repository root, regenerate the committed WAV hashes and run the fixed four-arm evaluation:

```sh
cargo build --release -p pancetta-research --bin gen-crowded-edge --bin eval-crowded-edge
target/release/gen-crowded-edge --config research/corpus/synth/manifests/pan161.config.json --output-manifest research/corpus/synth/manifests/pan161.manifest.json --output-dir /tmp/pan161-wavs
target/release/eval-crowded-edge --manifest research/corpus/synth/manifests/pan161.manifest.json --output /tmp/pan161-scorecard.json --wav-dir /tmp/pan161-wavs
```

The generator refuses a mismatched existing manifest. The evaluator ran in 198.8 seconds in the release build and exited 0 with `64 WAVs; 30 CRC-valid displaced edge targets; default_on=false`. The comparison is paired by identical WAV bytes: A is cap 200/partial off, B is cap 200/partial on, C is cap 600/partial off, and D is cap 600/partial on. Cap 600 is diagnostic only.

A second generation against the committed manifest exited 0, and a second release evaluation reproduced `manifest_sha256`, full and qualified totals, both bootstrap intervals, verdict, and all per-WAV records exactly. The following targeted tests also exited 0: `cargo test -p pancetta-research --lib` (182 tests), `cargo test -p pancetta-research --bin eval-crowded-edge` (5 tests), `cargo test -p pancetta-research --test pan161_generator` (1 test), and `cargo test -p pancetta-ft8 hb242 --lib` (5 tests).

## Measured counts

All arm totals below cover the **full 64-WAV corpus**. The eight controls have their own `control_tp` column, so they cannot count as edge targets or qualify the corpus.

| Arm | Cap | Partial B+C | Edge TP / 48 | Distractor TP / 2240 | Control TP / 8 | Crowded FP | Noise FP |
|---|---:|---|---:|---:|---:|---:|---:|
| A: production | 200 | off | 38 | 2240 | 7 | 1 | 0 |
| B: technique | 200 | on | 37 | 2240 | 6 | 1 | 0 |
| C: cap control | 600 | off | 44 | 2240 | 7 | 9 | 0 |
| D: paired cap | 600 | on | 44 | 2240 | 7 | 9 | 0 |

In the 30 CRC-valid displaced windows alone, edge TP is A 24, B 23, C 30, D 30; each arm also has 1200 distractor TP. Their crowded FP counts are 1, 1, 4, 4 respectively. These subset counts diagnose the premise; the full corpus above drives the ship verdict. All 48 edge windows were cap-bound, 34 had pass-0 displacement under production, and 30 met the stricter CRC-valid cap-600 qualification.

Per-WAV checks against the manifest and candidate dump illustrate the count. `pan161_0000_edge.wav` has 6,935 raw cap-200 candidates, zero pass-0 candidates within 12.5 Hz/0.16 s of the target at cap 200, and three at cap 600; a candidate at 2900 Hz and -0.16 s decodes the target. `pan161_0004_edge.wav` likewise has zero versus two nearby candidates and a CRC-valid cap-600 target; its edge TP is A 0, B 1, C 1, D 1. These are exact expected-message decodes, not unlabeled noise peaks. The fixed time/frequency gate is declared in `pancetta-research/src/crowded_edge.rs`.

The cap-only change A→C recovers six edge messages and adds eight crowded false positives. That effect is separate from the technique comparison A→B.

## Pre-registered decision

The paired full-corpus B−A difference is ΔTP = -2 and ΔFP = 0. The 1,000-repetition paired bootstrap uses seeds `16120260802` for total TP and `16120260801` for edge TP. Total TP CI is [-7, 3], and edge TP CI is [-5, 3]. Neither excludes zero.

| Gate | Result | Evidence |
|---|---|---|
| Corpus qualification: ≥20 CRC-valid cap-displaced edge windows | Pass | 30 |
| Batch 75: ΔTP ≥20 with ΔFP ≤50, or ΔTP >0 with ΔFP ≤0 | Fail | -2 TP, 0 FP |
| Total and edge paired bootstrap intervals exclude zero | Fail | [-7, 3] and [-5, 3] |
| No noise-tier FP increase | Pass | 0→0 |
| Novel crowded FP growth ≤2×ΔTP | Fail as calculated | 0 growth, while 2×ΔTP = -4; recall already fails |

The evaluator's `default_on` verdict is `false`; the source default stays `false` at `pancetta-ft8/src/decoder.rs` (`Ft8Config::default`). A cap-only rescue is not credited to the partial metric.

## Parameter search and limits

The initial committed six-distractor, -16/-13-dB target corpus had zero displaced edge targets in 48 windows. To find a setting that actually exercised candidate competition, I ran these exploratory grids before freezing the final seed. They are tuning evidence, not inputs to the final ship comparison. The early grids used the original eight-message generator list; later grids used the expanded distinct-message generator now committed.

| Trial | Windows | Target SNR / temporal start | Distractors | Pass-0 displaced | CRC-valid displaced |
|---|---:|---|---|---:|---:|
| A | 8 | -18/-20/-22/-24 dB, -0.65 s | 7 at -8 dB | 1 | 0 |
| B | 12 | -17/-18/-19 dB, -0.55/-0.65 s | 7 at 0 dB | 0 | 0 |
| C | 8 | -16/-17/-18/-19 dB, -0.65 s | 24 at 0 dB | 0 | 0 |
| D | 8 | -16/-17/-18/-19 dB, -0.65 s | 40 at 0 dB | 6 | 4 |

Each trial used two repetitions and noise calibration at -16 dB. Trial D showed that 40 simultaneous signals can create cap competition while a -17/-18-dB target remains decodable. The final recipe used a new seed, 24 repetitions, those two target SNRs, and the fixed control/noise tiers. The change in generator message list between B and C means those exploratory counts should not be treated as a reproducible comparison across code revisions. The committed final manifest and scorecard are the reproducible evidence for the verdict.
