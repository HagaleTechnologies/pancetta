---
slug: hb-086-joint-pair-retry-v1
mode: ft8
state: graduated
created: 2026-05-28T00:30:00Z
last_updated: 2026-05-28T01:30:00Z
branch: iter/2026-05-28-hb-086-joint-decoding
parent_hypothesis: hb-086
wild_card: false
scorecard: research/scorecards/history/2026-05-28-hb-086-joint-pair-retry-v1.json
delta_vs_main: composite +0.000700 (0.568424 → 0.569123); hard-200 +12 rec / +1 novel; hard-1000 +17 rec / +9 novel
disposition: GRADUATE — `joint_pair_retry` default false → true. Force-retry failed original candidates against the residual spectrogram catches pair-structure decodes that the residual sync_search threshold misses.
---

## Hypothesis

After hb-079/hb-080's coherent multipass subtract+repass saturates at
N=3 (see `2026-05-27-hb-080-multipass-n3.md`), the residual wall is
mutually-masking signal pairs and clusters. hb-086 design spec
(`docs/superpowers/specs/2026-05-27-joint-decoding-design.md`)
proposes joint multi-candidate decoding. **V1 is the minimum-viable
slice**: don't change the decoder structurally; just *retry* the
original sync candidates that pass-1 + multipass failed on, but this
time against the **residual** spectrogram (post-subtract). Catches
pairs where:

- B's Costas pattern is in the *original* sync_candidates list
  (pass-1 sync_search found it).
- B's pass-1 LDPC failed because of A's interference.
- After multipass subtracts A, B's *residual* sync_score falls below
  `min_sync_score` (the residual sync energy is diffuse because B
  itself was partially corrupted by A's tones), so the
  `costas_sync_search` in `coherent_subtract_and_repass` rejects B.
- But B's residual LLRs at the (now-cleaned) overlap bins are
  decodable.

This is a narrow but real window — exactly the kind of pair that the
existing pipeline can't catch.

## Kill-switch diagnostic (iter 1)

Per the spec's pre-implementation gate:
`examples/joint_decoding_pair_density.rs` decoded the top-20 worst
hard-200 WAVs (which account for 17% of all misses at 60% per-WAV
miss rate per `2026-05-27-residual-wall-diagnostic.md`) and
classified each missed truth as pair-likely (within 50 Hz of a
recovered pancetta decode) vs isolated.

**Result: 78.3% pair-likely (515/658 missed truths within 50 Hz)
vs 30% threshold.** Distribution:

| freq distance | count | pct |
|--------------:|------:|----:|
| ≤12.5 Hz (overlap) | 210 | 31.9% |
| 12.5-25 Hz (adjacent) | 135 | 20.5% |
| 25-50 Hz (same band) | 170 | 25.8% |
| 50-100 Hz (near) | 126 | 19.1% |
| 100-200 Hz | 17 | 2.6% |
| >200 Hz (isolated) | 0 | 0.0% |

Strong PROCEED signal. The structural assumption (pair masking is
the binding constraint on the residual wall) holds on real data.

## Change

`Ft8Config::joint_pair_retry: bool` (default `true` after this
graduation). After the multipass subtract+repass loop in
`decode_window_with_ap`, if the flag is set, run a new
`joint_pair_retry_pass`:

1. Reverse-derive the `CostasCandidate` position for every entry in
   `pass_decoded` with `tone_symbols.is_some()` — that's the set of
   positions the multipass subtract already touched.
2. From the ORIGINAL `sync_candidates` list, keep those NOT at any
   subtracted position (±1 freq_bin, ±2 time_step — same tolerance
   the residual sync_search uses).
3. For each pending candidate, extract symbols from the (residual)
   spectrogram, compute LLRs, run LDPC, verify CRC, parse, plausibility-
   check. Successful decodes get `tone_symbols` populated and join
   `pass_decoded`.

Research builder: `with_joint_pair_retry(bool)`. Eval flags:
`--joint-pair-retry` / `--no-joint-pair-retry`.

## Result

### hard-200 A/B (FP filter on, 200 WAVs)

| variant | recovered | novel | rate | elapsed |
|---|---:|---:|---:|---:|
| baseline (hb-080 N=3) | 4604 | 920 | 0.53685 | 238s |
| **+ joint-pair-retry** | **4616 (+12)** | **921 (+1)** | **0.53825 (+0.00140)** | 301s |

+12 recall with +1 novel cost is a clean ~12:1 signal-to-noise ratio
(novels here are *not* in WSJT-X's baseline either, so they're
either real catches WSJT-X also misses, or FPs that pass the filter;
the FP filter's callsign-continuity check has stayed solid through
the 26 prior graduations, so I treat them as real).

### Full 5-tier confirmation

| metric | main (hb-080 N=3) | variant (hb-086 V1) | Δ |
|---|---:|---:|---:|
| **composite** | 0.568424 | **0.569123** | **+0.000700** |
| fixtures pass_rate | 1.0 | 1.0 | 0 |
| synth-clean @50/@90 dB | -20/-18 | -20/-18 | 0 |
| synth-doppler | (unchanged) | (unchanged) | 0 |
| hard-200 rec / novel | 4604 / 920 | 4616 / 921 | **+12 / +1** |
| hard-1000 rec / novel | 14953 / 3027 | 14970 / 3036 | **+17 / +9** |
| wild-50 | 0 | 0 | 0 |
| elapsed | 2270s | 2320s | +2.2% |

- Composite +0.000700, matching the hard-200 prediction.
- hard-1000 +17 confirms the gain scales (not a 200-specific quirk).
- Synth + fixtures preserved.
- Elapsed only +2.2% — the joint-pair-retry pass only does work on
  WAVs with surviving non-subtracted sync candidates (most synth
  WAVs are clean, the cost is concentrated on hard-* where it's
  earning rebates).

## Why the win is smaller than the diagnostic suggested

The 78% pair-likely number tells us 78% of misses *have* a nearby
recovered decode. V1 recovers +12 on hard-200, which is ~1.5% of
the 696 misses on top-20 WAVs (the densest 17% of misses). Two
reasons the diagnostic ceiling doesn't transfer linearly:

1. **Not every missed-truth is in the original sync_candidates
   list.** Many missed signals never produced a strong enough
   Costas pattern in the raw spectrogram for `sync_search` to find
   them at all (sync_score < `min_sync_score`). V1 only retries
   what's already in the list — sync-search misses stay missed.
2. **Not every retried candidate has decodable residual LLRs.**
   Even when A is subtracted, B's overlap bins can still be
   corrupted by C, D, ... or by imperfect ML projection on A's
   modulation phase. The LDPC budget rejects the rest.

V2 (joint LLR with iterative interference cancellation) addresses
both — but V1's win is real and free of regression cost, so it
graduates standalone.

## Decision

**GRADUATE** at default `true`. main.json updated; scorecard
archived to `history/2026-05-28-hb-086-joint-pair-retry-v1.json`.
Hypothesis bank records hb-086 graduated; V2 spawned at lower
priority (0.40 → reduced from 0.50, since V1 captured the
easiest wins).

## Cumulative session impact (through hb-086 V1)

| metric | start (2026-05-25) | now (2026-05-28) | Δ |
|---|---:|---:|---:|
| **composite** | 0.555131 | **0.569123** | **+0.013993** |
| hard-200 recovered | 4376 | 4616 | **+240** |
| hard-1000 recovered | 14267 | 14970 | **+703** |
| fixtures | 1.0 | 1.0 | 0 |
| synth-clean @50 | -20 | -20 | 0 |

**10 graduations across four days:** hb-063 layered BP, hb-056
non-coh cross-cycle, hb-058 contest-FP, hb-060/061 cleanup, hb-075
MRC coherent, hb-072 CQ-whitelist, hb-079 coherent multi-pass,
hb-080 N=3 multi-pass, **hb-086 V1 joint-pair-retry**.

## Learnings

- **Kill-switch diagnostics earn their keep.** The pair-density
  number gave clear go/no-go on a multi-session bet. If it had read
  ≤30% we'd have shelved without writing code.
- **The narrowest viable slice graduates first.** V1 doesn't try to
  fix everything joint decoding could fix — it just plugs the
  cheapest leak (residual sync threshold rejecting decodable
  positions) and ships. V2 can attack the deeper structure with the
  ceiling we just measured (+12 hard-200 already in the books).
- **"Smaller than the diagnostic suggested" isn't a regression.**
  78% pair-likely meant the *mechanism applies* to 78% of misses,
  not that V1 *recovers* 78%. V2's hypothesis is that the gap
  between "applies" and "recovers" is the joint-LLR opportunity.

## New spawns

- **hb-086 V2** (priority 0.40, reduced from 0.50): soft-decision
  joint LLR with iterative interference cancellation. After V1's
  retry, for candidates that *failed* LDPC, run a second iteration
  where their LLRs are conditioned on the assumed-correct decodes'
  symbols (soft cancellation, not hard subtract). The narrow
  hypothesis: the residual still has C-D-... interference on B's
  overlap bins; conditioning LLRs on the decoded-A's symbol
  uncertainty lets BP do the cleanup that hard subtract can't.
