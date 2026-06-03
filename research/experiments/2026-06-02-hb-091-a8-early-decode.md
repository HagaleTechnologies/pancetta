# hb-091 — a8-style early-decode latency reduction (Session 1, PROCEED)

**Date**: 2026-06-02
**Branch**: iter/2026-06-02-hb-091
**Status**: **SESSION-1-COMPLETE — PROCEED to Session 2 (production wiring)**
**Effort**: ~1 hour (single diagnostic, 12-min hard-200 eval)

## Hypothesis

WSJT-X-Improved v3.x ships "a8" decoding technology that displays the
in-QSO partner's message ~0.5-1s earlier than the standard 15s slot
boundary (DG2YCB release notes, v3.0.0 250924). For pancetta's autonomous
coordinator, shaving 0.5-1s off each QSO leg's turnaround latency would
plausibly raise QSOs/hour under fast-fade and QSB conditions.

Bank entry: `research/hypothesis_bank.md` (hb-091, priority 0.42, spawned
2026-05-31 from mr-008).

The bank entry frames Session 1 as "design"; here we tighten that to a
**feasibility kill-switch**: before any production scoping logic is
designed, measure the sensitivity hit that pancetta's decoder takes when
fed only the first N seconds of the slot. If the cutoff costs too many
decodes, the operational QSO/hr gain can't survive the recall regression
even if everything else (scoped freq_bin, coordinator wiring) works
perfectly. So Session 1 = recall-vs-truncation curve on real-world WAVs;
PROCEED gate before any plumbing work.

## Method

For each WAV in `research/corpus/curated/ft8/hard_200.manifest.json` (200
real off-air recordings; aggregate 8853 jt9 truths):
- load 12 kHz mono samples
- truncate the buffer to 5 lengths: **13.0, 13.5, 14.0, 14.5, 15.0 s**
  (truncation only — no zero-padding back to 15s; the decoder accepts
  any buffer ≥ 12.64s = the FT8 transmission length).
- run `pancetta_ft8::Ft8Decoder::new(Ft8Config::default()).decode_window(&buf)`
  on each truncation independently (production-default config, no a8
  scoping logic — this measures the **full** search at the truncated
  length, which is an **upper bound** on a8's actual recall loss because
  the production a8 path will search a narrower freq_bin window).
- count how many jt9 truth strings appear in our `text.trim()` output.

Bootstrap 95% CI on the per-WAV delta `recovered(cutoff) − recovered(15s)`
via `pancetta_research::bootstrap_recall_delta` (N=1000, seed=091_2026_06_02).

Diagnostic: `pancetta-research/examples/hb091_early_decode_diagnostic.rs`.
Smoke: `--max-wavs 5`. Full hard-200: no args.

## PROCEED gate

- retention(14.0s) ≥ 95% of recall(15.0s), AND
- 95% bootstrap CI on Δ(14.0s − 15.0s) excludes a > 5% loss
  (i.e. `ci_low > −0.05 × baseline_recovered`).

5% loss on hard-200 = ~247 decodes. Rationale: a8 buys at most 1-2s of
latency. Anything worse than 5% loss would not survive an autonomous
QSO/hr comparison.

## Results

Full hard-200 run, 12.2 minutes elapsed wall, baseline_recovered = 4942
of 8853 truths (55.82% recall — matches main.json's
`curated-hard-200.recall` exactly, confirming the diagnostic reproduces
production behavior at the 15.0s cutoff).

```
  cutoff  recovered     truths     recall    retention      CI(95% Δ)
    13.0       4503       8853    0.5086       91.12% [-495.0,-381.0]
    13.5       4683       8853    0.5290       94.76% [-304.0,-213.0]
    14.0       4830       8853    0.5456       97.73% [-141.0, -80.0]
    14.5       4907       8853    0.5543       99.29% [ -56.0, -14.0]
    15.0       4942       8853    0.5582      100.00% [  +0.0,  +0.0]
```

**At the canonical a8 sweet-spot (1.0s early = 14.0s cutoff):**
- recall: 54.56% (vs 55.82% baseline)
- retention: **97.73%** — clears the 95% bar by 2.7 percentage points
- Δ = **−112 decodes** (95% CI [−141, −80])
- CI_low (−141) is well above the catastrophic-loss floor (−247.1).
- Result is **statistically significant** (CI excludes 0).

**At 0.5s early (14.5s cutoff):**
- retention: 99.29%
- Δ = −35 decodes (CI [−56, −14])

**At 2.0s early (13.0s cutoff):**
- retention: 91.12% — **below** the 95% bar.
- Confirms cutoff sensitivity has a knee around 2s; the bank entry's
  "0.5-1s earlier" framing matches the cliff edge.

## Decision: **PROCEED to Session 2**

The 1.0s early cutoff retains 97.73% of full-slot recall — well above the
95% bar — with a bootstrap CI that comfortably excludes the 5% loss
floor. The recall hit is real (Δ = −112, p < 0.05 by bootstrap) but
small enough to be plausibly net-positive on QSO/hr if the latency gain
materializes in the autonomous loop.

The "1s early" claim from the WSJT-X-Improved release notes is consistent
with the inflection point in the recall curve: between 13.5 and 14.0s
retention jumps from 94.76% to 97.73%, suggesting the FT8 LDPC code has
≥ 1s of redundancy budget against tail truncation in the typical hard-200
SNR distribution.

The 2.0s case (91.12% retention) sits below the bar — Session 2 should
**not** target 13.0s scoped decode in production.

## Session 2 scope (production wiring)

Bank notes already sketch the production path. With Session 1 PROCEED in
hand, Session 2 is:

1. **Decoder primitive**: add `decode_window_scoped(samples, freq_bin_range)`
   to `pancetta-ft8::Ft8Decoder`. The scoped variant restricts the
   Costas sync search to the supplied freq_bin range (±10 Hz around the
   in-QSO partner's last known freq). Keeps all other defaults
   (multipass, OSD, etc.). API design: take `Option<RangeInclusive<usize>>`
   on the existing `decode_window_with_ap` to avoid surface area bloat.

2. **Coordinator hot-path**: when `activeQso` is set, emit a partial
   buffer at t=13.0s into the slot (≈ −1.0s vs full window). Tag the
   buffer with the partner's last `freq_bin` (already tracked in
   `QsoManager`). Hand it to the scoped decoder. If a partner-relevant
   message decodes, advance the QSO state machine immediately; otherwise
   the standard t=15s full-window decode runs as today.

3. **A/B target**: pancetta loopback infrastructure simulates QSO
   sequences. Extend `loopback_qso` (`pancetta/tests/loopback_qso.rs`)
   to vary partner fade timing and run with vs without a8. Measure mean
   turnaround time + QSOs/hour. PROCEED to production-default-on if
   QSOs/hr improves by ≥ 10% under any tested fade scenario; otherwise
   SHELVE the feature but preserve the primitive (gated default-off).

4. **Recall regression check**: the scoped decode is **additive** in
   production — it runs at t=13s **alongside** the standard t=15s pass.
   If the scoped pass misses, the standard pass still recovers the
   decode. So Session 2 has no recall risk to hard-200 / hard-1000 in
   the additive design. The composite gate is automatic.

Session 2's expected effort: 1-2 sessions (decoder primitive + coordinator
plumbing + loopback A/B).

## Artifacts

- `pancetta-research/examples/hb091_early_decode_diagnostic.rs` — diagnostic
- `research/experiments/2026-06-02-hb-091-a8-early-decode.md` — this journal
- `research/hypothesis_bank.md` (hb-091, status → SESSION-1-COMPLETE-SESSION-2-PENDING)

## Lessons

1. **Truncation diagnostic vs. synthetic-SNR sweep**: the Batch 18 zombie
   draft of this diagnostic used controlled synthetic signals with a
   DT × SNR grid. That measures sensitivity at known SNR but not on the
   real-world WAV distribution. The truncation-on-real-WAV approach
   used here is what the bank actually needs — hard-200's noise +
   fading distribution is what production sees, and the headline recall
   numbers are directly comparable to main.json.

2. **The decoder accepts buffers ≥ 12.64s without zero-padding**: the
   internal min_samples check is `protocol_params.total_samples(12000)`
   = 151_680, not 180_000. So truncation to 13s (156_000) "just works"
   — the FFT bins shrink slightly and the spectrogram is a hair shorter,
   but no preprocessing changes were needed. This makes the production
   wiring in Session 2 cheap.

3. **Recall curve has a 1s knee, matching the release-notes claim**: the
   FT8 LDPC code has ~1s of tail-redundancy budget at the hard-200 SNR
   distribution. Beyond that (the 13.0s cutoff) recall falls off
   linearly; below it (14.0+s) loss is bounded. The WSJT-X-Improved
   "0.5-1s earlier" range is well-calibrated to the inflection point.

4. **Bootstrap CI policy compliance**: Δ at 14.0s is small (−112 of
   4942, −2.27%) but the bootstrap CI excludes 0 (CI = [−141, −80]) so
   the loss IS real, just small. Reporting CI alongside the headline Δ
   prevents the "noise-floor mistake" that motivated the Phase B
   bootstrap-CI infrastructure (`research/experiments/2026-06-01-phase-b-bootstrap-ci.md`).
