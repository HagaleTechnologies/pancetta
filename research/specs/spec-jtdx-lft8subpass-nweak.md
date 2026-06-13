# Algorithm spec: JTDX lft8subpass / nweak inner-subpass cascade

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File paths (for traceability, NOT to be quoted):
  - `lib/ft8b.f90` (the `nweak`/`nsubpasses` loop near line ~700-1620
    that drives the inner decode cascade)
  - `lib/ft8_decode.f90` (where `lft8subpass` is read from the
    decoder params and threaded into `ft8b`)
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

JTDX's inner decode loop in `ft8b` exists in two flavours: a "single-
pass" mode that runs each AP type once with the canonical soft-bit
metric, and a "weak-signal" mode (`nweak=2`) that adds a second inner
iteration using a *time-reversed conjugate* symbol stream as an
alternative source of soft information. The `lft8subpass` flag is the
trigger that turns the weak-signal cascade on globally for the whole
decode call (not per candidate). When `lft8subpass` is true, the flag
also enables `ldeepsync` (a longer Costas sync probe) and relaxes the
CQ-signal-gate threshold from 1.3 to 1.2 for AP type 1 (CQ template).

The combination — extra inner iteration, deeper sync, looser CQ gate —
is what makes JTDX's "deep decode" CPU-intensive but recall-positive.
Each piece is small individually; the combination is where the wins
add up. This is the spec for the *combination*, with each component
described in enough detail that pancetta can choose to adopt them
together or piecewise.

## Algorithm description (PROSE ONLY)

### Inputs

- A flag `lft8subpass` set externally (currently always false in the
  released JTDX binary; meant for an internal "Sub Pass" diagnostic
  mode but the code paths it activates are reachable and tested).
- A flag `lft8lowth` (low-threshold mode, operator-toggleable).
- The SWL mode flag `swl` (alternate corpus / longer DT range).
- The candidate's `dfqso = |f1 - nfqso|` (frequency distance from QSO
  partner).
- A flag `lsubptxfreq` (the candidate is at the operator's transmit
  frequency `nftx` — typical for the partner replying).

### Outputs

- An integer `nweak` ∈ {1, 2} controlling the outer subpass count.
- An integer `nsubpasses` ∈ {1, 2, 3, 5, 6, 8, 9, 11} that combines
  `nweak` with extra special-case lanes for CQ-signal averaging
  (`lcqsignal`), my-call averaging (`lmycsignal`), and QSO-candidate
  averaging (`lqsocandave`).
- A flag `ldeepsync` that propagates into the iqso=4 (FT8SD shortcut)
  branch and enables the deeper sync path there.

### Steps — nweak computation

After the candidate's complex symbol-magnitude matrix `cs(0:7,1:79)`
has been built and the alternate matrices (`csr` = reversed-conjugate,
`cscs` = lreverse-saved when applicable) have been computed:

1. Set `nweak = 1` as the default (single-pass weak-decode).
2. Promote to `nweak = 2` whenever any of the following are true:
   - `lft8subpass` is true (the global subpass-mode flag).
   - `swl` is true (the operator-mode SWL flag).
   - `dfqso < 2.0 Hz` (candidate is within 2 Hz of the QSO partner).
   - `lsubptxfreq` is true (candidate is at the operator's TX freq,
     meaning a partner reply is expected here).
3. Set `nsubpasses = nweak`.
4. If `lcqsignal` is true (the candidate's data pattern looks like a
   CQ template hit), bump `nsubpasses` to 3, and if a previously
   stored matching CQ symbol matrix exists for this `(freq, xdt)`,
   bump further to 5. The extra lanes 4 and 5 combine the current
   `cs` with the saved `csold` from a previous interval to do
   coherent-averaging across QSO partner repeats.
5. If `lmycsignal` is true (the candidate's data looks like a
   `MyCall ??? ???` template), bump `nsubpasses` to 6, and if a
   previously stored matching `mycsig` symbol matrix exists, bump
   further to 8.
6. If `lqsocandave` is true (in-QSO candidate averaging conditions
   met: `lapmyc` *and* `ndxt>2` *and* `nmic>2` *and* not
   `lqsomsgdcd` *and* both calls standard *and* `dfqso < napwid/2.0`),
   bump `nsubpasses` to 9, and if a previously stored matching
   `qsosig` symbol matrix exists, bump further to 11.

### Steps — inner subpass loop (drives the codeword metric)

The inner loop iterates `do isubp1 = 1, nsubpasses`. Each value of
`isubp1` picks a different soft-bit metric for the codeword:

- `isubp1 = 1`: the standard metric — absolute value of `cs(tone, k)`
  summed using the Gray-mapped tone alphabet, with a 1, 2, or 3-symbol
  joint span (the inner `nsym` loop). This is JTDX's baseline "normal"
  decode.
- `isubp1 = 2`: the reversed-conjugate metric — replaces `cs` with
  `csr` (the symbol matrix computed from time-reversed conjugate
  samples). This is the **`nweak = 2` extra inner iteration**. Same
  Gray-mapping, same nsym loop, but the underlying symbol energies
  come from a different transform of the audio. Skipped when
  `nweak = 1`.
- `isubp1 = 3, 6, 9`: power-add of `cscs` and `csr` — squares the
  magnitudes and sums in power-domain. Used for `lreverse` passes
  where the spectrogram was rebuilt with reversed audio.
- `isubp1 = 4, 7, 10`: power-add of current `cs` and a previously
  saved `csold` — coherent-averaging across QSO repeats.
- `isubp1 = 5, 8, 11`: linear-magnitude-add of current `cs` and
  `csold`.

For each `isubp1`, the inner loop computes the four bit-metric
arrays `bmeta`, `bmetb`, `bmetc`, `bmetd` (1-symbol max-vs-max,
2-symbol joint, 3-symbol joint, and a normalised 1-symbol variant
respectively), normalises them, and feeds them as LLRs into the
inner `do isubp2 = 1, 31` AP-type loop.

The inner-most `do isubp2 = 1, 31` loop runs the bp/OSD decoder
against up to 31 different AP-bit assignments per `isubp1`. For each
`isubp2 ≥ 5`, an AP type is looked up from `naptypes` /
`ndxnsaptypes` / `nmycnsaptypes` / `nhaptypes` arrays indexed by
`(nQSOProgress, isubp2-4)`. Critically, the *CQ gate threshold*
embedded in this loop depends on `lft8subpass`:

- When `lft8subpass` (or `lft8lowth`) is true: the gate threshold is
  `scqnr ≥ 1.2` (relaxed). The CQ gate filters AP type 1 (CQ template)
  by requiring `scqnr` (the CQ-symbol signal-to-noise ratio) to clear
  the gate.
- When neither is true: the gate threshold is `scqnr ≥ 1.3`.

That 1.3 → 1.2 relaxation is small numerically (~6 % SNR), but it
opens an extra band of weak-CQ candidates to the AP-type-1 decoder.
The same 1.3 → 1.2 relaxation also applies to the MyCall AP type
gated by `smycnr` and the `<MyCall> DxCall ???` AP type gated by
`smycnr ≥ 1.2 / 1.0`. All three relaxations fire together when
`lft8subpass` flips.

### Steps — ldeepsync enabling

At entry to `ft8b`, JTDX sets `ldeepsync = .false.` and then
promotes it to `.true.` if `lft8lowth`, `lft8subpass`, or `swl` is
true. The `ldeepsync` flag affects the **iqso=4** branch (the
"already-decoded text shortcut" lane):

- With `ldeepsync = .false.` and `iqso = 4`, JTDX runs
  `tonesd(msgd, lcq)` to generate the tone sequence from the stored
  text, then *skips* the full Costas refinement (jumps via `go to 32`)
  and goes straight to the FT8SD / FT8MF cascade.
- With `ldeepsync = .true.`, the same `iqso = 4` branch sets
  `cd0 = cd1` (saved baseband from iqso=1) and *continues* through
  the full Costas refinement and symbol-extraction pipeline before
  reaching the FT8SD / FT8MF cascade. The result is that the FT8SD
  cascade sees freshly-resynced symbol matrices, which improves its
  pickup rate on borderline signals.

The same `ldeepsync` flag also gates a re-entry to the FT8SD cascade
at line 507 (`if(iqso.eq.4 .and. .not.ldeepsync) go to 64`), which
controls whether `ft8sd1` is tried before or after `ft8mf1`.

### Numerical constants (facts, not expression)

- `nweak` baseline: `1`. Upgrade to `2` triggered by `lft8subpass OR
  swl OR dfqso < 2.0 Hz OR lsubptxfreq`.
- Near-partner trigger for `nweak = 2`: `dfqso < 2.0 Hz`.
- TX-freq trigger for `nweak = 2`: `|f1 - nftx| < 2.0 Hz` with last-TX
  conditions (`nlasttx == 1` if not skip-tx1, else `nlasttx == 2`).
- CQ-gate threshold (AP type 1) when `lft8subpass OR lft8lowth`:
  `scqnr ≥ 1.2`.
- CQ-gate threshold (AP type 1) when neither flag is set:
  `scqnr ≥ 1.3`.
- CQ initial-gate threshold (isubp2 = 20): `scqnr ≥ 1.0` (only when
  signal is not `lcqsignal`).
- MyCall AP-2 gate when `lft8subpass OR lft8lowth`: `smycnr ≥ 1.2`.
- MyCall AP-3 gates: `smycnr ≥ 1.0` (isubp2 = 5), `smycnr ≥ 1.2`
  (isubp2 = 6).
- `nsubpasses` value table:
  - default = `nweak`
  - `lcqsignal` alone: 3
  - `lcqsignal` + previous CQ match: 5
  - `lmycsignal` (with standard MyCall): 6
  - `lmycsignal` + previous match: 8
  - `lqsocandave`: 9
  - `lqsocandave` + previous match: 11

### Edge cases

- `lft8subpass` is reachable but not exposed in the released JTDX
  UI. The released binary always has `lft8subpass = .false.`. The
  weak-signal effects are still reachable via `lft8lowth = .true.`
  (the "Low Threshold Decoding" operator setting) which has the
  same effect on `ldeepsync` and the CQ-gate threshold but does
  *not* alter `nweak`.
- `nweak = 2` ALSO triggers automatically whenever `dfqso < 2.0 Hz`
  — that is, the second inner iteration *always* runs for any
  candidate near the operator's QSO partner, regardless of any
  flags. This is a structural commitment: JTDX is willing to pay
  ~2× the inner-decode CPU near the QSO partner unconditionally.
- The `csr` matrix (reversed-conjugate, used as the alternative
  source for `nweak = 2` isubp1=2) is computed unconditionally for
  every candidate, so the second iteration costs only the bp/OSD
  re-runs, not the symbol extraction.
- The `lreverse` flag (set when `ipass ∈ {2, 5, 7}` in 3-cycle / 9-
  cycle mode) swaps `cs` and `csr` so the "normal" `isubp1=1` is
  actually decoding the reversed-conjugate signal. This is JTDX's
  way of getting double coverage out of a single sync pass.
- The inner `isubp2` loop terminates early when a valid CRC-clean
  codeword is found (`exit` from inner loop). On a "weak" signal
  that needs all 31 AP types and both `isubp1 = 1, 2`, the inner
  loop can run ~60 codeword attempts per candidate.

## Conflict with pancetta's existing mechanisms

Pancetta's inner decode loop has a single soft-bit metric (the
standard `s(tone, k)` magnitude from the symbol-extracted spectrogram)
and runs the LDPC + OSD cascade once per (candidate, AP-type) pair.
There is no analogue of `nweak = 2` (the reversed-conjugate metric).

The mechanisms decompose into three independent ports:

1. **Reversed-conjugate alternate metric** (`nweak = 2`): pancetta
   would gain a second LLR computation path that uses a
   time-reversed-and-conjugated FFT of the same 79 symbols. This
   doubles inner-decode CPU. Worth the cost only for candidates near
   the QSO partner (`dfqso < 2 Hz`) — which is exactly where pancetta
   has the most to gain (hb-218 capture-effect work).
2. **Relaxed CQ-gate threshold** (1.3 → 1.2): pancetta's current
   AP-cascade does not have a `scqnr` gate at all — AP type 1 (CQ
   template) is always tried. So this relaxation is a no-op for
   pancetta unless pancetta adds a CQ-gate first.
3. **Deeper sync on already-decoded text** (`ldeepsync`): pancetta
   does not have an `iqso = 4` shortcut for already-decoded text in
   the slot. The closest analogue would be in pancetta-qso's
   continuity filter and the `qso_state.last_rx_msg` lookup, both
   pre-decode rather than inside-decode. A port would need a *post-
   decode replay* path that re-extracts symbols at a refined
   `(freq, dt)` if the text matches a previously-seen message.

### Compatibility with hb-091 (scoped fast path)

`nweak = 2` is the *opposite* of fast-path — it deliberately adds CPU
cost. The two mechanisms compose by tier: on the Fast tier (M4 in
hb-216), pancetta would run `nweak = 2` unconditionally for
near-partner candidates; on Moderate tier, only for candidates with
sync ≥ a higher threshold; on Slow tier, never.

### Compatibility with hb-218 (capture-effect joint decode)

The reversed-conjugate metric has different sensitivity to phase-
related capture effects than the forward-conjugate metric. Running
both and combining LLRs would offer *some* of the capture-effect
robustness hb-218 targets, without requiring the full joint decoder.
This is a smaller win than hb-218 proper, but it composes — both
mechanisms attack capture-effect from different sides.

## Estimated Rust port effort

- ~150 LOC to add the reversed-conjugate spectrogram path in
  pancetta-ft8's symbol-extraction module. The maths is small (mirror
  the time samples and conjugate before the FFT) but the plumbing
  through the LLR computation needs a `MetricSource { Forward,
  Reversed }` enum.
- ~80 LOC to add the `nweak = 2` outer iteration in the inner
  decode loop, gated by `near_qso_partner OR config.deep_decode`.
- ~120 LOC for an FT8SD-equivalent: a tone-template matched filter
  against the message templates currently held in
  `pancetta-qso::recently_decoded_messages`. This is the `iqso = 4`
  / `ldeepsync` shortcut.
- ~80 LOC for the CQ-gate (`scqnr`) computation and the 1.3 / 1.2
  threshold lookup.
- ~100 LOC of tests + 1 calibration example (gather scqnr histograms
  on hard-200, set the gate at 1.2 if low-threshold mode is on).
- 3 research sessions: one to validate the reversed-conjugate metric
  produces independent FPs/TPs from the forward metric, one to
  calibrate the CQ-gate against pancetta's existing scoring, one to
  ship.

Total: ~530 LOC, 3 sessions.

## Implementation notes for the implementer thread

- The reversed-conjugate symbol matrix is a small lift in
  pancetta-ft8's symbol-extraction code: take the same 32-sample
  symbol window, mirror it (sample 32 → sample 1, sample 31 →
  sample 2, etc), conjugate, and FFT. Pancetta's existing
  `extract_symbol_spectra` function returns a `[[Complex32; 8]; 79]`
  matrix; the reversed variant returns the same shape from the
  mirrored-conjugated samples.
- The decision of whether to run `nweak = 2` per candidate should
  happen *before* the inner AP-type loop. Gate on
  `qso_state.is_partner_freq(f1, tolerance=2.0)` to match JTDX's
  unconditional near-partner behaviour. Add a second gate on the
  tier-aware config flag for the operator-driven "deep decode" mode.
- The CQ-gate threshold relaxation is small and not load-bearing on
  its own; ship it only if pancetta first adds a `scqnr` gate. Don't
  port the gate just to relax it.
- The `ldeepsync` mechanism (post-text-match resync) is the most
  expensive in CPU but the highest-leverage in recall for partners
  whose DT has drifted within the slot. Cross-reference
  `spec-jtdx-lqsothread-virtual-candidates.md` — both mechanisms
  attack partner drift, the virtual-candidate path attacks out-of-DT-
  window drift, the `ldeepsync` path attacks in-DT-window drift.
- Critical sequencing: ship `nweak = 2` first (smallest blast
  radius, narrowest win at near-partner), then `ldeepsync` (highest
  CPU cost, biggest near-partner recall), then the CQ-gate relax
  (only if a CQ-gate has been added). Each step is independently
  reversible if it doesn't pay off.
- The reversed-conjugate metric should NOT be merged into the
  primary metric — keep it as a *separate* attempt at decoding the
  same candidate. JTDX's structure (separate isubp1 iterations) is
  the right model: each metric runs the full LDPC + OSD cascade and
  whichever finds a CRC-clean codeword first wins.
