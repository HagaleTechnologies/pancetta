# Algorithm spec: JTDX relaxed Costas sync threshold near QSO partner

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File path (for traceability, NOT to be quoted): `lib/sync8.f90`
  (candidate acceptance loop, two locations: the within-pass
  filtering loop and the final candidate ordering loop)
- License: GPL-3.0
- Reader date: 2026-06-05

## Purpose

A small frequency window (±3 Hz) around the active QSO partner is
the *only* place in the band where the operator already has high
prior probability that a useful signal exists and where any decode
failure has direct operational cost (the QSO stalls). JTDX exploits
this by accepting any Costas-sync candidate inside that window even
when its sync score would fail the regular per-pass threshold. The
relaxed threshold is a constant 1.1 regardless of which of the three
magnitude metrics is active (RSS at 1.225, power at 1.5, or L1 at
1.1).

This is complementary to the band-collapse mechanism described in
`spec-jtdx-qso-partner-filter.md`. Band-collapse drops candidates
outside the partner band entirely; this spec instead leaves the wide
band active but applies a different (lower) acceptance threshold to
the narrow window around the partner. They can run together (JTDX's
filter mode does both: it collapses the band *and* still applies the
near-partner relaxation inside it) or separately. The relaxed-
threshold mechanism is the one that survives even when the operator
keeps the filter toggle off.

## Algorithm description (PROSE ONLY)

### Inputs

- A per-bin normalised Costas sync score `red[i]` indexed by
  frequency bin `i`. Already normalised by the 40th-percentile
  baseline computed over the wide search band.
- The narrow search band `[ia, ib]` in bin indices.
- The QSO partner audio frequency `nfqso` in Hz.
- The bin spacing `df = 3.125 Hz`.
- A per-pass acceptance threshold `syncmin` (1.225 for RSS pass,
  1.5 for power pass, 1.1 for L1 pass — see
  `spec-jtdx-3method-sweep.md`).

### Outputs

- A filtered candidate list, with relaxed acceptance for bins near
  `nfqso`.

### Steps

1. **Iterate bins in decreasing sync order** within `[ia, ib]`.
2. **For each candidate bin `n`**, compute its audio freq
   `freq = n · df`.
3. **Branch on partner distance:**
   - If `|freq - nfqso| > 3.0 Hz`: apply the **regular** threshold —
     reject if `red[n] < syncmin`.
   - If `|freq - nfqso| ≤ 3.0 Hz`: apply the **relaxed** threshold —
     reject if `red[n] < 1.1`.
4. Otherwise, accept and add to the candidate buffer (with the
   regular DT-range check, near-duplicate suppression, etc.).
5. The same partner-distance branch runs a second time in the final
   candidate ordering loop, with an additional rule: candidates in
   the near-partner window are *promoted* to the top of the
   candidate list (so the inner FT8 decoder works them first, which
   matters when the candidate cap of 460 would otherwise truncate
   the list).

### Promotion to the top of the list

After the per-bin filtering pass, JTDX runs the final ordering loop:

1. First loop: iterate all surviving candidates in decreasing sync
   order; for any candidate within ±3 Hz of `nfqso` with `red ≥ 1.1`,
   copy it to the front of the output list. A within-window dedup
   keeps only one near-partner candidate per 3 Hz block (so multi-
   ple bins resolving to the same partner tone don't all get
   promoted).
2. Second loop: iterate again, this time applying the partner-
   distance threshold branch (regular `syncmin` outside, `1.1`
   inside) and appending all remaining survivors. The 460-candidate
   cap can truncate this second loop, but the near-partner
   candidates from the first loop are already safe at the front.

### Interaction with the three magnitude metrics

The relaxed threshold of 1.1 is the *same* across all three passes
of the three-method sweep (RSS at 1.225 normally, power at 1.5
normally, L1 at 1.1 normally). For the L1 pass, the regular and
relaxed thresholds are identical, so the near-partner branch is a
no-op. For the RSS and power passes, the relaxed branch produces
extra candidates that would otherwise be missed.

Net effect: within ±3 Hz of `nfqso`, the effective threshold is
`min(1.1, syncmin_for_method) = 1.1` for every pass.

## Numerical constants (facts, not expression)

- Near-partner half-window: **3.0 Hz** (≈ one frequency bin at
  `df = 3.125 Hz`).
- Relaxed sync threshold: **1.1** (always, regardless of method).
- Promotion dedup window inside the relaxed zone: 3 Hz (so at most
  one promoted candidate per partner tone).
- The promotion loop tracks a `fprev` state initialised to 5004 Hz
  (a sentinel above the FT8 audio passband) so the first candidate
  is always accepted; subsequent candidates within 3 Hz of `fprev`
  are skipped.

## Edge cases

- **No `nfqso` set** (no QSO in flight). The relaxed-threshold
  branch never fires because `|freq - nfqso| > 3.0` is true for the
  entire band. Behaviour matches the wide-band path exactly.
- **`nfqso` outside `[nfa, nfb]`.** Same as above — no relaxed
  branch fires.
- **Multiple candidates within ±3 Hz.** The 3 Hz dedup in the
  promotion loop keeps only the highest-sync one. This is *different*
  from the regular 4 Hz / 0.1 s near-dupe suppression, which is
  explicitly *bypassed* in the near-partner zone (see
  `spec-jtdx-3method-sweep.md` — the line `abs(candidate0(1,i) -
  nfqso) > 3.0` is part of the dedup condition).
- **`lqsothread` mode** (QSO partner is on a parity slot we own).
  JTDX additionally inserts two "virtual" candidates at the partner
  freq with DTs of ±5 s. These are unconditionally accepted (sync
  score zero) so that the FT8S subroutine in the inner decoder can
  try a decode at the partner freq even when no Costas sync was
  found. Related to spec-jtdx-qso-partner-filter.md, not strictly
  part of this spec, but worth flagging.

## Conflict with pancetta's existing mechanisms

Pancetta's FT8 coarse search currently has no notion of a "preferred
frequency". The Costas sync threshold (`min_score` in the coarse
search) is a single value applied uniformly across the search band.

What's missing:

1. A way to communicate "the partner audio freq is X Hz" from
   pancetta-qso → pancetta → pancetta-ft8 at decode time. This is
   the same plumbing as `spec-jtdx-qso-partner-filter.md` needs — a
   QSO-state observer on the coordinator that sets a per-decode
   parameter on the FT8 invocation.
2. A `relaxed_sync_window_hz` config knob (default 3.0) and a
   `relaxed_sync_threshold` config knob (default to the lowest of
   the active per-pass thresholds, e.g. ~1.1 normalised).
3. Logic in the coarse-search acceptance loop that switches
   threshold based on `|freq - preferred_freq| < relaxed_window`.

Compatibility with hb-217 (RR73 fix): hb-217 lives at the parser
layer, not the sync layer. The relaxed-near-partner mechanism is a
*coverage* fix at the sync layer — it ensures the candidate reaches
the parser at all. For weak partner RR73s where capture-effect or
threshold rejection is the bottleneck, this spec is the direct fix.
hb-217's parser-level synthesis still helps when the candidate is
demodulated but mis-classified; the two stack.

Compatibility with hb-218 (capture-effect joint decode): hb-218
addresses the case where a strong companion within ±25 Hz of the
truth blocks the sync. The relaxed-near-partner mechanism does *not*
help in that case — capture-effect blocks sync regardless of
threshold. The two mechanisms target different failure modes and
both need to land for full RR73 coverage.

## Estimated Rust port effort

- ~30 LOC to plumb `preferred_freq_hz: Option<f32>` and
  `relaxed_sync_threshold: f32` through the FT8 coarse-search API.
- ~20 LOC in the candidate acceptance loop to apply the branch.
- ~40 LOC for the "promote to front" ordering loop. (Or skip this
  initially; the threshold branch alone captures most of the win.)
- ~40 LOC of tests: weak signal at partner freq decodes with
  preferred_freq set and fails without it.
- ~30 LOC in the coordinator to thread QSO state into decode
  parameters (shares plumbing with spec-jtdx-qso-partner-filter.md).
- 1 implementation session.

Total: ~160 LOC, 1 session, low risk. If
`spec-jtdx-qso-partner-filter.md` lands first, the coordinator
plumbing is free.

## Implementation notes for the implementer thread

- The pancetta-ft8 coarse search builds a candidate list keyed by
  `(freq_bin, time_offset, sync_score)`. The acceptance check
  currently looks something like
  `if sync_score >= min_score && time_offset in range`. Add a
  branch on `preferred_freq_hz`:
    `let threshold = if let Some(p) = preferred_freq_hz {
        if (freq - p).abs() <= relaxed_window_hz { relaxed_threshold }
        else { min_score }
    } else { min_score };`
- The relaxed threshold default of 1.1 is for *JTDX's* normalised
  sync scale (per-bin sync / 40th-percentile baseline). Pancetta's
  scoring is currently raw Costas correlation, not baseline-
  normalised. **Empirical recalibration is required.** Concretely:
  collect normalised-vs-raw sync scores on a known partner signal at
  marginal SNR, find the score level corresponding to JTDX's 1.1,
  and use that as the pancetta default.
- The "promote to top" loop is optional in v1. Pancetta's coarse
  search returns a list bounded by a candidate cap; if the cap is
  much larger than typical candidate count, promotion does not
  matter. Check the current cap before deciding whether to port the
  promotion logic.
- The QSO-state observer for `preferred_freq_hz` is the same shape
  as the one for `freq_bin_range` (the band-collapse spec). They
  should be the same module: a `QsoAwareDecodeParams` struct that
  exposes both fields and is set by the coordinator on QSO state
  changes.
- Test invariant: with `preferred_freq_hz = Some(800.0)` and a
  marginal signal at 800 Hz that fails the regular threshold by a
  small margin, the decoder must produce the signal. Without
  `preferred_freq_hz`, it must not.
- Caution: the relaxed threshold can produce FPs in the partner
  window. Pair this with hb-058's `/R`-suffix filter, hb-103's
  content-score gate, and the existing trust-set check at the
  message layer — any high-FP candidate is filtered downstream by
  the cold-start / trust-set mechanisms already shipped. Do not
  pre-emptively widen the relaxed threshold beyond JTDX's 1.1; that
  number is already aggressive.
