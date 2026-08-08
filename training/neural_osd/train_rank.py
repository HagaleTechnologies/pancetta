#!/usr/bin/env python3
"""Train neural OSD for expected MRB reprocessing order on schema-v2 captures."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
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
    for sample_scores, sample_labels in zip(scores, labels, strict=True):
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


def load_contract():
    path = Path(__file__).resolve().parents[2] / "pancetta-ft8/assets/neural_osd_weights.provenance.json"
    return json.loads(path.read_text())


def load_corpus(path):
    import numpy as np

    contract = load_contract()
    divisor = float(contract["syndrome_normalization_divisor"])
    groups = {"train": ([], []), "val": ([], []), "test": ([], [])}
    for line in Path(path).read_text().splitlines():
        row = json.loads(line)
        if row.get("schema_version") != 2 or not row.get("osd_recovered"):
            continue
        perm = row.get("mrb_perm")
        codeword = row.get("osd_codeword")
        if not perm or not codeword:
            continue
        hard = [int(value < 0.0) for value in row["final_llrs"]]
        errors = [float(a != b) for a, b in zip(codeword, hard, strict=True)]
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
        groups[split][0].append(model_input)
        groups[split][1].append(labels)
    return {
        name: (np.asarray(values[0], dtype=np.float32), np.asarray(values[1], dtype=np.float32))
        for name, values in groups.items()
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("--output", type=Path, default=Path("rank_model.pt"))
    parser.add_argument("--epochs", type=int, default=60)
    parser.add_argument("--tau", type=float, default=0.1)
    args = parser.parse_args()
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
            loss = soft_rank_loss_tensor(model(batch_x), batch_y, args.tau)
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
