# Algorithm spec: JTDX three-method magnitude sweep for sync detection

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File path (for traceability, NOT to be quoted): `lib/sync8.f90` and the
  driving pass loop in `lib/ft8_decode.f90`
- License: GPL-3.0
- Reader date: 2026-06-05

## Purpose

JTDX runs the Costas sync search up to nine times per slot (three
"decoding cycles" of three passes each). The novelty in the spec we
care about is that, within each cycle, the three passes do not just
re-use the same magnitude spectrogram with different thresholds — they
compute three *different* magnitude statistics from the same complex
short-time FFT, and run the Costas sync detector independently on each
with a different acceptance threshold. The union of the candidate
lists across the three passes covers signals whose Costas correlation
is strongest under different non-linear compressions of the
spectrogram. The mechanism is orthogonal to multi-pass subtract-and-
re-decode (SIC), so the wins it produces stack with whatever SIC the
host decoder already runs.

## Algorithm description (PROSE ONLY)

### Inputs

- A real audio buffer for one 15 s FT8 slot (12 000 sps, ~180 000
  samples).
- A search band `[nfa, nfb]` in Hz (audio).
- A QSO partner audio frequency `nfqso` in Hz (used only for candidate
  prioritisation and the relaxed near-partner threshold; not for
  magnitude selection).
- A time-offset (DT) search range `[jzb, jzt]` in units of the
  symbol-spectrum stride (each unit = 40 ms). Default range is
  roughly ±2.5 s from nominal slot start, widened to ±3.5 s for SWL
  mode.
- An integer `ipass` in 1..9 selecting which of the three methods to
  run *and* which threshold to apply.
- A pass-dependent acceptance threshold `syncmin`.

### Outputs

- A list of up to ~460 candidate `(audio_freq, time_offset, sync_score,
  is_cq_only_pattern_flag)` tuples per pass.

### The three magnitude metrics

All three start from the same complex r2c FFT of a windowed symbol-
length block (the windowing — 200-sample raised-cosine tails on both
sides of the 1920-sample symbol with edge-sample boost of 1.9× — is
identical across all three).

Let the complex bin be `c = re + i·im`. The three pass groups compute,
for every bin and every spectrum row:

- **Method A** — root-sum-square (RSS) magnitude: `s = sqrt(re² + im²)`.
  This is the standard amplitude spectrum used by most FT8 decoders.
  Run during passes 1, 4, 7.
- **Method B** — power: `s = re² + im²`. Heavier tails (squares
  larger magnitudes more), so it emphasises stronger bins and
  de-emphasises noise floor variations. Run during passes 2, 5, 8.
- **Method C** — L1 magnitude: `s = |re| + |im|`. Approximately
  proportional to RSS for any single phasor but has different
  noise statistics under sums and different sensitivity to phase
  alignment within the FFT bin. Run during passes 3, 6, 9.

### Per-pass syncmin (the "low-threshold" mode)

When `lft8lowth` (low-threshold mode) or SWL mode is enabled, the
acceptance threshold `syncmin` is varied with the method:

- Passes 1, 4, 7 (RSS): `syncmin = 1.225`
- Passes 2, 5, 8 (power): `syncmin = 1.5`
- Passes 3, 6, 9 (L1):    `syncmin = 1.1`

When low-threshold mode is off, `syncmin = 1.5` is used for every
pass (the default conservative threshold).

The fact that the L1 metric uses the lowest threshold and the power
metric uses the highest is calibrated to each metric's noise
distribution: power has the heaviest tail, so the noise floor of the
sync score is higher, so the threshold must be higher. L1 has the
lightest tail, so noise scores stay low and a more permissive
threshold is safe.

### Costas sync scoring (per pass, after `s[i,j]` is built)

The sync score for an `(audio_bin, time_offset)` cell is built from
the same Costas-7 arrangement used by every FT8 decoder. JTDX uses
three Costas arrays (offsets 0, 36 and 72 symbols from the candidate
start), and there are two flavours of the inner computation depending
on whether `lagcc` (Costas + content correlation) is on. The key
points for the spec are:

1. For each of seven Costas symbol positions and each of the three
   arrays, look up the energy in the bin corresponding to the
   correct tone.
2. Compute a ratio of that energy to the sum of all eight tone
   energies at the same time position (the "signal vs. average" form
   of the Costas correlator).
3. Sum over the seven symbol positions and three arrays, with two
   variants — one using all three Costas arrays ("`sync_abc`") and
   one using the back-two arrays only ("`sync_bc`"). The reported
   sync is the max of the two.
4. (When `lagcc` is on, JTDX additionally probes nine "CQ-pattern"
   data symbol positions to detect candidates whose data resembles a
   CQ; that branch tags the candidate with a CQ flag but does not
   change the magnitude-metric selection.)
5. After scoring, the per-bin best time offset is selected
   (`maxloc` over the DT axis), then `red[i]` is normalised by a
   per-band baseline (the 40th-percentile sync score across the wide
   search band).

### Candidate acceptance loop

For each bin `i` in `[ia, ib]` (the narrow search band), iterate in
order of decreasing normalised sync:

- If the candidate is more than 3 Hz away from `nfqso`, accept it
  only if its sync ≥ `syncmin` (the per-pass threshold above).
- If the candidate is within 3 Hz of `nfqso`, accept it if its sync
  ≥ `1.1` (relaxed near-partner threshold — see the separate spec
  `spec-jtdx-relaxed-sync-near-partner.md`).
- Reject if the best DT is outside the allowed range (`[-49, 76]`
  spec units in normal mode, or `[-74, 101]` in SWL mode).
- Accumulate into a candidate buffer; cap at 450 raw candidates.

### Dedupe and merge across the three pass-group methods

JTDX's pass loop calls `sync8` once per pass with a different
`ipass`. Each call rebuilds the spectrogram for its own magnitude
metric, so the three pass groups produce three independent candidate
lists across the cycle. They are *not* merged inside `sync8`; the
caller (`ft8_decode.decode`) iterates passes 1..N sequentially, and
the inner decoder (`ft8b`) dedupes its message-level output across
all passes via the `allmessages` / `allfreq` / `allsnrs` arrays and a
±45 Hz frequency window.

In other words: the merge happens at the *decoded message* level, not
the candidate level. A candidate that fails the threshold under one
metric but passes under another simply gets a second chance, and any
duplicate message is filtered downstream by the SNR-and-frequency
dedupe rule.

### Within-pass near-duplicate suppression

Inside a single pass, candidates within `fdif0` Hz (4 Hz default,
3 Hz in SWL mode) and 0.1 s in DT of an already-accepted candidate
are suppressed, keeping only the higher-sync member. The near-QSO
zone (within 3 Hz of `nfqso`) is exempt from this suppression.

## Numerical constants (facts, not expression)

- FFT length (NFFT1) → bin spacing `df = 3.125 Hz`.
- Time stride between spectrum rows `tstep = 0.04 s` (40 ms).
- Steps per symbol `nssy = 4`.
- Frequency-bin oversampling factor `nfos = 2`.
- Default DT range (normal): bins `[-49, 76]` after the `(jpeak-1)`
  index shift; with `tstep = 0.04 s` this is roughly `[-2.0 s,
  +3.0 s]` relative to slot start.
- DT range (SWL): `[-74, 101]` → roughly `[-3.0 s, +4.0 s]`.
- Method-A (RSS) `syncmin = 1.225` in low-threshold mode.
- Method-B (power) `syncmin = 1.5` in low-threshold mode.
- Method-C (L1) `syncmin = 1.1` in low-threshold mode.
- Default (non-low-threshold) `syncmin = 1.5` for every pass.
- Near-`nfqso` (within ±3 Hz) `syncmin = 1.1` regardless of method.
- Raw candidate cap per pass: 450.
- Final candidate cap returned to the inner decoder: 460.
- Baseline percentile for `red` normalisation: 40% (`indx(nint(0.40*iz))`).

## Edge cases

- The pass-group / metric selection is hard-coded to `ipass mod 3`.
  Passes 1/4/7 → RSS, 2/5/8 → power, 3/6/9 → L1. There is no flag
  to choose a different metric without changing the cycle structure.
- When `lqsothread` (the slot is the QSO partner's parity *and*
  `nfqso` is inside `[nfa, nfb]`) is true, two "virtual" candidates
  at `nfqso ± 5 s` DT are appended to the list. These are unrelated
  to the magnitude-sweep mechanism but live in the same code path.
- Subtract-and-re-decode (SIC) is only applied for ipass ≤ 5 and ipass
  = 3 (for the 3-pass cycle); the final passes (6 onwards) disable
  subtraction. This is independent of which magnitude metric is
  active.
- Between cycle 1 and cycle 2 (i.e. before pass 4), the audio buffer
  `dd8` is replaced by a 2-tap moving-average smoothed copy
  `(dd8(i) + dd8(i+1))/2`. Before pass 7 the original buffer is
  restored (also via a 2-tap moving average from a saved copy).
  This is a separate mechanism from the three-method sweep — but it
  means RSS-pass-4 and RSS-pass-7 operate on different audio, which
  partially explains why JTDX re-runs the RSS metric three times.

## Conflict with pancetta's existing mechanisms

Pancetta's FT8 sync detector uses a single magnitude metric: 10·log10
of power (dB power). This is **not** one of the three JTDX metrics —
power and dB-power are monotonically related per bin but the Costas
sync ratio `s_correct_tone / sum(s_all_tones)` is *not* invariant to
the log transform. Empirically the dB-power sync correlator has yet
another noise distribution.

Consequences for porting:

1. The JTDX `syncmin` constants (`1.225`, `1.5`, `1.1`) are calibrated
   for *linear* power / RSS / L1. They will need empirical recalibration
   for pancetta's dB-power spectrogram if pancetta keeps that as its
   "Method A". Concretely: gather sync-score histograms from a
   noise-only WAV and a known-signal WAV per metric, pick the per-
   metric threshold that gives the same TP/FP operating point as the
   current scoped-fast-path.
2. The mechanism stacks cleanly with hb-091 (scoped fast-path) and
   hb-216 (tier wiring): the three metrics are CPU-independent — any
   one of them costs the same as the current single-metric path.
   Running all three is roughly 3× the FFT-magnitude-build cost. The
   per-bin Costas correlator is the same in all three.
3. The mechanism is orthogonal to hb-104 (joint vector decode) and
   hb-218 (capture-effect): both rely on having seen the candidate in
   the first place. The three-method sweep increases the candidate
   set; downstream improvements then have more to work with.

Pancetta already has a candidate-list dedup path inside the FT8
decoder (text-level HashSet + ±45 Hz SNR rule). The JTDX-style
candidate-level dedup (4 Hz / 0.1 s) inside `sync8` is *also* present
in pancetta's coarse-search code; the relevant adjustment is that the
near-`nfqso` exemption (3 Hz window) is not currently honoured. That
needs to be added if we want the relaxed-near-partner spec
(`spec-jtdx-relaxed-sync-near-partner.md`) to behave the same way.

## Estimated Rust port effort

- ~250 LOC to add two new magnitude builders next to the existing
  dB-power one in pancetta-ft8's spectrogram pipeline, gated by an
  enum `MagnitudeMetric { DbPower, Power, Rss, L1 }`.
- ~150 LOC to extend the FT8 outer decoder loop to run the three
  metrics back-to-back with their own threshold each, and to union
  results at the decoded-message level.
- ~200 LOC of calibration plumbing: histogram emitters + a small
  research example that picks the per-metric `syncmin` from a
  noise-only + signal corpus.
- 2 research sessions for calibration (one to gather histograms on
  hard-200, one to pick thresholds and verify recall delta).
- 1 implementation session.

Total: ~600 LOC, 3 sessions.

## Implementation notes for the implementer thread

- In pancetta-ft8, the spectrogram builder lives in the coarse-search
  module. Add a `MagnitudeMetric` enum and route through it; default
  to `DbPower` to keep current behaviour bit-identical when the
  feature is off.
- The pass loop in pancetta currently runs `max_decode_passes`
  passes that all share the same magnitude. Refactor that into an
  iterator over `(MagnitudeMetric, syncmin)` tuples. Default
  configuration (one pass) keeps current behaviour.
- The candidate dedup inside the coarse-search pass should be left
  alone, but downstream dedup at the message level must be confirmed
  to behave correctly when three passes produce the *same* signal
  with slightly different fine-frequency estimates. Pancetta's
  existing text-level HashSet already does this.
- Critical: the three-method sweep should ship gated behind a config
  flag. Initial scorecard runs on hard-200 will tell us whether it's
  a net win at pancetta's existing operating point (low FP, moderate
  recall) or whether the extra candidates produce too many FPs even
  with calibrated thresholds.
- Cross-reference hb-091's freq_bin_range plumbing: when the QSO-
  partner band-collapse is active, the three-method sweep runs over
  a narrow band and is much cheaper. That makes
  `spec-jtdx-qso-partner-filter.md` a natural prerequisite.
