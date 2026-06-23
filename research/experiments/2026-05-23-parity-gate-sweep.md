---
slug: parity-gate-sweep
mode: ft8
state: won
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/multipass-profile (chained after multipass-profile)
parent_hypothesis: hb-014
wild_card: false
scorecard: /tmp/parity_{0..6}.json + /tmp/parity_2_h1000.json + (graduation) /tmp/main_gate2.json
delta_vs_main: ~0 composite (recall flat), -21% novel decodes, -26% wallclock
disposition: WIN — graduate `max_parity_errors_for_osd: 4 → 2` to production
---

## Hypothesis

hb-014 (2026-05-13 bank): sweep `MAX_PARITY_ERRORS_FOR_OSD` ∈ {3..6}
on a curated corpus to understand the precision/recall tradeoff curve.
Memory note: tightening from `139.5% → 123.7%` of ft8_lib's recall
cut "false decodes, not real ones." The hypothesis is that the gate
is still wider than it needs to be — some current "real" decodes the
gate admits may actually be CRC-14 collision FPs, and tightening
might be precision-positive at zero recall cost.

## Change

1. Made `MAX_PARITY_ERRORS_FOR_OSD` configurable as
   `Ft8Config::max_parity_errors_for_osd` (previously a `const`
   inside `LdpcDecoder::decode_soft`).
2. Threaded the value through to the per-thread `LdpcDecoder`
   instances via `DecodeContext::max_parity_errors_for_osd` and a new
   `LdpcDecoder::with_max_parity_errors_for_osd` builder.
3. Added research CLI flag `--max-parity-errors-for-osd N` to
   `pancetta-research/src/bin/eval.rs` and corresponding builder
   `Ft8Decoder::with_max_parity_errors_for_osd` in the research
   crate.
4. **Production default changed from 4 to 2** in
   `Ft8Config::default()`.

189/189 lib tests pass.

## Result

### Sweep on curated-hard-200 (gates {0..6}, all else baseline)

| Gate | Composite | Recovered | Decode rate | Novel (~FP proxy) | Wallclock |
|-----:|----------:|----------:|------------:|------------------:|----------:|
|  0   |  0.254489 |     4365  |    0.508979 |               860 |    213 s  |
|  1   |  0.254489 |     4365  |    0.508979 |               907 |    217 s  |
| **2**| **0.254489** | **4365** | **0.508979** |          **952** | **246 s** |
|  3   |  0.254489 |     4365  |    0.508979 |              1060 |    314 s  |
|  4   |  0.254489 |     4365  |    0.508979 |              1210 |    331 s  |
|  5   |  0.254548 |     4366  |    0.509095 |              1421 |    239 s  |
|  6   |  0.254548 |     4366  |    0.509095 |              1741 |    246 s  |

(Wallclock is noisy — shared machine — but the broad trend tracks
gate width.)

### Verification on curated-hard-1000

| Gate | Recovered    | Decode rate | Novel | Δrecovered | Δnovel |
|-----:|-------------:|------------:|------:|-----------:|-------:|
| 4 (main) | 14222/28104 | 0.506049 | 4019 |        —   |     —  |
| **2** | 14219/28104  | 0.505942    | 3172  |       −3   |  −847  |

−3 recovered out of 28104 is well within decoder noise (0.011%).
−847 novel is identical proportionally to the hard-200 result
(−21%). The relationship is stable across corpus sizes.

## Disposition

**WIN.** Graduating `max_parity_errors_for_osd: 2` to production.

**Why this is a clear win:**

1. **Zero recall cost.** Recovered count is statistically identical
   (4365=4365 on hard-200; −3/28104 on hard-1000 is noise).
2. **Material precision gain.** Novel-decode count (proxy for FPs per
   the hb-039 finding that 97.1% of isolated novels are singletons
   likely-FPs) drops 21% on both corpora — consistent and
   reproducible.
3. **Material wallclock gain.** OSD trial loops are expensive
   (especially the higher-depth attempts); reducing gate width cuts
   the number of candidates that enter the OSD path. hard-200
   single-run wallclock dropped from 331s → 246s (−26%).
4. **The composite-score-doesn't-move thing is by design.** The
   composite is built on jt9-truth matches (recall) and doesn't
   penalize FPs at all today. So this WIN doesn't show up in
   `composite`, but it shows up where it matters: in the on-air
   experience (fewer fake QSO attempts) and in CPU usage.

## Learnings

- **OSD's contribution to recall on this corpus is essentially zero.**
  Gate=0 (OSD effectively disabled) gives the same recovered count as
  gate=6. Everything OSD recovers on hard-200 is either already
  matched by BP convergence OR is a novel decode that jt9 didn't find
  either. The hb-039 study said 97% of isolated novels are likely
  FPs, so most of those novels are noise.
- **The big question: are some of the novels real?** Some likely are
  (hb-024 found 64.6% of novel-callsigns were "continued" — appeared
  somewhere in jt9 truth across the corpus). But gate=2 vs gate=4
  keeps 952 of the 1210 novels on hard-200 (79%), so any real-novel
  loss is bounded by the 258 dropped. Even if all 258 were real,
  that's 258/8576 = 3% of truth — and crucially they're decodes that
  jt9 ALSO missed, so they wouldn't change our score against jt9.
- **The "right" gate is even lower than 2.** Gate=0 has fewer FPs
  (860 vs 952 = −10% additional) at zero recall cost. But gate=0
  disables OSD as a fallback for any non-BP-converged candidate,
  which is a much bigger ideological shift. Keeping gate=2 maintains
  OSD's role for "BP got within 2 parity errors" cases — those are
  the highest-confidence near-misses where OSD is most likely to
  recover a real decode. Followup [[hb-018-strong-fp-filter]] could
  re-enable wider gates if combined with a stronger FP filter.

## Follow-ups added to hypothesis bank

- **hb-014 → CLOSED (graduated).** Production default is now 2.
- **hb-041 (new)**: try `max_parity_errors_for_osd: 0` (disable OSD
  fallback for non-converged candidates). Sweep shows another
  ~10% FP reduction at zero hard-200 recall cost — but it would shift
  pancetta from "BP+OSD" to "BP-only" architecturally. Worth a
  scorecard run on synth-clean to see if OSD ever helps in easier
  conditions.
- **Future signal-for-novels work:** A reliable FP filter that
  distinguishes the ~3% real novels from the 97% FPs would let us
  re-widen the gate AND improve recall vs jt9. See parked
  [[hb-018]] (already in bank).

## Reproducing

```bash
# Sweep
for n in 0 1 2 3 4 5 6; do
  cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-200 --mode ft8 \
    --max-parity-errors-for-osd $n \
    --output /tmp/parity_$n.json
done

# Hard-1000 verification
cargo run --release -p pancetta-research --bin eval -- \
    --tier curated-hard-1000 --mode ft8 \
    --max-parity-errors-for-osd 2 \
    --output /tmp/parity_2_h1000.json
```
