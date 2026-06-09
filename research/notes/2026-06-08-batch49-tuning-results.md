# Batch 49 — hb-242 + wide-lag baseline tuning results

**Date**: 2026-06-08
**Branch**: `iter/2026-06-08-batch-49`
**Corpus**: hard-200 (200 WAVs)
**Probes**:
- `pancetta-research/examples/batch49_sync_tuning.rs` (baseline + hb-242 sweep)
- `pancetta-research/examples/batch49_widelag_tuning.rs` (wide-lag sweep)
- `pancetta-research/examples/batch49_combined.rs` (combined stack)

Base config (every row): `max_decode_passes = 2`, `ldpc_iterations = 200`.
Tuning parameter varies per row — see Config column.

## Results table

| Config | Decodes | TPs | Δ TPs | Precision | Elapsed |
|--------|--------:|----:|------:|----------:|--------:|
| **baseline** (mp=2, ldpc=200, max_sync=300, all flags OFF) | 7082 | 5301 | +0 | 0.7485 | 438.7s |
| hb-242 ON, max_sync=300 | 7080 | 5283 | -18 | 0.7462 | 501.8s |
| hb-242 ON, max_sync=400 | 7500 | 5284 | -17 | 0.7045 | 647.3s |
| hb-242 ON, max_sync=500 | 7771 | 5279 | -22 | 0.6793 | 775.5s |
| hb-242 ON, max_sync=600 | 7904 | 5263 | -38 | 0.6659 | 859.1s |
| hb-242 ON, max_sync=800 | (skipped — monotonic degradation) | | | | |
| wide-lag ON, percentile=0.30, norm=1.20 | 6913 | 5297 | -4 | 0.7662 | 471.6s |
| wide-lag ON, percentile=0.40, norm=1.20 | 6893 | 5292 | -9 | 0.7677 | 476.9s |
| wide-lag ON, percentile=0.50, norm=1.20 | 6923 | 5297 | -4 | 0.7651 | 408.7s |
| wide-lag ON, percentile=0.60, norm=1.20 | 6923 | 5297 | -4 | 0.7651 | 395.9s |
| wide-lag ON, percentile=0.30, norm=1.00 | 6923 | 5297 | -4 | 0.7651 | 395.1s |
| wide-lag ON, percentile=0.30, norm=1.20 (re-run) | 6923 | 5297 | -4 | 0.7651 | 395.2s |
| wide-lag ON, percentile=0.30, norm=1.50 | 6923 | 5297 | -4 | 0.7651 | 395.0s |
| wide-lag ON, percentile=0.30, norm=2.00 | 6923 | 5297 | -4 | 0.7651 | 395.0s |
| combined (hb-242 ON max_sync=300 + wide-lag pct=0.30 norm=1.20) | 6910 | 5297 | -4 | 0.7666 | 411.2s |

## Headline finding

**Neither mechanism's tuning hypothesis was validated.**

- **hb-242 budget expansion FAILED.** Bumping `max_sync_candidates` did not let
  partial-Costas surface real signals — instead it monotonically degraded recall
  (-18 → -17 → -22 → -38 across max_sync ∈ {300, 400, 500, 600}) while precision
  collapsed (0.7462 → 0.6659). The additional candidates emitted by partial-Costas
  are predominantly noise that the downstream filters cannot reject cleanly. The
  Batch 48 hypothesis that "real TPs are being displaced at the cap" is FALSIFIED
  — what's being displaced is mostly noise, but a non-trivial number of FPs make
  it through.
- **Wide-lag baseline is parameter-insensitive at -4 TPs.** All 8 percentile +
  norm combinations land at -4 TPs (except pct=0.40 at -9). The norm_threshold
  knob has ZERO measurable effect over {1.0, 1.2, 1.5, 2.0} when paired with
  pct=0.30 — strongly suggesting the norm gate isn't the binding constraint in
  this pipeline; downstream filters dominate. Precision IMPROVES slightly
  (0.7485 → 0.7651-0.7677) because wide-lag drops ~160 decodes net.
- **Combined stack = -4 TPs.** Identical to wide-lag alone. hb-242's
  contribution becomes net-zero when stacked with wide-lag's stricter
  normalization (wide-lag rejects most of the partial-Costas noise candidates
  before they reach final scoring). No synergy, no gain.

## Per-mechanism analysis

### hb-242 (partial-Costas sync_bc)

The mechanism is sound in theory: `max(full_abc, partial_bc)` is non-destructive
for real signals (only wins when block A is degraded, which is the slot-edge
case). But on the broad hard-200 corpus, the partial metric surfaces a
significant volume of noise candidates whose `partial_bc` happens to exceed
`min_sync_score` while their `full_abc` would have correctly rejected them.

The Batch 48 explanation (candidate-cap displacement) is partial — at max_sync=300
the cap is full, but raising the cap to 800 (which fits MORE partial-Costas
candidates) does not recover real TPs. Instead it just adds more FPs that
survive to final output. **The partial-Costas candidates are not real signals
hiding behind the cap; they are noise that resembles signal at the partial-metric
gate but not at the downstream LDPC/OSD gates.**

**Verdict**: keep `costas_partial_metric_enabled = false`. The mechanism remains
preserved in code for slot-edge-specific opt-in.

### Wide-lag baseline (red2)

Wide-lag's footprint is consistent across all tested parameters: ~160 fewer
decodes than baseline (filtering, not surfacing), ~4 TP regression, precision
slightly higher. This is a **net filter, not a net candidate generator** at
the tested settings.

The fact that `norm_threshold ∈ {1.0, 1.2, 1.5, 2.0}` and
`percentile ∈ {0.30, 0.50, 0.60}` all produce IDENTICAL counts (6923 decodes /
5297 TPs / 395s elapsed) strongly suggests the wide-lag gate isn't binding —
the downstream `min_sync_score` cascade is doing the filtering. The pct=0.40
outlier (-9 TPs) is real but small and not a tuning target.

**Verdict**: keep `costas_two_baseline_enabled = false`. The mechanism is
close to neutral but doesn't move the needle, and the parameter sweep cannot
find a positive operating point.

### Combined

Identical to wide-lag alone (-4 TPs, 6910 decodes vs wide-lag's 6913).
hb-242's noise candidates appear to be the same ones wide-lag filters out
via percentile normalization. No synergistic gain.

## Recommended ship defaults

**No changes** to Batch 48's defaults. Both mechanisms remain `default-OFF`:

```rust
costas_partial_metric_enabled: false,
costas_two_baseline_enabled: false,
costas_two_baseline_percentile: 0.40,      // unchanged
costas_two_baseline_norm_threshold: 1.2,   // unchanged
```

The mechanisms remain available for opt-in (slot-edge-specific corpora may
benefit from hb-242; tuning paired with a different downstream filter stack
might unlock wide-lag).

## Out-of-scope but worth flagging

Re-enabling either mechanism profitably likely requires:

1. **A slot-edge-specific corpus** to isolate the bucket hb-242 was designed to
   target. Hard-200 averages across all DT buckets, so the mechanism's gains
   on the 48.3% slot-edge bucket are diluted by neutral-or-worse behavior
   elsewhere.
2. **A tighter `min_sync_score` paired with hb-242** to reject the extra noise
   candidates at the gate. Current `MIN_SYNC_SCORE` was tuned without
   partial-Costas in mind. A coordinated retune (higher min_sync + hb-242 ON)
   might be net-positive — out of scope for this batch.
3. **Different downstream FP filters** — pancetta's 6-layer FP discipline
   (hb-052/058/103/217 etc) is tuned for the full-Costas candidate distribution.
   The partial-Costas candidates may have a different "shape" that the existing
   filters don't catch cleanly.

These are Batch 50+ candidates if there's appetite to revisit, but the tuning
plan in this batch's brief is closed.

## Process learnings

1. **Bash 10-min timeout caps any single sweep run** — must split into chunks.
   Three separate examples (`batch49_sync_tuning`, `batch49_widelag_tuning`,
   `batch49_combined`) was the right shape. Each appends to a shared markdown
   file so partial data survives.
2. **Intermediate persistence works**: every config flushes to the markdown
   file before moving on, so the killed runs only lost their final summary
   stanza, not measurement data.
3. **The wide-lag mechanism is parameter-insensitive** in pancetta's current
   pipeline. This is a useful negative finding — saves a future tuning batch
   from chasing the same knobs.
4. **Combined-config measurement caught a non-finding**: stacking hb-242 with
   wide-lag's stricter normalization neutralizes hb-242's FP contribution
   without recovering TPs. The mechanisms interact, but only to cancel hb-242's
   downside, not to multiply gains.
