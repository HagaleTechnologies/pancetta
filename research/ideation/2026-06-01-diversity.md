---
slug: ideation-diversity-2026-06-01
mode: ft8
state: ideation
created: 2026-06-01T00:00:00Z
last_updated: 2026-06-01T00:00:00Z
branch: iter/2026-06-01-ideation-diversity
parent_hypothesis: meta — DIVERSITY-class ideation pass (multi-stream / multi-receiver)
wild_card: mixed (D5, D9, D12, D13 flagged wild_card:true)
scorecard: n/a (idea generation; not a sweep)
delta_vs_main: 0 (no production code change)
disposition: 14 candidates drafted, distinct from V2/V3/hb-088/hb-087/hb-064-S2/hb-036/hb-069
  and from the parallel architectural pass. Aggregator decides bank admission;
  this file is the raw idea inventory only.
---

## Framing — why diversity now

Every shelved hypothesis from the last batch (V2, V3, hb-088, hb-087-S2, hb-036,
hb-069) operated on the **same single-receiver waveform** that pancetta already
ingests. The structural lesson is consistent: the residual at sub-Costas
positions is **noise-dominated** for the interfering-neighbor regime, and no
amount of post-hoc magic on that one waveform pushes recall further without
inflating FPs. The conjecture that drives this pass:

> The single-stream ceiling is real. The next 1.0 multiplier on composite is
> outside the existing WAV — in a *second* measurement of the same RF event
> whose noise is statistically independent of the first.

Diversity is the textbook way out. Pre-FT8 weak-signal HF practice (Q65, JT65,
MSK144, even WSJT-X's own multi-RX experiments, NCDXF beacon network, JTAlertX
co-decoding) all lean on diversity when single-RX hits a wall. The class is
under-mined in pancetta because the original architecture assumed one
FTdx10 + one USB audio path.

The ideas below are explicitly **not** "another decoder pass on the same WAV."
Each one introduces or fuses an independent measurement. Combination strategy
matters as much as the diversification: LLR-sum, OSD-coupling, codeword-vote,
posterior mixing, and conditional Bayesian update each have different sweet
spots noted per-idea.

Distinction from closed shelves:
- V2/V3/hb-088: single-WAV residual-quality fixes. Diversity is orthogonal.
- hb-087-S2: per-position SNR gating on one stream. Diversity gates against
  *another* stream's measurement.
- hb-064 S2: DIA-OSD on one bit-LLR vector. Diversity proposes *multiple* LLR
  vectors to feed OSD.
- hb-036: NMS suppression of within-stream peaks. Diversity is between-stream.
- hb-069: linear-vs-dB on one spectrogram. Diversity is across spectrograms.

Distinction from the parallel architectural pass (branch
`iter/2026-06-01-ideation-architectural`): this file scopes only ideas where
the structural lever is **a second physical or logical signal source**, not
a re-organization of the existing decode pipeline.

---

## D1 — Dual-KiwiSDR space-diversity LLR fusion (operational MVP)

### Mechanism
For a target propagation event, simultaneously capture the same 15-s slot via
**two geographically separated public KiwiSDRs** (e.g., one in EU, one in NA;
or two ends of an auroral path). Both streams are slot-aligned at the source
with the kiwirecorder cron from hb-073's procedure doc. Each WAV is decoded
through pancetta independently *up to the demodulator's per-symbol soft
LLRs* (the 174-length LDPC LLR vector for each candidate position). At a
candidate (df, dt) that both decoders flag, the per-bit LLRs are **summed**
(equivalent to maximum-ratio-combining under independent Gaussian noise) and
the combined LLR vector is fed to a *single* LDPC+OSD pass.

Combination point: post-demodulator, pre-LDPC. The cost of a wasted decode is
absorbed by the candidate intersection (only positions both RXs see). The
recall lever is: a decode that fails LDPC on either RX alone may pass on the
sum, because noise variance halves while signal grows linearly.

### Defensible prior
This is textbook MRC, the same trick Q65 uses across multiple receive
intervals (Taylor 2020 Q65 docs). MRC-on-LLR also matches the principle
behind hb-075 (MRC-weighted coherent cross-cycle, which GRADUATED 2026-05-29).
hb-075 already proved MRC works for one specific within-WAV diversity case;
D1 extends it to *between-WAV* with vastly more independent noise.

### Assumption challenged
"Pancetta's input is one WAV from one rig." A KiwiSDR-pair manifest is
already half-built (hb-073 procedure doc has the kiwirecorder one-liner).

### Kill-switch sketch
Pre-impl: take any 5 already-captured (single-RX) wild-doppler WAVs once
hb-073 manifest exists, synthesize a "second RX" by adding independent
Gaussian noise of equal variance, and measure LDPC-pass-rate of the sum vs
each individual stream. If sum doesn't beat the better-of-two by ≥30% on at
least 3 of 5 WAVs, MRC has no headroom in this regime → KILL before
operator capture work.

### Effort
Plan-sized (3 sessions): (1) eval-harness mod to accept paired WAV manifest +
LLR-sum primitive, (2) synthetic kill-switch on hb-073 corpus, (3) live
paired-Kiwi capture for a single auroral opening + measurement.

### Headline risk
The two KiwiSDRs may not actually see *independent* noise. Both are HF
receivers in the ionosphere; common-mode interference (lightning crash, polar
absorption event) hits both. MRC degrades to ~+1.5 dB instead of the full
+3 dB. Mitigation: kill-switch measures real independence.

---

## D2 — Decoder-diversity vote: pancetta ⊕ jt9 ⊕ jtdx, codeword-level merge

### Mechanism
Run the same WAV through **three independent decoders** — pancetta (this
codebase), jt9 (WSJT-X), jtdx (JTDX fork). Each emits its own list of
`(call_a, call_b, locator, df, dt, snr)` plus, where available, a per-decode
*confidence proxy* (jt9: sync score; jtdx: hint indicator; pancetta: LDPC
residual + OSD step). The merge layer dedupes by `(call_a, call_b, df±5Hz,
dt±0.3s)`, and a decode is **accepted** if ≥2 of 3 decoders report it OR if
1 decoder reports it with a confidence above each decoder's calibrated
"solo-trust" threshold.

Combination point: post-LDPC, candidate-list level (codeword vote). Implemented
purely as a wrapper script for offline-eval; no production-decoder change. This
is essentially "ensemble inference" applied to FT8.

### Defensible prior
jtdx outperforms jt9 on certain weak-signal classes (its own benchmarks);
pancetta has graduated past jt9 (123.7% per current status) on hard-200. The
**union** of three decoders is empirically larger than any one — every
operator who runs both WSJT-X and JTDX notices each catches things the other
misses. Formal: classifier ensembles improve when component errors are weakly
correlated (Dietterich 2000).

### Assumption challenged
"Pancetta is the decoder." For corpus-recall measurement, the decoder of
record can be an ensemble where pancetta is one voter. The composite metric
becomes ensemble-recall.

### Kill-switch sketch
Pre-impl: pick top-50 hard-200 WAVs, run jt9 + jtdx baseline against each (we
already have jt9 baselines cached). Compute |jt9 ∪ jtdx ∪ pancetta| vs
|pancetta alone|. If union exceeds pancetta-only by < 20 net decodes,
ensemble headroom is small → KILL. If union shows 100+ new true-positives,
proceed and the question becomes calibration of the vote threshold.

### Effort
1 session for kill-switch (jtdx install + baseline cache + union count); if
PROCEED, 1 plan for a `corpus-decode --ensemble` mode in pancetta-research.

### Headline risk
False-positive amplification: each decoder has its own FP profile, and OR-of-
three triples the FP rate. The 2-of-3 vote rule mitigates but caps the gain.
hb-052 live-FP filter (already shipped) helps offline as well.

---

## D3 — AGC-diversity: re-decode each WAV at multiple synthetic gain settings

### Mechanism
A single 12 kHz WAV is rescaled to **three amplitude levels** (-12 dB, 0 dB,
+12 dB relative to original) before entering pancetta. Each rescaling is
decoded through the *full* pipeline independently. The candidate intersection
is taken; a decode at the same `(call, df, dt)` from ≥2 of 3 gain settings is
accepted with confidence boost. The hypothesis: pancetta's dB-spectrogram
log-quantization, demodulator soft-decisions, and OSD weighting all have
subtle gain-dependent behavior; varying gain perturbs the decision boundary
and exposes near-threshold true-positives.

Combination point: candidate-list, with optional LLR-sum at intersection.

### Defensible prior
Wild_card-adjacent: log-domain DSP is *supposed* to be gain-invariant but in
practice quantization, dynamic-range compression, and any clipping (even
soft) break invariance. This is a behavioral diversity probe — and the
analogous trick (decoding under multiple SNR assumptions) is what JTDX's
"agc auto" actually does, just at a different layer.

### Assumption challenged
"Pancetta's pipeline is amplitude-invariant." Strictly false (hb-069's dB-vs-
linear shelve proved log-domain matters); D3 exploits this.

### Kill-switch sketch
Pre-impl, on 20 hard-200 WAVs: rescale to ±12 dB, run pancetta on each, count
*new* true-positives at off-baseline gains. If the count is < 1.0 per WAV
mean (i.e., < 20 across the set), gain-sensitivity is too weak to mine → KILL.

### Effort
1 session: scaling primitive + harness wrapper + measurement.

### Headline risk
Pancetta's float32 internal pipeline may already be gain-invariant enough
that all three runs converge to the same candidate list. If so, the
diversity is nominal and the elapsed-3x cost has no payoff. Kill-switch
catches this cheaply.

---

## D4 — IQ-pair-diversity from one KiwiSDR: USB + LSB simultaneous

### Mechanism
A single KiwiSDR session captures the **same RF block** demodulated as USB
*and* as LSB in parallel (kiwirecorder supports multiple `-m` slots per
session). Both 12 kHz WAVs are decoded independently. For a true upper-
sideband FT8 signal, the USB stream contains the signal cleanly while the
LSB stream contains it mirrored about the carrier — but **noise in the two
streams is partially independent** because they're filtered through
different SSB filter weightings and any sideband-asymmetric interference
(carriers, AM splatter) hits only one. The LSB-mirrored signal can be
flipped in software (`f → -f` in the spectrogram) and treated as a
second-RX measurement.

Combination point: post-FFT spectrogram averaging, then standard pancetta
decode.

### Defensible prior
This is a wild_card variant of the classic "image-rejection" measurement.
In SDR practice, USB and LSB on the same fc actually do contain partly
independent local noise contributions because of filter rolloff
asymmetries. Operationally untested — flag as **wild_card: true**.

### Assumption challenged
"There's only one way to demodulate a given RF block to baseband audio."

### Kill-switch sketch
Capture one 5-min KiwiSDR USB+LSB pair, compute the cross-correlation of
their noise floors (with signals notched). If ρ > 0.85, noise is mostly
common-mode and there's no diversity gain → KILL.

### Effort
0.5 session for the kill-switch capture + correlation measurement; 1 plan
if PROCEED.

### Headline risk
The noise correlation is exactly what makes or breaks this. If kiwiclient
applies a shared AGC across both demods, ρ → 1 and the idea is dead.
**Wild card** because the underlying claim hasn't been measured on
KiwiSDRs specifically.

---

## D5 — Frequency-diversity via simultaneous 20m + 40m capture of the same QSO

### Mechanism
Some stations CQ on multiple bands either simultaneously (true multi-band
operators) or sequentially. When pancetta sees the same callsign on 20m and
40m within a short window (say, ≤10 min), the two QSOs are treated as
**joint observations** of the same operator's *message generation
distribution*. This is not waveform-diversity — it's **message-prior
diversity**: knowing a callsign is currently CQing on 20m sharpens the
prior over which messages he's likely to send on 40m. The combination
folds into the AP/prior layer of the decoder (not the LLR layer).

Specifically: a partial-decode on 40m that resolves only `<call_a>` ambiguity
gets a Bayesian boost when that call already has a complete decode on 20m
within the prior 5 minutes (same locator, same RR73/73 sequence
expectation). Sub-Costas residual decodes that fail LDPC by 1-2 bits become
admissible under the conditional prior.

Combination point: AP/prior layer (conditional Bayesian update).

### Defensible prior
mr-006 noted "AP-context is worthless offline (ceiling 1/8576)" — but D5
flips this: the *cross-band, multi-WAV* AP context **is** rich, because
operators don't randomly hop bands. The PSK Reporter and cqdx.io spot
streams confirm the cross-band coherence empirically. **Wild_card**
because no FT8 decoder publicly does this today.

### Assumption challenged
"Each 15-s decode is independent of every other 15-s decode." False
operationally — operator behavior has temporal + spectral structure.

### Kill-switch sketch
Pre-impl on hard-1000: for every true-positive callsign, count how often
the same callsign appears on a different band in the corpus (if the corpus
contains multi-band recordings). If < 5% co-occurrence, the conditional
prior has no measurable support → KILL.

### Effort
2 sessions: cross-band callsign-index builder + prior-update primitive +
kill-switch measurement.

### Headline risk
hard-200/-1000 are single-band corpora by construction; cross-band benefit
might only materialize on a multi-band capture corpus we don't have. The
mr-006 closed-source decision applies.

---

## D6 — Operator-network diversity (K5ARH ⊕ trusted-friend rigs) via cqdx

### Mechanism
A small network of **3-5 trusted ham friends** runs pancetta-eval-lite (a
stripped-down decoder + uploader) on their own rigs and posts decode
hashes (codeword + (call, df, dt)) to a cqdx.io endpoint. K5ARH's pancetta
queries the endpoint for any peer-confirmed decodes within ±2s of his own
slot. A peer confirmation acts as a **trust-amplifier** for K5ARH's marginal
decodes (sub-Costas, LDPC-near-fail), pushing them through the
confidence-threshold cliff.

Critically: this is **not** sharing audio (privacy/bandwidth/access
problems). It's sharing **decision artifacts** — a 174-bit codeword hash +
metadata — which is small and privacy-safe (decoded messages are already
public spectrum content). The credentialed-integrations rule (cqdx stays
non-credentialed) is satisfied: friends' decode hashes are aggregated by
cqdx as anonymized public spots.

Combination point: posterior-probability mixing — P(decode | peer-confirms)
vs P(decode | no peer).

### Defensible prior
PSK Reporter and RBN already do exactly this aggregation for *operator-
facing* purposes. D6 closes the loop and feeds the aggregate **back into
the decoder** at decode time, which neither RBN nor PSKReporter do today.
Defensible: the aggregate is provably more informative than any single
RX — see RBN's accuracy vs single-skimmer accuracy in published Skimmer
performance data.

### Assumption challenged
"Pancetta operates on K5ARH's data alone." With a tiny social-graph
extension, a constellation of friends becomes a virtual second RX.

### Kill-switch sketch
Use existing PSKReporter spot data (public). For 100 hard-1000 WAVs (if
their RX time is known), check how many decodes pancetta misses that show
up on PSKReporter from another RX within ±10s. If < 10%, the social-graph
diversity has thin headroom → KILL.

### Effort
Plan-sized (4 sessions, slow): protocol + cqdx schema + minimal lite-eval
binary + social bootstrap. Slow because the operational/social side
gates technical progress.

### Headline risk
Cold-start social graph. Until 3+ friends opt in, there's no fleet. The
credentialed-integrations rule disallows pancetta posting on the friends'
behalf — they each must run their own lite-eval.

---

## D7 — Time-diversity by long-window IF acquisition (Q65-style)

### Mechanism
The FT8 slot is 15 s, but many real-world QSOs **repeat** the same message
across multiple slots (CQ-CQ-CQ; or station calling with no answer
repeating their call). Detect repeating messages via per-codeword sequence
matching, then **coherently average the IF-level (post-FFT, pre-demod)
spectrograms** of the matching slots. The averaged spectrogram has
~√N better SNR (for N repeats with independent noise) and is fed to a
single decode pass.

Distinct from hb-074/075/079/085 which average pre-decode within one
recording: D7 averages **across slots that have already been partially
decoded as the same message**. Combination point: spectrogram level, after
codeword-match confirms repetition.

### Defensible prior
Q65 (Taylor's slow-mode protocol) explicitly does this with a 60-s slot and
multi-period averaging. The technique is proven for weak HF; FT8 just
hasn't applied it because its protocol design assumes 1-shot decode. mr-008
hb-089 spawned similar logic for *residual* accumulation; D7 is the
**raw-spectrogram** sibling, with the per-codeword-match gate to avoid
averaging unrelated signals.

### Assumption challenged
"15-s slots are independent decode units." For repeating-content cases,
they aren't.

### Kill-switch sketch
Pre-impl on hard-1000: count how often the *same callsign appears in
consecutive slots in the same 90s sample*. If <10% of true-positive
callsigns repeat, the diversity has thin support → KILL.

### Effort
2 sessions: repeat-detector + spectrogram-averaging primitive + measurement.

### Headline risk
Drift over the averaging window — Doppler, slot-timing drift, frequency
drift — destroys coherence. Needs a phase-tracking compensator, which is
itself non-trivial.

---

## D8 — Decoder-diversity intra-pancetta: parallel demodulators with different sync windows

### Mechanism
Run pancetta's demodulator **three times in parallel on the same WAV** with
different sync-window placements (e.g., Costas-only, prefix+Costas, full-
Costas+postfix). Each variant emits its own per-symbol LLR vector for the
same (df, dt) candidate. The three LLR vectors are **averaged** before LDPC.
The diversity here is *algorithmic*: the three sync windows have correlated
but not identical noise on each symbol estimate, especially in the
presence of timing-jitter or partial overlap with neighbors.

Combination point: pre-LDPC, LLR-average across sync variants.

### Defensible prior
WSJT-X-Improved 3.x ships multiple sync strategies and merges them (a3/a4).
JTDX does similar. Pancetta uses one sync strategy throughout. Mining the
intra-decoder design space is the cheapest diversity available — no second
hardware, no second corpus.

### Assumption challenged
"One sync window per decode." Pancetta's one-window choice is a hard
default never empirically benchmarked against averaging.

### Kill-switch sketch
Pre-impl: pick one hard-200 WAV with known FP behavior, hand-instrument
the demod to dump LLRs from two sync windows (cheap diff). Compute Pearson
correlation of the LLR vectors. If ρ > 0.97, the windows are redundant →
small averaging gain → KILL. If 0.7 < ρ < 0.9, diversity is real and
worth a full sweep.

### Effort
1 session for kill-switch (LLR dump + correlation); 1 plan if PROCEED.

### Headline risk
Sync-window choice may already be near-optimal for pancetta's specific
spectrogram quantization, leaving no room for averaging gain. Closely
related to hb-088's family of sub-Costas residual issues already shelved.

---

## D9 — Polarization-diversity emulator via two physical antennas (wild card)

### Mechanism
K5ARH installs (or borrows) a **second antenna with different polarization
or orientation** — e.g., a horizontal dipole alongside the vertical, or
a rotated yagi — and runs **two SDR receivers** (FTdx10's IF tap + a
RTL-SDR or similar) capturing the same RF block via different antennas.
Each path produces a separate 12 kHz WAV. Streams are LLR-summed exactly
as D1.

This is *real-physical* space/polarization diversity, the holy grail of
HF diversity reception. Used by NCDXF beacon network and by every serious
DXer with a switchable antenna pair. Brings actual independent noise, not
just "different filtering of the same path."

Combination point: pre-LDPC LLR-sum (same as D1).

### Defensible prior
Polarization diversity gains 3-6 dB on HF, routinely measured. NCDXF
beacon receivers, ARRL handbook chapters on diversity, MFJ-1025 "Phase
Combiner" all proven. **Wild_card** flag is for "does pancetta's
architecture make multi-antenna feasible without invasive coordinator
rewrite" — the answer is unclear without an architectural pass.

### Assumption challenged
"K5ARH has one antenna." Cheap upgrade.

### Kill-switch sketch
Pre-impl: measure ρ between the two antenna noise floors on quiet
spectrum (no signal). If ρ < 0.6, diversity is real → PROCEED. If ρ > 0.9,
the antennas are too close to be independent → KILL (or move antennas).

### Effort
Plan-sized (5 sessions, real hardware): SDR procurement + driver + dual-
audio coordinator support + sync-alignment + measurement campaign.

### Headline risk
Real HW + driver complexity. Pancetta's audio layer assumes one
`cpal::Stream`. **Wild_card** for that reason.

---

## D10 — Posterior-probability mixing with cqdx live-spot-stream as a prior

### Mechanism
cqdx's live-spots endpoint emits a stream of `(callsign, band, freq, time,
SNR)` for the last ~15 min. For a candidate decode at `(call, df)`, the
prior `P(call active right now)` is sharply non-uniform — most callsigns
are silent in any given 15-min window. The decoder's existing posterior
(LDPC + OSD) is **multiplied** by this prior before the accept threshold.
A marginal decode at LDPC-near-fail gets accepted iff cqdx confirms the
callsign was active on this band recently.

Combination point: post-LDPC posterior, prior multiplication.

This differs from rarity-scoring (already shipped): rarity weights *which*
decodes the operator wants to chase, not whether to *accept* a marginal
decode as valid.

### Defensible prior
Bayesian posterior mixing with an empirical prior is statistically sound.
The risk is the prior is itself noisy (cqdx spot stream lags real-time
by 30-90s). Defensible only if the lag-induced miscalibration is bounded.
Closely related to hb-058 (callsign-prior gate) but D10 uses **live
spot stream as the prior source**, not a per-callsign DT history.

### Assumption challenged
"Decoder confidence is the only acceptance signal." cqdx live-state is
already a credentialed input pancetta has access to for non-decoder
purposes; D10 plumbs it into the decoder.

### Kill-switch sketch
On hard-1000: compute the per-WAV "active in last 15 min" set from PSK
Reporter retrospectively (close proxy for cqdx live). Measure: of
pancetta's currently-rejected candidates (LDPC-near-fail), what fraction
would be accepted with prior multiplication AND are in truth-set? If
<5%, prior is too weak to help → KILL. If >20%, large recall lever.

### Effort
2 sessions: cqdx-live-stream tap (offline cache) + prior-mult primitive +
measurement.

### Headline risk
**Self-fulfilling prophecy**: the spot stream is *generated* by the same
decoders pancetta is trying to beat. If pancetta uses the stream as a
prior, it inherits any survivorship bias in the spot stream. Could
inflate composite without genuine information.

---

## D11 — IQ-vs-audio diversity: pair USB audio with KiwiSDR `--ncomp` IQ

### Mechanism
For the same QSO, capture **both** USB audio from the FTdx10 (current
pancetta input) **and** the `--ncomp` (no-companding) IQ stream from a
local KiwiSDR tuned to the same fc. The two paths share **the antenna**
but diverge at the receiver: USB audio is post-IF-filter, post-AGC, post-
DSP; the KiwiSDR IQ is raw quadrature. Noise correlation between the two
paths is moderate (shared antenna) but the *processing-induced
distortions* differ enormously.

The IQ path bypasses the FTdx10's IF filter shape — a known source of
amplitude distortion on weak signals. LLR-sum at candidate intersection.

Combination point: pre-LDPC LLR-sum.

### Defensible prior
hb-077 (phase-coherent SDR-IQ corpus) already scoped exactly this capture
path. D11 elevates it from *eval-corpus expansion* to *decoder fusion*.
Defensible: the IF filter shape in FTdx10 is documented to roll off
sharply, and the KiwiSDR's flat-passband IQ provides a complementary
view.

### Assumption challenged
"FTdx10 USB audio is the canonical signal." It's already filtered/AGCed;
the raw KiwiSDR IQ at the same antenna is a more faithful measurement.

### Kill-switch sketch
On 10 paired WAV/IQ captures (one auroral opening): decode each path
separately, count decodes unique to each. If pancetta-on-IQ finds >5
decodes per WAV that pancetta-on-USB misses, fusion has headroom →
PROCEED. If overlap is >95%, paths are functionally identical → KILL.

### Effort
Plan-sized (3 sessions, blocked on hb-073 capture infra): paired-capture
plumbing + LLR-sum on heterogeneous source + measurement.

### Headline risk
KiwiSDR-on-K5ARH-antenna requires an antenna splitter and a KiwiSDR
(~$300). Splitter loss may exceed the diversity gain. Hardware-gated.

---

## D12 — Adversarial-noise injection diversity: decode at multiple synthetic noise floors (wild card)

### Mechanism
On a single hard-200 WAV, **add** independent synthetic Gaussian noise at
three levels (+3 dB, +6 dB, +9 dB above the WAV's measured noise floor)
to produce three *worse* copies. Decode each. The intersection of decodes
that *survive at all three noise levels* are very-high-confidence; the
decodes that survive only at low noise are *low-confidence*. This is a
**confidence calibrator**, not a recall improver — it lets pancetta
distinguish "decoded because the signal is strong" from "decoded because
of a lucky residual."

For recall, it could feed into D2's ensemble vote by providing per-decode
robustness scores. The novel angle: it generates a **per-decode SNR
margin estimate** for free, which the FP filter (hb-052) and the QSO
state machine can both consume.

Combination point: confidence vector, post-LDPC, downstream of decode.

### Defensible prior
This is *test-time augmentation* — the ML practice of evaluating a model
under multiple input perturbations to estimate prediction confidence. Well-
established in DL literature. **Wild_card** because no FT8 decoder has
publicly applied it.

### Assumption challenged
"A decode is binary accept/reject." Adding a margin estimate enables soft
downstream decisions (e.g., FP gating, QSO-loop confidence-aware
sequencing).

### Kill-switch sketch
On 20 hard-200 WAVs: compute for each true-positive decode at the
current noise floor, does it also survive at +3/+6/+9 dB injected noise?
The TP-survival distribution should differ from the FP-survival
distribution — measure AUC. If AUC < 0.65, the margin estimate is weak
→ KILL.

### Effort
1 session for kill-switch (noise-inject primitive + AUC measurement). 1
plan if PROCEED for FP-filter integration.

### Headline risk
3x or 4x elapsed cost for a confidence signal that may already be
encodable from LDPC residual + sync score (existing signals). Has to
beat those cheap alternatives.

---

## D13 — Anonymous KiwiSDR constellation: opportunistic Nth-RX scavenging (wild card)

### Mechanism
A daemon polls **5-10 public KiwiSDRs** on a low-duty-cycle (one
slot every N minutes per Kiwi) capturing the band pancetta is currently
QSO-ing on. Each Kiwi's WAV is decoded; any decode of a callsign K5ARH
has an open QSO with becomes a **second-source confirmation** for the
in-flight QSO. This addresses Phase-5 QSO-loop reliability rather than
offline-recall: it provides redundant decode of the QSO partner's
messages, dramatically reducing missed-RR73 cases.

This is the **constellation** model: zero coordination with Kiwi
operators, purely "scavenging" the public network. The 30-second-per-Kiwi
polling stays well under any reasonable load.

Combination point: QSO-state-machine level — augments the decode source
to the autonomous operator.

### Defensible prior
The kiwirecorder infra already exists (hb-073). The polling-multiplexer
approach is novel for FT8 — most QSO automation tools (JTAlert, GridTracker)
use one RX. **Wild_card** because nobody has tried to use the KiwiSDR
public network as a confirmation fleet for in-flight QSOs.

### Assumption challenged
"My QSO partner's TX reaches my antenna or nothing." Sometimes my Kiwi
buddies hear it when I don't (auroral fade, local QRM at my QTH).

### Kill-switch sketch
Pre-impl: during a Phase-5 trial QSO, manually monitor 3 KiwiSDRs at
similar latitudes for the QSO partner's TX. Count how often a Kiwi
decodes the partner when K5ARH doesn't. If <1 in 10 slots, the fleet
adds nothing → KILL.

### Effort
Plan-sized (4 sessions): polling daemon + decode pipeline per Kiwi +
QSO-state-machine hook + acceptable-use-policy / rate-limit safety.

### Headline risk
KiwiSDR sysops may rate-limit or ban a scraper. Acceptable-use
compliance is non-trivial. Operationally risky if the network notices.

---

## D14 — Cross-pol synthetic diversity: H/V LLR variance as a within-stream feature

### Mechanism
The poor-man's-polarization-diversity-without-a-second-antenna trick:
process the same WAV through pancetta with **two different audio pre-
filters** that emulate H-pol-like and V-pol-like channel responses
(e.g., one with a slight notch at the local QRN center frequency,
one without). The differential response across the two pseudo-pols
yields a per-symbol *variance* signal — symbols with high cross-variance
are likely corrupted by narrowband interference, and the per-symbol
LLR can be **down-weighted** before LDPC.

Combination point: per-symbol LLR re-weighting (variance-down-weighting),
pre-LDPC.

### Defensible prior
The technique is borrowed from MIMO LLR processing (channel-uncertainty
weighting). It's a *single-stream emulation* of two-stream diversity,
which makes it cheap but speculative — true polarization gain requires
real antennas. The within-stream emulation may produce nothing.

### Assumption challenged
"All symbols in an LLR vector are equally trustworthy." Already partially
challenged by hb-079 (residual subtraction weighted decoding); D14 adds
a between-pseudo-stream variance estimate.

### Kill-switch sketch
On 20 hard-200 WAVs: compute the variance signal, identify the symbols
with the highest variance, and check whether they correspond to bit
positions where LDPC currently fails (truth-known case). If correlation
< 0.3, the variance signal is uninformative → KILL.

### Effort
1 session: pseudo-pol filter design + variance computation + correlation
measurement.

### Headline risk
The pseudo-pol filters may both pass the same noise (no diversity at all),
making this idea functionally identical to "applying one filter and
re-decoding," which has been measured many times. Cheap kill-switch is
the saving grace.

---

## Summary table

| ID | Title (short) | Diversity class | Combination point | Kill-switch effort | Wild card |
|---:|---|---|---|---|---|
| D1 | Dual-KiwiSDR space + MRC | Space | LLR-sum | 0.5 session | no |
| D2 | jt9⊕jtdx⊕pancetta ensemble | Decoder | Codeword vote | 1 session | no |
| D3 | AGC-level re-decode | AGC | Candidate-intersect + LLR-sum | 1 session | no |
| D4 | USB+LSB pair on one Kiwi | Sideband filtering | Spectrogram avg | 0.5 session | yes |
| D5 | 20m+40m cross-band conditional prior | Frequency (operator behavior) | Bayesian update | 1 session | yes |
| D6 | Friend-network decode-hash fleet | Social/space | Posterior mix | plan, gated | no |
| D7 | Repeating-message slot averaging | Time | Spectrogram avg post-match | 1 session | no |
| D8 | Multi-sync-window LLR average | Algorithmic | LLR avg | 1 session | no |
| D9 | True H/V polarization diversity | Polarization | LLR-sum | plan, HW-gated | yes |
| D10 | cqdx live-spot prior multiplication | Operator-state | Posterior mult | 2 sessions | no |
| D11 | USB-audio ⊕ KiwiSDR IQ pair | Receiver path | LLR-sum | plan, hb-073 gated | no |
| D12 | Multi-noise-level test-time augmentation | Adversarial | Confidence vector | 1 session | yes |
| D13 | Public-KiwiSDR scavenging fleet | Space (constellation) | QSO-state augment | plan | yes |
| D14 | Pseudo-pol variance weighting | Within-stream emulation | Per-symbol LLR weight | 1 session | no |

## Notes for the aggregator

- **Operationally feasible soonest** (no new HW, no operator-physical
  work, kill-switch in ≤1 session each): **D2, D3, D7, D8, D12, D14**.
  These can be picked up in a single session and either killed or
  promoted to a full plan.
- **Highest potential disruption** (biggest composite lever if they
  work): **D1 (dual-Kiwi MRC, +3 dB equivalent), D6 (friend network —
  scales linearly with fleet size), D2 (decoder ensemble — proven trick
  not yet applied)**.
- **Gated on hb-073**: D11 (paired audio+IQ). The Kiwi capture infra is
  shared.
- **Self-fulfilling-prophecy risk** (worth flagging during admission):
  D10. The cqdx prior is generated by decoders, so pancetta consuming
  it could inflate composite without real information.
- **Distinct from parallel architectural ideation**: this set deliberately
  excludes pipeline-reshape ideas (those go in the architectural file).
  Every entry here introduces a *second physical or logical input
  source*; the architectural file should explore single-input pipeline
  restructurings.
- **Distinct from closed shelves**: V2/V3/hb-088 (single-WAV residual),
  hb-087-S2 (single-stream SNR gate), hb-064-S2 (single-LLR-vector DIA-
  OSD), hb-036 (single-stream NMS), hb-069 (single-spectrogram dB/lin).
  No overlap.

End of ideation file.
