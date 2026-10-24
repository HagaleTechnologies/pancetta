---
slug: pan110-production-pipeline-snr
mode: ft8, ft4
state: shipped
created: 2026-10-22T16:58:56Z
last_updated: 2026-10-22
branch: PAN-110
parent_hypothesis: published decode-quality claims (">95% at -20 dB", "Max means unlimited") describe neither the native-only research curves nor the shipped ft8_lib + native merge
wild_card: false
scorecard: research/scorecards/pan110-*.json
delta_vs_main: measurement only; no decoder change (the coordinator's merge moved to pancetta_ft8::merge unchanged)
disposition: SHIPPED. Published figures in FEATURES.md, README.md and docs/decoder-comparison.md now quote these scorecards; pancetta/tests/published_decode_claims.rs ties the quoted −20 dB figure to the deep scorecard.
---

# PAN-110: SNR sensitivity of the shipped decode pipeline

## Question

What does the pipeline Pancetta actually ships decode on the committed
synthetic SNR corpora? Production runs ft8_lib, then the native decoder under
the per-window effort budget (deadline anchored at window receipt), and
merges the two sets (`pancetta_ft8::merge::merge_ft8lib_first`, then
`drop_unresolved_hash_twins`). Every published curve so far measured the
native decoder alone, unbudgeted.

## Commands

```bash
cargo build --release -p pancetta-research --bin eval --bin gen-synth
# Regenerate the gitignored WAVs (the committed manifests are not touched):
./target/release/gen-synth --config research/corpus/synth/manifests/clean.config.json --output /tmp/clean.manifest.json
./target/release/gen-synth --config research/corpus/synth/manifests/ft4_clean.config.json --output /tmp/ft4_clean.manifest.json
# One at a time: the budget is wall-clock, so concurrent runs distort it.
B="--budget-anchor receipt --merge-ft8lib --ap-my-call W1PAN"
./target/release/eval --tier synth-clean --mode ft8 $B --effort deep --output research/scorecards/pan110-synth-clean-deep-merged.json
./target/release/eval --tier synth-ft4 --mode ft4 $B --effort max --output research/scorecards/pan110-synth-ft4-max-merged.json
./target/release/eval --tier synth-clean --mode ft8 $B --effort max --max-passes 2 --time-varying-subtraction-enabled --output research/scorecards/pan110-synth-clean-max-merged.json
```

## Host

- AMD Ryzen 9 5950X 16-Core Processor, 6 CPUs visible to the process (cgroup `cpu.max` `600000 100000`), rayon 6 threads
- production's hardware-tier probe (`probe_hardware_tier(10)`) classified it **fast** (p95 249 ms); `Auto` therefore resolves to `Deep` (1000 ms) here.
- Build: `release`, rustc 1.98.0 (88d9e12ae 2026-08-18); ft8lib_available: true
- Commit: `3d1c7bed3612fd0491e226b3fcd84fc0a38aab59`, `git.dirty: false` in all three scorecards
- The `Deep` run had the host to itself. A `nice -n 19` debug compile of the
  workspace ran during the FT4 and `Max` runs. The scheduler gives it little
  CPU while `eval` is busy, but treat those two runs' window timings as upper
  bounds.
- **Raspberry Pi / MiniPC: unmeasured.** Nothing here was run on target
  hardware.

## Corpus

WAVs are gitignored and were regenerated from the committed configs; the
manifests were not rewritten. The WAV-set hash is sha256 over each WAV's
lower-case hex sha256, in manifest order.

  - FT8: `research/corpus/synth/manifests/clean.manifest.json` sha256 `8ef9967ddff13946ce8dc4e125cde47ed479d1053744a54c17205cb0abc1f773`, config sha256 `4dd762347e8fd741461dcf9de6b23f07bbdc732a4624ed6c520c7a921e82ea75`, WAV-set sha256 `5040ca6fa3adcc58f6ed4172f5e73f0d84c6cf8a2e06e74c6de27db5cc6e0f19` (550 WAVs)
  - FT4: `research/corpus/synth/manifests/ft4_clean.manifest.json` sha256 `b7eeaeee6e7cc186324ea6b10539b2a6b7025b17212ea5b2683605162c6131a9`, config sha256 `4f1b84c915aa6f10926a1131fe352ff691a2113d86c009fa7fd34aa950731783`, WAV-set sha256 `b2f472ce37860c243e8cd59f642abf17d8b8b7bc04b0fbef46b2b729df78e686` (550 WAVs)

## Results

### FT8

| SNR (dB) | merged, `Deep` | native alone, `Deep` | ft8_lib alone | merged, `Max` | native alone, `Max` | false decodes `Deep` / `Max` |
|---:|---:|---:|---:|---:|---:|---:|
| −24 | **0** | 0 | 0 | **0** | 0 | 0 / 0 |
| −23 | **0** | 0 | 0 | **0** | 0 | 1 / 1 |
| −22 | **0** | 0 | 0 | **0** | 0 | 1 / 1 |
| −21 | **1** | 1 | 0 | **1** | 1 | 0 / 0 |
| −20 | **11** | 6 | 7 | **11** | 6 | 1 / 1 |
| −19 | **37** | 32 | 30 | **37** | 32 | 0 / 0 |
| −18 | **47** | 46 | 46 | **47** | 46 | 1 / 1 |
| −17 | **50** | 50 | 50 | **50** | 50 | 2 / 2 |
| −16 | **50** | 50 | 50 | **50** | 50 | 0 / 0 |
| −15 | **50** | 50 | 50 | **50** | 50 | 0 / 0 |
| −14 | **50** | 50 | 50 | **50** | 50 | 1 / 1 |

### FT4

| SNR (dB) | merged, `Max` | native alone | ft8_lib alone | false decodes |
|---:|---:|---:|---:|---:|
| −24 | **0** | 0 | 0 | 1 |
| −23 | **0** | 0 | 0 | 0 |
| −22 | **0** | 0 | 0 | 2 |
| −21 | **0** | 0 | 0 | 1 |
| −20 | **0** | 0 | 0 | 1 |
| −19 | **0** | 0 | 0 | 3 |
| −18 | **0** | 0 | 0 | 1 |
| −17 | **0** | 0 | 0 | 1 |
| −16 | **6** | 3 | 5 | 2 |
| −15 | **21** | 11 | 16 | 2 |
| −14 | **43** | 38 | 38 | 2 |

### Thresholds

| | FT8 `Deep` | FT8 `Max` | FT4 `Max` |
|---|---:|---:|---:|
| SNR@50%, merged (shipped) | **−19.46** | **−19.46** | **−14.82** |
| SNR@50%, native alone | −19.27 | −19.27 | −14.48 |
| SNR@50%, ft8_lib alone | −19.22 | −19.22 | −14.59 |
| SNR@90%, merged (shipped) | −18.20 | −18.20 | not reached |
| SNR@90%, native alone | −18.07 | −18.07 | not reached |
| False decodes, all 550 WAVs | 7 | 7 | 16 |

### Window timing

| Run | budget | windows over deadline | max overrun | window total p50 / p95 / max | pre-native p50 | ft8_lib p50 | multipass ran |
|---|---:|---:|---:|---:|---:|---:|---:|
| FT8 `Deep` | 1000 ms | 269/550 | 106 ms | 997 / 1067 / 1106 ms | 20 ms | 14 ms | 0/550 |
| FT8 `Max` | 2000 ms | 22/550 | 80 ms | 1267 / 1989 / 2080 ms | 18 ms | 13 ms | 176/550 |
| FT4 `Max` | 800 ms | 519/550 | 77 ms | 818 / 870 / 877 ms | 15 ms | 13 ms | 0/550 |

The native floor stage is unconditional, so most windows end past the
deadline; that is production's behaviour too, not a harness artefact.

## Not modelled

  - fp_filter (station ADIF/cqdx/seed; cold start passes all)
  - cross_sequence_a7 (default off)
  - ap_eval_mode (default off)
  - scoped fast-path (no QSO partner)
  - recent-call AP pool (empty at start-up)

## Comparison with the planning probe

The planning phase ran a throwaway probe of the same sequence on a 2-CPU
quota (no hash-twin dedup) and counted 1/50 merged at −21 dB, 11/50 at −20 dB
and 37/50 at −19 dB for every preset. This run, on a 6-CPU quota with the
shared merge code, measured the same 1/50, 11/50 and 37/50 at both `Deep` and
`Max`. The probe saw no false decodes in those three steps; this run counted
one, at −20 dB. Where the two differ, the scorecards are authoritative. The FT4 probe counted 9 false decodes over 350 WAVs; this run
counted 16 over 550.

## Conclusion

At `Deep` the shipped FT8 pipeline reaches 50% recall at −19.46 dB, 0.19 dB lower than the native decoder alone (−19.27 dB): ft8_lib's decodes add sensitivity at the threshold. At −20 dB it decodes 11/50 (22%), far from the ">95% at −20 dB" earlier
docs claimed. `Max` (2000 ms, two passes) decodes exactly what `Deep` does at every step on this single-signal corpus. Published claims now quote these scorecards.
