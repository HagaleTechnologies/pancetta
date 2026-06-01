---
slug: 2026-06-01-architectural-ideation
mode: ft8
state: ideation
created: 2026-06-01T16:50:00Z
last_updated: 2026-06-01T16:50:00Z
branch: iter/2026-06-01-ideation-architectural
category: architectural
parent_thread: hb-086 V2 / V3 / hb-087 / hb-088 shelf cluster (2026-05-30 → 2026-05-31)
generated_by: claude-opus-4-7 (1M context) under brainstorming skill
n_ideas: 14
target_for_aggregator: research/hypothesis_bank.md (DO NOT EDIT FROM HERE — aggregator merges)
---

## Framing

Every shelved hypothesis from the past 48 hours bumps the same wall:
**pancetta's pipeline is hard-decision at every stage**, and a successful
decode passes through three sharpening filters (BP → CRC-14 →
plausibility) before becoming a `DecodedMessage`. That `DecodedMessage`
is a point estimate. The soft-cancellation family (hb-086 V2, hb-081)
needs *distributional* output from upstream and gets delta functions.
The bypass-Costas family (hb-087, hb-088) needs *correlated-with-truth*
LLRs at sub-threshold positions and gets coin-flip noise. The marginal-
tail family (hb-064 DIA-OSD, hb-073, hb-057) needs OSD to *generalise*
beyond the 91-info-bit flip neighborhood and gets enumeration limits.

The ideas below challenge that hard-decision architecture itself, in
several directions. They are NOT a re-spawn of any shelved mechanism;
they replace pipeline stages or invert the information flow. Wild cards
(no defensible prior) are explicitly flagged.

Cross-cutting principles applied throughout:

- **Distributional output**: any new stage emits a probability over
  outcomes, not a point.
- **Late binding**: hard decisions deferred until the latest possible
  stage (often: never within the decoder, exported to the QSO layer).
- **Asymmetric cost gates**: cheap diagnostic kills before expensive
  implementation (V3 doctrine).
- **Distinct from shelved**: each idea attacks a pipeline stage no
  shelved hypothesis touched, OR attacks the same stage with a
  mechanism that's structurally orthogonal to what was tried.

---

## A1 — Soft-output decoder with codeword-posterior export

### Mechanism

Replace the boolean output of LDPC+CRC+plausibility with a *list of
candidate codewords each annotated with a posterior probability*. After
BP converges (or hits max-iter), instead of taking the hard projection
and checking CRC, run a **list-decoding** pass: enumerate the top-K
codewords by post-BP belief sum, check CRC on each, and emit the full
CRC-passing subset as `(codeword, posterior)` tuples. K is tunable
(start K=8). The single CRC-passer becomes posterior=1.0 (current
behavior); multiple CRC-passers split posterior by belief mass.

Downstream (multipass subtract, FP filter, QSO layer) consumes the
posterior. Multipass subtracts the **MMSE estimate** of the tone
pattern (a weighted average of candidate tone patterns), not the hard
hypothesis. The FP filter takes posterior as a continuous trust score
input. The QSO layer can hold ambiguous decodes ("either KJ7IFD or
K7IFD with 60/40 split") until a follow-up slot disambiguates.

### Defensible prior

List-decoding for LDPC is well-studied (Vardy & Be'ery 1991 for
Reed-Muller; Hou, Siegel, et al. for LDPC list-BP; ft8_lib's own
secondary-decode pass is a degenerate K=2 case). The novel claim is
exporting the list intact rather than collapsing.

### Assumption challenged

CRC-14 as a *boolean gate* discards the BP belief distribution. CRC-14
provides 14 bits of validation against 174 bits of payload; when two
adjacent codewords both pass CRC (1 in 16384 collision), we currently
keep one arbitrarily. More importantly, a "near-CRC-passer" (1 bit
away, BP-strong) carries information that the current pipeline
destroys at the CRC gate.

### Kill-switch sketch

Diagnostic: on the top-20 hard-200 missed truths, run BP and enumerate
the top-K=8 codewords by belief sum. Measure: (a) fraction of missed
truths where the truth codeword is in the top-K (any K), (b) fraction
where the top-K contains a non-truth CRC-passer (FP risk). PROCEED if
(a) ≥ 15% AND (b) ≤ 5%. If (a) is low, BP isn't even bringing the
truth into its neighborhood — list-export won't help. If (b) is high,
this becomes an FP-generator.

### Effort

Plan-sized (1 spec, 3-4 sessions). Touches BP post-processing,
multipass subtract path, FP filter signature.

### Headline risk

The CRC-14 budget says we get 1 spurious CRC-passer per 16384 random
codewords; list-decoding multiplies our CRC-budget exposure by K.
False-positive rate could climb sharply at the marginal tail.

---

## A2 — Probability-of-existence map (don't pre-commit to candidates)

### Mechanism

Replace Costas's *binary* candidate gate ("score ≥ MIN_SYNC_SCORE →
candidate") with a continuous **probability-of-FT8-signal-here** map
over (time_step, freq_bin, freq_sub). The map is built from Costas
score, broadband SNR estimate, residual coherence after subtract, and
adjacency to existing decodes (a successful decode at (t, f) raises
the prior at nearby cells).

Downstream stages — LDPC, OSD, plausibility — consume the map. OSD
specifically gets a per-position *budget multiplier*: positions with
high prior get higher OSD-order; low-prior positions get OSD-0 only or
are skipped. This replaces today's uniform OSD treatment and the V3
binary-relaxation knob.

### Defensible prior

Bayesian saliency maps in vision (e.g., object-detection RPNs);
probabilistic CFAR in radar. Closer to home: hb-080 multipass already
implicitly uses adjacency ("decoded a strong signal here, residual is
worth re-examining"), but the signal is binary (yes/no rerun), not
continuous.

### Assumption challenged

Costas score is a hard gate. Either you're a candidate or you aren't.
But the relevant downstream question — "how much compute should I
spend here?" — is continuous. We're throwing away gradient and
forcing all downstream stages to re-derive position-priors implicitly.

### Kill-switch sketch

Compute the proposed prob-of-existence map on top-20 hard-200 WAVs
(no decoder change yet — read-only diagnostic). Score it against
ground truth: AUC of "missed truth position has high prob" vs "no
truth at random position has low prob". PROCEED if AUC ≥ 0.75. The
hb-088 sub-Costas LLR sign-agreement work suggests Costas itself is
already information-rich at sub-threshold positions; the question is
whether a continuous integration beats the binary cut.

### Effort

Plan-sized (1 spec, 4-6 sessions). Touches every downstream stage's
budget-allocation logic.

### Headline risk

Adds runtime cost (more candidates surfaced, more OSD spent). The
budget multiplier MUST keep total compute bounded or we tank elapsed.

---

## A3 — Continuous trust-score FP filter (replace boolean gates)

### Mechanism

Today's FP filter is a stack of boolean gates: callsign continuity,
plausibility check, decode-pass selector. Replace with a **calibrated
trust score** in [0, 1] that integrates evidence from: BP belief sum
at convergence, post-CRC margin (how many CRC-passing competitors
existed), Costas sync_score, callsign-continuity match strength
(string-distance, not hard match), and recency-of-callsign in the WAV
window. The final emit decision is `trust ≥ τ`; τ is tunable per
operational mode (rare-DX-hunt: low τ, accept FPs; logging: high τ).

The trust score becomes part of the `DecodedMessage` API. Downstream
QSO layer can use it for confidence weighting (e.g., don't auto-call
back to a trust=0.3 decode of a CQ).

### Defensible prior

Calibrated classifier output is standard in ML (Platt scaling,
isotonic regression). FT8 false-positive scoring is essentially a
binary classifier; we're operating it at a single threshold today.

### Assumption challenged

"FP filter" is a binary funnel: passes or doesn't. But the operator's
real cost function depends on *what they do with the decode*. A
log-only decode tolerates more FP risk than an auto-response decode.
The current architecture can't express this distinction.

### Kill-switch sketch

Train a calibrated classifier on existing scorecard data (decoded
messages labeled true/false against jt9 baseline). If on hard-200 the
classifier doesn't achieve AUC ≥ 0.85, the features aren't separable
enough to beat the current rule stack. PROCEED if AUC ≥ 0.85 AND the
classifier has at least one decision threshold where it dominates the
current rule stack (higher TP at same FP).

### Effort

2-3 sessions for first cut (classifier training + threshold tuning +
DecodedMessage API change). Plan-sized if QSO layer threading is in
scope.

### Headline risk

Changes `DecodedMessage` API → ripples to QSO layer, ADIF writer,
TUI. The downstream change cost may dwarf the decoder win.

---

## A4 — Joint multi-candidate decoder (vector decode, not sequence-of-singletons)

### Mechanism

Replace the current "decode candidate i, subtract, decode candidate
i+1" loop with a **joint inverse problem**: given the full spectrogram
S and a candidate set C = {(t_i, f_i)}, solve for tone amplitudes
{a_i,sym} that best reconstruct S under an LDPC-codeword constraint
on each candidate's bits. This is a constrained least-squares problem
with binary-codeword side constraints; solvable via
alternating-minimization (LSQ on amplitudes given fixed codewords →
BP on each candidate given residual after others subtracted) or
ADMM.

The current iterative-subtract pipeline is a *greedy* approximation of
this joint problem. A proper joint formulation could untangle
overlapping decodes that the greedy version loses.

### Defensible prior

Joint source separation in audio (NMF + sparsity). Joint detection in
radar (MIMO). FT8 specifically: this is the "multi-stream separation
BEFORE LLR extraction" path that hb-088's journal flagged as a
structural fix for interferer-dominated sub-Costas positions.

### Assumption challenged

The pipeline is *sequential*: decode-one-then-subtract-then-decode-the-
next. This is a greedy choice. The hb-079 coherent-iterative-subtract
win showed that better residual quality unlocks decodes; the joint
formulation is the limit of "perfect residual quality" at the cost of
optimisation difficulty.

### Kill-switch sketch

Diagnostic: on top-20 hard-200 WAVs with ≥3 overlapping decodes (where
overlapping means freq-bin proximity < 5), run the greedy pipeline AND
a one-step ALS (alternating least squares: re-fit amplitudes given
current decoded codewords, then re-decode each candidate against the
re-fit residual). PROCEED if the ALS step recovers ≥ 5% more decodes
than greedy on the overlap-heavy subset.

### Effort

Plan-sized (2 specs: scoping + production-design). The optimisation
formulation is the hard part; touches the multipass loop core.

### Headline risk

Optimisation may not converge in a budget compatible with real-time
operation. Could be a "research-only diagnostic that informs greedy
heuristics" win rather than a production replacement.

---

## A5 — Decoder fusion at LLR level with jt9 (cross-decoder LLR sum)

### Mechanism

Run jt9 (or ft8_lib) in parallel on the same audio, extract its
internal LLRs (jt9 exposes them in its diagnostic builds; ft8_lib has
them in `decode.c`). For any (time, freq) candidate that both pancetta
and jt9 surface, **sum the LLR vectors** (after scaling to common
units), then run pancetta's LDPC+CRC+plausibility on the fused LLRs.
Decodes that succeed under the fused LLR but failed under either
single LLR are the gain.

This is NOT decoder ensembling at the output level (we already do
that via WSJT-X comparison). It's fusion at the channel-decoding
input, which is where the information content actually lives.

### Defensible prior

Maximal-ratio combining in diversity-receiver systems. LLR addition
is the canonical fusion rule for independent observations of the
same codeword.

### Assumption challenged

jt9 and pancetta are treated as competing implementations. Reframing
them as *complementary observation channels* (different windowing,
different sync, different tone-mag estimators) lets us combine their
strengths.

### Kill-switch sketch

For 20 hard-200 WAVs, capture jt9 LLRs (build jt9 with diagnostic
flag; or use ft8_lib equivalent). For every (time, freq) candidate
present in BOTH decoders, compute (a) pancetta-only LDPC pass rate,
(b) jt9-only, (c) sum-LLR LDPC pass rate. PROCEED if (c) > max(a, b)
+ 5pp. The risk: pancetta and jt9 may have correlated errors at the
same candidate (both use Costas-like sync), in which case LLRs are
correlated and the sum doesn't help.

### Effort

4-6 sessions. The jt9 LLR-extraction tooling is the bulk of the
effort. Could land as a research-only experiment first, productionise
later.

### Headline risk

License entanglement (jt9 is GPL). Production integration may force a
GPL boundary or a re-implementation. Research-only is safer.

---

## A6 — Variational message-set inference (Bayesian decoder)

### Mechanism

Replace LDPC's BP-then-CRC pipeline with a **variational Bayesian
posterior over (callsign1, callsign2, locator)** directly. The
generative model: each FT8 message is a structured object (type,
hash, callsign1, callsign2, suffix); the channel produces tone
magnitudes via the modulation + AWGN model. Variational inference
(e.g., a graphical-model with the structured-message prior) computes
posterior over the *message tuple* rather than the codeword.

Crucially, the prior over message tuples is NOT uniform — it can
incorporate callsign frequency, recent activity, common QSO
exchanges (CQ → grid → report → R-report → RR73). The current
plausibility check is a hard post-hoc filter; in this architecture
the priors are *integrated into inference*.

### Defensible prior

Structured prediction in NLP (CRFs). Bayesian decoding in turbo codes
(BCJR). FT8's message structure is unusually amenable to this — it's
not free-form text, it's a tiny grammar with ~30 bits of effective
entropy after structural constraints.

### Assumption challenged

LDPC is a *channel code*; FT8's *source code* (the message grammar)
is enforced separately as a post-hoc plausibility check. The two are
not jointly inferred. This loses information: a non-CRC-passing
codeword that decodes to a plausible message is more likely true than
one that decodes to garbage.

### Kill-switch sketch

Build a generative model for FT8 message tuples (probability of each
callsign-pair, each grid, each exchange-type). Score: for top-200
worst hard-200 WAVs, compute the prior probability of the truth
message under the model. PROCEED if the truth message has prior
probability ≥ 1e-4 (i.e., the prior carries ≥ 13 bits of information,
enough to provide a meaningful regularisation signal against AWGN).
If the prior is essentially uniform (truth at 1e-9 like any random
tuple), this architecture has no leverage.

### Effort

Plan-sized (multiple specs). This is a research project, not an
iter. ~10+ sessions to get to a defensible prototype.

### Headline risk

Variational inference on a model this complex may not converge fast
enough for 15-second slot deadlines. The genuine win may take a year
of research; the project may not have that runway. **wild_card-adjacent:
defensible prior exists but the engineering risk is high.**

---

## A7 — Partial-decode first-class object ("callsign with no message")

### Mechanism

Introduce a `PartialDecode` type alongside `DecodedMessage`. A
partial decode carries: detected callsign (from a callsign-hash match
at the right bits, OR a partial codeword that strongly agrees on the
callsign bits but not the locator/report bits), confidence interval
on the callsign, and the (time, freq) position. CRC failure does NOT
discard the candidate; instead the codeword is inspected for
callsign-bit consistency and emitted as a partial if those bits agree
on a hashed callsign.

Downstream: the QSO layer can use partial decodes for *presence
detection* ("we know KJ7IFD is on the air at this freq, even though
we couldn't decode their exchange"). The autonomous operator can wait
for the next slot to re-decode the same station. PSKReporter spotting
gets richer data.

### Defensible prior

The 28-bit callsign hash is the most structured sub-portion of the
174-bit codeword. Partial-decode is a standard tactic in
communication systems (header decoded, payload corrupted → still
useful). Closer to home: ft8_lib's "hash22" mechanism for unsuccessful
decodes that contain a recognised hash is a degenerate version of
this.

### Assumption challenged

The CRC gate decides "everything or nothing". But the 174-bit
codeword's 91 info bits split into ~28 callsign1, ~28 callsign2, ~16
locator+type, ~14 CRC, ~5 free. A near-decode that gets callsign1 bits
right but garbles the locator IS still useful — currently we discard
it.

### Kill-switch sketch

Diagnostic on top-20 hard-200: for every CRC-failing post-BP codeword,
compare its first-28 bits against the known-hashes table (built from
this WAV's successful decodes' callsigns + recent activity + bundled-
common). PROCEED if at least 10% of CRC-failing candidates have a
known-callsign hash match — meaning the partial-decode mechanism has
real signal to harvest. If sub-1%, the CRC-fail codewords are random
and partial-decode is sand-castles.

### Effort

2-3 sessions for first-cut PartialDecode type + emission path.
Plan-sized if QSO-layer integration is in scope.

### Headline risk

False-positive callsign matches: random codeword bits will collide
with hash entries at rate ~ |hash_table|/2^28. With 75 bundled +
recent + cqdx, this is ~3e-7 per CRC-fail; manageable, but the
attempted-callsign-set inflates this if we use a wide prior.

---

## A8 — Time-frequency uncertainty distribution at sync

### Mechanism

Replace Costas's *point estimate* of (time_step, freq_bin, freq_sub)
with a **posterior distribution** over a small neighborhood. Instead
of "candidate at (t=42, f=320, sub=1)", surface "candidate centered at
(t=42, f=320, sub=1) with covariance Σ". Downstream extracts tone
magnitudes via a **weighted average** over the posterior, not a single
point extraction.

This is sub-bin tone extraction: marginalise over the uncertain
position. It particularly helps marginal-SNR truths where Costas's
peak is broad and the current point-pick lands off-peak.

### Defensible prior

Marginalisation over nuisance parameters is the Bayesian standard.
Sub-pixel registration in image processing. WSJT-X's `f1` sweep is a
brute-force version of this; we'd replace the sweep with a calibrated
posterior.

### Assumption challenged

Sync output is a 3-tuple of integers (after sub-bin selection). The
underlying continuous-position uncertainty is collapsed. For marginal
candidates, this collapse can land us 1-2 bins off true position, and
LDPC/CRC then fails because the tone magnitudes were extracted at the
wrong center.

### Kill-switch sketch

Diagnostic: for missed truths in hard-200, re-extract tone magnitudes
at a fine grid around Costas's point estimate (±2 bins, 5 sub-bin
fractions). For each grid point, run LDPC+CRC. PROCEED if ≥ 8% of
missed truths have at least ONE grid point that decodes successfully
— meaning the truth IS within Costas's posterior support, we just
picked the wrong point.

### Effort

2-3 sessions for grid-search version (cheap, low-risk). 4-6 for true
posterior-weighted version. Plan-sized if it gets folded into the
main multipass loop.

### Headline risk

Grid search inflates compute by 10x; needs strict candidate-limit
pruning. The posterior-weighted version may have numerical issues
(weighted-average tone magnitudes don't have a clean physical
interpretation in dB).

---

## A9 — Per-tone confidence propagation (symbol-level posterior throughout)

### Mechanism

Today's pipeline computes 8-tone magnitudes per symbol, then a single
LLR triplet (bits_per_symbol=3), then LDPC. The intermediate
representation — *the 8-way posterior over which tone was sent* — is
collapsed at the LLR step. Instead, propagate the full
P(tone=k | observation) for k=0..7 through LDPC's check-node updates
as **symbol-level beliefs**, not bit-level beliefs.

This is non-binary BP over the 8-ary alphabet. The LDPC code is
binary, but the bit groups within a symbol are correlated through the
channel — independent-bit LLRs throw away that correlation.

### Defensible prior

Non-binary LDPC is well-studied (Davey & Mackay 1998). Q-ary BP runs
on the Galois-field alphabet; for FT8 the alphabet is GF(8) groupings
of bit-triples. The "max-log" LLR derivation in `par_compute_soft_
llrs_db` is exactly the lossy step.

### Assumption challenged

Bit-wise LLRs are sufficient. They aren't, when symbol bits are not
independent given the channel observation (which they aren't, for
8-FSK — a single tone determines all 3 bits jointly).

### Kill-switch sketch

Theoretical: compute information loss from bit-LLR projection vs full
symbol posterior. If the mutual-information loss is < 0.1 bits per
symbol, this architecture has no leverage. Practical: implement a
symbol-aware variant of BP that updates check beliefs using symbol
posteriors; benchmark on top-20 hard-200. PROCEED if recovery rate
improves by ≥ 3% AND elapsed inflation < 20%.

### Effort

Plan-sized. Touches LDPC core (BP update rules). Numerically tricky;
risk of regression on easy decodes if check updates aren't carefully
calibrated.

### Headline risk

Regression on easy decodes. The current bit-LLR + binary BP pipeline
is bit-exact with ft8_lib and very well-tuned. Replacing the core
BP update is a deep surgery.

---

## A10 — Learned soft decoder (replace LDPC+OSD with a neural codec) — wild_card: true

### Mechanism

Train a neural decoder (transformer or graph-NN, ~2-10M params) that
takes 174 LLRs as input and outputs a 91-bit info-bit posterior. The
neural decoder is the *only* channel-decoding stage; CRC-14 is the
post-hoc verification. Training data: synthesise infinite labeled
codeword/LLR pairs from the LDPC encoder + AWGN channel model;
fine-tune on real-world residuals to capture interferer statistics.

The neural decoder learns the OSD-style "fix the last few bits"
behavior and the BP-style "iterate beliefs" behavior end-to-end,
without the hard codeword-projection step. Output is a continuous
posterior over codewords (parameterised as info-bit probabilities,
since codewords are deterministic functions of info bits).

### Defensible prior

Cammerer, Hoydis, et al. "Trainable communication systems"
(2020+). DeepReceiver-style work for 5G/LDPC. Demonstrated in
literature; production deployment in cellular is happening. The
*wild* part for pancetta is whether it beats a bit-exact ft8_lib
match given pancetta's much smaller deployment scale (no operator
fleet generating training data).

### Assumption challenged

LDPC+BP+OSD is the right algorithmic family. It may not be —
end-to-end-trained decoders can capture channel correlations the
algorithmic decoders can't.

### Kill-switch sketch

Pre-implementation: train a small (200K param) test model on
synthesised data; benchmark on hard-200. PROCEED only if the small
model matches BP within 5% recovery rate (proves the architecture
can learn the basic decoding task; full model will exceed). If
small model is 50% worse than BP, the architecture or training
loss is wrong and scaling won't fix it.

### Effort

Plan-sized (research project; 1-3 months realistic). Needs GPU
training infrastructure, training-data pipeline, inference
integration. Production cost: model weights shipped with binary
(~10-50MB).

### Headline risk

Inference latency: a 10M-param transformer per candidate per slot
might not fit the real-time budget. Worse risk: subtle behavior
divergence from ft8_lib that's hard to debug.

### wild_card: true

Defensible prior exists but pancetta-specific feasibility is open.
Flagged as wild card because (a) production deployment in a 1-2
operator project lacks the data-and-compute resources that make
neural codecs win in cellular, and (b) the alternative implementations
are not "competing" the same way — pancetta is unique in being a
hobby decoder targeting hard cases. The training-distribution
mismatch risk is real.

---

## A11 — Iterative channel estimation joint with decoding (turbo equalisation)

### Mechanism

Today's pipeline assumes a fixed channel between sync (time-freq
estimation) and decode (LDPC). But the channel has structure:
amplitude per tone may vary (selective fading), time drift may exist
(receiver clock vs transmitter clock). **Turbo equalisation** runs
the channel estimator and the decoder in a loop: BP output → improved
channel estimate → re-extract LLRs → BP → ... until convergence.

For FT8 specifically: after a candidate BP pass produces info-bit
beliefs, use those beliefs to *re-estimate per-tone gain* (which tones
are most reliable, given the now-better codeword hypothesis), then
recompute LLRs with the improved per-tone gain. Iterate 2-3 times.

### Defensible prior

Turbo equalisation in 3GPP / DSL (Tüchler et al. 2002). The principle
applies anywhere channel and decoder are jointly inferrable. FT8's
channel is simpler than cellular (AWGN-dominant) but not trivial —
amplitude per tone DOES vary with selective fading.

### Assumption challenged

Sync-then-decode is a one-shot pipeline. The decoder output carries
information about the channel that's not fed back.

### Kill-switch sketch

Diagnostic: on top-20 hard-200 successful decodes, perturb the
per-tone gain assumption (multiply tone-k magnitudes by 0.7..1.3,
random per tone) and re-run BP. Measure how often BP still converges.
If perturbation has < 5% effect on convergence rate, channel
mis-estimation isn't the bottleneck and turbo eq won't help. If it
has > 20% effect, there's leverage.

### Effort

Plan-sized. Touches the inner decode loop and creates a multi-pass
structure inside what's currently single-pass.

### Headline risk

Loops within loops within the multipass loop. Easy to introduce a
runtime regression and hard to control numerically. Convergence isn't
guaranteed; need safe-fallback to single-pass.

---

## A12 — Decoder as Bayesian model averaging over config grid

### Mechanism

Today we run one decoder config. Run N (say N=5) decoders in parallel
with diverse configs (different freq_osr, time_osr, sync thresholds,
multipass aggression). For each candidate position, **average the
output posterior** across the N decoders, weighted by each decoder's
empirical recovery rate on the operating regime (hard-band-detected
or sparse-band-detected).

Distinct from ensembling at the message-output level (which we
informally do via comparison to WSJT-X): this is averaging *before*
the hard CRC decision, so it's BMA over decoder uncertainty as well
as channel uncertainty.

### Defensible prior

Bayesian model averaging is foundational. Ensemble methods routinely
beat single-best in classification. The novel piece is doing it at
the LLR/posterior level rather than the message-list level.

### Assumption challenged

There IS one right config. Hard-band and sparse-band conditions argue
otherwise. Different configs optimise for different scenes; the
operator doesn't know which scene is in front of them at slot t.

### Kill-switch sketch

Run 5 configs offline on top-20 hard-200, see if the *union* of
decodes across configs is meaningfully larger than any single
config's. PROCEED if union recovery > 1.1 * best-single recovery.
If union ≈ best-single, the configs all decode the same easy stuff
and averaging won't expand the frontier.

### Effort

2-3 sessions for the scoping diagnostic; plan-sized for production
integration (5x compute is not free).

### Headline risk

5x compute is incompatible with real-time on the MiniPC target. Could
end up as a research-only baseline-improver. The interesting variant
might be *adaptive* (run config 2-5 only if config 1 missed
candidates the prob-of-existence map says should be there).

---

## A13 — Hierarchical decode: callsign-only fast pass then full decode given context

### Mechanism

Two-stage decoder. **Stage 1** (fast, light): for every candidate
position, run a stripped-down decoder that *only* tries to recover
the 28-bit callsign1 hash. This is a much smaller code (28 bits +
some CRC-like check) and admits aggressive enumeration. Output: a
hash table {position → likely_callsign1}.

**Stage 2** (deep, slow): for each (position, callsign1) pair from
stage 1, run the full LDPC+OSD pipeline with callsign1 pinned via
AP1 (the bits-0-27 prior path). This is hb-087's AP1-injection
mechanism, but now driven by stage-1's *empirical* callsign detection
rather than external priors. Crucially: this is OUR-observation-based
prior, not a recent-activity prior.

### Defensible prior

Hierarchical decoding is standard (turbo-code constituent decoders,
HARQ retransmission with prior). The novel piece is: stage-1's
output is an EVIDENCE-based prior, not an external prior — escapes
the shelved hb-087 finding that "external priors don't manufacture
signal where there isn't any" because stage 1's prior IS empirical
evidence of signal.

### Assumption challenged

LDPC is a single codeword space. We're treating the 28-bit-callsign
subspace as a separate (much smaller) code, exploiting its hash
structure for early commitment.

### Kill-switch sketch

Diagnostic: on top-20 hard-200, for each missed truth, see if its
callsign1 bits in the post-BP codeword agree with the true callsign
hash, ignoring CRC and the rest of the codeword. If ≥ 25% have
callsign-bit agreement (in isolation from CRC), stage-1 has a signal
to detect. If < 5%, callsign-bit information is no better than
random and stage-1 has no leverage.

### Effort

Plan-sized. The stage-1 decoder is new code; stage 2 reuses existing
AP1 machinery.

### Headline risk

Stage 1's FP rate. A "callsign-only" decoder will surface many false
positives because the code is much weaker. Stage 2's AP-pinning may
amplify rather than damp these.

---

## A14 — Generative-prior decoder (FT8 messages as samples from a learned generative model) — wild_card: true

### Mechanism

Train a generative model (small autoregressive transformer, ~1M
params) on a large corpus of historical FT8 traffic (decades of
WSJT-X logs, PSKReporter exports — millions of messages). The model
learns the *distribution* of FT8 messages: which callsigns appear
together, which grids are common, what exchange-types follow what.

At decode time, the model provides a **structured prior over the
91-bit message space**. The decoder combines this prior with the
channel LLRs (via Bayes' rule: posterior ∝ prior × likelihood). The
hardware decoder still uses LDPC for the channel code, but the
plausibility/output ranking is now informed by the generative model
rather than a hand-coded rule stack.

This is *different* from A6 (variational decoder) because the prior
is a *learned* generative model, not a hand-specified graphical
model. And different from A1 (list decoding) because the prior is
applied even when there's a clear single CRC-passer — it can re-rank
or reject a CRC-passer that's structurally implausible (FP filter
becomes generative).

### Defensible prior

Language models as generative priors for source decoding — Park et
al., Vaswani-era work on text decoding under noise. Generative-model
priors for error correction — emerging research (Kang et al., NeurIPS
2023). The FT8 corpus exists (multi-decade public PSKReporter
archive), training is feasible.

### Assumption challenged

Plausibility is a fixed set of rules (locator-format check,
callsign-format check, message-type whitelist). It could be a
*learned* distribution that captures actual usage patterns (e.g.,
"after CQ TEST de KJ7IFD EM12, the next slot likely contains
KJ7IFD's call as callsign2").

### Kill-switch sketch

Train a small generative model on the public PSKReporter export (~1
day of historical traffic, easily ingestable). Measure perplexity on
held-out messages. PROCEED if perplexity < 2^60 (the model captures
≥ 30 bits of structure beyond what uniform-over-tuples assumes — a
meaningful prior strength). If perplexity ≈ 2^91 (uniform), the
generative model adds no information and this architecture has no
leverage.

### Effort

Plan-sized (multi-month research project). Training, evaluation, and
integration into the decoder are each substantial pieces.

### Headline risk

**Cheating concerns**: a strong generative prior can hallucinate
callsigns that weren't actually transmitted (it knows "K5ARH is on
20m a lot, this LLR pattern could be K5ARH"). Robust evaluation needs
careful FP-vs-TP separation; the temptation to overfit to the
operator's own callsigns is real.

### wild_card: true

Defensible prior exists in adjacent fields (NLP generative priors for
noisy decoding) but no published FT8 application. The risk profile is
high: generative priors are powerful and dangerous in equal measure.

---

## Summary / aggregator hints

| ID | Stage attacked | Distributional output | Wild card | Effort | Likely composite Δ |
|---|---|---|---|---|---|
| A1 | CRC-as-gate | yes | no | plan | medium-high |
| A2 | Costas-as-gate | yes | no | plan | medium |
| A3 | FP-filter-as-gate | yes | no | 2-3 | medium |
| A4 | Multipass sequence | implicit | no | plan | high (if it converges) |
| A5 | LLR-extraction (input fusion) | no | no | 4-6 | medium |
| A6 | LDPC-as-channel-code | yes | no | plan | high |
| A7 | CRC-as-gate + emit type | yes | no | 2-3 | low-medium (operational value) |
| A8 | Sync as point-estimate | yes | no | 2-3 | medium |
| A9 | Bit-LLR-projection | yes | no | plan | low-medium |
| A10 | LDPC+OSD entire | yes | YES | plan (3-month) | unknown |
| A11 | Channel-decoder separation | yes | no | plan | medium |
| A12 | Single-config decoder | yes | no | 2-3 | low-medium |
| A13 | LDPC as monolithic | partial | no | plan | medium-high |
| A14 | Plausibility-as-rules | yes | YES | plan (3-month) | unknown |

**Recurring theme**: nearly every idea above replaces a binary gate
with a probability distribution, and several push the hard-decision
boundary downstream to the QSO layer or even the operator. The
project's current architecture treats the decoder as a deterministic
oracle; the project's open hard-200 frontier may require treating it
as a probabilistic estimator.

**For the aggregator**: A1 and A8 are the cheapest paths into the
"distributional output" thesis — both have diagnostic kill-switches
that can run pre-implementation in 2-3 sessions and either definitively
unlock the architecture or close it. A6 and A14 are the biggest
potential disruptors but require research-project commitment beyond
single-session iters. A4 (joint multi-candidate) is the most aligned
with hb-088's "multi-stream separation BEFORE LLR extraction" structural
finding — that's the natural follow-up to the bypass-Costas shelf
cluster.
