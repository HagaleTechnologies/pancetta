---
slug: pan158-eco-fine-sync
mode: ft8
state: shipped
created: 2026-10-25T11:28:29Z
last_updated: 2026-10-25
branch: PAN-158
parent_hypothesis: Task W3.3's per-candidate fine time/frequency sync (`fine_sync_enabled`) still recovers more signals at the Eco preset's 1 ms floor-only budget, in the merged ft8_lib + native output production delivers, on today's decoder
wild_card: false
scorecard: research/scorecards/pan158-eco-*.json
delta_vs_main: synth-clean merged recovery 296 → 321 of 550 (+25, paired 95% CI [+16, +35]); 0 → 0 false decodes on synth and on noise_1000; elapsed −24.5% (synth), −18.8% (noise)
disposition: SHIPPED. `apply_effort_overrides` turns `fine_sync_enabled` on for FT8 at a resolved Eco preset with a published 1 ms budget (PAN-158); the library default stays false.
---

# PAN-158: fine sync at the Eco preset, re-measured on today's decoder

## Question

Task W3.3b (`research/experiments/2026-07-08-w33b-budgeted-fine-sync-remeasurement.md`)
found that `Ft8Config::fine_sync_enabled` wins at Eco (hard-200: +27 TP,
95% CI [+15, +40]) and loses at Standard (−23 TP), because at 250 ms one slow
candidate starves the budget-gated rest of the pool, while at 1 ms only the
unconditional floor set runs. That measurement predates months of decoder
changes, scored the native decoder alone, and started its deadline right before
the native call. Production anchors the deadline at window receipt, runs
ft8_lib first and delivers the merged set. This A/B asks whether the Eco win
survives all three differences before the coordinator turns the flag on for
Eco.

## Commands

```bash
cargo build --release -p pancetta-research --bin eval --bin compare --bin gen-synth --bin gen-noise
PAN158_RUN=$(mktemp -d /tmp/pan158-measurement.XXXXXX)
export RAYON_NUM_THREADS=1
# Rewrites the gitignored WAVs under research/corpus/synth/wavs/clean; the
# committed manifest is not touched (eval's synth-clean tier always reads it).
target/release/gen-synth --config research/corpus/synth/manifests/clean.config.json \
  --output "$PAN158_RUN/clean.manifest.json"
target/release/gen-noise --count 1000 --seed 20260706 --birdie-fraction 0.3 \
  --output-dir "$PAN158_RUN/noise-wavs" \
  --manifest "$PAN158_RUN/noise.manifest.json" --label noise_1000

# One eval at a time: the budget is wall-clock.
target/release/eval --tier noise_1000 --mode ft8 --effort eco --budget-anchor receipt --merge-ft8lib \
  --noise-manifest "$PAN158_RUN/noise.manifest.json" --output research/scorecards/pan158-eco-noise-control.json
target/release/eval --tier noise_1000 --mode ft8 --effort eco --budget-anchor receipt --merge-ft8lib --fine-sync-enabled \
  --noise-manifest "$PAN158_RUN/noise.manifest.json" --output research/scorecards/pan158-eco-noise-variant.json
target/release/eval --tier synth-clean --mode ft8 --effort eco --budget-anchor receipt --merge-ft8lib \
  --output research/scorecards/pan158-eco-synth-control.json
target/release/eval --tier synth-clean --mode ft8 --effort eco --budget-anchor receipt --merge-ft8lib --fine-sync-enabled \
  --output research/scorecards/pan158-eco-synth-variant.json

target/release/compare research/scorecards/pan158-eco-synth-control.json \
  research/scorecards/pan158-eco-synth-variant.json --bootstrap-n 1000 --bootstrap-seed 0xb007
target/release/compare research/scorecards/pan158-eco-noise-control.json \
  research/scorecards/pan158-eco-noise-variant.json
```

The plan's command shape passed `--synth-manifest` to eval. eval has no such
flag (`Error: unknown arg: --synth-manifest`), so the synth arms read the
committed manifest. For all 550 entries, the generated and committed manifests
agree on every field eval reads (`wav_path`, `encoded_message`, `snr_db`). They
differ only in a `mode` key and in the last digit of `base_freq_hz`/`dt_s`
(at most 9.1e-13 Hz). PAN-110 measured the same way.

The synth tier now writes one `per_wav_records` entry per WAV (keyed by the
WAV's SHA-256, `truth` 1, `recovered` 0/1, `novel` = delivered messages that
are not the WAV's one signal). That is what lets `compare` bootstrap a paired
recall CI on a synthetic corpus. Before PAN-158 it skipped synth tiers.

## Host and run conditions

- AMD Ryzen 9 5950X 16-Core Processor, 6 CPUs visible to the process (cgroup `cpu.max` `600000 100000`), `RAYON_NUM_THREADS=1`, so `cores_used` 1 in every scorecard. Production's tier probe, run at the start of each eval, classified the host **fast** in three runs and **moderate** in the synth control run (p95 328 to 470 ms; the probe is load-sensitive), so `Auto` resolves to Deep or Standard here: this host measures the literal Eco preset, not Auto on Slow hardware.
- Build: `release`, rustc 1.98.0 (88d9e12ae 2026-08-18), `ft8lib_available: true`. Base commit `d15556481930e361bcf2186a618562ca9c461e4a`; `git.dirty: true` in all four scorecards, because the per-WAV record change was uncommitted when they were generated.
- The container shares a 32-thread host with other tenants (load average 10 to 20 throughout). The runs and what overlapped them:

| Run | Start (UTC) | Wall (s) | Overlapping work in this container |
|---|---|---|---|
| noise control | 11:28:29 | 1173.7 | none |
| noise variant | 11:48:03 | 953.4 | `nice -n 19` `cargo test -j 4 --no-run` (11:48:05 to 11:58:47), then two short niced unit-test rebuilds (12:00 to the arm's end at 12:03:56) |
| synth control | 12:05:21 | 596.1 | none |
| synth variant | 12:15:18 | 450.1 | none |

  Compilation overlapped only the noise *variant* arm. It was started after
  the control arm exited, and the synth pair was held until it finished.
  Contention there can only make the variant look slower, so it biases the
  elapsed gate against the change. Noise false-positive counts come from Eco's
  unconditional floor stage and do not depend on CPU time.
- **Raspberry Pi / Windows MiniPC / on-air: unmeasured.** Nothing here ran on target hardware.

## Corpus

- synth-clean: committed config `research/corpus/synth/manifests/clean.config.json` (sha256 `4dd762347e8fd741461dcf9de6b23f07bbdc732a4624ed6c520c7a921e82ea75`, seed 42, 50 messages × 11 SNR steps −24…−14 dB, AWGN, one signal per WAV), manifest sha256 `8ef9967ddff13946ce8dc4e125cde47ed479d1053744a54c17205cb0abc1f773`, WAV-set sha256 `5040ca6fa3adcc58f6ed4172f5e73f0d84c6cf8a2e06e74c6de27db5cc6e0f19` (550 WAVs, same value in both arms' `provenance.corpus`).
- noise_1000: regenerated with the committed `noise_1000` manifest's own parameters (1000 WAVs, seed 20260706, birdie fraction 0.3, 300 with birdies); generated manifest sha256 `6fd856c0e7beb5233a92e4a59bc5912059bf0d4bfce13ea987f370848bd59917`. Both arms read the same generated manifest.
- Neither is the original `curated-hard-200` recording set. Its manifest entries point at a developer machine (200 entries, 0 files present here), so this result does not reproduce or replace the July +27.

## Provenance check

All four scorecards record `pipeline: ft8lib+native-merge`, `budget_anchor: receipt`, `effort: eco`, `budget_ms: 1`, `ft8lib_available: true`, `ap_my_call: null`. `compare`'s config diff is exactly `decoder.decoder.fine_sync_enabled false → true` for both pairs. `not_modelled` names the production stages the harness leaves out: the station FP filter (cold start passes everything), cross-sequence A7 (default off), AP eval mode (default off), the scoped fast path (no QSO partner) and the recent-call AP pool (empty at start-up).

## Results

### synth-clean (550 WAVs, merged output)

| | control | variant | Δ |
|---|---:|---:|---:|
| WAVs processed / per-WAV records | 550 / 550 | 550 / 550 | |
| merged recovered | 296 | 321 | **+25** |
| native decoder recovered | 284 | 318 | +34 |
| ft8_lib recovered | 283 | 283 | 0 |
| false decodes (Σ `by_snr_db.fp`, = Σ record `novel`) | 0 | 0 | 0 |
| SNR @ 50% (dB) | −19.5 | −20.0 | −0.5 |
| SNR @ 90% (dB) | −18.2 | −19.0 | −0.8 |
| elapsed (s) | 596.1 | 450.1 | −24.5% |

Merged recovered per SNR bin (control → variant): −21 dB 1 → 1, −20 dB 11 → 25,
−19 dB 37 → 45, −18 dB 47 → 50; −24…−22 dB stay 0 and −17…−14 dB stay 50/50.
The native decoder's +34 matches the October research run (284 → 318). Nine of
those extra native decodes were signals ft8_lib already delivered, so the
operator-visible gain is +25.

`compare` (synth): WIN SNR@50% −19.5 → −20.0 dB; no regressions;
`rec Δ=+25 (95% CI [+16.0, +35.0], n_bootstrap=1000) — significant`;
`novel Δ=+0 (95% CI [+0.0, +0.0]) — NOT significant`; no alignment caveat
(550 common WAVs); elapsed gate `596.1 → 450.1 (−24.5%, budget +20.0%)`.

Window timing (receipt-anchored, 550 windows each; ms, p50 / p95 / max):

| | control | variant |
|---|---|---|
| pre-native (waterfall + ft8_lib) | 20.2 / 33.3 / 39.1 | 21.6 / 34.2 / 41.9 |
| headroom at the native call | 0 / 0 / 0 | 0 / 0 / 0 |
| native call | 1044.5 / 1215.1 / 1393.0 | 775.8 / 903.4 / 1023.4 |
| whole window | 1066.4 / 1239.2 / 1420.3 | 799.2 / 932.2 / 1053.4 |
| windows over deadline / max overrun | 550 / 1419.3 | 550 / 1052.4 |

Every window overruns Eco's 1 ms in both arms: the budget is spent before the
native call starts, so only the unconditional floor runs, and that floor costs
about 0.8 to 1.4 s per window on this host. Fine sync makes the floor cheaper
here: one matched-demod attempt replaces the legacy fallback's up to 21 blind
LDPC trials.

### noise_1000 (1000 WAVs, merged output)

| | control | variant |
|---|---:|---:|
| WAVs processed | 1000 | 1000 |
| `false_positives_total` | 0 | 0 |
| `noise_files_decoded` | 0 | 0 |
| elapsed (s) | 1173.4 | 953.2 (−18.8%) |

Both arms are actually zero, not assumed zero. The noise tier records no
`window_timing`; per-window timing comes from the synth arms.

`compare` (noise): no regressions; FP-on-noise hard gate passed; elapsed gate
`1173.4 → 953.2 (−18.8%, budget +20.0%)`.

## Gate

Every pre-registered condition from the PAN-158 plan holds:

- Coverage: both synth arms processed 550 WAVs with 550 per-WAV records, aligned on all 550 SHA-256 identities (no drops) and the same WAV-set hash; both noise arms processed the same 1000 WAVs.
- Provenance: FT8, Eco, 1 ms, receipt anchor, `ft8lib+native-merge`, `ft8lib_available: true` in all four; the only config difference is `fine_sync_enabled`.
- Recall: merged Δ +25, paired 95% CI lower bound +16 > 0 (1000 resamples, seed `0xb007`).
- False decodes: synth Σ`fp` 0 → 0; noise `false_positives_total` 0 → 0 and `noise_files_decoded` 0 → 0. Both arms really are zero.
- Cost: same host and `cores_used` 1 on both sides, so `compare`'s elapsed gate was enforced. Synth −24.5%, noise −18.8%, against a +20% limit.

## Decision

Ship it, bounded to the measured regime. `apply_effort_overrides`
(`pancetta/src/coordinator/effort.rs`) sets `fine_sync_enabled` on every
publish to: resolved preset is Eco (literal Eco, or Auto on Slow), protocol is
FT8, and published budget is 1 ms. `fine_sync_enabled` joins
`DecoderRebuildKey`, so the live decoder is rebuilt when the flag changes. The
library default stays false. Standard, Deep, Max, Auto on Moderate or Fast,
FT4, and any published budget other than 1 ms are untouched: W3.3b's
Standard regression came from starving a budget-gated candidate pool, which
this corpus does not test. The policy reads the published budget, so an
explicit `budget_ms = 1` is on, and an explicit `budget_ms` other than 1 is
off only until a live effort cycle publishes Eco's own 1 ms. This result does not reproduce the July hard-200
+27; it shows the same direction on a different, synthetic corpus.

## Limits

- Synthetic single-signal AWGN and noise-only corpora on one x86 host. No crowded bands, fading, the original hard-200 recordings, MiniPC, Pi or on-air session.
- `per_wav_records` on the synth tier assume exactly one signal per WAV; the synth-pair tier has its own runner and is unaffected.
- Eco's 1 ms is a scheduling budget: it stops budget-gated stages, but floor work is not cancelled mid-candidate, so it is not a 1 ms latency promise. The window timing above is what the floor actually costs on this host.
