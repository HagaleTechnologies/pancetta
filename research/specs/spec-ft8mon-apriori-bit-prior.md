# Algorithm spec: A-priori bit-probability prior fused into initial LLRs

## Source attribution
- Origin: ft8mon
- File paths in `ft8.cc`:
  - ~386–407: the constant `apriori174` array literal
  - ~1633–1692: the `bayes()` function that fuses the prior into LLRs
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Real-world FT8 traffic is *not* uniformly distributed over the 174-bit
LDPC codeword space. Specific bit positions correspond to specific
fields of the parsed FT8 message (callsign hash, message type bits,
grid encoding, etc.), and the empirical distribution of those bits
across all valid messages on the air is heavily skewed at many
positions. As a concrete example, many message-type bits are nearly
always the same value across all CQs, and certain callsign-hash bit
positions cluster because callsigns themselves are not uniformly
distributed. Encoding this empirical bias as a 174-element prior and
folding it into the per-bit LLR before the LDPC belief-propagation
iterations gives the decoder a modest precision/recall edge on common
message shapes (CQs, grid responses, RR73s). The literature claim is
small but real: typical gains are a few percent on common message
types and approximately zero on uncommon ones.

**Critical for clean-room extraction**: ft8mon's specific 174 numeric
values are corpus-derived from the ft8mon author's training set. Those
specific values cannot be lifted. Pancetta needs to derive its own
table from its own corpus. This spec describes (a) the *mechanism* for
fusing the prior into the LLR computation, and (b) the *extraction
methodology* for producing the table. The numbers themselves are an
implementation detail to be re-derived.

## Algorithm description (PROSE ONLY)

### Inputs
- A 174-element table `apriori174[i] ∈ [0, 1]` representing the empirical
  P(bit i = 1) across a large corpus of validated FT8 frames.
- The per-slot empirical statistics on observed tone magnitudes (the
  `Stats` distributions for "signal" and "noise" magnitudes that the
  existing soft demodulator builds during its sweep).
- The two per-bit "best" magnitudes from the soft demodulator:
  `best_zero[i]` and `best_one[i]` — the strongest tone-magnitude
  evidence for the bit being 0 and 1 respectively, derived from the
  79-symbol scan in the same way the existing single-symbol soft demod
  produces them.

### Outputs
- A 174-element `f32` LLR vector — same shape as the existing soft
  demod output — but with the prior fused in. Drop-in replacement for
  the current initial LLRs handed to LDPC belief propagation.

### Steps — fusion mechanism (the `bayes()` function)

For each bit position `i` from 0 to 173:

1. **Compute the likelihood of the observed evidence under each
   hypothesis.**
   - `p0 = P(observe magnitude ≥ best_zero[i] | bit = 0)` — i.e. the
     probability under the "signal at the bit=0 tone" hypothesis that
     the demodulator would see something at least as strong as
     `best_zero[i]`. Read this off the slot-local `signal` distribution.
   - `q0 = P(observe magnitude ≥ best_zero[i] | bit = 1)` — i.e. the
     probability under the alternate hypothesis. Read this off the
     slot-local `noise` distribution.
   - Compute `p1`, `q1` symmetrically using `best_one[i]`.

2. **Convert the empirical probabilities into prior odds.**
   - `prior_0 = 1 − apriori174[i]` (prior probability bit = 0)
   - `prior_1 = apriori174[i]`     (prior probability bit = 1)

3. **Apply Bayes' rule to get posterior odds.**
   The posterior probability that bit = 0, given the evidence, is
   proportional to `prior_0 × p0 × q1` (prior × evidence-for-zero ×
   evidence-against-one). Similarly for bit = 1:
   `posterior_1 ∝ prior_1 × p1 × q0`. The ratio is the posterior odds:

   ```
   odds(bit=0 / bit=1) = (prior_0 × p0 × q1) / (prior_1 × p1 × q0)
   ```

4. **Take the natural log to produce the LLR.**

   ```
   LLR[i] = ln(odds) = ln(prior_0/prior_1) + ln(p0/p1) + ln(q1/q0)
   ```

   This is the additive-log-domain form. Note that the prior is just an
   additive bias on the LLR: `ln((1 − apriori174[i]) / apriori174[i])`.

5. **Apply this fusion only once, at initialization.** The prior bias is
   added to the initial LLR before the LDPC + belief-propagation
   iterations begin. It does **not** modify message updates *during*
   BP. After BP converges or hits its iteration limit, the standard
   CRC validation gates whether the codeword is accepted; the prior
   doesn't participate in CRC.

### Steps — extraction methodology (how to build `apriori174[]` for pancetta)

This is the part pancetta must redo from scratch (the ft8mon values are
GPL-licensed corpus output and not portable).

1. **Collect a large corpus of validated FT8 decodes.** Pancetta's
   research harness already has the `hard-200` corpus and a broader WAV
   archive; the larger corpus is desired here (tens of thousands of
   frames at minimum). Use `pancetta-research`'s evaluation pipeline,
   not a third-party tool — keeps the corpus license-clean.

2. **Filter to LDPC + CRC validated decodes only.** No OSD-only
   decodes, no marginal sync decodes; the goal is to characterize the
   bit distribution of *correct* codewords, so every input frame must
   have a verified CRC.

3. **For each validated decode, extract the 174-bit corrected
   codeword** (after LDPC correction but before any post-processing).

4. **Count per-bit 1-frequencies.** Maintain a 174-element `u64`
   counter array. For each codeword, increment counter `i` for every
   bit position where the codeword has bit `i` = 1.

5. **Divide by total codeword count** to obtain
   `apriori174[i] = ones_count[i] / total_count` in `[0, 1]`.

6. **Apply Laplace smoothing** to avoid zero or one extremes:
   `apriori174[i] = (ones_count[i] + 1) / (total_count + 2)`.
   This bounds the prior away from the asymptotes and prevents
   numerical instability when a bit position is fully one-sided in the
   training corpus.

7. **Optionally segment the prior by message type.** Different FT8
   message types (CQ, grid response, RR73, free text) have very
   different bit distributions in the structured fields. ft8mon uses a
   single global prior; pancetta could improve on this by maintaining
   a per-message-type prior and selecting at decode time based on the
   tentative parse. Mark as a follow-on experiment; start with a single
   global table.

8. **Store as a static const in the pancetta-ft8 crate**, with a
   comment recording the corpus name, sample size, and generation date.

### Numerical constants (facts, not expression)
- 174 codeword bits → 174 prior entries.
- Range: `apriori174[i] ∈ [0, 1]`, most values near 0.5 (uncertain),
  with a notable subset deviating to extreme values reflecting
  structural FT8 message biases.
- ft8mon's table is from "ft8-n4" corpus per source comment; pancetta
  derives its own — do not lift these values.
- Laplace smoothing: prior in `[1/(N+2), (N+1)/(N+2)]` for a corpus of
  size N.

### Edge cases
- **Apriori = 0 or 1 exactly**: would produce infinite log-odds bias.
  Laplace smoothing in extraction prevents this. Additionally, in the
  fusion step clamp `prior_0` and `prior_1` to `[eps, 1-eps]` with
  `eps = 1e-6` for belt-and-suspenders safety.
- **Corpus bias**: if the training corpus over-represents one band, one
  region, or one contest, the prior will be biased toward that
  population. Pancetta's eval corpus is global hard-200, which mitigates
  this. Document corpus provenance alongside the table.
- **Re-derivation cadence**: as FT8 conventions evolve (new contest
  exchanges, new message formats, new propagation patterns), the prior
  drifts. Re-derive periodically — e.g. annually, or whenever the
  research harness's broader WAV archive grows by a significant
  fraction.
- **Disable switch**: a `use_apriori: bool` flag (ft8mon equivalent
  defaults true) lets the operator turn the prior off for A/B tests
  or for forensic decoding where the operator wants no statistical
  bias.
- **Interaction with OSD**: OSD operates on the LLR vector. The prior is
  applied to the initial LLRs *before* the OSD pathway is invoked, so
  OSD also benefits from the prior — this is the desired behavior, but
  it does mean the prior compounds with OSD's reliability ranking. No
  additional handling needed; just be aware of it during eval.

## Conflict with pancetta's existing mechanisms

The prior fusion happens at the same step where pancetta's existing
single-symbol soft demod produces its initial LLR vector. The
modification is local to the demod output: add the per-bit log-odds
bias term `ln((1 − apriori174[i]) / apriori174[i])` to each LLR before
handing to LDPC + BP. No structural change to LDPC, BP, OSD, or CRC.

If the pair-decode pathway (see `spec-ft8mon-soft-decode-pairs.md`) is
also implemented, the same prior bias should be added to **its** output
LLRs as well — same code path, same constant, called from both demod
producers.

Interaction with the existing FP filters (hb-058 /R, hb-062
callsign-continuity, hb-103 content score, hb-217 RR73 fix): the prior
operates pre-CRC, so any decode it produces still has to pass CRC. The
hb-* FP filters apply post-CRC. The prior shouldn't create new FPs that
the existing filters wouldn't catch — but if eval shows a regression on
FP rate, investigate whether the prior is helping CRC pass on noise
windows that should have failed.

The autonomous TX path uses `hb-103` at `SHIP_CONSERVATIVE` — the prior
might shift the corpus statistics that hb-103's content score depends
on, so re-eval hb-103 thresholds after the prior is shipped.

## Estimated Rust port effort

- Extraction tool: ~150–200 LOC in `pancetta-research/src/bin/build_apriori.rs`
  or similar. Iterates the validated-decode corpus, accumulates counts,
  writes the table.
- Fusion code: ~30–50 LOC in `pancetta-ft8/src/decoder/soft_decode.rs`
  to add the per-bit log-odds bias. Trivial change once the table is
  available.
- 2 sessions: (S1) build the extraction tool, run it on the corpus,
  hand-eyeball the resulting table for sanity (most bits near 0.5,
  message-type bits sharply biased); (S2) wire the fusion into the
  soft demod output, eval on hard-200, measure precision/recall delta
  by message type.

## Implementation notes for the implementer thread

- The prior is just a 174-element constant; embed as
  `pub static APRIORI174: [f32; 174] = [...]` in the soft-demod module
  with a generation-date and corpus-name comment above it.
- The fusion is a single line per bit: `llr[i] += LN_PRIOR_BIAS[i]`
  where `LN_PRIOR_BIAS[i] = ln((1 - APRIORI174[i]) / APRIORI174[i])`.
  Precompute the log-bias table once at module init so the hot path is
  pure addition.
- Add a `Ft8Config::use_apriori: bool` (default `true`) so operators can
  A/B test.
- Keep the extraction tool reproducible: take a corpus directory and a
  seed as inputs, emit a deterministic table. The Laplace-smoothed
  formula is closed-form, so reproducibility comes for free as long as
  the corpus listing is sorted.
- Document the corpus and date in a `// generated from
  /path/to/corpus on YYYY-MM-DD, N validated decodes` comment above
  the constant. This is the single most important piece of provenance
  for clean-room defensibility.
- Unit test: synthesize a 174-bit codeword at a known LLR; apply the
  prior; verify the LLR shifted by the expected log-odds amount.
- Eval: split hard-200 by message type (CQ vs grid vs report vs RR73 vs
  free-text). The prior is expected to help CQ/grid/report/RR73 and be
  approximately neutral on free-text. Confirm this shape; if free-text
  *regresses*, the prior is over-fit to structured messages and you may
  want to gate it off when the tentative parse looks free-text.
- Optional follow-on: per-message-type priors selected by the tentative
  parse pre-LDPC. Adds complexity; defer until the single-global prior
  is shipped.
- Bank entry suggestion: `hb-NEW apriori-bit-prior (priority 0.4,
  ~modest +TPs on structured messages, 2 sessions)`.
