---
slug: hb-146-synth-pair-adversarial-corpus
mode: ft8
state: shipped
created: 2026-06-01T13:00:00Z
last_updated: 2026-06-01T13:00:00Z
branch: iter/2026-06-01-hb-146
parent_hypothesis: hb-146 (synthetic adversarial corpus targeting measured walls — C3)
wild_card: false
scorecard: research/scorecards/sweep/synth-pair-200-baseline.json
delta_vs_main: 0 (no decoder change — INFRA tier)
disposition: SHIPPED — pair-synth generator + tier wired; baseline measured; corpus reproduces marginal-SNR pair structure V2/V3 need; V2 + V3 flagged for re-eval.
---

## Motivation

hb-086 V2 (joint LLR with iterative soft cancellation) and V3
(subtract-aware sync threshold relaxation) were SHELVED across the two
real-audio corpora available in batch 14 (2026-05-31; Phase A honesty
pass 2026-06-02 replaced "DEFINITIVELY SHELVED") because the available
real-audio corpus (hard-200) had no marginal-SNR pair structure: every
decoded neighbor on hard-200 was already a strong, high-confidence
decode. Soft cancellation collapses to hard subtraction when LLRs are
sharp; sync-threshold relaxation surfaces sub-Costas noise rather than
weak signals. This experiment was designed precisely to be the re-test
gate that could unshelve them.

Corpus ideation (`research/ideation/2026-06-01-corpus.md`, entry C3 →
hb-146) proposed the missing complement: an adversarial synthetic
corpus that algorithmically generates the mutual-masking pair regime
V2/V3 were designed for. If pancetta provably misses weak signals in
that regime (close Δf, large ΔSNR), V2/V3 unshelve as worth re-running
against a corpus where their primary criterion is satisfied.

This experiment ships the corpus, baselines pancetta on it, and
verifies the regime is reproduced.

## Implementation

### Generator (`gen-synth-pair`)

New binary `pancetta-research/src/bin/gen_synth_pair.rs`. For each
parameter tuple `(strong_template_idx, ΔSNR, Δf, Δt)`:

1. Encode + modulate the strong message at 1500 Hz (canonical base).
2. Encode + modulate the weak message at `1500 + Δf` Hz.
3. Allocate a 15 s slot buffer (180,000 samples at 12 kHz).
4. Place the strong signal starting at `slot_lead_in_s` (default 1.0 s).
5. Place the weak signal starting at `slot_lead_in_s + Δt`, scaled by
   `10^(-ΔSNR/20)`.
6. Add AWGN with `noise_rms = strong_signal_rms / 10^(strong_snr_db/20)`.

Seeds are deterministic per (run-seed, pair_idx, ΔSNR, Δf, Δt) so
regeneration is byte-identical.

### Library types

`pancetta-research/src/synth.rs` gains `SynthPairConfig`,
`SynthPairEntry`, `SynthPairManifest` (schema version 1). The pair
manifest schema is independent of the existing `SynthManifest` (single
signal); they share the workspace dir but not the wire format.

### Eval tier

`synth-pair-200` added to `eval.rs` tier dispatch. Reports:

- Per-bucket recovery for `(ΔSNR, Δf, Δt)` — strong-decoded fraction,
  weak-decoded fraction.
- Aggregate `truth_decodes_total = 2 × n_wavs`,
  `truth_decodes_recovered = strong_total + weak_total`, `decode_rate`.

Diagnostic tier (NEVER primary): the design constraint is that V2/V3
graduation must require co-improvement on hard-200, never just on this
tier.

### Config

`research/corpus/synth/manifests/synth_pair_200.config.json`:

- 6 message templates (canonical FT8 exchange)
- `strong_snr_db = -8.0` (clean, well above pancetta's sensitivity)
- `delta_snr_db_steps = [0, 3, 6, 9, 12]` (weak signal 0..12 dB below)
- `delta_freq_hz_steps = [6, 12, 25, 50]` (close → wide)
- `delta_time_s_steps = [0, 0.1, 0.25]`
- `max_wavs = 200` (stride-2 subsample of 360 grid points → 180 actual)

## Results

Pancetta baseline on the 180 generated WAVs (eval @ HEAD = main,
default production config):

- Strong recovery: **177/180 (98.3%)** — the strong signal is decoded
  in nearly every WAV, as expected.
- Weak recovery: **92/180 (51.1%)** — pancetta misses 49% of the weak
  signals.

The regime map (per-bucket weak-recovery rate) is the punchline:

| ΔSNR | Δf=6 Hz | Δf=12 Hz | Δf=25 Hz | Δf=50 Hz |
|------|---------|----------|----------|----------|
| 0 dB | 50–100% | 100%     | 83–100%  | 100%     |
| 3 dB | 0–83%   | 83%      | 83%      | 100%     |
| 6 dB | 0–17%   | 17%      | 67–100%  | 100%     |
| 9 dB | 0%      | 0%       | 0–83%    | 100%     |
| 12 dB| 0%      | 0%       | 0%       | 83%      |

(Cells aggregate over the three Δt steps; full per-Δt detail printed
to stderr at eval time and recorded in
`research/scorecards/sweep/synth-pair-200-baseline.json`.)

### Verification gates

The corpus targets two structural claims; both are satisfied:

1. **Pancetta DOES miss weak signals in low-Δf / high-ΔSNR.** Weak
   recovery is 0% in the corner (ΔSNR ≥ 9 dB AND Δf ≤ 12 Hz). This is
   exactly the regime V2 (soft cancellation) is designed to crack: if
   the strong signal is subtracted with soft tone posteriors, the
   residual weak signal at a close Δf becomes the dominant local
   structure.

2. **The marginal-SNR neighbor structure exists in this corpus.** At
   the 0% buckets, the strong signal is still decoded — so pancetta
   IS producing the "decoded neighbor with weak unresolved truth"
   configuration that V2's geometric kill-switch on hard-200 found at
   78.3% (pair-likely) but where V2's mechanism couldn't help because
   the neighbor itself was already strong+sharp. Here the weak truth
   is by-construction marginal, not absent — the V2 mechanism has a
   well-posed input.

### Wall-clock

Eval over 180 WAVs at default decode config: ~290 s (1.6 s/WAV).
Generation: <5 s for all 180 WAVs. Storage: 180 × ~360 KB = ~65 MB.

## Disposition

- **hb-146 SHIPPED.** Generator, config, manifest, eval tier, baseline,
  journal all landed.
- **hb-086 V2 + V3 flagged for re-eval** on `synth-pair-200`. Bank
  entries updated with `synth_pair_revisit_candidate: true`. A future
  iter (Session 2 of the joint-decoding family) can re-run V2/V3 with
  the existing CLI flags (`--joint-pair-retry`,
  `--joint-residual-sync-relax-db`,
  `--joint-residual-sync-window-bins`) against this tier in addition
  to hard-200. Graduation still requires co-improvement on hard-200;
  this tier is diagnostic.

## Caveats

- The corpus is synthetic AWGN — no fading, no multipath, no DT
  jitter. It isolates the pair-decoding regime cleanly but doesn't
  exercise propagation effects.
- The strong signal sits at a fixed 1500 Hz canonical base. A future
  extension could randomize the base across the FT8 audio passband to
  exercise frequency-dependent decoder behavior. Out of scope for this
  iter (V2's mechanism is base-frequency-invariant by construction).
- Time offsets are limited to [0, 0.1, 0.25] s, all positive. Negative
  Δt (weak arrives before strong) is in the `slot_lead_in_s` headroom
  but not in this config's sweep. Easy extension if needed.
- The 200-cap is approximate (180 actual after stride subsample of 360
  full-Cartesian). The label kept as "synth_pair_200" for stability;
  the manifest entry count is the source of truth.
