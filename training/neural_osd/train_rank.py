#!/usr/bin/env python3
"""Train neural OSD for expected MRB reprocessing order on schema-v2 captures."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import tempfile
from pathlib import Path

N_CODEWORD = 174
K_INFO = 91
BP_ITERS = 25


def invert_permutation(perm):
    inverse = [0] * len(perm)
    for new_position, original_position in enumerate(perm):
        inverse[original_position] = new_position
    return inverse


def apply_mrb_permutation(values, perm):
    return [values[original_position] for original_position in perm]


def soft_rank_loss(scores, labels, tau=0.1):
    """Dependency-free reference for the design's all-j soft-rank formula."""
    losses = []
    if len(scores) != len(labels):
        raise ValueError("scores and labels must contain the same number of samples")
    for sample_scores, sample_labels in zip(scores, labels):
        positives = sum(sample_labels)
        if positives <= 0:
            continue
        ranks = []
        for score_i in sample_scores:
            rank = 0.0
            for score_j in sample_scores:
                z = max(-60.0, min(60.0, (score_j - score_i) / tau))
                rank += 1.0 / (1.0 + math.exp(-z))
            ranks.append(rank)
        losses.append(sum(y * rank for y, rank in zip(sample_labels, ranks)) / positives)
    if not losses:
        raise ValueError("soft-rank loss requires at least one positive label")
    return sum(losses) / len(losses)


def soft_rank_loss_tensor(scores, labels, tau=0.1):
    """Differentiable PyTorch implementation used for optimization."""
    import torch

    pairwise = (scores.unsqueeze(1) - scores.unsqueeze(2)) / tau
    soft_ranks = torch.sigmoid(pairwise).sum(dim=2)
    positives = labels.sum(dim=1)
    valid = positives > 0
    if not valid.any():
        raise ValueError("soft-rank minibatch has no positive labels")
    return ((soft_ranks * labels).sum(dim=1)[valid] / positives[valid]).mean()


def combined_loss(scores, labels, tau=0.1, bce_weight=0.1):
    """Soft-rank loss plus a small BCE term to anchor the absolute score scale.

    Soft-rank depends only on pairwise score differences within a sample, so
    adding the same constant to every logit in a sample leaves it (and
    checkpoint selection, which uses it) completely unchanged. Production
    inference doesn't have that invariance: `reliability_sorted_indices`
    converts each score to `ln((1-p)/p)` and compares it directly against
    parity-bit |LLR| magnitudes, so a common offset shifts every information
    bit relative to every parity bit and can change the MRB/OSD enumeration.
    The BCE term is a low-weight regularizer (design doc: lambda ~= 0.1) that
    trains the scores to also be calibrated probabilities on the absolute
    scale production reads them on, without dominating the ranking objective.
    """
    import torch

    rank = soft_rank_loss_tensor(scores, labels, tau)
    bce = torch.nn.functional.binary_cross_entropy_with_logits(scores, labels)
    return rank + bce_weight * bce


def load_contract():
    path = Path(__file__).resolve().parents[2] / "pancetta-ft8/assets/neural_osd_weights.provenance.json"
    return json.loads(path.read_text())


def _iter_samples(path):
    """Stream qualifying (split, model_input, labels) triples from the JSONL corpus.

    Each row's transient JSON/array construction is O(1) per row and discarded
    immediately after yielding, so a caller that only counts (rather than
    retaining) keeps this generator's memory bound at O(1) regardless of
    corpus size — that's what lets `load_corpus` below do a cheap counting
    pass before allocating fixed-size backing storage.
    """
    import numpy as np

    contract = load_contract()
    divisor = float(contract["syndrome_normalization_divisor"])
    with open(path, "r", encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, start=1):
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            # A non-v2 row is a corpus-construction error, not something to skip:
            # silently dropping schema-v1 captures trains on an unnoticed subset
            # and reports plausible but misleading metrics.
            schema_version = row.get("schema_version")
            if schema_version != 2:
                raise ValueError(
                    f"{path}:{line_number}: unsupported schema_version "
                    f"{schema_version!r} (expected 2); regenerate the corpus "
                    "with the schema-v2 generator"
                )
            if not row.get("osd_recovered"):
                continue
            perm = row.get("mrb_perm")
            codeword = row.get("osd_codeword")
            if not perm or not codeword:
                continue
            hard = [int(value < 0.0) for value in row["final_llrs"]]
            if len(codeword) != len(hard):
                raise ValueError("osd_codeword and final_llrs lengths differ")
            errors = [float(a != b) for a, b in zip(codeword, hard)]
            if sorted(perm) != list(range(N_CODEWORD)):
                raise ValueError("mrb_perm is not a permutation of 0..173")
            # Rust consumes output slot i as natural systematic bit i.
            labels = errors[:K_INFO]
            if not any(labels):
                continue
            trajectory = np.asarray(row["trajectory_flat"], dtype=np.float32).reshape(BP_ITERS, N_CODEWORD)
            syndrome = np.asarray(row["syndrome_counts"], dtype=np.float32) / divisor
            model_input = np.concatenate([trajectory, syndrome[None, :]], axis=0)
            split_key = row["split_key"]
            bucket = int(hashlib.sha256(split_key.encode()).hexdigest()[:8], 16) % 10
            split = "train" if bucket < 8 else "val" if bucket == 8 else "test"
            yield split, model_input, np.asarray(labels, dtype=np.float32)


def load_corpus(path):
    """Two-pass load into memory-mapped backing arrays.

    A T1 corpus is 100k-1M qualifying samples; retaining every decoded
    26x174 sample in a Python list before a final `np.asarray` copy costs
    several times the corpus size in peak RSS (~18 GB at 1M samples) and can
    prevent the run from starting. Pass 1 counts samples per split without
    retaining them; pass 2 writes directly into pre-sized `np.memmap` arrays,
    so peak RSS stays O(1) regardless of corpus size and training reads pull
    pages from disk on demand instead of holding the whole split in RAM.
    """
    import numpy as np

    counts = {"train": 0, "val": 0, "test": 0}
    for split, _model_input, _labels in _iter_samples(path):
        counts[split] += 1

    # A fresh temp dir per call avoids collisions between concurrent training
    # runs; it's not cleaned up automatically (a training run may want to
    # inspect it after the fact), so it accumulates under the OS temp dir
    # like other run-scoped artifacts elsewhere in this tree.
    cache_dir = Path(tempfile.mkdtemp(prefix="pan9_rank_corpus_"))
    feature_dim = BP_ITERS + 1
    arrays = {}
    for name, count in counts.items():
        if count:
            x = np.memmap(cache_dir / f"{name}_x.dat", dtype=np.float32, mode="w+", shape=(count, feature_dim, N_CODEWORD))
            y = np.memmap(cache_dir / f"{name}_y.dat", dtype=np.float32, mode="w+", shape=(count, K_INFO))
        else:
            x = np.zeros((0, feature_dim, N_CODEWORD), dtype=np.float32)
            y = np.zeros((0, K_INFO), dtype=np.float32)
        arrays[name] = (x, y)

    cursors = {"train": 0, "val": 0, "test": 0}
    for split, model_input, labels in _iter_samples(path):
        idx = cursors[split]
        x, y = arrays[split]
        x[idx] = model_input
        y[idx] = labels
        cursors[split] += 1

    for x, y in arrays.values():
        if isinstance(x, np.memmap):
            x.flush()
        if isinstance(y, np.memmap):
            y.flush()

    return arrays


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("--output", type=Path, default=Path("rank_model.pt"))
    parser.add_argument("--epochs", type=int, default=60)
    parser.add_argument("--tau", type=float, default=0.1)
    parser.add_argument("--bce-weight", type=float, default=0.1)
    args = parser.parse_args()
    if not math.isfinite(args.tau) or args.tau <= 0.0:
        # tau=0 divides by zero in the pairwise term (NaN loss, val never
        # improves, a reused --output silently keeps a stale checkpoint);
        # tau<0 reverses the ranking objective and trains errors the wrong way.
        raise SystemExit(f"--tau must be finite and positive, got {args.tau}")
    if not math.isfinite(args.bce_weight) or args.bce_weight < 0.0:
        raise SystemExit(f"--bce-weight must be finite and non-negative, got {args.bce_weight}")
    import torch
    import torch.nn as nn
    from torch.utils.data import DataLoader, TensorDataset

    class RankModel(nn.Module):
        def __init__(self):
            super().__init__()
            self.conv1 = nn.Conv1d(26, 32, 3, padding=1)
            self.conv2 = nn.Conv1d(32, 16, 3, padding=1)
            self.conv3 = nn.Conv1d(16, 1, 1)
            self.linear = nn.Linear(N_CODEWORD, K_INFO)

        def forward(self, x):
            x = torch.relu(self.conv1(x))
            x = torch.relu(self.conv2(x))
            return self.linear(self.conv3(x).squeeze(1))

    splits = load_corpus(args.corpus)
    train_x, train_y = splits["train"]
    val_x, val_y = splits["val"]
    if not len(train_x) or not len(val_x):
        raise SystemExit("schema-v2 corpus produced an empty train or validation split")
    model = RankModel()
    optimizer = torch.optim.Adam(model.parameters(), lr=1e-3, weight_decay=1e-4)
    loader = DataLoader(TensorDataset(torch.from_numpy(train_x), torch.from_numpy(train_y)), batch_size=64, shuffle=True)
    best = float("inf")
    for epoch in range(args.epochs):
        model.train()
        for batch_x, batch_y in loader:
            optimizer.zero_grad()
            loss = combined_loss(model(batch_x), batch_y, args.tau, args.bce_weight)
            loss.backward()
            optimizer.step()
        model.eval()
        with torch.no_grad():
            metric = soft_rank_loss_tensor(model(torch.from_numpy(val_x)), torch.from_numpy(val_y), args.tau).item()
        print(f"epoch={epoch + 1} tier1_expected_soft_rank={metric:.6f} [MODEL SELECTION ONLY — NOT A SHIP SIGNAL]")
        if metric < best:
            best = metric
            torch.save(model.state_dict(), args.output)


if __name__ == "__main__":
    main()
