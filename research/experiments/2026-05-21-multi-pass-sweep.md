---
slug: multi-pass-sweep
mode: ft8
state: shelved
created: 2026-05-21T00:00:00Z
last_updated: 2026-05-21T00:00:00Z
branch: experiment/ft8/multi-pass-sweep
parent_hypothesis: hb-001
wild_card: false
scorecard: research/scorecards/sweep/ (hard200-passes-{1..4}.json + synth-passes-{1..4}.json)
delta_vs_main: ~+0.0001 composite at max_passes=4 vs 3 (noise floor)
disposition: SHELVED with diagnostic findings; status quo (max_passes=3) stays
---

## Hypothesis

Multi-pass subtract-and-redecode is the documented primary WSJT-X
advantage on busy bands. Pancetta defaults to `max_decode_passes: 3`
but the contribution of passes 2 and 3 had never been measured against
the curated corpus. Sweep `max_decode_passes ∈ {1, 2, 3, 4}` against
curated-hard-200 (primary metric) and synth-clean (sanity), expecting
+0.05 to +0.15 real decode rate on hard-200 from multi-pass.

## Change

Pure research infrastructure — no production behavior changed.

- `pancetta-research/src/decoder.rs` — added `Ft8Decoder::with_max_passes(usize)`
  builder method, enabling sweep experiments without touching the
  production `Ft8Config::default()`.
- `pancetta-research/src/bin/eval.rs` — added `--max-passes N` CLI
  flag, threaded through to the decoder wrapper. Defaults to absent
  (production default, currently 3).
- Sweep run on curated-hard-200 + synth-clean at max_passes ∈ {1..4}.

Production `Ft8Config::max_decode_passes` left at 3 — no change.

## Result

**hard-200 sweep:**

| max_passes | recovered | novel | rate   | wall-clock | Δ recovered vs pass-1 |
|------------|-----------|-------|--------|------------|-----------------------|
| 1          | 3786      | 633   | 0.4415 | 9.6 s      | +0                    |
| 2          | 3829      | 674   | 0.4465 | 76.5 s     | +43                   |
| 3          | 3832      | 676   | 0.4468 | 92.8 s     | +46                   |
| 4          | 3833      | 676   | 0.4469 | 99.7 s     | +47                   |

**synth-clean sweep:** *identical 35/60 decodes at every max_passes
setting*. Per-SNR table is bit-identical between passes=1 and passes=4.
Multi-pass contributes **zero** on clean signals.

## Disposition

**SHELVED.** The hypothesis predicted +5-15% real decode rate from
multi-pass; measured contribution from passes 2+ is +1.2% on hard-200
(+47 / 3786) and 0% on synth-clean. Below-hypothesis by an order of
magnitude. The composite delta of raising max_passes from 3 to 4 is
+0.0001 — well into noise floor; not promotable.

Status quo (`Ft8Config::max_decode_passes = 3`) stays as default. The
infrastructure changes (`--max-passes` flag + `with_max_passes`
builder) cherry-pick to main as reusable research tooling. The sweep
scorecards stay in `research/scorecards/sweep/` as historical evidence.

## Learnings

- **Multi-pass is not where the WSJT-X gap lives.** Pass 1 alone
  recovers 98.8% (3786 / 3832) of the current 3-pass total on
  hard-200. Passes 2+ add only ~1.2% — far below the hb-001 prior of
  5-15%. The decoder gap to WSJT-X is not in pass count.

- **`subtract_with_sidelobes` residuals are not clean enough.** The
  sharp diminishing returns (pass 2 +43, pass 3 +3, pass 4 +1) say
  that after one subtraction, the residual contains so little new
  signal that re-running the full pipeline doesn't surface anything.
  Either (a) the subtraction is leaving artifacts that mask weak
  signals, or (b) the residual signals are below sync threshold
  regardless of subtraction quality. Without per-pass instrumentation
  at the candidate level (which we deferred for scope), we can't
  separate these.

- **Pass 2 has an 8× compute multiplier for 1.1% gain.** Pass 1 is
  48 ms/WAV; pass 2 is 382 ms/WAV (the additional pass costs ~335 ms
  on top of pass 1). That's a brutal cost-benefit. For on-air
  deployment a single-pass mode with `max_passes=1` is 90% as
  effective at 10% of the cost — a candidate for an `aggressive=false`
  fast path.

- **Synth-clean is the wrong corpus for multi-pass evaluation.** Clean
  signals have nothing to subtract from. Future multi-pass experiments
  should use synth-doppler (when generated) or a busy-band synth
  variant — never synth-clean.

- **The hb-001 prior was wrong about which gap to close.** The
  "5-10% of WSJT-X" memory note attributed the gap to multi-pass.
  This sweep shows that's not the dominant cause. WSJT-X's lead must
  come from elsewhere — likely sync sensitivity, OSD depth, candidate
  count, or AP coverage. The remaining high-priority hypotheses
  (hb-003 sync count, hb-004 AP retune, hb-005 OSD beta) are now
  better-targeted than hb-001 was.

## Follow-ups added to hypothesis bank

- **hb-030 (new)** — Audit subtract_with_sidelobes residual quality.
  Why does pass 2 only add 1.2% on busy bands? Generate a controlled
  two-signal synth scenario (strong + weak at known SNRs), decode +
  subtract the strong, measure residual energy at the weak signal's
  TF cell vs raw audio. If residual artifacts mask the weak signal,
  the subtraction kernel needs improvement. Priority ~0.60. Estimated
  effort: 1-2 sessions.

- **hb-031 (new)** — `aggressive=false` fast-path with max_passes=1.
  On-air deployment context: the operator's autonomous loop benefits
  from latency, not last-1.2% recall. Wire a runtime config that
  switches between max_passes=1 (latency mode) and max_passes=3
  (deep mode) at the coordinator level. Pure plumbing; no decoder
  change. Priority ~0.40. Estimated effort: 0.5 sessions.

## Reproducing

```bash
# Sweep on hard-200:
for N in 1 2 3 4; do
    cargo run --release -p pancetta-research --bin eval -- \
        --tier curated-hard-200 --mode ft8 \
        --max-passes $N \
        --output research/scorecards/sweep/hard200-passes-$N.json
done

# Sweep on synth-clean:
cargo run --release -p pancetta-research --bin gen-synth -- \
    --config research/corpus/synth/manifests/clean.config.json \
    --output research/corpus/synth/manifests/clean.manifest.json
for N in 1 2 3 4; do
    cargo run --release -p pancetta-research --bin eval -- \
        --tier synth-clean --mode ft8 \
        --max-passes $N \
        --output research/scorecards/sweep/synth-passes-$N.json
done
```
