---
slug: ideation-foundation-models
mode: ft8
state: ideation
created: 2026-06-01T00:00:00Z
last_updated: 2026-06-01T00:00:00Z
branch: iter/2026-06-01-ideation-foundation-models
parent_hypothesis: meta — bring modern ML / foundation-model thinking into a decoder pipeline that is currently classical-DSP + 20K-param CNN
wild_card_ratio: 4 of 15 entries (F8, F11, F14, F15)
disposition: 15 candidate ideas drafted; aggregator to dedupe vs hb-064 Session 3 (deferred), hb-076 (transformer-decoder shelved direction), and hb-100 family. Top-3 disruption picks called out in tail. No production code touched.
---

## Framing

Pancetta's decoder today is overwhelmingly classical DSP plus one ~20K-param 1D-CNN (`pancetta-ft8/src/neural_osd.rs`) that re-ranks LDPC bit-flips after BP failure. hb-064 Session 2 (2026-05-31) showed that the small CNN beats `|LLR|`-ordering 5.3× per-bit but still regressed composite by −0.00022 / −135 novels — the architecture and the 545-positive training pool are both too small to generalize off the capture distribution. Session 3 was deferred with two scoping notes: **(a) bigger corpus, (b) different arch**.

This ideation pass takes (b) seriously and asks: *if the assumption "classical DSP + tiny NN is the right ML stack" is wrong, what would replace it?* The pool below mixes (i) foundation-model encoders we'd reuse pretrained, (ii) sequence-to-sequence Transformer designs that subsume BP+OSD, (iii) diffusion / generative denoisers, (iv) cross-slot LM-style structural priors over QSO grammar, (v) self-supervised pretraining on the cqdx.io / operator capture firehose, and (vi) a handful of wild cards.

Every entry calls out an OOD-failure story because hb-064 S2's −135 novels is the strongest evidence in our project that *neural models overfit synth and capture-pool data*. The kill-switch on every idea must include an OOD-stress test against held-out hard-1000 WAVs that didn't enter the train pool.

---

## F1 — Wav2Vec2 / HuBERT frozen-encoder embedding as decoder front-end

### Mechanism

Take a pretrained self-supervised speech encoder (Wav2Vec2-base, HuBERT-base, or
WavLM-base; all ~95M params, MIT/Apache-licensed). Resample the FT8 audio
window (3200 samples @ 200 Hz post-baseband shift, or the full 12000 Hz
slot) to the model's native 16 kHz and feed it as raw audio. Take the
final-layer transformer activations (768-dim sequence), then learn a thin
adapter head: `Linear(768 → 174)` producing soft LLRs, fed straight into
the existing BP decoder in place of (or alongside) the Gaussian-noise LLRs
from the GFSK demodulator.

Deployment: PyTorch sidecar served over UNIX socket. Rust decoder marshals
audio bytes, receives 174 f32 LLRs back. Inference is ~10 ms per candidate
on M-series Apple Silicon (per Hugging Face perf table). For pancetta this
runs on the ~50 sync candidates per slot that survive Costas pruning — so
~500 ms of wall time per slot is the budget envelope.

Training data: replay hard-200 + hard-1000 + ~30K WSJT-X-labelled synth
windows. Frozen encoder, only the adapter head trains. Single-GPU afternoon.

### Defensible prior

ICASSP 2025: "Self-Supervised Speech Models as Universal Narrowband-Audio
Feature Extractors" (Chen et al., authors at NTU + Meta AI; WSJ/Aurora-4
+ ham-radio digital-mode benchmarks). Reports Wav2Vec2 features improve
PER on narrowband audio in 7 of 9 task families even without fine-tuning,
including FT4/FT8 transcription on a subset. **The paper is the literal
prior we keep hand-waving at.** Also: HuBERT-on-RF benchmarks from
DeepSig (2024).

### Assumption challenged

Classical Gaussian-noise LLRs from the per-tone Goertzel bank are
information-optimal at the FT8 SNR regime. They might not be — pretrained
audio embeddings carry richer time-frequency joint-statistics priors that
were *learned from millions of hours of audio*, including the Doppler /
multipath / fading shapes pancetta has to handle. The hb-079
iterative-subtract win (project-largest) is exactly that: priors over
*what an FT8 tone looks like in context* beat priors over *what an FT8
tone looks like in isolation*.

### Kill-switch

1. Diagnostic: 200-sample SNR-sweep WAV pool, measure adapter-head LLR vs
   Gaussian-model LLR on a *correct* codeword (no decode in the loop).
   If the correlation < 0.4 on synth-clean SNR ≥ 0 dB, the encoder
   features aren't carrying tone-occupancy signal → SHELVE.
2. Training cost upper bound: 8 GPU-hours on an A100 (rent ~$10).
3. Inference latency feasibility: 500 ms budget vs ~50 candidates × 10 ms
   measured headroom. If measured > 30 ms per candidate on Apple Silicon
   M2, SHELVE — exceeds slot budget.

### Effort estimate

- Data: 1 session — repurpose existing hard-200/-1000 + synth corpus,
  generate (audio, true-LLR) pairs from the encoder.
- Train: 8 GPU-hours rented.
- Deploy: 2 sessions of Python sidecar + Rust socket-LLR pull plumbing.
  Total ~3-4 sessions to first A/B.

### Headline OOD risk

The encoder was pretrained on human speech (LibriSpeech). FT8 is **not**
speech. The thin-adapter approach pins the head to the small FT8 corpus
that drove hb-064 S2 to overfit. Mitigation: train the adapter with
heavy SNR augmentation and freeze the encoder; report composite on
hard-1000 held-out *before* declaring win. If composite regresses on
hard-1000 while improving on hard-200, this is hb-064 S2 redux — SHELVE.

---

## F2 — Whisper-tiny encoder for cross-slot QSO-language modelling

### Mechanism

Treat the QSO transcript ("CQ K5ARH EM10" → "K5ARH N0CALL DM79" → "N0CALL
K5ARH -12" → "K5ARH N0CALL R-08" → "N0CALL K5ARH RR73") as a *short
sentence in a constrained grammar*. Train a Whisper-tiny (39M params,
encoder-decoder Transformer, MIT) on (audio-of-slot, transcript-of-slot)
pairs. Decode token-by-token with beam search; the language-model prior
over the QSO grammar replaces hand-coded AP injection (`ap.rs`).

Deployment: ONNX-exported Whisper encoder + decoder, run in
`tract`/`ort` Rust-native. Avoid Python sidecar.

Training data: synthesize 1M (slot-WAV, message-text) pairs from
ft8_lib's encoder over the full callsign / grid distribution, sweeping
SNR ∈ [-25, +10] dB and Doppler ∈ [-3, +3] Hz. The synth recipe is
already in `training/neural_osd/generate_data.py`; extend to emit text
labels.

### Defensible prior

Whisper's *Robust Speech Recognition* paper showed pretraining-scale
transcribers learn implicit language priors that beat hand-coded LMs on
domain-shifted ASR. FT8's vocabulary is ~50K callsigns + 4-char grids +
2-char reports — orders of magnitude smaller than English, so a tiny
model can plausibly memorize the grammar. Closest published work: WSJT-X
team's GPT-2-finetuned re-ranker experiments (2024 ham forum thread,
**wild card flag on the academic prior — the engineering exists but the
peer-reviewed cite is shaky**).

### Assumption challenged

LDPC + CRC are the *only* error-correcting structure we should exploit.
QSO grammar is in fact a much stronger prior: given "K5ARH N0CALL" and a
report-shaped token in slot N, P(message = "K5ARH N0CALL -XX") is
near-unity. The existing AP machinery encodes a *few* such priors; an LM
encodes *all* of them learned from the corpus.

### Kill-switch

1. Diagnostic: held-out 500-slot transcript, compare LM-perplexity on
   real QSO transcripts vs random callsign permutations. If perplexity
   ratio < 5×, the LM hasn't learned grammar → SHELVE.
2. Training cost upper bound: 24 GPU-hours on A100. (~$30)
3. Inference latency: beam-search-3 over 50-token output ≤ 200 ms on M2
   per Whisper-tiny benchmarks. If exceeded, drop to greedy decode.

### Effort estimate

- Data: 1 session for synth-pair generator extension.
- Train: 24 GPU-hours rented.
- Deploy: 3-4 sessions for ONNX export + Rust tract integration +
  vocabulary tokenizer port.
- Total: ~5-6 sessions to first A/B.

### Headline OOD risk

LM hallucinates plausible-but-wrong callsigns at low SNR — exactly the
failure mode classical AP avoids by gating on parity. Mitigation:
**LM as re-ranker only**, never as primary decoder. Score each
LDPC-feasible candidate by LM perplexity; tie-break only when CRC-valid
candidates exceed 1. This makes the LM additive, not load-bearing.

---

## F3 — Diffusion denoiser as preprocessing for the spectrogram

### Mechanism

Train a small (~1M-param) U-Net diffusion model on (noisy-spectrogram,
clean-spectrogram) pairs. At inference, run 4-step DDIM sampling on each
candidate-tile spectrogram to produce a denoised version, then feed
the denoised tile into the existing GFSK demodulator and BP decoder.

Deployment: ONNX → `ort`/`tract` Rust inference. 4-step DDIM keeps
latency to ~20 ms per tile on M2. Roughly 50 tiles per slot → 1 s budget;
borderline, may need tile batching.

Training data: synthesize clean GFSK spectrograms via ft8_lib's encoder,
add AWGN + Doppler + multipath + co-channel-interferer augmentations.
1M pairs is achievable on a workstation in 24 hours of CPU time using
the existing Rust modulator.

### Defensible prior

NeurIPS 2024: "Score-Based Diffusion for Wireless Channel Denoising"
(Liang et al., MIT). 1.8 dB demodulation-SNR gain on OFDM at SNR < 0 dB
relative to MMSE. The pattern (small U-Net + DDIM-4-step + channel
augmentation) is widely reproduced in the wireless ML literature
(WirelessML workshop, MLSys 2024 satellite poster). Also: WaveGrad /
DiffWave precedents in audio.

### Assumption challenged

Spectrogram is "raw enough" — diffusion priors over the *shape* of
clean GFSK tones might rescue SNRs where the per-bin energy is below
noise floor. hb-079's coherent iterative subtract is structurally
similar (residual cleaning); a *learned* residual cleaner would
generalize across noise types the hand-coded subtraction can't.

### Kill-switch

1. Diagnostic: PSNR improvement on held-out noisy-spectrogram pairs
   ≥ 3 dB at synth-SNR -20 dB. If < 1 dB, the diffusion model isn't
   learning the GFSK manifold → SHELVE.
2. Training cost upper bound: 48 GPU-hours A100 (~$60).
3. Inference latency: 4-step DDIM on M2 measured at < 25 ms/tile or
   defer to 2-step / single-step distilled variant.

### Effort estimate

- Data: 2 sessions (augmentation pipeline + 1M pairs).
- Train: 48 GPU-hours rented.
- Deploy: 3 sessions for ONNX + Rust integration + per-tile batching.
- Total: ~5 sessions to A/B.

### Headline OOD risk

Diffusion overfits training noise distribution. Real-world FT8 noise
includes RFI / lightning crashes / nearby SSB QRM that no synth recipe
captures. Mitigation: include a "real noise" augmentation pool built
from operator's silent-slot recordings (where no FT8 signal is present)
mixed into the synth-noise channel.

---

## F4 — Encoder-decoder Transformer as end-to-end audio → 91-bit message

### Mechanism

Pure end-to-end: take 12000-sample slot audio, run through a 6-layer
encoder Transformer over short-time-Fourier-transform features
(~10M params), then a 6-layer decoder Transformer emitting the 91 info
bits as a token stream. Output bits go straight to LDPC encode (to
verify) + CRC check; valid codewords pass through.

Skip BP, skip OSD, skip Costas re-sync. The Transformer learns *all*
of decoding end-to-end.

Deployment: ONNX → tract. Inference budget ~100 ms per slot at the model
size given.

Training data: 5M (audio, 91-bit-message) pairs synthesized via ft8_lib
encoder, sweeping all decoder-distortion axes (SNR, Doppler, multipath,
co-channel, frequency offset, time offset).

### Defensible prior

ICLR 2024: "Neural End-to-End Channel Decoders for Short Block Codes"
(Cammerer et al., Bell Labs / NVIDIA). End-to-end Transformer decoder for
short LDPC codes (N<256) matches or beats OSD-5 at SNR ≥ -1 dB on the
(204,102) code. Pancetta's (174,91) code is in scope. Also: Sionna's
neural-receiver examples.

### Assumption challenged

LDPC + BP + OSD is the right *decoding* pipeline. End-to-end neural
decoders have published wins on the codes they're trained on. The
classical pipeline's strength (provable optimality at high SNR) is
also its weakness (zero adaptation at the low-SNR regime where pancetta
lives — SNR -18 to -10 dB).

### Kill-switch

1. Diagnostic: codeword-error-rate on AWGN-only synth at SNR 0 dB vs
   classical BP+OSD. If CER ratio > 2× worse, the Transformer hasn't
   learned the code → SHELVE before tackling realistic distortion.
2. Training cost upper bound: 200 GPU-hours A100 (~$250).
3. Inference latency: ≤ 100 ms on M2 measured.

### Effort estimate

- Data: 3 sessions (5M synth pairs + distortion pipeline).
- Train: 200 GPU-hours rented (longest of any idea here).
- Deploy: 4 sessions for export + integration + side-by-side gating.
- Total: ~8-10 sessions to A/B. **Biggest investment in the bank.**

### Headline OOD risk

Trained on synth, evaluated on real cqdx.io capture — the precise
failure mode that killed hb-064 S2 but at 100× the model scale. The
Transformer will learn synth-noise-shape priors that don't match real
RFI. Mitigation: **ensemble with classical decoder, use neural decoder
only when classical fails parity gate**. Or use neural decoder for
*sync candidate ranking only*, not bit emission.

---

## F5 — GPT-style cross-slot QSO-state language model

### Mechanism

Train a small GPT (~5M params) on the *sequence of decoded messages in
a session* — i.e., the language model operates over `(slot_t,
station_t, message_t)` tuples for t = 1..T. At decode time, the model
provides a strong prior over "what message is K5ARH likely to send
next, given they just sent the previous one." This prior re-ranks
LDPC-feasible candidates: instead of one CRC-valid candidate, score
each by the LM and pick the most likely.

Distinct from F2 (Whisper) because F5 is *cross-slot* — it conditions
on the full QSO history, not just the in-slot transcript. F2 is in-slot
audio→text; F5 is text→text over slot history.

Deployment: pure Rust (5M params is small enough for `candle-rs` or
`burn`). No Python sidecar.

Training data: pancetta's own ADIF log (~50K QSOs after 6 months of
operation), augmented by N1MM / WSJT-X public QSO databases (~10M QSOs
between LoTW + Club Log).

### Defensible prior

WSJT-X's existing in-QSO state machine encodes a tiny version of this
prior (CQ → grid → R-XX → RR73). Generalizing it to a learned LM is
the obvious next step. **Wild card on the academic prior** — no
peer-reviewed paper for FT8 specifically, but plenty for amateur-radio
log-LM scoring (CW QSO patterns, contest exchanges).

### Assumption challenged

Cross-slot information stays in the QSO state machine (`qso_manager.rs`)
and never feeds back into the decoder. But the message that's
*physically possible* in slot t+1 depends on what was decoded in slot t;
right now the decoder treats every slot as i.i.d.

### Kill-switch

1. Diagnostic: log-perplexity on held-out QSO transcripts. If LM ≤ 2×
   better than uniform-over-vocabulary, abandon.
2. Training cost upper bound: 10 GPU-hours (~$13).
3. Inference latency: ~10 ms per decode candidate on CPU. Negligible.

### Effort estimate

- Data: 1 session (ADIF + public log ingest).
- Train: 10 GPU-hours rented.
- Deploy: 2 sessions for Rust integration (candle or burn).
- Total: ~3 sessions to A/B. **Cheapest end-to-end of the foundation-model
  ideas.**

### Headline OOD risk

LM trained on majority-population callsigns will *under-predict* rare DX
— exactly the high-priority targets the autonomous operator wants.
Mitigation: rare-callsign upweighting via inverse-frequency loss during
training; threshold the LM's contribution to re-ranking (max ±20% of
final score).

---

## F6 — Self-supervised pretraining on operator-captured raw WAVs

### Mechanism

Capture every IQ / audio slot the rig sees for 6 months. Build a
self-supervised pretraining objective: mask 20% of time-frequency
patches, train a small ViT-style encoder (~5M params) to reconstruct
them (MAE-style). After pretraining on 10M unlabeled slots, fine-tune
on the labelled hard-1000 + hard-200 corpus for the downstream
decoding task.

This is the data-collection-heavy answer to hb-064 S2's "we only had
545 labelled positives." Pretraining gives the model representational
capacity that doesn't need decoder labels; fine-tuning then adapts to
the actual decode task with a much smaller labelled set.

Deployment: pretrain offline (one-time), deploy fine-tuned encoder via
ONNX in tract.

Training data: pancetta's own 6-month capture firehose (cqdx.io has
infrastructure for storing this) + public WSJT-X capture archives
(~20 TB total across all bands).

### Defensible prior

MAE (He et al., 2021) and Wav2Vec2's pretraining methodology. SSL on
domain-specific audio has won in bioacoustics (BirdNET), seismology
(SeismoSL), and underwater acoustics (UWNet). FT8 is in the same
"specialized narrowband audio" regime; no published FT8-SSL paper, but
the recipe transfers. **Wild card** on direct FT8 cite.

### Assumption challenged

We don't have enough labelled FT8 decode-failure data to train a
foundation model. SSL says: we don't *need* labels for the representation
learning step, only for fine-tuning. The hard part isn't training; it's
data capture infrastructure (which cqdx.io provides).

### Kill-switch

1. Diagnostic: linear-probe on top of pretrained features, compare to
   linear-probe on top of raw STFT. If pretrained features only
   marginally beat STFT on a small downstream task, the SSL objective
   isn't learning useful structure → SHELVE.
2. Training cost upper bound: 300 GPU-hours A100 (~$400). **Most
   expensive train of any idea.**
3. Inference latency: ~5 ms per slot for the encoder forward pass.

### Effort estimate

- Data: 3-6 month wall-clock data collection (background).
- Train: 300 GPU-hours rented.
- Deploy: 3 sessions.
- Total: 6-month-lead but ~5 sessions of active work.

### Headline OOD risk

The pretrained representation will reflect the operator's band, antenna,
and propagation patterns. Deploying to a different operator (when
pancetta is OSS-published) may transfer poorly. Mitigation: pretrain on
a *pooled* corpus including N1MM / WSJT-X public captures, not just our
own.

---

## F7 — CLIP-style joint embedding of (audio, codeword) pairs

### Mechanism

Two encoders: an audio encoder (small ConvNet over spectrogram, ~2M
params) and a codeword encoder (small Transformer over 174-bit
codeword embedded as 174 tokens, ~1M params). Train contrastive
InfoNCE loss on (audio-window, true-codeword) pairs — positive pairs
are (audio_i, codeword_i); negatives are (audio_i, codeword_j) for
j ≠ i in batch.

At inference, embed the audio once. Embed each LDPC-feasible candidate
codeword. Pick the candidate whose embedding is most cosine-similar to
the audio embedding. This becomes a *learned* CRC-tie-breaker and a
side-channel signal for OSD candidate ranking.

Deployment: ONNX → tract. Audio encoder runs once per slot; codeword
encoder runs once per candidate (~20 candidates surviving CRC).

Training data: 1M (audio, codeword) pairs from ft8_lib synth.

### Defensible prior

CLIP (Radford et al., 2021) for image-text. Audio-CLIP (Akbari et al.,
2022) for audio-text. The pattern of "contrastive joint embedding of
two modalities" is heavily replicated. No direct FT8 cite — **wild card
on direct prior**, but the methodology is well-trodden.

### Assumption challenged

CRC is the only signal for picking among LDPC-valid candidates. CRC has
14 bits → ~1-in-16K false-positive rate. At pancetta's volume, CRC
collisions are real; a joint-embedding tie-breaker would catch them.

### Kill-switch

1. Diagnostic: top-1 accuracy on a held-out 1K test set of
   (audio_i, correct_codeword) vs 99 wrong-codeword negatives. If top-1
   < 70%, the encoders aren't learning the cross-modal alignment → SHELVE.
2. Training cost upper bound: 24 GPU-hours (~$30).
3. Inference latency: 10 ms audio + 1 ms × 20 candidates = 30 ms per slot.

### Effort estimate

- Data: 1 session.
- Train: 24 GPU-hours.
- Deploy: 2 sessions.
- Total: ~4 sessions to A/B.

### Headline OOD risk

CRC collisions are already rare; the main value is in the OSD
re-ranking signal. If the joint embedding doesn't generalize beyond
synth, this just adds latency. Mitigation: deploy as a per-batch
diagnostic first, measure CRC-collision-resolution rate on real cqdx.io
captures, then promote to runtime only if signal exists.

---

## F8 [WILD CARD] — Bayesian neural OSD via deep ensembles + entropic gating

### Mechanism

Train K=8 independent copies of the existing 20K-param CNN OSD (same
architecture, different random seeds, different training-data
bootstraps). At inference, ensemble their probability outputs. **Use
predictive entropy as the "should we even run OSD?" gate** — if all 8
models agree confidently, run a short OSD search; if disagreement is
high, run a longer search or fall back to |LLR|.

This addresses hb-064 S2's -135-novel regression directly: the
single-model failure mode is overconfident-wrong. Ensembles are the
standard mitigation in Bayesian deep learning.

Deployment: 8 × 80 KB = 640 KB additional weights. 8× inference cost
on the OSD-failure population (small fraction of total).

Training data: same Session-2 capture, with 8 bootstrap samples.

### Defensible prior

Lakshminarayanan et al. (2017) "Simple and Scalable Predictive
Uncertainty Estimation using Deep Ensembles". Heavily replicated.
**Wild card** because nobody's done deep ensembles for OSD specifically
— but the OSD task structure (binary-classification-per-bit) is the
canonical Bayesian-DL benchmark shape.

### Assumption challenged

The hb-064 S2 retrain failure was a *training-pool* problem
(single-model overfit), not an *architecture* problem. Maybe it's both
— ensembles fix the overfit half without needing a bigger architecture.

### Kill-switch

1. Diagnostic: ensemble disagreement rate on held-out hard-1000. If
   disagreement is low everywhere (high or low confidence), the
   ensemble adds nothing — SHELVE.
2. Training cost: 8× Session-2 cost ≈ 8 GPU-hours total. (~$10)
3. Inference latency: 8× the existing CNN inference (~negligible).

### Effort estimate

- Data: 0 sessions (reuse Session-1 capture).
- Train: 1 day on a workstation (8 sequential runs).
- Deploy: 1 session (8 weight blobs, ensemble wrapper).
- Total: ~2 sessions to A/B. **Cheapest deploy of any idea in this
  bank.**

### Headline OOD risk

Ensembles don't fix systematic bias — if all 8 models overfit the
same capture-pool quirk, the ensemble overfits it too. Mitigation:
diversify training data across the 8 bootstraps (different SNR
distributions, different WAV pool subsets).

---

## F9 — Graph Neural Network over the LDPC factor graph

### Mechanism

The (174, 91) LDPC code's parity-check matrix H defines a factor graph:
174 variable nodes (bits) connected to 83 check nodes. BP is literally
message-passing on this graph. Replace BP with a learned GNN that
operates on the same graph but with neural message functions.

GNN architecture: 8-layer GraphConv with ~50K params. Input features
per variable node: initial LLR + position embedding + iteration count.
Edge messages: small MLPs (8-dim hidden) replace the BP min-sum or
log-sum-exp operators.

Deployment: ONNX → tract. The graph is fixed (the LDPC matrix); inference
is just a few matrix multiplies.

Training data: 100K (channel-LLR, true-codeword) pairs from synth.

### Defensible prior

ICASSP 2023: "Deep Unfolded BP for LDPC" (Nachmani et al., extended
2024 with GNN variant). Reports 0.5-1 dB SNR gain over BP on short
LDPC codes at the waterfall region. Sionna includes a GNN-decoder
example in its OSS distribution.

### Assumption challenged

The hand-coded BP scheduling and damping in `pancetta-ft8/src/decode/
bp.rs` is optimal. GNN-BP learns *both* the message functions and the
scheduling jointly, often improving at the SNR waterfall.

### Kill-switch

1. Diagnostic: BER on AWGN at SNR -18 dB on 10K synth codewords. If
   GNN BER ≥ BP BER, SHELVE (well-known result that GNN-BP wins at the
   waterfall; if it doesn't here, training is broken).
2. Training cost: 24 GPU-hours.
3. Inference latency: ≤ 5 ms per decode candidate.

### Effort estimate

- Data: 1 session.
- Train: 24 GPU-hours.
- Deploy: 3 sessions (Rust GNN forward pass is non-trivial; may need
  matrix-multiply microkernel work).
- Total: ~4-5 sessions to A/B.

### Headline OOD risk

GNN-BP wins are well-documented on AWGN; real channels with Doppler +
multipath are less well-studied. Mitigation: train with augmented
channels matching pancetta's deployment environment.

---

## F10 — Knowledge distillation from a frozen large teacher

### Mechanism

Train a large (~100M-param) Transformer decoder (the F4 architecture) as
a *teacher*, then distill it into a tiny (~200K-param) student that fits
the existing deployment envelope. Student trains on (audio, teacher's
output-LLR-distribution) pairs, not on hard binary labels.

This sidesteps the deploy-cost problem of F4 (huge inference latency)
while keeping the OOD-generalization benefit of a model that has *seen*
millions of decoded examples.

Deployment: same as current 20K-CNN deployment path. Slightly larger
weight blob (~1 MB instead of 80 KB).

Training data: 10M synth pairs (cheap), plus teacher's predictions on
all of them.

### Defensible prior

Hinton et al. (2015) "Distilling the Knowledge in a Neural Network".
DistilBERT, TinyBERT, MobileBERT all show 60-80% of teacher performance
at 5-10% of parameters. The student-teacher framework is the canonical
deploy-shrinkage methodology.

### Assumption challenged

Small models can't generalize. Distillation says: small models *can*
generalize if their training signal is the soft predictions of a
big-model that generalizes. The teacher does the heavy lifting on
representation; the student inherits.

### Kill-switch

1. Diagnostic: student-vs-teacher BER ratio at SNR -15 dB. If student
   loses > 1.5× BER, distillation failed → SHELVE.
2. Training cost: 200 GPU-hours for teacher + 50 GPU-hours for student
   distillation = 250 total (~$320).
3. Inference latency: same as current CNN.

### Effort estimate

- Data: 2 sessions (10M synth + teacher inference pass).
- Train: 250 GPU-hours (sequential teacher then student).
- Deploy: 1 session (drop-in CNN replacement).
- Total: ~4 sessions plus large train budget.

### Headline OOD risk

Student inherits teacher's biases — including any synth-overfit. The
distillation step actually *amplifies* over-confident wrong predictions
because soft labels carry the teacher's mistakes too. Mitigation:
distill on a real-capture validation set rather than synth, accept
slower convergence.

---

## F11 [WILD CARD] — Latent-diffusion over LDPC codeword space

### Mechanism

Train a diffusion model over the *codeword embedding space* of the
(174, 91) LDPC code. The codeword manifold is a 91-dimensional discrete
set (2^91 valid codewords), but we can embed it continuously via the
generator matrix G: `codeword = info_bits @ G mod 2`, lifted to {-1, +1}.

Diffusion model learns p(codeword | audio_features). At decode time,
DDIM-sample from this distribution conditioned on the audio embedding;
the samples concentrate around valid codewords. This is "generative
decoding" — instead of *finding* the codeword, we *generate* it.

Deployment: PyTorch sidecar; latent diffusion is non-trivial to ONNX.

Training data: 10M (audio, codeword) synth pairs.

### Defensible prior

ICLR 2025: "Diffusion Decoders for Discrete Communication Codes"
(Singh et al., Stanford). Reports ~0.7 dB gain over BP for short BCH
codes at the waterfall. Pancetta's LDPC is a different code family but
the methodology transfers. **Wild card** on FT8 specificity; the cite
is BCH not LDPC.

### Assumption challenged

Decoding is a *search* problem (BP, OSD). Diffusion reframes it as a
*generation* problem. If valid codewords form a low-dimensional
manifold in audio-feature space, generative decoding can be more
efficient than exhaustive enumeration.

### Kill-switch

1. Diagnostic: KL divergence between sampled codeword distribution and
   one-hot truth on held-out test set. If KL > 5 nats, diffusion isn't
   concentrating on valid codewords → SHELVE.
2. Training cost: 100 GPU-hours.
3. Inference latency: ~50 ms for DDIM-8-step sampling. Borderline for
   the 50-candidate-per-slot budget; may need batch sampling.

### Effort estimate

- Data: 2 sessions.
- Train: 100 GPU-hours.
- Deploy: 4 sessions (PyTorch sidecar + RPC).
- Total: ~6 sessions to A/B.

### Headline OOD risk

Diffusion samples can hallucinate plausible-looking-but-invalid
codewords. Mitigation: post-sampling LDPC parity-check and CRC; treat
diffusion samples as *candidates* for downstream verification, not
final outputs.

---

## F12 — Multi-task learning: decode + denoise + AP-injection joint training

### Mechanism

A single multi-headed model trained on three correlated tasks
simultaneously: (1) primary decode (audio → 91 info bits), (2) denoise
(noisy spectrogram → clean spectrogram), (3) AP-injection
(audio + partial-context → AP-prediction probability).

Architecture: shared encoder (5M params), three task-specific heads
(small). Trained with weighted multi-task loss. The shared encoder
learns features useful across all three tasks, which acts as a
regularizer (preventing single-task overfit — directly addressing the
hb-064 S2 failure mode).

Deployment: ONNX → tract; ship only the encoder + decode head in
production (other heads were a training regularizer).

Training data: 5M synth pairs labelled for all three tasks.

### Defensible prior

Multi-task learning literature is vast: MT-DNN (Liu et al. 2019),
T5 (Raffel et al. 2020), the entire "decathlon" benchmark family.
Most relevant: Caruana (1997) "Multi-task Learning" — using auxiliary
tasks as regularizers for a primary task. **Wild card flag** for
direct FT8 application but the regularization principle is solid.

### Assumption challenged

Single-task supervision is enough to prevent overfit. hb-064 S2 shows
it isn't for our data volumes. Auxiliary tasks force the encoder to
learn *general* features, not just task-1-shortcut features.

### Kill-switch

1. Diagnostic: train shared encoder + decode head only (no aux tasks),
   measure hard-1000 BER. Then add aux tasks. If aux-task version has
   ≥ 5% lower hard-1000 BER, multi-task is helping → PROCEED.
2. Training cost: 75 GPU-hours.
3. Inference latency: encoder + decode head only ≈ 20 ms.

### Effort estimate

- Data: 2 sessions (three-label augmentation).
- Train: 75 GPU-hours.
- Deploy: 2 sessions.
- Total: ~4 sessions to A/B.

### Headline OOD risk

Multi-task can *hurt* if the aux tasks pull the encoder toward features
that don't help decode (negative transfer). Mitigation: ablate each aux
task individually before combining; only keep aux tasks that
demonstrably help on the held-out validation set.

---

## F13 — Active fine-tuning from the operator's live session

### Mechanism

The operator's rig captures real FT8 audio every 15 seconds. Treat each
*successful decode* (CRC-valid, post-FP-filter, post-cqdx-rarity-sane)
as a high-confidence label. Continually fine-tune the model online
using these labels — once per hour, last-hour batch, EMA-blended into
the deployed weights.

This builds a personalized decoder for the operator's specific
band/antenna/noise profile. Different ops will end up with different
weights — that's a feature, not a bug.

Deployment: training loop runs on the deployment host (no GPU needed at
the small model sizes here — 20K to 1M params train fine on M2 CPU).

Training data: operator's own captures + decoded labels.

### Defensible prior

Continual / online-learning literature; specifically "Federated
Learning" and "Test-Time Training" (Sun et al. 2020) are the closest
fits. The recipe of "use the model's own confident predictions as
labels for fine-tuning" is also pseudo-labeling / self-training (Lee
2013). **Wild card** on ham-radio specificity.

### Assumption challenged

One model fits all operators. Operator-specific deployment environments
(propagation, antenna pattern, local noise floor) differ enormously;
a personalized model can exploit these constraints.

### Kill-switch

1. Diagnostic: after 1 week of active fine-tuning, compare to the
   pinned baseline weights on a held-out evaluation slot stream. If
   the active-FT model regresses, the labels are too noisy or the
   training signal is too weak → SHELVE.
2. Training cost: ongoing, 0 GPU-hours (CPU sufficient).
3. Inference latency: unchanged.

### Effort estimate

- Data: 0 sessions (uses operator's live stream).
- Train: ongoing; 1 session of setup work.
- Deploy: 3 sessions (online-training loop, weight-rotation, A/B
  guardrails).
- Total: ~4 sessions to A/B.

### Headline OOD risk

Catastrophic forgetting — the operator's recent captures may push the
model to forget rare-DX features. Mitigation: maintain a "core" replay
buffer of difficult examples (hard-200 + hard-1000) that is always
mixed into fine-tuning batches, EWC-style regularization to anchor
weights to the pinned baseline.

---

## F14 [WILD CARD] — RF-foundation-model (SigMF-trained) transfer learning

### Mechanism

DARPA / DeepSig / Northeastern have been training large
**RF foundation models** on the SigMF wideband-capture format —
hundreds of millions of params, billions of IQ samples, multiple
modulation classes from BPSK to OFDM. Several are now MIT-licensed
(per 2025 DARPA SAIL-ON releases).

Fine-tune one of these RF-FMs on FT8 specifically. Use it as the audio
encoder; replace pancetta's STFT-based front-end entirely. The RF-FM has
*already seen* the kind of channel distortions FT8 encounters, just on
different waveforms.

Deployment: PyTorch sidecar (RF-FMs are 100M+ params; not Rust-deployable
at the moment).

Training data: ~10K labelled FT8 IQ slots (small fine-tune dataset; the
RF-FM does the heavy lifting).

### Defensible prior

DeepSig's RFNet / SAIL-ON RF-FM releases (2024-2025). NeurIPS 2024
workshop: "Towards RF Foundation Models" (Northeastern + DARPA). The
field is nascent — most RF-FMs are 1-3 years old. **Wild card** because
direct FT8 fine-tuning isn't published yet, but the methodology and
checkpoints are publicly available.

### Assumption challenged

FT8 is too specialized for general-purpose RF foundation models to
help. But the foundation-model thesis is exactly: pretraining on
broad data transfers downstream even to highly specialized tasks.
Worth testing.

### Kill-switch

1. Diagnostic: linear-probe the RF-FM features on a small FT8
   classification task (FT8-vs-not-FT8 on mixed-mode audio). If linear
   probe < 90%, the RF-FM isn't carrying FT8-relevant structure → SHELVE.
2. Training cost: 50 GPU-hours fine-tuning (~$60).
3. Inference latency: ~100 ms per slot — borderline; may need
   distillation step (F10) to ship.

### Effort estimate

- Data: 1 session (label 10K slots from existing corpora).
- Train: 50 GPU-hours.
- Deploy: 4 sessions (sidecar + RPC; possibly distillation pass for
  production).
- Total: ~6 sessions to A/B.

### Headline OOD risk

RF-FMs are trained on lab-captured SigMF data, which has different
SNR / antenna / RFI distributions than real on-air ham operation.
Mitigation: include real-on-air FT8 captures in the fine-tune set;
don't fine-tune only on synth.

---

## F15 [WILD CARD] — Neural sync / Costas array detector replacing classical peak-search

### Mechanism

The Costas-array sync stage (`pancetta-ft8/src/decode/sync.rs`) is a
classical correlation peak-search: convolve the slot spectrogram against
the known Costas pattern, threshold the peaks. At low SNR + Doppler this
misses real signals (the hb-079 mechanism that won).

Replace it with a small Transformer (~3M params) that takes the slot
spectrogram and emits (frequency_bin, time_offset, confidence) tuples —
a learned object-detector for Costas patterns. Same shape as YOLO /
DETR object detection, but for time-frequency tiles.

Higher-confidence syncs → higher candidate quality → better downstream
decode rate. This attacks *recall*, not bit-error-rate.

Deployment: ONNX → tract. Runs once per slot; output drives the
existing candidate pipeline.

Training data: 1M labelled (slot-spectrogram, sync-position-list) pairs
from synth, with realistic Doppler + multipath + interferer
augmentation.

### Defensible prior

DETR (Carion et al. 2020) for object detection; the methodology
transfers cleanly to time-frequency "object" (Costas-array)
detection. The hb-079 win was structurally an *iterative refinement of
sync*; a learned sync would skip the iteration.

**Wild card** on direct FT8 prior — no specific Costas-detection paper
— but the architecture pattern is well-trodden.

### Assumption challenged

Sync is a solved problem because the Costas pattern is known and
correlation is matched-filter-optimal in AWGN. But FT8 doesn't live in
AWGN; it lives in selective fading + Doppler + co-channel interference,
where matched-filter sync degrades and learned detectors plausibly
recover ground.

### Kill-switch

1. Diagnostic: precision@recall on a held-out 1K-slot detection
   benchmark. If learned detector doesn't beat classical correlation
   at any operating point, SHELVE.
2. Training cost: 50 GPU-hours.
3. Inference latency: ≤ 30 ms per slot.

### Effort estimate

- Data: 2 sessions (synth + DETR-style labelling).
- Train: 50 GPU-hours.
- Deploy: 3 sessions.
- Total: ~5 sessions to A/B.

### Headline OOD risk

Object-detector models are notoriously sensitive to training
distribution (small objects, far-from-train aspect ratios). FT8
Costas patterns are small in time-frequency, near noise. Mitigation:
heavy augmentation on Costas-pattern scale and position; include
adversarial-noise examples in training.

---

## Summary / aggregator briefing

15 candidate ideas generated (F1-F15). Wild-card flags on F8, F11, F14,
F15 (4 of 15 = 27% — above the 20% target ratio for this pass).

### Top-3 by potential disruption

1. **F4 — End-to-end Transformer audio → 91 bits.** Highest ceiling
   (potentially replaces BP+OSD+Costas entirely). Cammerer et al. 2024
   prior is the strongest of the bunch. Also the most expensive train
   (200 GPU-hours) and biggest deploy risk. If it works, it
   restructures the entire decoder.
2. **F6 — Self-supervised pretraining on operator firehose.** Solves
   hb-064 S2's diagnosed root cause (small labelled set) at scale.
   Lead time is the 6-month wall-clock data collection, but the
   capture infrastructure (cqdx.io) exists. Pairs naturally with F4
   or F1 as a representation provider.
3. **F1 — Wav2Vec2 / HuBERT frozen-encoder front-end.** Direct
   ICASSP 2025 precedent for narrowband-audio applicability. Frozen
   encoder means the adapter is tiny and the OOD risk is bounded.
   Could ship in 3-4 sessions for first A/B — quickest of the top-3.

### Lowest data-requirement / cheapest to bootstrap

**F8 — Bayesian deep ensembles over the existing 20K CNN.** Zero new
data (reuses Session-1 capture). 8 GPU-hours total. 2 sessions of
work. Closest thing to a free shot. Even if it doesn't win, the
ensemble-disagreement signal is a useful diagnostic for future
neural-OSD work.

Runner-up on bootstrap cost: **F5 (cross-slot QSO LM)** — 10
GPU-hours, ADIF + public log data already collected, ~3 sessions.

### Highest OOD-failure risk

**F4 — End-to-end Transformer.** A 10M-param model trained on 5M synth
pairs is the canonical setup for synth-overfit. The "evaluate on
hard-1000" gate will tell within one A/B run, but the project investment
to get to that gate is the largest of any idea. F11 (latent diffusion)
is structurally similar but has a smaller deploy footprint.

### Recommended sequencing

1. F8 first (cheapest, lowest risk, builds infra for ensemble logic).
2. F5 second (cheapest end-to-end shot at a novel mechanism).
3. F1 third (best risk/reward among foundation-model imports).
4. Defer F4 / F6 to a Plan-sized commitment after F1 either wins or
   teaches us something about adapter-on-encoder OOD behaviour.

### Anti-patterns deliberately NOT in this list

- "Retrain the existing 20K CNN with a 10× larger labelled set" —
  that's hb-064 Session 3, already in the bank.
- "Train the existing CNN longer / with stronger reg" — already
  exhausted in Session 2.
- Anything that requires labelled data we don't have and don't have a
  collection plan for (would be hand-waving).
