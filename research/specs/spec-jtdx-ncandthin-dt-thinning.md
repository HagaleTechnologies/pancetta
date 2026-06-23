# Algorithm spec: JTDX ncandthin DT-weighted candidate thinning

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File paths (for traceability, NOT to be quoted):
  - `lib/sync8.f90` (the `rcandthin` / `dtcenter` weighting at
    ~lines 17-18, 175-179, 199-205, 233-236, 276-286)
  - `lib/ft8_decode.f90` (where `ncandthin` and `ndtcenter` are
    threaded into the per-pass `sync8` call: line 215)
  - `mainwindow.cpp` and `mainwindow.ui` (UI control:
    `candListSpinBox`, `DTCenterSpinBox`)
- License: GPL-3.0
- Reader date: 2026-06-08

**Status note**: this spec is included **for reference and to make
the anti-recommendation explicit**. Pancetta has a documented
*negative-DT (slot-edge) hole*: at slot edges (negative DT), recall
drops to 48.3 % on the hard-200 corpus (vs ~67 % average; see
MEMORY.md → batch 33 status). The JTDX `ncandthin` mechanism is a
*sort key that biases candidates toward a chosen DT center*. Adopting
it without an extremely wide `dtcenter` setting would directly
worsen pancetta's slot-edge hole. **Do not port this mechanism**.

## Purpose

JTDX has a candidate-list-thinning option for operators with slow
CPUs in wideband decode mode. The operator picks two settings:

- `ncandthin` (UI: "CL" / "Candidate List Thinning", range 5-100 %,
  step 5, default 100): when below 100, candidates are weighted by
  a DT bias function before being sorted, and the bottom portion of
  the resulting sort is *dropped* before the inner decoder ever sees
  them.
- `ndtcenter` (UI: "DT Center", range -2.0 to +2.5 s, step 0.1 s):
  the DT value around which the weighting is centred. The operator
  is expected to set this to the median DT of their band's decoded
  signals — typically near 0 for well-synchronised operators, but
  potentially nonzero in overcrowded conditions or when clock drift
  is present.

When `ncandthin = 100` (the default), the weighting is bypassed and
candidates are sorted by raw sync. When `ncandthin < 100`, the
weighting kicks in.

## Algorithm description (PROSE ONLY)

### Inputs

- `ncandthin`: integer, percent. Internally converted to a
  multiplier `rcandthin = ncandthin / 100.0`.
- `ndtcenter`: integer, scaled by 100. Internally converted to
  `dtcenter = ndtcenter / 100.0` (a DT in seconds).
- The per-candidate `(freq, xdt, sync)` tuples already collected by
  `sync8`.
- The `filter` flag (a global "narrowband filter" mode): when true,
  `rcandthin` is increased to `min(rcandthin * 3.0, 1.0)` — i.e.
  narrowband filter mode aggressively *disables* thinning by pushing
  the multiplier toward 1.0.
- `ipass`: the current pass index ∈ {1..9}. Different passes scale
  `rcandthin` differently.

### Outputs

- A reduced candidate list, sorted by a DT-weighted sync score
  instead of raw sync. The bottom `(1 - rcandthin) * 100 %` of
  candidates is discarded (after rebuilding `rcandthin` with the
  per-pass scaling at the end of `sync8`).

### Steps — DT weighting

For each accepted candidate `k` in the raw candidate buffer, compute
a weighted score in field `candidate0(5, k)`:

- **Passes 1, 4, 7** (the RSS-magnitude passes; these passes set
  `lpass1 = .true.`): use a linear DT weight:
  `score_weighted = sync / (|xdt - dtcenter| + 1.0)`.
- **Passes 2, 5, 8** (the power-magnitude passes; these set
  `lpass2 = .true.`): use a quadratic DT weight:
  `score_weighted = sync / (|xdt - dtcenter| + 1.0)^2`.
- **Passes 3, 6, 9** (the L1-magnitude passes): no special weighting
  set in the inner loop (the `lpass1` / `lpass2` flags are both
  false). The weighting field is populated only when either flag is
  true.

The `+1.0` in the denominator prevents division by zero when
`xdt == dtcenter` exactly. Candidates at the centre get weight 1
(linear) or 1 (quadratic); candidates 1 second off centre get
weight 0.5 (linear) or 0.25 (quadratic); candidates 5 seconds off
centre get weight 0.167 (linear) or 0.028 (quadratic) — i.e. they
are effectively erased from the sort.

### Steps — sort and drop

After all candidates are collected and the per-pass weighting field
is populated, the sort key swaps:

- When `rcandthin > 0.99` (default, no thinning): sort by raw
  `candidate0(3, k)` (sync).
- When `rcandthin ≤ 0.99` (thinning active): sort by
  `candidate0(5, k)` (the DT-weighted score).

Then the near-`nfqso` candidates are extracted and placed at the
head of the output buffer (those are NOT subject to thinning — they
get the relaxed-sync 1.1 threshold and bypass the DT weighting).

The bulk-of-band candidates are then appended in sort order, capped
at 460 total.

### Steps — per-pass rcandthin scaling

Before the final drop, `rcandthin` is rescaled by the pass type:

- Passes 1, 4, 7 (RSS): `rcandthin = min(rcandthin * 1.27, 1.0)`.
- Passes 2, 5, 8 (power): if `rcandthin > 0.79`, `rcandthin =
  rcandthin^2`; else `rcandthin = rcandthin * 0.79`.
- Passes 3, 6, 9 (L1): `rcandthin = min(rcandthin * 5.0, 1.0)`.

These scalings calibrate the drop fraction per pass to the number
of candidates each metric produces. RSS produces a moderate count
and gets a mild softening (×1.27). Power produces fewer (heavier
tails, sparser sync hits) and gets a *tightening* (square in the
high-density range). L1 produces many low-quality hits and gets
aggressive softening (×5.0) — effectively L1 is barely thinned.

The final drop is:

`ncand = ncandfqso + round((ncand - ncandfqso) * rcandthin)`

i.e. keep all near-QSO candidates verbatim, drop `(1 - rcandthin)`
of the rest.

### Numerical constants (facts, not expression)

- `ncandthin` UI range: 5 to 100 percent, step 5, default 100.
- `ndtcenter` UI range: -2.0 to +2.5 seconds, step 0.1.
- Filter-mode `rcandthin` boost: `× 3.0`.
- Linear DT weight: `sync / (|xdt - dtcenter| + 1.0)`.
- Quadratic DT weight: `sync / (|xdt - dtcenter| + 1.0)^2`.
- Per-pass rescaling: RSS ×1.27, Power × × (square in high range),
  L1 × 5.0.
- Near-QSO zone exemption: `|freq - nfqso| ≤ 3.0 Hz` candidates are
  never thinned.
- The default `ncandthin = 100` corresponds to `rcandthin = 1.00`
  which bypasses every step above — the entire thinning pathway is
  guarded by `if (rcandthin < 0.99) ...`.

### Edge cases

- When the operator's clock has drifted, signals on the air arrive
  at a consistent non-zero DT. Setting `dtcenter` to that DT
  preserves them under thinning; leaving it at 0 erases them.
- When the band is overcrowded with split-second-late stations
  (typical EU contest), median DT can be ~0.4-0.6 s; the operator
  is expected to tune `dtcenter` upward to compensate.
- For passes 3, 6, 9 the weighting is not applied — but the rescaled
  `rcandthin` still drops the bottom 1-rcandthin*5.0 of the L1 list.
  Because the rescale is ×5.0 (very aggressive softening), most L1
  candidates survive.
- The mechanism is only effective when `ncandthin < 100` — at the
  default it is a no-op. Operators who never touch the UI never see
  the mechanism activate.
- Narrowband filter mode triples `rcandthin` at function entry,
  meaning operators using the filter effectively get little or no
  thinning even with low `ncandthin` values. This is intentional —
  filter mode already pre-restricts the candidate pool.
- The `ndtcenter` is per-decoder-instance, not per-candidate. If
  the operator has a half-band of late stations and a half-band of
  on-time stations, the single `dtcenter` value must compromise.

## Conflict with pancetta's existing mechanisms

This is where the anti-recommendation lands. Pancetta's documented
*slot-edge hole* (negative-DT recall = 48.3 % vs corpus average
~67 %, MEMORY.md batch 33) means a meaningful fraction of pancetta's
real signal recall comes from candidates with `xdt < 0` (transmitted
late, or arriving late from a partner whose clock is slow).

The JTDX thinning mechanism's quadratic weight at distance 2 s from
centre = 1/9 = 0.111. At distance 4 s = 1/25 = 0.04. A signal at
`xdt = -2 s` with `dtcenter = 0` gets only 11 % of its raw sync as
its weighted score; in a candidate list with 200 entries and
`rcandthin = 0.5` (drop the bottom 50 %), that signal is almost
certain to be dropped.

Specifically:

1. **Default-safe**: `ncandthin = 100` (the JTDX default) bypasses
   the mechanism. Pancetta should default to the equivalent and
   never set the multiplier below 1.0.
2. **Operator-toggleable but anti-recommended**: even if pancetta
   added a `ncandthin` config knob for parity with JTDX, the right
   default would be 100, and the documentation should warn that
   lowering it will erase slot-edge decodes.
3. **CPU-budgeting alternative**: pancetta's preferred CPU-budgeting
   path is hb-216 hardware-tier classification, which adjusts
   `max_decode_passes` and `osd_depth` — *not* the candidate count.
   That mechanism trades the *depth* of decode per candidate, not
   the *breadth* of candidates considered. It does not interact
   negatively with slot-edge.
4. **If ever ported with a wide enough dtcenter range**: the JTDX
   UI caps `dtcenter` at +2.5 s. Pancetta's slot-edge truths exist
   at DTs out to roughly ±4 s (in the SWL extended search window).
   The JTDX UI cap would *prevent* an operator from setting the
   centre wide enough to cover the slot-edge zone on either side.
   A pancetta port would have to extend the UI cap *and* shift to
   a uniform-weight mode in the slot-edge zone — at which point the
   mechanism has been so heavily modified that the residual value
   is unclear.

## Why anti-recommended

Three independent reasons:

1. **Direct conflict with pancetta's documented hole.** The
   mechanism's central assumption (signals cluster around `dtcenter`)
   is true for *most* operators *most* of the time, but pancetta's
   recall improvement opportunity is specifically the
   non-clustered tail. Adopting the mechanism would erase the
   opportunity.
2. **Wrong-tool-for-the-problem.** Pancetta's CPU bottleneck is in
   the inner LDPC + OSD cascade, not in the candidate count.
   hb-091 + hb-216 (scoped fast path + tier-driven decoder config)
   are the correct CPU controls; they reduce per-candidate cost
   without touching candidate breadth.
3. **No FP-reduction value.** JTDX's mechanism exists for CPU
   triage, not for FP reduction. Pancetta's FP rate is already low
   (cumulative FP reductions across batches 24-33 = ~55 % per
   MEMORY.md). The mechanism would not help with FPs and would
   actively hurt recall.

## Estimated Rust port effort

(Only counted because the spec was asked for; not recommended.)

- ~50 LOC for the config knob.
- ~80 LOC for the DT weighting calculation, plumbed into
  pancetta-ft8's coarse-search output.
- ~30 LOC for the per-pass rescaling.
- ~100 LOC of tests.
- 1 session.

Total: ~260 LOC, 1 session, *anti-recommended*.

## Implementation notes for the implementer thread

- **Do not port this spec.** It is included only for reference and to
  document the JTDX precedent for CPU-budgeted candidate thinning.
- If a future operator request asks for parity with JTDX's "CL"
  setting, default to 100 (no-op) and surface the spec's
  anti-recommendation in the help text. The right answer is almost
  always to lower `max_decode_passes` (hb-216 Slow tier) instead.
- The one piece of useful precedent buried in the mechanism: the
  *sort key swap* between raw sync and DT-weighted score is a model
  for any pancetta mechanism that wants to bias the inner-decode
  order. A future variant of pancetta might want to *prioritise*
  (not eliminate) candidates near `dtcenter` to get an earlier
  exit-on-success in the inner loop. That variant — priority
  reordering with no drop — has none of the anti-recommendation
  baggage and would be a useful CPU-budgeting tool. If considering
  that variant, separate it into a new hypothesis (e.g. an hb-300
  series on inner-decode order optimisation) and do NOT carry over
  the `rcandthin < 1.0` drop semantics.
- Cross-reference `spec-jtdx-3method-sweep.md`: the per-pass
  rescaling (×1.27 / square / ×5.0) is calibrated to the candidate
  counts from each of the three magnitude metrics. If pancetta ever
  adopts the 3-method sweep, those rescaling factors are not
  directly portable — they depend on the absolute count of
  candidates each metric produces, which will differ for pancetta's
  dB-power baseline metric.
