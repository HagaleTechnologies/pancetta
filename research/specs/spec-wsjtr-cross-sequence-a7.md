# Algorithm spec: cross-sequence A7 — opposite-parity callsign AP decoding

## Source attribution
- Origin: wsjtr (https://github.com/bodiya/wsjtr)
- Primary doc (traceability only, NOT quoted): `docs/cross_sequence_decoding.md`
- Secondary docs: `docs/wsjtr.md` §"Cross-Sequence (A7) Decoding (Feb 2026)"
- Code paths cited for traceability only, NOT to be read by implementer:
  `crates/wsjtr/src/sequence.rs` (sequence state),
  `crates/wsjtr/src/main.rs` (window-loop wiring),
  `crates/jt9r/src/a7.rs` (decode engine),
  `crates/jt9r/src/wsjt_ft8.rs` (fine sync / soft-symbol extraction reused by a7)
- Upstream lineage: this is wsjtr's port of WSJT-X `ft8_a7.f90` (`iaptype=7`),
  available in mainline WSJT-X since v2.6.0 (Jun 2022).
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

FT8 QSOs are an alternating sequence of 15-second transmissions: station A in
even-parity slots, station B in odd-parity slots. After station A is decoded
in a window of parity X, station B's reply is *expected* in the very next
window (parity ~X). At that point, the decoder knows — with high prior —
which callsign(s) will appear in the next window.

The cross-sequence A7 algorithm capitalizes on this. It maintains a per-parity
memory of the previous window's standard decodes, and in the *next* window of
the same parity (i.e., 30 seconds later), iterates over the previous window of
the **opposite** parity's decoded callsigns. For each remembered callsign-pair
it enumerates the small set of canonical reply messages the FT8 protocol
allows ("call1 call2", "call1 call2 RRR/RR73/73", signal report variants), and
tries to match each as a hypothesis against the current window's residual
audio. Because both callsigns are constrained, the effective message search
space collapses from ~2^77 to ~2^8 (about 206 candidates per seed entry), which
buys roughly 6–8 dB of effective coding gain.

The mechanism rescues weak QSO partners — the station calling back at -20 dB
to a CQ that was decoded normally at -5 dB — that the standalone BP/OSD
pipeline can never pull out of the noise.

Pancetta's prior `a7.rs` (hb-048) only enumerated *within-window* templates
rooted at a callsign decoded in the SAME slot. Cross-sequence A7 is a strictly
different mechanism that uses the **previous** slot's decodes as the seed
set, and that turns out to be a much stronger AP source because of the
parity-alternation structure of QSOs.

## Algorithm description (PROSE ONLY — no code)

### Inputs

The algorithm operates on a per-window basis. Inputs at each invocation:

- **The current window's residual audio** (12 kHz mono float). "Residual"
  means the audio after all confirmed BP/OSD decodes from this window have
  been subtracted out. A7 runs as a post-pass cleanup over what the
  standard pipeline left behind.
- **A persistent decode-history structure** carried across windows, holding
  per-parity tables of recent decodes. Conceptually:
  - `current[Even]`, `current[Odd]`: decodes accumulated in the window
    being processed right now (filled as decodes occur).
  - `prev[Even]`, `prev[Odd]`: the most-recent *completed* window's
    decodes for that parity. These are the AP seed sets.
- **Sequence/parity classifier**. Each 15-second window is even or odd
  by the wall-clock convention: parity = `(utc_seconds / 15) mod 2`,
  where `0` = even (`:00`, `:30`), `1` = odd (`:15`, `:45`).
- **The current window's already-built decode context**, specifically
  the cached 192 k-point FFT of the window's residual (so per-candidate
  baseband extraction is cheap). Pancetta should reuse whatever cached
  FFT structure its multipass decoder already builds.

### Outputs

A list of new accepted decodes (callsign-pair messages with `dt`, `freq`,
SNR estimate, and the underlying tone sequence). Each accepted decode is
also fed back into the decode-history structure under the current window's
parity, so cascading A7 decodes within the same window can chain.

### Steps

1. **Window-boundary maintenance.** When a new window begins, classify
   its parity. For that parity, move `current[parity]` → `prev[parity]`
   and clear `current[parity]`. The opposite-parity tables are left
   untouched. This is the only inter-window state operation.

2. **Save filter — what gets remembered.** Each time the standard pipeline
   produces an accepted decode in the current window, attempt to parse the
   message text into a (call1, call2, grid4) triple, with these filtering
   rules:
   - Reject messages that contain `/` (portable / non-standard suffixes).
   - Reject messages that contain `<` (already-hashed callsigns).
   - Reject messages whose first token is the contest variant `CQ_…`.
   - Accept the standard two-word "callA callB" form and the three-word
     "callA callB grid|report" form.
   - For the three-word `CQ` forms: "CQ DX call" stores `call` as call1;
     "CQ call grid" stores `call` as call1 and `grid` as grid4. (The
     classifier between the two CQ shapes is: if the middle token is
     ≤4 alphanumeric characters it is treated as the DX modifier.)
   - Any other shape is silently dropped from the save table.
   The accepted record stores `dt`, `freq`, `call1`, `call2`, `grid4`,
   and a `skip` flag (initially false) into `current[parity]`.

3. **Duplication suppression on save.** Immediately after saving, walk the
   *opposite-parity* `prev` table. For every entry there whose frequency
   is within ±3 Hz of the new decode AND whose call1 equals the new
   decode's call2 OR whose call2 equals the new decode's call1, set the
   old entry's `skip` flag to true. This prevents A7 from re-trying a
   seed whose corresponding reply has already been decoded by the
   normal pipeline.

4. **A7 trigger condition.** After all standard passes for this window
   are complete (i.e., after the multipass loop and all subtractions),
   look up `prev[opposite_parity]` — the most-recent completed window of
   the OPPOSITE parity. If empty, skip A7 entirely. Otherwise, every
   non-skipped entry is a seed for one A7 decode attempt.

5. **Per-seed attempt — baseband extraction.** For a given seed
   (call1, call2, grid4, prev_dt, prev_freq), downsample the current
   window's residual audio to a 200 Hz complex baseband centered on
   `prev_freq`. The baseband length is the standard FT8 frame length at
   200 Hz (~3200 complex samples for ~16 seconds). Pancetta should reuse
   its existing narrowband downsampler if it already has one; if not,
   the standard approach is: take a 192k-point FFT of the full residual
   once (cached across all A7 attempts), then per-seed extract the
   ±100 Hz band around `prev_freq`, take a 3200-point IFFT, and that is
   the complex baseband.

6. **Per-seed attempt — fine sync.** Within the baseband, search a
   ±10-sample window in time and a ±2.5 Hz window in frequency around
   the seed's `(prev_dt, prev_freq)`. (A smaller refine pass of ±4
   samples / ±0.5 Hz inside the coarse search is also used.) Score each
   candidate offset by Costas-array correlation; pick the best
   `(dt_refined, freq_refined, sync_quality)`. If the sync quality
   falls below a configurable floor analogous to the standalone
   sync_min, reject this seed and continue to the next.

7. **Per-seed attempt — soft-symbol extraction.** At the refined
   `(dt, freq)`, extract the 79-symbol FT8 tone array's complex bin
   amplitudes. Compute four LLR variants. The four variants are the
   amplitude-domain-only set (no power-squared variants): two different
   per-symbol coherence windows (single-symbol coherent vs. block
   coherent) times two different normalization choices. (Power-domain
   variants are intentionally *not* tried in A7 — the AP signal is
   already weak, and amplitude metrics are empirically more reliable in
   that regime.)

8. **Per-seed attempt — candidate enumeration.** Build the candidate
   message set for this seed. The wsjtr enumeration totals **up to 206
   candidates**, structured as follows. Let (c1, c2) iterate over BOTH
   orderings — i.e., `(call1, call2)` and `(call2, call1)`. For each
   ordering:
   - 1 "basic" message: `"c1 c2"` alone.
   - 3 "fixed-completion" messages: `"c1 c2 RRR"`, `"c1 c2 RR73"`,
     `"c1 c2 73"`.
   - 100 SNR-report messages: `"c1 c2 ±NN"` for the 100 signal-report
     values from -50 dB through +49 dB.
   - 100 R-prefixed signal-report messages: `"c1 c2 R±NN"` for the same
     100 values, with the R prefix.
   Skipping the second ordering when call2 is empty (single-callsign
   seeds, e.g., from CQ) reduces the count; in that case the seed yields
   only a small number of partial-information templates.

9. **Per-seed attempt — codeword scoring.** For each candidate string:
   - Pack the message text via the standard 77-bit FT8 packer.
   - LDPC-encode to the 174-bit codeword.
   - Compute the weighted Hamming distance against each of the four LLR
     variants: for every codeword bit position, if the LLR's sign
     disagrees with the codeword bit, add `|LLR|` to a running sum; if
     it agrees, add nothing. The minimum across the four variants is
     this candidate's distance score `d`.
   - Track running first-best `dmin` and second-best `dmin2` across all
     enumerated candidates for this seed.

10. **Acceptance criterion.** After enumeration, the seed produces a
    decode if and only if **all three** of these gates hold:
    - `dmin ≤ 100.0` (the absolute distance to the best candidate is
      small).
    - `dmin2 / dmin ≥ 1.3` (the best candidate beats the second-best by
      at least 30%, ensuring a "clear winner").
    - The best message text does **not** start with `CQ` (any "CQ …"
      decode would be a same-sequence decode and is the standard
      pipeline's job; A7 is for *replies* in the opposite-parity
      window).

11. **Save and cascade.** Any accepted A7 decode is itself fed back into
    `current[parity]` via the same save filter as standard decodes (so
    its callsigns become A7 seeds for the NEXT same-parity window). A
    decode also enables *cascading* within the current window: if a
    later seed's enumeration is processed after this one, the freshly
    added context is already available (but in practice all A7 seeds
    are processed in one batch from `prev[opposite_parity]`, so
    in-window cascade is more conceptual than algorithmic).

12. **Output marking.** Decoded messages produced by the A7 pass should be
    flagged with a distinct provenance marker (wsjtr uses a single-character
    `a` in its print output to distinguish from the `~` of BP/OSD decodes).
    Pancetta's `DecodedMessage` should carry the equivalent — likely a new
    enum value on the existing decode-mechanism tag or a boolean
    `via_cross_sequence_a7` field.

### Numerical constants (facts, not expression)

- Parity formula: `parity = (utc_seconds / 15) mod 2`. Even = `{:00, :30}`,
  Odd = `{:15, :45}`.
- Candidate enumeration upper bound: 206 messages per seed entry
  (2 orderings × (1 basic + 3 fixed + 100 SNR + 100 R-SNR), minus
  duplicates and minus the second ordering when call2 is empty).
- Signal report integer range: `-50` to `+49` inclusive, both for plain
  and R-prefixed variants.
- Acceptance gate, distance ceiling: `dmin ≤ 100.0` (weighted-Hamming
  units; the same scale as the LLR magnitudes).
- Acceptance gate, margin: `dmin2 / dmin ≥ 1.3` (best beats second-best by
  at least 30%).
- Duplication-suppression frequency window: ±3 Hz between save event and
  opposite-parity prev entry.
- Fine-sync search window (coarse): ±10 samples in time, ±2.5 Hz in
  frequency around the seed's (dt, freq).
- Fine-sync search window (refine): ±4 samples in time, ±0.5 Hz in
  frequency.
- Baseband centre frequency: the seed's `prev_freq` from the opposite-parity
  decode.
- Baseband sample rate: 200 Hz (complex), standard FT8 narrowband.
- LLR variant count tried per seed: **4 amplitude-domain variants only**
  (no power-domain variants, unlike the standalone pipeline).
- Output marker: per-window, A7-provenance decodes should be visibly
  distinguishable downstream.

### Edge cases

- **Empty opposite-parity history (cold start).** First window after process
  start: `prev[~parity]` is empty. Skip A7 silently; this is normal and
  recovers itself after the first full minute of operation.
- **Operator QSY mid-minute.** If the operator retuned between adjacent
  same-parity windows, the seed callsigns may be at completely irrelevant
  frequencies. The fine-sync step naturally rejects these (no signal at the
  expected freq → low sync quality → reject). No additional defense is
  required, though pancetta's coordinator may wish to clear the history
  on explicit QSY events.
- **Cascading A7 within a window.** If a A7 seed produces a new decode that
  itself contains a new previously-unseen callsign, that callsign enters
  `current[parity]` and becomes an A7 seed for the NEXT opposite-parity
  window, not this one. The 206-message enumeration is fixed at seed-loop
  start; no rescan is triggered.
- **Same callsign already decoded by the standard pipeline.** The
  duplication-suppression step (step 3) flags the relevant opposite-parity
  seed as skip so A7 never wastes time on it. Without this guard the
  decoder would happily "re-decode" something it already has and waste CPU.
- **Hashed callsigns.** The save filter rejects messages containing `<` so
  hashed callsigns never make it into the seed table. The corresponding
  cross-sequence decode opportunity is forfeited rather than risking a
  spurious decode from an incomplete callsign.
- **AP modes 3–6 not feeding A7.** wsjtr's doc explicitly calls this out as
  a known gap: in WSJT-X, even AP-decoded messages go into the A7 history,
  but wsjtr only feeds BP/OSD + CQ-AP + MyCall-AP decodes. Cascading
  AP→A7 gains are correspondingly diminished. Pancetta's QSO state machine
  (which knows the operator's own callsign and the DX call) can in
  principle close this gap.
- **Non-finite LLRs.** A pathologically loud signal can saturate
  `|LLR|` and yield NaN/Inf in the weighted-distance sum. Guard with a
  finite-check; treat non-finite distance as +∞ (reject).
- **Empty residual or short audio.** If the current window's residual is
  too short for a full 79-symbol frame at the seed frequency, the
  downsample/fine-sync step must gracefully handle the truncation and
  reject the seed rather than panic on out-of-bounds addressing.
- **prev_freq near band edge.** Downsampling at frequencies very close to
  0 Hz or to the upper band limit can produce baseband artifacts from
  filter rolloff. The fine-sync sync_quality gate handles most of these,
  but a configurable absolute frequency-bracket filter (e.g.,
  100 Hz < prev_freq < 4500 Hz) is a reasonable additional guard.

## Conflict with pancetta's existing mechanisms

Pancetta already has an `A7Template` machinery in `pancetta-ft8/src/a7.rs`
that resulted from hb-048. The hb-048 module is **within-slot**: given a
callsign C decoded in slot N, it generates templates rooted at C and
cross-correlates them against slot N's *own* residual. That work was
labeled SHELVED in the hypothesis bank because it surfaced very few
additional truths in pancetta's eval — the within-slot AP is too narrow,
as the slot's standard decode already had access to those candidates.

Cross-sequence A7 is a **different mechanism on the same primitive**. The
template enumeration and cross-correlation primitives from hb-048 can be
re-used; the change is in *where the seed callsigns come from* and *which
slot's residual they're matched against*. Specifically:

- **State sharing.** Cross-sequence A7 requires inter-window state (the
  `prev[Even]/prev[Odd]` tables). Pancetta's decoder is currently stateless
  across windows; the state belongs in `pancetta-qso` (it's QSO-context
  state, not pure DSP state) or in a thin coordinator-level history
  struct. The decoder accepts this state as an input.

- **Acceptance gate change.** Pancetta's hb-048 uses snr7 / snr7b
  (best vs. second-best signal-to-noise correlation). wsjtr's
  cross-sequence A7 uses `dmin` (weighted Hamming distance to the best
  codeword) and `dmin2/dmin` (margin). Adopting the wsjtr-style
  `dmin ≤ 100 AND dmin2/dmin ≥ 1.3` is recommended — it ties directly
  into the LLR scale that pancetta's BP already produces, and the
  exact constants are well-tested in WSJT-X mainline.

- **Cost shape.** Per-window cost is `O(seeds_in_prev_opposite_parity ×
  ~206)`. On a quiet band with 5 prev decodes this is ~1000 candidate
  scorings — negligible. On a crowded band with 50 prev decodes this is
  ~10,000 — still fast (each scoring is a pack+ldpc-encode plus a
  174-element distance computation, on the order of microseconds). The
  bigger cost is the per-seed baseband extract + fine-sync, which is
  ~50× more expensive. Total: tens of milliseconds per window. Well
  within Slow-tier budget.

- **FP risk.** Cross-sequence A7 *can* produce false decodes if the
  seed callsigns themselves were FPs in the previous window. In that
  case the AP would inject fictitious "expected" templates into the
  current window's residual and any false match would surface as a
  high-confidence FP. Pancetta's existing FP-filter pipeline (hb-052
  callsign continuity, hb-058 /R filter, hb-103 content score) sits
  downstream of A7's accept gate and should catch most such cases, but
  pancetta should consider gating A7 seed acceptance on the seed's own
  confidence (e.g., only seed from decodes with `confidence ≥ medium`
  or that survived hb-052's trust check).

- **Interaction with hb-217 RR73 fix.** Mildly positive. The RR73 fix
  improved pancetta's RR73 recall ~150-fold. RR73 messages decoded in
  window N are now reliable seeds for cross-sequence A7 in window N+1,
  whereas before hb-217 they were largely missing from the seed table.

- **Interaction with hb-091 scoped fast path / hb-216 tier.** A7 is an
  optional cleanup post-pass. On the Slow tier (where hb-216 already
  forces `max_decode_passes=1`, `osd_depth=Some(1)`), A7 should be
  *disabled* by default — the seeds are unreliable and the CPU saved
  matters. On Moderate and Fast tiers, A7 should be enabled. A new
  tier-driven flag `enable_cross_sequence_a7: bool` on `Ft8Config`
  fits the existing pattern.

- **Interaction with hb-115 / hb-100 / hb-218 (capture-effect line).**
  Orthogonal but mutually beneficial. Joint-decode work on
  capture-locked pairs can produce additional decodes that feed the
  seed table, and cross-sequence A7 can recover the QSO replies to
  those joint-decoded signals.

## Estimated Rust port effort

- New module(s):
  - `pancetta-qso/src/cross_sequence.rs`: `SequenceHistory`, `Sequence`
    enum, `A7Entry` struct, save/advance/get-context APIs.
    ~150 LOC + ~80 LOC tests.
  - Extension to `pancetta-ft8/src/a7.rs`: add a
    `generate_cross_sequence_templates(seed, ...) -> Vec<A7Template>`
    helper that enumerates the 206-candidate set described in step 8.
    ~120 LOC + ~60 LOC tests.
  - New decoder entry point in `pancetta-ft8/src/decoder.rs`:
    `try_cross_sequence_decodes(ctx, residual, seeds) -> Vec<DecodeResult>`.
    Reuses existing baseband-extract, fine-sync, soft-symbol extraction.
    ~150 LOC + ~80 LOC tests.
  - Coordinator wiring in `pancetta/src/coordinator/`: feed standard
    decodes into the history, call A7 after the multipass loop, fold
    A7 decodes back into the same downstream pipeline as BP/OSD
    decodes. ~80 LOC + ~50 LOC tests.

- Total: ~500 LOC + ~270 LOC tests.

- 4–6 iter sessions (matching the existing hb-237 design):
  1. SequenceHistory + tests in pancetta-qso.
  2. Cross-sequence template enumeration in pancetta-ft8::a7.
  3. Decoder-side cross-correlation entry point.
  4. Coordinator wiring (history maintenance + A7 invocation).
  5. Hard-200 eval + bootstrap-CI.
  6. Ship/shelve decision.

## Implementation notes for the implementer thread

- **State lives in pancetta-qso, not pancetta-ft8.** The decoder should be
  stateless across slots. The history struct lives in pancetta-qso (it's
  QSO-context state). The decoder API takes a borrow of the prev opposite-
  parity Vec<A7Entry> as input; it does not mutate the history. After
  decoding, the coordinator is responsible for calling `history.save(...)`
  for both the standard decodes AND any new A7 decodes.

- **Reuse hb-048 primitives.** `A7Template` and `cross_correlate()` in
  pancetta-ft8::a7 already exist (from hb-048). The new code adds a
  *different enumeration* (cross-sequence) but reuses the same primitive
  for matching. Do NOT duplicate the cross-correlation code.

- **Adopt the dmin / dmin2 gate.** Don't try to translate the hb-048
  snr7 / snr7b gate into something that "feels equivalent." The wsjtr
  gate is `dmin ≤ 100 AND dmin2/dmin ≥ 1.3` on weighted Hamming distance
  in LLR units. That is well-tested at WSJT-X scale and pancetta's BP
  already produces LLRs on a compatible scale. Use it directly.

- **Save filter is critical for FP avoidance.** Implement the filter in
  step 2 carefully — reject `/`, `<`, and `CQ_…`; only save standard
  two-callsign and CQ shapes. Pancetta's `Ft8Message` parser already has
  most of this introspection; the cross-sequence save filter can be a
  thin adapter.

- **Provenance marker on output.** Add a `DecodeProvenance` enum (or a
  field on `DecodedMessage`) with at least the variants `Bp`, `Osd`,
  `Ap`, `CrossSequenceA7`. Print this through the existing coordinator
  log path so operators can tell A7 decodes apart. Pancetta's hb-103
  content-score gate may want to special-case `CrossSequenceA7` (the
  decoded message is structurally a reply; some content-score features
  apply differently).

- **Seed quality gate.** For pancetta's FP-paranoid culture, gate the
  save filter on `confidence ≥ medium` (or whatever the equivalent is)
  *in addition to* the standard message-shape filter. A FP seed
  produces a worse outcome than no seed at all.

- **Tier gating.** Add `enable_cross_sequence_a7: bool` to `Ft8Config`,
  default `true` on Fast/Moderate, `false` on Slow. The hb-216 tier
  initializer already rewrites Ft8Config under the shared RwLock;
  extend it to set this field.

- **Test fixture.** A clean end-to-end test is: take two consecutive
  WAV files from the same QSO (one even-parity, one odd-parity). Decode
  the first normally. Feed callsigns to the history. Decode the second
  with A7 enabled and confirm that at least one A7 decode appears that
  was NOT in the second WAV's standalone decode result. The "QSO test
  fixtures" Pancetta already uses for loopback tests should suffice.

- **Don't try to implement AP modes 3-6 at the same time.** wsjtr
  notes that A7's full headroom depends on AP modes 3-6 *also* feeding
  the seed table. That is true but a separate work item; ship A7
  first with only BP/OSD + CQ-AP + MyCall-AP feeding it, then add AP
  modes 3-6 as a follow-on. Pancetta's QSO state machine is the
  natural place to do AP modes 3-6 since it already knows the operator
  callsign and the DX call.

- **Citation hygiene.** Cite as `wsjtr-inspired (cross-sequence A7)`
  in the journal entry. The original mechanism is in WSJT-X
  `ft8_a7.f90` (wsjtr's reference); cite "WSJT-X v2.6.0 cross-sequence
  decoding (Franke/Taylor)" if a primary academic source is needed.
