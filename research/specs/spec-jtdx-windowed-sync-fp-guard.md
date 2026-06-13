# Algorithm spec: JTDX windowed per-symbol sync false-decode guard

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File paths (for traceability, NOT to be quoted):
  - `lib/ft8b.f90` (the per-symbol sync scoring at lines ~301-355
    and the cascading rejection logic at lines ~414-504)
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

After the coarse sync detector has accepted a candidate and `ft8b`
has extracted the symbol-magnitude matrix `s8(0:7, 1:79)`, JTDX runs
a *secondary* false-decode guard based on how well the matrix's
Costas-arrayed symbols stand above their per-symbol noise floor. The
guard is a tree of `if` cascades that combines two scores:

- `nsyncscore` (and per-Costas-array sub-scores `nsyncscore1`,
  `nsyncscore2`, `nsyncscore3`): the count of Costas tones at each of
  the 21 sync-symbol positions whose `s8(correct_tone, k)` exceeds
  the average of the other 7 tones at position k.
- `nsyncscorew` (the "windowed" or "wide" sync score): the count of
  Costas tones whose summed energy across all three Costas arrays
  exceeds the average across the corresponding sweep of data
  positions.

The guard is conditioned on `rrxdt` (the candidate's DT relative to
the slot midpoint) — three regimes are handled separately because
the relevant sweep of symbols changes when the signal arrived early
or late.

The cascade is *parameter-dense* — each branch has hard-coded
thresholds calibrated to a specific FP/TP operating point JTDX
established empirically. The comments next to each `return` indicate
the raw count of FPs each branch eliminated on a calibration corpus
(e.g. `! 377 out of 20709` = 377 FPs eliminated, calibrated against
a 20 709-decode corpus). This is one of the *most heavily
calibrated* false-decode mechanisms in the JTDX pipeline.

## Algorithm description (PROSE ONLY)

### Inputs

- `s8(0:7, 1:79)`: per-symbol, per-tone magnitude matrix.
- `icos7(0:6)`: the Costas-7 reference pattern (FT8 sync tones).
- `xdt`: candidate DT relative to slot start (0..15 s).
- `rrxdt = xdt - 0.5`: DT relative to slot midpoint.
- `dfqso = |f1 - nfqso|`: candidate distance from QSO partner.
- `stophint`: idle-mode flag (not in active QSO).
- `lcqcand`: candidate is CQ-pattern-tagged.

### Outputs

- `nbadcrc = 1` (rejected) or fall through to the bp/OSD decoder.
- A modified `lapcqonly` flag for downstream AP-cascade behaviour
  (true means only CQ-shaped AP types are permitted on this
  candidate).
- A modified `lskipnotap` flag (true means skip non-AP regular
  decoding paths).

### Steps

#### Phase 1: compute the windowed sync score `nsyncscorew`

Choose the Costas array sweep based on `rrxdt`:

- **Mid-window** (`-0.5 ≤ rrxdt ≤ 2.13`): use all three Costas arrays
  at symbol positions 1-7, 37-43, 73-79.
  - `syncw(tone) = s8(tone, k) + s8(tone, k+36) + s8(tone, k+72)`
    summed across k = 1..7.
  - `sumk(tone) = (sum over all 79 symbols of s8(tone, .) - syncw)
    / 25.333`.
- **Late** (`rrxdt < -0.5`): use only the back two Costas arrays
  (positions 37-43, 73-79).
  - `syncw(tone) = s8(tone, k+36) + s8(tone, k+72)`.
  - `sumk(tone) = (sum over symbols 26-79 - syncw) / 26.0`.
- **Early** (`rrxdt > 2.13`): use only the front two Costas arrays
  (positions 1-7, 37-43).
  - `syncw(tone) = s8(tone, k) + s8(tone, k+36)`.
  - `sumk(tone) = (sum over symbols 1-54 - syncw) / 26.0`.

Then count: `nsyncscorew = number of k where syncw(k) > sumk(k)`.
Range 0..7. Also accumulate `scoreratiow(k) = syncw(k) / sumk(k)`
for downstream use.

#### Phase 2: compute hard-sync counts (`is1`, `is2`, `is3`,
`nsync2`)

For each of the 7 Costas positions in each of the 3 arrays:

- Find the argmax tone at position k. If it equals the correct
  Costas tone, increment `is1` / `is2` / `is3` (per array).
- Otherwise, zero out the argmax and find the 2nd-argmax. If *that*
  equals the correct Costas tone, increment `nsync2`.

Also accumulate `nsyncscore1`, `nsyncscore2`, `nsyncscore3` (count
of positions where correct tone exceeds the per-position 7-tone
average) and corresponding `scoreratio1`, `scoreratio2`,
`scoreratio3` (averages of the ratios where they exceeded).

`nsync = is1 + is2 + is3` (range 0..21).

`nsyncscore = nsyncscore1 + nsyncscore2 + nsyncscore3`.

#### Phase 3: CQ-bail-out (when `lcqcand`)

If the candidate was tagged as CQ-pattern by the coarse-search:

- Compute `rscq` = how strongly the data positions 8-17 match the
  expected `CQ` template (tone-0 at most positions). The exact
  scoring is a sum of 1 (full match) + 0.5 (partial match for known
  ambiguity positions).
- If `nsync == 4`: reject unless `nsync + nsync2 ≥ 12` or
  `rscq ≥ 6.6`. Set `lapcqonly = true`.
- If `nsync == 5`: reject unless `nsync + nsync2 ≥ 12` or
  `rscq ≥ 6.1`. Set `lapcqonly = true`.
- If `nsync == 6`: reject unless `nsync + nsync2 ≥ 11` or
  `rscq ≥ 5.6`. Set `lapcqonly = true`.

If the candidate is not CQ-tagged and `nsync < 7`: reject outright.

#### Phase 4: AP-disable check (`lskipnotap`)

When `nsync < 11` and not `lapcqonly`, walk a "sync distance"
neighbourhood (the `syncdist.f90` include) — count `nsmax(d)` = how
many sync positions have correct-tone at offset d from the argmax.
If the 7-8 distance bin or 5-6 distance bin exceeds the 2-3 bin,
the candidate is structurally suspect: set `lskipnotap = true`,
meaning the AP cascade is the only decode path allowed.

#### Phase 5: cascade of rejection thresholds (when
`dfqso ≥ 2 Hz` or in idle mode)

This is the bulk of the guard. The thresholds depend on the `rrxdt`
regime. Mid-window thresholds:

- `nsyncscore < 8`: reject (377 FPs / 20709 corpus).
- `nsyncscore < 10` and `scoreratio < 5.5`: reject.
- `nsyncscore < 11` and `scoreratio < 3.63`: reject.
- `nsyncscore == 11` and `scoreratio < 5.37`: reject *unless*
  `nsyncscore1 ≥ 5` or `nsyncscore3 ≥ 5` or `scoreratio1 ≥ 4.2` or
  `scoreratio3 ≥ 4.2` (per-Costas-array escape hatches).
- `nsyncscore == 12` and `scoreratio < 4.6`: similar escape hatches.
- `nsyncscore == 13` and `scoreratio < 4.4`: similar.
- Now switch to the `nsyncscorew` regime (the windowed sync score):
  - `nsyncscorew < 3`: pass *only* if any single Costas array has
    `nsyncscoreN > 5` AND `scoreratioN > 13.8`.
  - `nsyncscorew == 3`: pass *only* if any `scoreratioN > 15.0`.
  - `nsyncscorew == 4`: pass if any `nsyncscoreN == 7` OR any
    `scoreratioN > 10.0`.
  - `nsyncscorew == 5`: pass if `nsyncscore > 17` OR any
    `nsyncscoreN == 7` OR any `scoreratioN > 10.0`.

Late-window (rrxdt < -0.5) and early-window (rrxdt > 2.13) regimes
have analogous cascades with tighter thresholds (only 2 Costas
arrays available, so absolute counts are lower).

When inside the QSO-partner zone (`dfqso < 2 Hz` and not idle), the
Phase 5 cascade is **bypassed entirely** — the partner gets the
benefit of the doubt.

### Numerical constants (facts, not expression)

DT regime boundaries:

- Mid-window: `-0.5 ≤ rrxdt ≤ 2.13`.
- Late: `rrxdt < -0.5`.
- Early: `rrxdt > 2.13`.

Phase 1 noise-floor divisors:

- Mid-window: `25.333` ((79 - 3) / 3).
- Late: `26.0` ((54 - 2) / 2).
- Early: `26.0` ((54 - 2) / 2).

Phase 3 CQ thresholds:

- `nsync == 4`: `rscq ≥ 6.6`.
- `nsync == 5`: `rscq ≥ 6.1`.
- `nsync == 6`: `rscq ≥ 5.6`.

Phase 5 mid-window thresholds (selected):

- `nsyncscore < 8`: hard reject.
- `nsyncscore == 11` escape: `nsyncscore1/3 ≥ 5` OR `scoreratio1/3 ≥
  4.2`.
- `nsyncscorew < 3` escape: `scoreratioN > 13.8`.
- `nsyncscorew == 3` escape: `scoreratioN > 15.0`.
- `nsyncscorew == 4-5` escape: `scoreratioN > 10.0`.

Phase 5 late-window thresholds (tighter):

- `nsyncscore < 6`: hard reject.
- `nsyncscore == 8` mid-tier: `scoreratio2/3 ≥ 6.6`.
- `nsyncscore == 9`: `scoreratio2/3 ≥ 6.5/6.6`.

Per-pass FP elimination counts (from comments next to each
`return`): branch-level audit, ranging from `! 2` to `! 377` per
branch, totalling several thousand FPs eliminated on the
calibration corpus.

QSO partner zone (bypass): `dfqso < 2.0 Hz` and not `stophint`.

### Edge cases

- The `rrxdt` regime thresholds (`-0.5`, `+2.13`) correspond to FT8
  symbol boundaries: 0.16 s × 3 ≈ 0.5 s and 0.16 s × 13 ≈ 2.08 s.
  Inside this range, all three Costas arrays fall within the
  signal's expected time window; outside, one of the three is
  partially missing.
- The QSO-partner-zone bypass (`dfqso < 2 Hz`) is **the** dominant
  exception: any candidate near the operator's partner gets to skip
  the entire Phase 5 cascade. This is the source of JTDX's
  asymmetric FP profile near `nfqso` vs the rest of the band.
- The CQ-bail-out (Phase 3) is much more permissive than the
  non-CQ path because CQ template-matching downstream provides
  additional FP filtering. CQ candidates can pass Phase 3 with as
  few as 4 hard syncs.
- The Phase 4 `lskipnotap` flag effectively bans the regular
  (non-AP) decode path on a structurally suspect candidate. It
  forces the candidate through the AP cascade only, which is more
  conservative because AP templates require known callsigns.
- Each `go to 32` skips Phase 5 and jumps directly to the
  next-phase (the AP-cascade entry point). The branch logic is
  essentially "if score is high enough on any one dimension, let
  the candidate through".
- The mechanism is calibrated to JTDX's *upstream* sync detector
  (the 3-method sweep + `syncmin` thresholds). A different upstream
  sync detector (like pancetta's dB-power Costas) produces a
  different `s8` distribution, and these thresholds will need
  recalibration before they have the same FP/TP operating point.

## Conflict with pancetta's existing mechanisms

Pancetta has multiple FP-rejection layers:
- `is_plausible` (hb-024 extended to reject DXpedition/FreeText in
  Batch 32) — message-text-based.
- `CallsignContinuityFilter::accept()` with `/R` and degenerate-grid
  patterns (Batches 30-32) — callsign-pattern based.
- `MessageContentScore` (hb-103, Batch 32) — content-feature based.
- The neural OSD layer.

What pancetta lacks is a per-candidate, *sync-quality*-based
rejection layer of the kind JTDX runs in Phases 3-5. The closest
pancetta analogue is the LDPC + CRC cascade itself, which rejects
on the basis of hard-decision codeword validity. JTDX's mechanism
rejects *before* even invoking LDPC, on the basis that the symbol
matrix doesn't show enough Costas-tone alignment to be worth
decoding.

Two non-trivial considerations for adoption:

1. **The thresholds are calibrated to JTDX's upstream sync
   detector.** Pancetta's coarse-search emits a different `s8`
   distribution. A direct port of the thresholds without
   recalibration would have unpredictable effects — likely
   accidentally rejecting too many real signals at pancetta's
   operating point.
2. **The mechanism's value is in the *cascade structure*, not the
   exact thresholds.** Even if pancetta uses different numerical
   values, the structural pattern (compute `nsyncscore` and
   `nsyncscorew`, gate on `rrxdt` regime, allow escape via
   per-Costas-array high-scoreratio) is a reusable design.

### Compatibility with hb-218 (capture-effect)

The mechanism's QSO-partner-zone bypass (`dfqso < 2 Hz` skips Phase
5) is *opposite* of what hb-218 needs — hb-218 wants more candidates
near the partner to be decoded jointly with the dominant signal.
The bypass is consistent with this: it lets weak signals near the
partner through. Adopting the mechanism would slightly *help* hb-218
by accepting more weak-companion candidates rather than rejecting
them at the sync-quality gate.

## Estimated Rust port effort

- ~80 LOC for the Phase 1 windowed sync score (3 regimes, per-tone
  energy summation).
- ~60 LOC for the Phase 2 hard-sync counts (3 Costas arrays, top-2
  argmax tracking).
- ~40 LOC for Phase 3 (CQ-bail-out with thresholds).
- ~20 LOC for Phase 4 (`lskipnotap` based on sync-distance
  histogram).
- ~250 LOC for the Phase 5 cascade tree — best implemented as a
  table-driven match expression over `(rrxdt_regime, nsync,
  nsyncscore, nsyncscorew, scoreratios)` rather than literal Fortran
  translation.
- ~200 LOC of tests + 1-2 calibration examples to recalibrate the
  thresholds against pancetta's `s8` distribution. *This is the
  expensive part.*
- 3-4 research sessions: one to gather pancetta's
  `(s8_matrix, was_real)` corpus, one to fit thresholds to FP/TP
  operating point, one to wire and ship, optional one to verify
  the QSO-partner-zone bypass interaction with hb-218.

Total: ~650 LOC, 3-4 sessions.

## Implementation notes for the implementer thread

- The mechanism's structural pattern is "pre-LDPC sync-quality
  rejection". Pancetta's current pipeline goes straight from
  coarse-search to LDPC; this would insert a new stage between them.
- Implement as a separate `SyncQualityGuard::evaluate(s8, xdt, ...)`
  returning an enum `{Reject, RejectButTryAp, Pass}`. Keep it
  independent of the rest of the FT8 decoder so it can be A/B-tested
  cleanly via scorecard.
- The Phase 5 cascade is fundamentally a heuristic decision tree
  with ~30 leaves. Encode it as a match expression on a 5-tuple
  rather than translating the Fortran `if-else if-else if` chain
  literally — the result is more readable, more reviewable, and
  easier to mutate during calibration.
- DO NOT port the thresholds verbatim. Treat them as a starting
  point for calibration on pancetta's hard-200 + noise corpus.
  Build a small calibration example that sweeps each threshold
  across ±20 % and reports the FP/TP delta.
- The QSO-partner-zone bypass (`dfqso < 2 Hz`) interacts with
  multiple other specs in this batch: pancetta's relaxed-sync
  spec, the lqsothread virtual-candidate spec, and the lft8subpass
  near-partner `nweak = 2` behaviour. All four mechanisms agree on
  the same near-partner principle: give the partner the benefit of
  the doubt. When combined, they should be consistent — never have
  one of them reject what another would have accepted on the
  partner's frequency.
- Initial ship gate: should not reduce TPs by more than 1 % on
  hard-200 at any FP threshold. Acceptable FP reduction target:
  20-30 % at the chosen operating point. Beyond that, accept that
  pancetta's existing layers (`is_plausible`, hb-103, neural OSD)
  are catching most of what this would catch.
