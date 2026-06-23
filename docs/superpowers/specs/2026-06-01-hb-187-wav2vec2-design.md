# hb-187 — Wav2Vec2 frozen-encoder front-end (Session 2 design)

**Status:** Design spec for Session 2 implementation. Session 1 feasibility
PROCEED on 2026-06-01.

**Source:** F1 in `research/ideation/2026-06-01-foundation-models.md`.

**Prior session:** `research/experiments/2026-06-01-hb-187-session1.md` —
Wav2Vec2 loads on MPS, embeddings show non-degenerate structure (pairwise
cosine mean 0.698, PC1=45%, effective rank 4.7), per-WAV latency ~145 ms.

---

## One-line goal

Train a small adapter head on top of the frozen `facebook/wav2vec2-base`
encoder to emit **174 LDPC-pre-bit LLRs**, then plug those LLRs into the
existing BP decoder in place of (or alongside) the Gaussian-noise LLRs
from the GFSK demodulator.

---

## Architecture

```
audio_window_16k  [T_audio samples]                                  (3.2s window @ 16kHz; per-candidate)
       │
       ▼
Wav2Vec2Model (frozen, facebook/wav2vec2-base, 94M params)
       │
       ▼
last_hidden_state  [T_seq, 768]                                       (T_seq ≈ T_audio/320 ≈ 160 frames)
       │
       ▼  attention-pool over time, OR mean-pool, OR keep-as-sequence
       ▼
adapter_head: MLP(768 → 512 → 256 → 174)
  - Dense(768, 512) + GELU + LayerNorm + Dropout(0.1)
  - Dense(512, 256) + GELU + LayerNorm + Dropout(0.1)
  - Dense(256, 174)  -> 174-dim soft logit
       │
       ▼
LLR_out  [174 floats]   -> scale to match Gaussian-LLR distribution, into BP
```

Total head params: 768·512 + 512·256 + 256·174 ≈ **574K params** trainable.
Encoder is frozen — `requires_grad=False` on all 94M Wav2Vec2 weights.

### Why mean-pool initially (not attention-pool)

For Session 2 V1, mean-pool the 768-dim sequence to a single 768-dim vector
before the MLP. Rationale:
1. Session 1 confirmed mean-pooled embeddings carry structure (cos in
   [0.33, 1.0], effective rank 4.7 on 10 unlabeled samples).
2. Mean-pool is the dumbest viable baseline; if it fails, attention-pool
   probably won't save it.
3. Attention-pool adds ~200K params and a hyperparameter (number of heads)
   that would compete for the same training budget.

If V1 mean-pool clears the kill-switch but composite is sub-graduation,
V2 will swap to attention-pool with 4 heads.

### Why the 3.2s window per candidate

FT8 is a 12.64s slot, but the decoder operates on **per-candidate** sync
windows after Costas pruning. The existing decoder hands each candidate
a 79-symbol × 160-samples/symbol = 12640-sample audio buffer at 12 kHz,
or **3.16s of audio**. Resampled 4/3 → 4213 samples at 16 kHz, which is
13 Wav2Vec2 frames. We will use the full 3.16s as the input window per
candidate, not the entire slot.

Tradeoff: slot-level encoding (12.64s, 51 frames) sees more context but
costs more inference. Session 2 starts with per-candidate; if results are
strong, V3 explores slot-level pooling with attention over the 51-frame
sequence.

---

## Output choice: 174-bit codeword LLRs

The ideation entry lists three output options:

| Option | Cardinality | Pro | Con | Choice |
|---|---|---|---|---|
| 174 codeword LLRs | continuous | drops into BP, composable with existing OSD | requires accurate calibration | **PICK** |
| 91-bit message bits | 2^91 | end-to-end | bypasses LDPC error-correction; can't reuse BP | reject |
| 28-bit callsign tokens | 2^28 | callsign-aware | only solves 1/3 of the message; useless for grids/reports | reject |

Picking **174 codeword LLRs** is the lowest-disruption integration path:
the existing `pancetta-ft8/src/decoder.rs` already consumes a 174-LLR
vector. The adapter just produces a second LLR source, and we A/B:

- Baseline: GFSK Gaussian LLRs (current production)
- Treatment A: Wav2Vec2 adapter LLRs only
- Treatment B: weighted-sum fusion (α·Gaussian + (1-α)·Wav2Vec2)

Treatment B is the realistic deployment target. Treatment A is a sanity
check: if Wav2Vec2-only beats Gaussian-only on any non-trivial subset,
the encoder is carrying signal.

---

## Training data

| Corpus | Source | Size | Purpose |
|---|---|---|---|
| hard-200 | `research/corpus/hard_200/` | 200 WAV slots | OOD validation (held-out, never train) |
| hard-1000 | `research/corpus/hard_1000/` | 1000 WAV slots | composite eval (held-out by default; can sub-sample for train) |
| WSJT-X synth | regenerate via `pancetta-ft8/src/synth.rs` | 30K windows | primary train pool |
| K5ARH capture | `~/.pancetta/recordings/ft8_20260530_*.wav` | 2066 WAVs | secondary train pool (real-world distribution) |

**Labels:** the LDPC-pre-bit ground-truth comes from running the full
existing decoder + OSD on each WAV. Only successful decodes contribute
labels; failed decodes are filtered. Synth windows have ground-truth bits
by construction.

**Augmentation:**
- AWGN at SNR ∈ [-20, +5] dB (matches FT8 operating range)
- Doppler shift ±5 Hz (per WSJT-X spec)
- DT jitter ±0.5s

**Train/eval split:** stratified by source corpus. hard-200 = pure
held-out OOD. hard-1000 = 80/20 train/eval. Synth = 90/10 train/eval.
Capture = 90/10 train/eval.

---

## Loss + optimization

- Loss: **binary cross-entropy** per bit (174 independent sigmoid heads,
  averaged). BCE is the standard for soft-LLR training; the 174 bits are
  conditionally independent given the audio (LDPC constraints are
  enforced downstream by BP, not by the head).
- Optimizer: AdamW, lr=1e-3, weight_decay=1e-4
- LR schedule: cosine to 0, 20 epochs
- Batch: 64 windows (95% memory budget on M2 MPS at FP32)
- Early stopping: patience=3 epochs on held-out eval BCE

**Frozen encoder is critical.** Fine-tuning all 94M Wav2Vec2 params on
~30K windows would overfit catastrophically (≫ the hb-064 Session 2
regression). The whole point of foundation-model transfer is freezing
the encoder.

---

## Inference deployment

### Option A: PyTorch sidecar over UNIX socket (recommended for Session 2)

- Decoder marshals 174-bit candidates as raw audio bytes over UDS
- Python sidecar batches up to 50 candidates per slot, runs Wav2Vec2 +
  head, returns 174 f32 LLRs per candidate
- Latency budget: 500 ms / slot total
- Measured Session 1: 145 ms / full-WAV → ~36 ms / candidate at 3.16s
  audio. At 50 candidates that's 1800 ms — **OVER BUDGET** without
  batching. With batching (50 at once on MPS), expected ~300 ms total
  based on Wav2Vec2 batch-size scaling on Apple Silicon (HuggingFace
  perf table: 8× batched throughput vs serial on M2).
- Kill-switch verification (Session 2 first deliverable): measure actual
  batched-50 latency on M2; abort if > 500 ms.

### Option B: ONNX → Rust (deferred to Session 3+)

- Export Wav2Vec2 to ONNX, run via `tract` or `ort` in-process
- Zero IPC overhead, no Python at deploy time
- Risk: Wav2Vec2 has dynamic-shape convolutions that some ONNX runtimes
  handle poorly; investigate after Session 2 proves the head works

---

## A/B vs production decoder

Standard pancetta scorecard sweep on `cargo run -p pancetta-research --
eval` against:

1. **fixtures** (sacred): must not regress; 0 tolerance
2. **synth** (sacred): must not regress; 0 tolerance
3. **hard-200** (primary): target +20 rec / +10 novel for graduation
4. **hard-1000** (composite): target +30 rec / +20 novel for graduation
5. **composite**: target ≥ +0.005 (this is a foundation-model swing; the
   bar is higher than incremental tweaks)

Comparison baseline: `main` (currently `9121732`). Composite values
quoted in `research/scorecards/` for the same fixtures.

---

## Kill-switches

1. **Diagnostic (BEFORE FULL TRAIN):** 200-sample SNR-sweep WAV pool,
   measure adapter-head LLR ↔ Gaussian-model LLR correlation on a
   *correct* codeword (no decode in the loop). If correlation < 0.4 on
   synth-clean SNR ≥ 0 dB, the encoder isn't carrying tone-occupancy
   signal → SHELVE.
2. **Latency:** batched-50 inference > 500 ms on M2 MPS → SHELVE (or
   defer to Option B/ONNX path; either way, V1 sidecar deployment
   is dead).
3. **Training cost:** if a full 20-epoch run takes > 12 wall-clock hours
   on M2 MPS, switch to rental A100 (~$10) or reduce model to
   wav2vec2-base-960h-lv60 distilled variant.
4. **OOD regression:** hard-200 sample_recovery_rate regresses > 5% vs
   baseline → SHELVE (same gate as hb-064 S2, the failure mode we're
   explicitly trying to avoid).

---

## Session 2 deliverables

1. **Data prep script** — `training/wav2vec2_osd/prep_corpus.py`
   - Loads hard-200, hard-1000, synth, capture corpora
   - Resamples to 16 kHz, extracts per-candidate 3.16s windows + ground-truth bits
   - Writes a `.pt` torch dataset
2. **Train script** — `training/wav2vec2_osd/train.py`
   - Frozen Wav2Vec2 + MLP head, BCE loss, AdamW, cosine LR
   - Checkpoints best by held-out eval BCE
3. **Diagnostic script** — `training/wav2vec2_osd/diagnostic_kill_switch.py`
   - Implements kill-switch (1): LLR correlation on clean synth
   - Run BEFORE the full train
4. **Inference sidecar** — `training/wav2vec2_osd/sidecar.py`
   - UDS server, batches per-slot candidates, returns 174 LLR f32
5. **Scorecard sweep**
   - Run `pancetta-research --eval` with Treatment B (fusion) at α=0.5
     and α=0.7 against `main`
   - Journal disposition: GRADUATE / SHELVE / iterate

---

## Effort estimate

| Phase | Wall-clock |
|---|---|
| Session 2: data prep + diagnostic kill-switch | 2-3 hours |
| Session 2: train script + run | 4-8 hours (M2 MPS) |
| Session 2: sidecar + scorecard A/B | 2-3 hours |
| Session 2 total | ~1 working day |

---

## Open questions for Session 2

1. Is `facebook/wav2vec2-base` the right pretrained checkpoint, or should
   we try `wav2vec2-large` (300M params) or `hubert-base`? Session 1 used
   base; Session 2 starts with base. If V1 graduates, V2 explores larger.
2. Does the Costas-pruned 3.16s window cover enough context, or does the
   encoder need the full 12.64s slot for self-attention to see the
   Costas array repetitions? Stretch goal: an ablation in Session 2 that
   feeds both, pools each, compares head accuracy.
3. Should the head emit 174 LLRs **or** 174 LLRs + 91 message bits (auxiliary
   loss as a regularizer)? Defer — V1 emits 174.
