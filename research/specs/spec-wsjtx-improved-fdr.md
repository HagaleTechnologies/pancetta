# Algorithm spec: FDR — False Decodes Reduction (per-message-type confidence gates)

## Source attribution
- Origin: WSJT-X Improved by Uwe Risse, DG2YCB
  (https://sourceforge.net/projects/wsjt-x-improved/)
- Primary documents (traceability only, NOT quoted):
  `Release_Notes.txt` line ~1670 (v2.5.0 introduction — verbatim
  description of behavior),
  `Release_Notes.txt` line ~1553 (v2.6.0 — two-level structure
  introduced),
  `Release_Notes.txt` line ~1582 (v2.5.4 update — "further improvement"),
  `Release_Notes.txt` line ~990 (v2.7.0 line "Some improvements to the
  FDR"),
  `Release_Notes.txt` line ~1348 (v2.7.0 — "simplified, purely optional
  again, less often needed").
- Fortran source paths cited for traceability only, NOT to be read by implementer:
  Modifications scattered across the FT8 decoder driver
  `wsjtx/lib/ft8/ft8b.f90` and the BP/OSD wrappers
  `wsjtx/lib/ft8/bpdecode174_91.f90`, `wsjtx/lib/ft8/osd174_91.f90`,
  the candidate post-acceptance plausibility wrapper around
  `wsjtx/lib/ft8/ft8d.f90`.
- License: GPL-3.0.
- Reader date: 2026-06-08.
- Reader thread: clean-room extraction (prose only).
- Upstream lineage: FDR is exclusive to WSJT-X Improved (since v2.5.0,
  Sep 2021); not in WSJT-X mainline. JTDX has a structurally similar
  per-message-type confidence dialing mechanism that predates FDR but
  differs in detail.

## Purpose

In FT8, the LDPC decoder is a probabilistic device — even when its
internal checks pass, the resulting "decoded" message may be a false
decode (an arbitrary 77-bit payload that happens to satisfy the LDPC
parity equations but does not correspond to a real signal). The standard
WSJT-X gate is uniform: any decoded message whose LDPC + CRC pass is
accepted.

Empirically, certain message *types* generate disproportionately many
false decodes. The structural reasons vary:

- Free-text and DXpedition-format messages have higher payload entropy
  than ordinary callsign-grid-report messages; the LDPC code's
  per-bit error rate maps to a higher chance of a noise-driven payload
  satisfying the constraint set with plausible-looking text.
- Hashed-callsign messages encode part of the callsign as a hash; an
  FP can produce a syntactically valid but operationally meaningless
  hashed message.
- Telemetry messages and contest-mode variants have small structural
  redundancy that the standard pipeline does not exploit.

FDR is a *post-decode confidence gate* keyed by message type. Standard
two-call message types (`call1 call2 [grid|report|RRR|RR73|73]`) get the
permissive uniform threshold, exactly as in WSJT-X mainline — sensitivity
is preserved. Unusual message types (free-text, DXpedition format,
telemetry, certain contest variants) get a *higher* confidence threshold,
trading a small loss of unusual-message sensitivity for a significant
reduction in false decodes corpus-wide.

This is the mechanism release notes verbatim describe at line ~1671 as
"reduces significantly the number of FT8 false decodes without a
reduction of the sensitivity. It only increases the required confidence
level for some unusual message formats."

## Algorithm description (PROSE ONLY — no code)

### Inputs

The algorithm operates per accepted-from-LDPC decode candidate. Inputs:

- **Decoded payload** (77 bits) plus the LDPC + CRC convergence metadata:
  - `bp_iterations_used`: how many BP iterations the decoder ran before
    convergence. Lower = more confident.
  - `osd_depth_used`: if BP failed and OSD ran, the OSD depth at which
    convergence happened. Lower = more confident.
  - `nharderrs`: number of hard-decision bit errors corrected by OSD.
    Lower = more confident.
  - `min_llr_magnitude` (or `min_distance`): smallest weighted-LLR
    magnitude found among the converged-codeword bits. Higher = more
    confident.
  - `sync_quality`: Costas sync correlation peak height at the chosen
    `(dt, freq)`. Higher = more confident.
  - `snr_db`: estimated signal-to-noise ratio. Higher = more confident.
- **Parsed message structure**:
  - `message_type`: one of `StandardCallsignPair`, `CqVariant`,
    `HashCall1`, `HashCall2`, `BothHashed`, `Telemetry`, `FreeText`,
    `DxpeditionHound`, `DxpeditionFox`, `ContestNonStandard`. The exact
    set in WSJT-X is the union of the FT8 77-bit message types defined
    in the protocol; see Joe Taylor's FT4/FT8 QEX paper for the
    authoritative type list.
  - `tokens`: the parsed text tokens (already produced by the message
    parser).
- **FDR mode flag**:
  - `Off`: no extra filtering (default in WSJT-X Improved v2.7.0
    onward).
  - `Level1`: lightweight basic protection (in v2.5.0/v2.6.0 was always
    on; in v2.7.0 became optional). Filters only the highest-FP-rate
    message types.
  - `Level2`: comprehensive protection (was opt-in across all versions).
    Filters all unusual message types. Automatically disabled in
    Special Operating modes (contest, Fox/Hound, etc.).
- **Operator-context flag**:
  - `is_special_operating_mode`: true if the operator has enabled a
    contest, DXpedition, or Echo mode. When true, Level2 is suppressed
    even if the operator selected it. (Per release notes line ~1555,
    "automatically deactivated during special operating activities.")

### Outputs

A boolean accept/reject decision per candidate, and optionally a
per-message-type confidence score for downstream consumers (e.g.,
pancetta's hb-103-style content scoring).

### Steps

1. **Classify message type.** Use the existing FT8 message parser to
   bucket the candidate into the message-type vocabulary above.

2. **Identify "easy" vs "unusual" types.** The "easy" set — for which
   FDR applies the WSJT-X-mainline-equivalent uniform gate — is:
   - `StandardCallsignPair` (`call1 call2 [grid|report|RRR|RR73|73]`).
   - `CqVariant` (`CQ ... call1 [grid]`) — provided both `call1` and
     the optional grid pass standard validators.
   The "unusual" set, gated harder by FDR:
   - `FreeText`.
   - `Telemetry`.
   - `DxpeditionFox` and `DxpeditionHound` outside of declared
     Fox/Hound operation.
   - `HashCall1` / `HashCall2` / `BothHashed` (hashed callsigns).
   - `ContestNonStandard` types other than the operator's currently-
     declared contest variant.

3. **Per-type confidence threshold.** For each "unusual" type, define a
   numeric threshold on a combined confidence metric. Sketch:
   - `confidence = w_sync * sync_quality
                 + w_llr_min * min_llr_magnitude
                 - w_bp_iters * bp_iterations_used
                 - w_nharderrs * nharderrs
                 + w_snr * snr_db`.
   - Weights are calibration-specific; the WSJT-X Improved exact values
     are not in the release notes and live in the modified Fortran.
   - Per-type thresholds are looser for `Level1` and tighter for `Level2`.
4. **Easy types pass unchanged.** Easy-type candidates use the
   WSJT-X-mainline-equivalent gate (essentially "LDPC + CRC OK"). FDR
   does *not* touch sensitivity for the bulk of normal QSO traffic.

5. **Apply per-type threshold.** For each unusual-type candidate:
   - Compute the combined confidence.
   - Compare to the per-type threshold for the active FDR level.
   - Accept if confidence ≥ threshold; reject otherwise.

6. **Special-operating-mode override.** If `is_special_operating_mode`
   is true and `level == Level2`, downgrade silently to `Level1` (or
   `Off`, depending on the contest type). Per release notes line ~1555,
   the original WSJT-X Improved behavior is to fully disable Level 2
   in special modes because contest exchanges legitimately use
   formats that look "unusual" to the off-contest filter.

7. **Per-type rejection telemetry (optional).** WSJT-X Improved does
   not surface a per-type FP-rejection counter, but a useful pancetta
   addition would be `rejected_by_fdr: HashMap<MessageType, u32>` for
   the operator to inspect.

### Numerical constants (facts, not expression)

- **Number of FDR levels**: 2 (Level1 + Level2). Off is a third
  meta-state.
- **Special-operating auto-disable**: Level 2 is silently disabled in
  special operating modes (contest, DXpedition Fox/Hound, Echo). Level 1
  remains active.
- **Easy-type set size**: 2 (StandardCallsignPair + CqVariant).
- **Unusual-type set size**: 6+ (FreeText, Telemetry, DxpeditionFox/Hound,
  hashed variants, off-contest non-standard).
- **Sensitivity claim**: per release notes line ~1671 (verbatim),
  Level 1 protection is achieved "without a reduction of the
  sensitivity." This is the design constraint, not a measurement.
  Pancetta should validate by hard-200 eval — bootstrap-CI on
  per-message-type accept rate before and after FDR.
- **Empirical FP-reduction headroom**: the release notes describe FDR
  as "reduces significantly the number of FT8 false decodes." No
  numeric percentage is published. Pancetta's hb-058 /R-filter +
  hb-062 callsign-continuity baseline already removes ~55% of FPs; FDR
  is structurally orthogonal (it filters by *type*, not by content).
  Recommended starting hypothesis: a tuned FDR catches an additional
  10–25% of remaining FPs.

### Edge cases

- **Operator explicitly worked a non-standard station.** If the operator
  is in a confirmed QSO with a non-standard callsign (e.g., a
  DXpedition hound), the FDR filter must not reject that station's
  legitimate reply. The implementation provides an exception: any
  message containing the operator's currently-active QSO partner
  callsign bypasses FDR. (Analogue: pancetta's hb-052 callsign
  continuity already maintains a trust set; the FDR exception piggybacks
  on this.)
- **Contest start/stop transitions.** Operator enables a contest mode
  partway through a session. The FDR level switches *only on transition*,
  not retroactively for in-flight decodes. Decodes already presented
  to the operator remain on screen.
- **False decode in the easy types.** Standard callsign pairs do
  produce occasional FPs (this is the bulk of pancetta's FP corpus —
  see hb-058, hb-062, hb-103). FDR explicitly does NOT filter these;
  they are handled by orthogonal mechanisms (per-callsign continuity,
  /R-suffix filter, content scoring). Do not collapse FDR with those.
- **CRC + LDPC convergence margin not exposed.** Older WSJT-X
  decoder paths may not expose `bp_iterations_used` or
  `min_llr_magnitude` to the post-decode gate. The Improved fork
  modified the decoder to expose them. Pancetta's BP/OSD already
  computes these internally; plumbing them out is straightforward.
- **Level 2 + special-mode interaction**. Operator enables Level 2,
  *then* enables a contest. The level silently degrades to Level 1.
  When the contest is disabled, Level 2 reactivates. This is
  invisible to the operator (no notification); document it in the
  pancetta UI / config.

## Conflict with pancetta's existing mechanisms

- **Strong overlap with hb-103 (content score).** hb-103 is pancetta's
  in-progress content-score gate: a fused feature score (trust-set
  membership, confidence, dt, SNR) used to discriminate FPs from TPs.
  FDR is structurally a *typed* version of the same idea — instead of
  one threshold across all decodes, separate thresholds per message
  type. Recommend integrating: extend hb-103's score with a
  per-message-type threshold dial, exposed as `Ft8Config::fdr_level`.
  This is a cleaner fit than building FDR as a parallel mechanism.

- **Strong overlap with is_plausible.** Pancetta's `is_plausible`
  function in pancetta-qso already filters DXpedition and FreeText
  message types (Batch 32 work). FDR is the structured generalization —
  swap the binary is_plausible check for a per-type confidence gate
  with a 3-state level dial.

- **Hb-058 (`/R` filter) is FDR-Level-1-flavored.** The `/R` suffix
  filter already pancetta-ships filters one specific high-FP-rate
  pattern; FDR formalizes this approach across message types. Hb-058
  should stay (it's an exact-pattern gate that is cheap and reliable),
  and FDR layers on top with the typed confidence gate.

- **No conflict with hb-062 (callsign continuity).** hb-062 filters by
  *callsign trust set*; FDR filters by *message type*. Both can apply.

- **No conflict with hb-156 (lid_of_band tier).** That mechanism gates
  by SNR tier; orthogonal.

- **Interaction with hb-103 SHIP_PRECISE / SHIP_CONSERVATIVE thresholds.**
  Already-shipped content-score thresholds were calibrated without
  per-type variation. With FDR, the autonomous-TX path could move to
  a `MessageType → threshold` map. Bootstrap-CI on the resulting
  yield/FP tradeoff before shipping.

- **Interaction with hb-091/hb-216 (tier classification).** FDR is CPU-
  cheap (a few comparisons per accepted decode); no tier-driven
  gating needed. Default to Level1 on all tiers; default to Off if
  hb-103 already handles content scoring and the operator-visible UX
  doesn't need both.

## Estimated Rust port effort

- New module:
  - `pancetta-qso/src/fdr.rs`: defines `MessageType` enum (probably
    already exists), `FdrLevel { Off, Level1, Level2 }`, the per-type
    confidence threshold table, and a `should_reject(decode, level,
    is_special_mode) -> bool` function. ~120 LOC + ~100 LOC tests.
  - Extend `Ft8Config` with `fdr_level: FdrLevel` (default `Level1` on
    Fast/Moderate, `Off` on Slow) and an `is_special_mode: bool`
    runtime flag.
  - Extend `DecodedMessage` with `confidence_features: ConfidenceFeatures`
    if not already present (these are the inputs to the FDR gate). Some
    of these already exist (`confidence`, `snr_db`, `time_offset_s`);
    add `bp_iterations_used`, `osd_depth_used`, `nharderrs`,
    `min_llr_magnitude`. The decoder modifications to plumb these out
    are ~30 LOC in pancetta-ft8.
- Coordinator wiring:
  - Call `should_reject` after the standard decode-message pipeline,
    before the message is presented to the autonomous operator or the
    logger. ~30 LOC + ~30 LOC tests.
- Calibration:
  - Hard-200 eval pass with FDR Off, Level1, Level2. Bootstrap-CI on
    accept rate (per type) and FP rate. Tune per-type thresholds.
- Total: ~200 LOC + ~160 LOC tests + 1 calibration session.
- 3–4 iter sessions:
  1. ConfidenceFeatures plumbing in pancetta-ft8 decoder.
  2. FDR module + per-type thresholds.
  3. Coordinator wiring + tier defaults.
  4. Calibration + ship decision.

## Implementation notes for the implementer thread

- **Reuse the existing `MessageType` enum.** Pancetta's
  `pancetta-ft8/src/message.rs` already parses FT8 messages into a
  type-tagged structure; the FDR enum should map onto that. Do not
  create a parallel taxonomy.

- **Plumb confidence features through `DecodedMessage`.** The features
  FDR needs (`bp_iterations_used`, `osd_depth_used`, `nharderrs`,
  `min_llr_magnitude`) all already exist somewhere in the BP/OSD
  internals. Surface them via a `ConfidenceFeatures` struct attached
  to each `DecodedMessage`. Several other mechanisms (hb-103,
  autonomous-TX gating) will benefit.

- **FDR is a *post-acceptance* gate.** Run the standard BP/OSD pipeline
  unchanged. FDR sees only candidates that already passed LDPC + CRC.
  Do not push FDR into the decoder hot loop.

- **Level1 vs Level2 thresholds are calibration-driven.** Do not
  hard-code generous numbers. Set up a calibration script (hard-200
  WAV corpus + truth labels), grid-search per-type thresholds for
  Level1 (target: zero TP loss, modest FP reduction) and Level2 (target:
  ≤1% TP loss, significant FP reduction). Report per-type FP-rate
  reduction and per-type TP-rate change. Bootstrap-CI brackets.

- **Special-operating gate is critical.** Without it, FDR will reject
  legitimate contest exchanges (e.g., "K1JT 1A NH" is a valid 77-bit
  ARRL Field Day exchange that looks unusual to the off-contest
  classifier). The coordinator must set `is_special_mode = true` whenever
  the operator activates any contest or DXpedition mode. Mirror this
  with pancetta's existing contest detection if present, or add it.

- **Provenance preservation.** When FDR rejects a candidate, do *not*
  silently drop it. Log at `debug!` level (`target: "decoder.fdr"`)
  with the decode text, message type, computed confidence, and
  threshold. This lets operators tune.

- **Don't run FDR on AP decodes.** a7 and a8 produce high-confidence
  decodes by construction (AP candidates that pass weighted-distance
  and CRC). Skip FDR for `DecodeProvenance::A7CrossSequence` and
  `DecodeProvenance::A8EarlyQsoState` — they are pre-vetted.

- **Operator UI.** Expose `fdr_level` as a configuration option in
  pancetta-config. Hot-reload friendly (FDR has no warm-up cost,
  level changes take effect on next decode).

- **Citation hygiene.** Cite as "inspired by WSJT-X Improved FDR
  v2.5.0+ (DG2YCB)" in the journal entry. The conceptual lineage is
  Bayesian per-class hypothesis testing; FDR is a specific operational
  implementation. Pancetta's implementation is independent.
