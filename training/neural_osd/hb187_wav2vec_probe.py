"""hb-187 Session 1: Wav2Vec2 frozen-encoder feasibility probe.

Loads a small sample of FT8 WAVs (12 kHz mono, ~12.64s), resamples to 16 kHz,
feeds them through facebook/wav2vec2-base, mean-pools the per-time-step
transformer activations to a single 768-dim embedding per WAV, and inspects
whether the embeddings carry any meaningful structure (PCA explained variance,
nearest-neighbor cohesion, pairwise distance distribution).

This is a FEASIBILITY check, not a training run. No production code, no labels
required. The question this session answers:

    Does the encoder produce non-degenerate, non-random embeddings when fed
    raw FT8 audio, such that a downstream classification head could plausibly
    learn from them?

Run:
    source training/neural_osd/.venv/bin/activate
    python training/neural_osd/hb187_wav2vec_probe.py
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

import numpy as np
import scipy.io.wavfile as wf
import scipy.signal as sps
import torch
from sklearn.decomposition import PCA
from transformers import Wav2Vec2FeatureExtractor, Wav2Vec2Model

RECORDINGS_DIR = Path("~/.pancetta/recordings")
WAV_GLOB = "ft8_20260530_*.wav"
NUM_WAVS = 10
MODEL_ID = "facebook/wav2vec2-base"
SOURCE_SR = 12000
TARGET_SR = 16000


def device() -> torch.device:
    if torch.backends.mps.is_available():
        return torch.device("mps")
    if torch.cuda.is_available():
        return torch.device("cuda")
    return torch.device("cpu")


def resample_12k_to_16k(audio: np.ndarray) -> np.ndarray:
    """12000 -> 16000 Hz via polyphase (rational ratio 4/3)."""
    return sps.resample_poly(audio, up=4, down=3).astype(np.float32)


def load_wavs(paths: list[Path]) -> list[tuple[str, np.ndarray]]:
    out: list[tuple[str, np.ndarray]] = []
    for p in paths:
        sr, data = wf.read(p)
        assert sr == SOURCE_SR, f"unexpected sr {sr} in {p}"
        if data.dtype == np.int16:
            audio = data.astype(np.float32) / 32768.0
        else:
            audio = data.astype(np.float32)
        if audio.ndim > 1:
            audio = audio.mean(axis=1)
        # peak-normalize so feature-extractor's instance-norm has consistent scale
        peak = np.max(np.abs(audio)) + 1e-9
        audio = audio / peak
        audio = resample_12k_to_16k(audio)
        out.append((p.name, audio))
    return out


def embed(
    model: Wav2Vec2Model,
    fe: Wav2Vec2FeatureExtractor,
    audio: np.ndarray,
    dev: torch.device,
) -> np.ndarray:
    inputs = fe(audio, sampling_rate=TARGET_SR, return_tensors="pt")
    input_values = inputs.input_values.to(dev)
    with torch.no_grad():
        out = model(input_values, output_hidden_states=False, return_dict=True)
    # last_hidden_state: [1, T, 768]
    hidden = out.last_hidden_state.squeeze(0).cpu().numpy()  # [T, 768]
    return hidden


def main() -> int:
    paths = sorted(RECORDINGS_DIR.glob(WAV_GLOB))[:NUM_WAVS]
    if len(paths) < NUM_WAVS:
        print(f"FAIL: only found {len(paths)} WAVs matching {WAV_GLOB}", file=sys.stderr)
        return 2
    print(f"Loaded {len(paths)} WAVs from {RECORDINGS_DIR}")

    wavs = load_wavs(paths)
    durations_s = [len(a) / TARGET_SR for _, a in wavs]
    print(f"Durations (s): min={min(durations_s):.2f} max={max(durations_s):.2f}")

    dev = device()
    print(f"Device: {dev}")
    print(f"Loading {MODEL_ID} ...")
    t0 = time.time()
    fe = Wav2Vec2FeatureExtractor.from_pretrained(MODEL_ID)
    model = Wav2Vec2Model.from_pretrained(MODEL_ID).to(dev)
    model.eval()
    print(f"Model loaded in {time.time() - t0:.2f}s, hidden_size={model.config.hidden_size}")

    pooled_list: list[np.ndarray] = []
    seq_shapes: list[tuple[int, int]] = []
    embed_ms: list[float] = []
    for name, audio in wavs:
        t1 = time.time()
        hidden = embed(model, fe, audio, dev)
        dt_ms = (time.time() - t1) * 1000.0
        embed_ms.append(dt_ms)
        pooled = hidden.mean(axis=0)
        pooled_list.append(pooled)
        seq_shapes.append(hidden.shape)
        print(
            f"  {name}: seq_shape={hidden.shape} "
            f"pooled_norm={np.linalg.norm(pooled):.3f} "
            f"pooled_mean={pooled.mean():+.4f} "
            f"pooled_std={pooled.std():.4f} "
            f"latency_ms={dt_ms:.1f}"
        )

    pooled_arr = np.stack(pooled_list, axis=0)  # [N, 768]
    print(f"\nPooled stack: shape={pooled_arr.shape}")
    print(f"Per-WAV latency: mean={np.mean(embed_ms):.1f}ms median={np.median(embed_ms):.1f}ms max={np.max(embed_ms):.1f}ms")

    # ----- structure probes -----

    # 1. Pairwise cosine distances: a degenerate encoder collapses everything
    # to near-identical embeddings (cos ≈ 1 for all pairs). A noise encoder
    # would give cos ≈ 0 with high variance. We want intermediate structure.
    norm = pooled_arr / (np.linalg.norm(pooled_arr, axis=1, keepdims=True) + 1e-9)
    cos = norm @ norm.T
    # upper triangle without diagonal
    iu = np.triu_indices(len(norm), k=1)
    cos_vals = cos[iu]
    print(
        f"\nPairwise cosine: mean={cos_vals.mean():.4f} "
        f"std={cos_vals.std():.4f} "
        f"min={cos_vals.min():.4f} max={cos_vals.max():.4f}"
    )

    # 2. PCA on the 10 pooled embeddings: how much variance is in the top
    # components? If the encoder collapses, PC1 explains ~100%. If random,
    # variance is uniform across components.
    n_components = min(5, len(pooled_arr) - 1)
    pca = PCA(n_components=n_components)
    pca.fit(pooled_arr)
    evr = pca.explained_variance_ratio_
    print(f"PCA explained_variance_ratio (top {n_components}): {np.round(evr, 4).tolist()}")
    print(f"  cumulative: {np.round(np.cumsum(evr), 4).tolist()}")

    # 3. Effective rank ≈ exp(entropy of singular value distribution / 2)
    # gives a unitless estimate of how many "directions" the embeddings span.
    # A degenerate encoder has rank ≈ 1; a random encoder has rank ≈ min(N, D).
    s = pca.singular_values_
    p = s / (s.sum() + 1e-9)
    entropy = -np.sum(p * np.log(p + 1e-12))
    eff_rank = float(np.exp(entropy))
    print(f"Effective rank (entropy-based, top {n_components}): {eff_rank:.3f}")

    # 4. Decision: are the embeddings "structured enough" to bother with?
    # Decision rule (calibrated to be conservative):
    # - pairwise cosine mean in [0.3, 0.95] => non-collapsed, non-random
    # - PC1 explains < 0.80 => not dominated by a single direction
    # - PC1 explains > 0.15 => not pure noise (random would be ~1/D for top PCs)
    mean_cos = float(cos_vals.mean())
    pc1 = float(evr[0])
    non_collapsed = 0.3 <= mean_cos <= 0.95
    non_dominated = pc1 < 0.80
    above_noise_floor = pc1 > 0.15
    proceed = non_collapsed and non_dominated and above_noise_floor

    print("\n--- Decision ---")
    print(f"  non-collapsed (cos in [0.3, 0.95]): {non_collapsed} (cos_mean={mean_cos:.3f})")
    print(f"  non-dominated (PC1 < 0.80):         {non_dominated} (PC1={pc1:.3f})")
    print(f"  above-noise (PC1 > 0.15):           {above_noise_floor}")
    print(f"  => Session 2 PROCEED: {proceed}")

    return 0 if proceed else 1


if __name__ == "__main__":
    sys.exit(main())
