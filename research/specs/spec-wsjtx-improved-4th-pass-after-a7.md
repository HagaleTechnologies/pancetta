# Algorithm spec: 4th decode pass after a7

## Source attribution
- Origin: WSJT-X Improved by Uwe Risse, DG2YCB
  (https://sourceforge.net/projects/wsjt-x-improved/)
- Primary document (traceability only, NOT quoted):
  `Release_Notes.txt` lines ~73–75 (v3.1.0 260522): "Added a7 decoding
  technology, sub-sample DT refinement, and a 4th pass after a7 for
  both FT4 and FT2."
- Fortran source paths cited for traceability only, NOT to be read by implementer:
  `wsjtx/lib/ft8/ft8_a7.f90` (a7 cross-sequence cousin, already extracted
  as spec-wsjtr-cross-sequence-a7.md — the structurally analogous FT8
  mechanism),
  `wsjtx/lib/ft4/` (FT4 decoder driver — the actual home of the
  4th-pass-after-a7 addition for FT4),
  `wsjtx/lib/ft8/ft8d.f90` and `wsjtx/lib/ft8/ft8b.f90` (FT8 multipass
  loop drivers — the structural model for what "pass N" means).
- License: GPL-3.0.
- Reader date: 2026-06-08.
- Reader thread: clean-room extraction (prose only).
- Upstream lineage: this addition is exclusive to WSJT-X Improved
  v3.1.0 (May 2026) and applies to FT4 and FT2 (not directly to FT8 —
  FT8 already had a7 prior to v3.1.0). However, the pattern translates
  cleanly to FT8 and pancetta should consider it for the FT8 hot loop.

## Purpose

The standard WSJT-X FT8 multipass decoder iterates a fixed number of
passes (3 in mainline; 3 by default in pancetta with hb-091/hb-216
tier-driven adjustments). Each pass:

1. Searches the audio spectrum for tone-candidate locations.
2. Demodulates each candidate's soft symbols and feeds them to LDPC/OSD.
3. Accepts decoded messages, parses them, and subtracts their reconstructed
   waveforms from the residual audio.

a7 (the cross-sequence AP pass — see spec-wsjtr-cross-sequence-a7.md) is
normally invoked as the *last* operation in the multipass sequence: it
runs over the residual audio after all standard passes have completed,
trying to pull additional weak decodes from QSO-context-seeded
candidate templates. Each a7-accepted decode is itself subtracted from
the residual.

The "4th pass after a7" insight is that **a7's subtraction may unmask new
decode candidates that the standard pipeline could not see while a7's
decode was still in the audio**. After a7 has finished, the residual is
cleaner than it has been at any prior point in the pipeline. Running
*one more* standard pass over this cleaner residual recovers additional
decodes that were previously sitting under an a7-decoded signal's
ambiguity.

The release notes verbatim phrasing (line ~74): "Added a7 decoding
technology, sub-sample DT refinement, and a 4th pass after a7 for both
FT4 and FT2."

This is structurally distinct from increasing `max_decode_passes` to 4
in the standard multipass loop. The difference: in standard multipass,
each pass runs against the residual *as left by the prior standard
pass*. The 4th-pass-after-a7 runs against the residual as left by a7,
which has access to AP-seeded candidates the standard pipeline cannot
reach. The 4th pass therefore sees a qualitatively different residual.

## Algorithm description (PROSE ONLY — no code)

### Inputs

The algorithm is a small modification to the existing multipass loop.
Inputs:

- **Audio buffer for the current window** (12 kHz mono float; FT8
  standard rate, or 6 kHz for FT4 — the specific mechanism is the same).
- **Existing decoder state**: the accumulated decode list, the per-pass
  residual audio, and the standard pipeline's "passes" counter
  (1, 2, 3 for the standard passes; pancetta's `Ft8Config::max_decode_passes`).
- **a7 seed table**: opposite-parity prev-window decode list, per
  cross-sequence a7 spec.
- **`enable_a7_4th_pass: bool` flag** (new config). Default behavior
  per WSJT-X Improved is to enable this for FT4 and FT2; pancetta
  should expose the flag for FT8 too with a sensible default.

### Outputs

A potentially-larger list of accepted decodes. Each 4th-pass-attributed
decode is provenanced as `Standard4thAfterA7` (or equivalent) so
downstream consumers can tell it apart.

### Steps

1. **Run passes 1 through N** (standard pipeline), where N is the
   configured `max_decode_passes`. Each pass:
   - Searches the residual audio.
   - Decodes candidates via BP / OSD.
   - Subtracts accepted decodes from the residual.

2. **Run a7 (cross-sequence AP pass)** as detailed in
   spec-wsjtr-cross-sequence-a7.md:
   - Iterate over seed entries from the previous opposite-parity window.
   - For each seed, enumerate ~206 candidate messages and pick the
     best match by weighted-Hamming distance.
   - Accept any seed whose `dmin ≤ 100` and `dmin2/dmin ≥ 1.3`.
   - Subtract each accepted decode from the residual.

3. **Trigger the 4th pass.** If `enable_a7_4th_pass` is true and at
   least one a7 decode was accepted in step 2, invoke ONE additional
   standard pass against the current (post-a7) residual:
   - Re-run the spectrum-candidate search over the residual.
   - For each new candidate, run BP/OSD.
   - Accept any decode that passes the standard pipeline's gates.
   - Subtract each accepted decode from the residual.
   - Mark each accepted decode with provenance `Standard4thAfterA7`.

4. **Skip the 4th pass if no a7 decodes happened.** If a7 produced no
   new decodes, the residual after step 2 is identical to the residual
   after step 1. Running another standard pass at that point would
   re-discover the same candidates the third pass already considered
   and rejected, yielding nothing. Save the CPU.

5. **Run final dedup and acceptance.** The 4th pass's decodes
   merge into the same decoded-message list as the standard passes,
   and the same downstream filters (FP-pattern, callsign-continuity,
   FDR, content score) apply.

### Numerical constants (facts, not expression)

- **Modes confirmed by release notes**: FT4 and FT2. FT8 is not
  explicitly mentioned in the v3.1.0 line ~74 release note. However:
  - The Improved fork's v3.1.0 changelog separates FT8 work (the
    auto-passband baseline, line ~76) from FT4/FT2 work (the 4th
    pass + a7 + sub-sample DT, line ~74).
  - FT8 already has a7 in WSJT-X mainline (and in WSJT-X Improved). The
    4th-pass-after-a7 extension is presumably also applicable to FT8;
    the absence in the FT8 release notes likely means it was already
    in place or the author has not surfaced it.
  - Pancetta is FT8-focused. Pancetta should implement the 4th-pass
    pattern for FT8 *anyway* — the structural logic transfers.
- **Pass count delta**: the standard pipeline N (typically 3) becomes
  N+1 when a7 produces decodes.
- **CPU cost**: one additional spectrum-search + BP/OSD pass. On a Fast
  tier host, this is ~50–100 ms per window. On Slow tier, where
  hb-216 forces `max_decode_passes=1`, the 4th pass would not run
  anyway because pancetta should suppress it on Slow tier.

### Edge cases

- **a7 produced 0 decodes.** Skip the 4th pass entirely (step 4). No
  CPU spent.
- **a7 produced N decodes but all at the same frequency as standard
  pass decodes.** The subtraction is a no-op (the standard pass had
  already subtracted that signal), so the residual is unchanged, and
  the 4th pass discovers nothing new. Cheap and harmless.
- **a7 decode at a previously-unseen frequency.** The expected win
  scenario: a7 unmasks a previously-hidden signal nearby. The 4th
  pass discovers that signal in the cleaner residual.
- **Cascading 4th-pass decodes.** Should the 4th pass's decodes
  themselves trigger another a7 pass + 5th pass + ...? Per release
  notes (v3.1.0 line ~74), the answer is no — exactly one a7 pass and
  exactly one subsequent standard pass. The pipeline is bounded and
  deterministic.
- **a7 false-positive seed.** If a7 erroneously accepts a wrong-text
  decode and subtracts it, the residual now contains a "negative ghost"
  of the wrong signal — which may trigger spurious decodes in the 4th
  pass. The mitigation is the same as for a7 itself: the
  `dmin/dmin2/CRC` gate keeps a7 false positives rare. The 4th pass's
  downstream FP filters (callsign continuity, etc.) catch the residual
  fallout.
- **`max_decode_passes` already set to 4 in `Ft8Config`.** The
  standard pipeline runs 4 passes regardless of a7. Adding the
  4th-pass-after-a7 layer means the total can reach 5. This is fine —
  the mechanism is additive — but the operator should be informed via
  the config docs.

## Conflict with pancetta's existing mechanisms

- **Pancetta does not yet have a7.** The cross-sequence a7 spec
  (spec-wsjtr-cross-sequence-a7.md) is queued; the 4th-pass-after-a7
  mechanism depends on it. Ship a7 first; the 4th pass is a small
  follow-on (~30 LOC).

- **`Ft8Config::max_decode_passes` semantics overlap.** Pancetta's
  current `max_decode_passes` controls the standard pipeline. The
  4th-pass-after-a7 is *not* a generic +1 to that count — it is
  specifically a pass that runs AFTER a7 with the express purpose of
  re-mining the a7-cleaned residual. The two are distinct config
  knobs:
  - `max_decode_passes`: standard passes. hb-216 Slow tier forces 1.
  - `enable_a7_4th_pass`: bool. Default `true` on Fast/Moderate, `false`
    on Slow.

- **Synergy with hb-091 (scoped fast path) and hb-216 (tier).** Slow
  tier: a7 + 4th pass both disabled by tier policy. Moderate: a7
  enabled, 4th pass enabled. Fast: same as Moderate. The tier rewrite
  logic in `coordinator/tier.rs` should set both flags.

- **No conflict with the FP filter line (hb-058/hb-062/hb-103, FDR).**
  4th-pass decodes pass through the same downstream filters as
  standard-pass decodes. The `provenance: Standard4thAfterA7` field
  is informational (for telemetry / debugging) but does not bypass any
  gate.

- **Interaction with hb-217 (RR73 fix).** Positive. RR73 is a common
  short message that often appears late in QSO sequences. With a7
  enabling QSO-context-seeded recovery of opposite-parity RR73, plus
  the 4th pass mining the now-cleaner residual, recall on QSO closing
  messages improves further.

- **Interaction with hb-218 (capture-effect joint decoding, deferred).**
  Strongly positive. Capture-effect-locked signal pairs often resolve
  one signal in a standard pass; a7 then has clean context to seed a
  matching reply; the 4th pass mines the doubly-cleaned residual. Each
  layer compounds.

## Estimated Rust port effort

- New code:
  - Conditional 4th-pass invocation in `pancetta-ft8/src/decoder.rs`
    (or wherever the multipass loop lives). The conditional is simple:
    "if a7 produced at least one decode AND enable_a7_4th_pass is true,
    run one more standard pass on the post-a7 residual." ~30 LOC.
  - Provenance variant on `DecodeProvenance` enum: `Standard4thAfterA7`.
    ~5 LOC.
  - `enable_a7_4th_pass: bool` on `Ft8Config`. ~5 LOC.
  - Tier wiring in `coordinator/tier.rs`: default off on Slow. ~10 LOC.
  - Tests: synthetic two-signal WAV where standard passes find one,
    a7 finds another, and the 4th pass finds a third. ~60 LOC tests.
- Total: ~50 LOC + ~60 LOC tests.
- 1 iter session:
  - Wiring after a7 ships, hard-200 eval pass to confirm yield bump,
    bootstrap-CI, ship/shelve.

## Implementation notes for the implementer thread

- **This is a small follow-on to a7.** Do NOT attempt to implement the
  4th pass without a7 in place. The whole point of the pass is to
  exploit a7's residual cleaning.

- **Bound the cascading.** The mechanism is exactly one additional
  pass. Do not recurse (no 5th pass after the 4th pass's decodes).
  Per release notes, the upstream design is bounded and deterministic.

- **Reuse the existing pass driver.** The 4th pass is a standard
  decode pass — same spectrum search, same BP/OSD, same acceptance
  gates. Call the existing `run_one_pass` function (or equivalent) one
  more time with the post-a7 residual. Do not duplicate the per-pass
  logic.

- **Skip when a7 produced nothing.** Save CPU. Test this early in the
  fast path to keep the typical-window cost unchanged.

- **Provenance is informational.** The `Standard4thAfterA7` tag is for
  telemetry, debugging, and operator visibility ("how many 4th-pass
  decodes are we getting on this band?"). It does not change downstream
  filter behavior.

- **Tier gating.** Slow tier: `enable_a7_4th_pass = false` (a7 itself
  is also disabled on Slow per the cross-sequence a7 spec). Moderate
  + Fast: enabled by default.

- **Calibration.** After a7 ships and the 4th pass lands, run a
  hard-200 eval comparing:
  - Baseline: a7 enabled, 4th pass disabled.
  - Treatment: a7 enabled, 4th pass enabled.
  Bootstrap-CI on yield delta and FP-rate delta. A clean win is
  > +2% yield with no FP-rate regression.

- **FT8 vs FT4 vs FT2 scope.** The release notes specifically mention
  FT4 and FT2 for v3.1.0. Pancetta is FT8-only; the mechanism transfers
  cleanly. Implement for FT8.

- **Citation hygiene.** Cite as "inspired by WSJT-X Improved v3.1.0
  4th-pass-after-a7 (DG2YCB)" in the journal entry. The pattern (one
  extra mining pass after AP-driven residual cleanup) is general
  iterative-decoding wisdom; the specific WSJT-X Improved instantiation
  is the inspiration. Pancetta's implementation is independent.
