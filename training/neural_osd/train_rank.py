#!/usr/bin/env python3
"""Train neural OSD for expected MRB reprocessing order on schema-v2 captures."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import shutil
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
        # A minibatch that's entirely zero-label rows (parity-only
        # recoveries, retained for BCE calibration — see load_corpus) has
        # no positive to rank against, but that's a legitimate corpus
        # composition, not a caller error: raising would abort the whole
        # training run over one unlucky shuffle. `combined_loss`'s BCE
        # term can still optimize these rows; contribute a differentiable
        # zero here so it does, instead of failing the batch outright.
        return scores.sum() * 0.0
    return ((soft_ranks * labels).sum(dim=1)[valid] / positives[valid]).mean()


def validation_metrics(model, val_x, val_y, tau, bce_weight, batch_size=256, device=None):
    """Batched validation metrics, equivalent to a single full-split call
    but bounded to O(batch_size) peak activation memory.

    At T1 scale (~100k validation samples) a single forward pass over the
    whole split materializes roughly 1.8 GB of input alone plus several GB
    of conv1 activations, which can OOM checkpoint selection even though
    training itself is batched. Aggregates the same per-sample quantities
    `soft_rank_loss_tensor`/BCE average — `(soft_ranks * labels).sum(dim=1)
    / positives` restricted to samples with a positive label, and
    per-element BCE over every score — as running sums/counts across
    batches instead of a mean-of-batch-means, so both results are
    numerically equivalent to a single full-split call regardless of batch
    size or how samples are distributed across batches.

    Returns `(rank_metric, combined_metric)`: `rank_metric` alone is
    shift-invariant (adding the same constant to every score in a sample
    leaves it unchanged) and is the interpretable "expected reprocessing
    order" figure. `combined_metric` folds in the BCE term at the same
    weight `combined_loss` trains with, so checkpoint selection is
    sensitive to the absolute score scale production actually reads
    (`reliability_sorted_indices` compares scores directly against
    parity-bit |LLR|) instead of only the rank-preserving component.
    """
    import torch
    from torch.utils.data import DataLoader, TensorDataset

    loader = DataLoader(TensorDataset(torch.from_numpy(val_x), torch.from_numpy(val_y)), batch_size=batch_size)
    rank_total = 0.0
    rank_count = 0
    bce_total = 0.0
    bce_count = 0
    for batch_x, batch_y in loader:
        if device is not None:
            batch_x = batch_x.to(device)
            batch_y = batch_y.to(device)
        scores = model(batch_x)
        pairwise = (scores.unsqueeze(1) - scores.unsqueeze(2)) / tau
        soft_ranks = torch.sigmoid(pairwise).sum(dim=2)
        positives = batch_y.sum(dim=1)
        valid = positives > 0
        if valid.any():
            per_sample = (soft_ranks * batch_y).sum(dim=1)[valid] / positives[valid]
            rank_total += per_sample.sum().item()
            rank_count += int(valid.sum().item())
        bce = torch.nn.functional.binary_cross_entropy_with_logits(scores, batch_y, reduction="sum")
        bce_total += bce.item()
        bce_count += batch_y.numel()
    if rank_count == 0:
        raise ValueError("soft-rank validation split has no positive labels")
    rank_metric = rank_total / rank_count
    bce_metric = bce_total / bce_count
    return rank_metric, rank_metric + bce_weight * bce_metric


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
            #
            # An all-zero label vector (recovery confined to parity
            # positions — no info-bit error) is still a valid, useful
            # sample: the rank loss can't use it (no positive to rank), but
            # BCE can — it's exactly the kind of "confidently no error"
            # example the calibration term needs to learn the unconditional
            # absolute error probability production compares directly
            # against parity-bit |LLR|. soft_rank_loss_tensor's own `valid
            # = positives > 0` mask already excludes zero-label samples
            # from the rank component per-batch, so retaining them here
            # only feeds the BCE term, not the rank term.
            labels = errors[:K_INFO]
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

    Returns `(arrays, cache_dir)` — the caller owns `cache_dir`'s lifetime
    (roughly 18.5 GB on disk at 1M samples) and is responsible for removing
    it once training is done with the arrays; a few reruns left behind
    would otherwise exhaust the temp filesystem.
    """
    import numpy as np

    counts = {"train": 0, "val": 0, "test": 0}
    for split, _model_input, _labels in _iter_samples(path):
        counts[split] += 1

    # A fresh temp dir per call avoids collisions between concurrent training
    # runs. Once created, its cleanup is main()'s job (see load_corpus's own
    # docstring) — but ONLY once this function has actually returned it. Any
    # failure below (most plausibly memmap allocation exhausting temp
    # storage at T1 scale) must clean up here instead, or a partially
    # populated multi-gigabyte cache_dir is left behind with nothing left
    # holding a reference to remove it.
    cache_dir = Path(tempfile.mkdtemp(prefix="pan9_rank_corpus_"))
    # Tracked independently of `arrays` below: if allocating a split's `y`
    # memmap fails right after its `x` succeeded, `x` never makes it into
    # `arrays` (the pair is only inserted once both exist) — the except
    # handler needs its own reference to close that orphaned mapping too,
    # or an open mapping blocks rmtree from deleting its backing file on
    # Windows while ignore_errors=True hides the failure.
    created_memmaps = []
    try:
        feature_dim = BP_ITERS + 1
        arrays = {}
        for name, count in counts.items():
            if count:
                x = np.memmap(cache_dir / f"{name}_x.dat", dtype=np.float32, mode="w+", shape=(count, feature_dim, N_CODEWORD))
                created_memmaps.append(x)
                y = np.memmap(cache_dir / f"{name}_y.dat", dtype=np.float32, mode="w+", shape=(count, K_INFO))
                created_memmaps.append(y)
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
    except BaseException:
        for mapped in created_memmaps:
            if hasattr(mapped, "_mmap"):
                mapped._mmap.close()
        shutil.rmtree(cache_dir, ignore_errors=True)
        raise

    return arrays, cache_dir


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("--output", type=Path, default=Path("rank_model.pt"))
    parser.add_argument("--epochs", type=int, default=60)
    parser.add_argument("--tau", type=float, default=0.1)
    parser.add_argument("--bce-weight", type=float, default=0.1)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--device", type=str, default="auto", choices=["auto", "cpu", "mps", "cuda"])
    args = parser.parse_args()
    if not math.isfinite(args.tau) or args.tau <= 0.0:
        # tau=0 divides by zero in the pairwise term (NaN loss, val never
        # improves, a reused --output silently keeps a stale checkpoint);
        # tau<0 reverses the ranking objective and trains errors the wrong way.
        raise SystemExit(f"--tau must be finite and positive, got {args.tau}")
    if not math.isfinite(args.bce_weight) or args.bce_weight < 0.0:
        raise SystemExit(f"--bce-weight must be finite and non-negative, got {args.bce_weight}")
    if args.epochs <= 0:
        # epochs<=0 skips the training loop entirely and exits 0 without
        # writing a checkpoint — if --output already exists from a prior
        # run, it's left untouched and a later export step can silently
        # package that stale model as the new candidate.
        raise SystemExit(f"--epochs must be positive, got {args.epochs}")
    import numpy as np
    import torch
    import torch.nn as nn
    from torch.utils.data import DataLoader, TensorDataset

    # Seed before RankModel's weight init and the shuffled DataLoader
    # construction below — identical corpus + CLI inputs must produce
    # identical initial weights, minibatch order, checkpoints, and exported
    # blob hashes, or a PAN-9 result can't be reproduced and algorithm
    # changes get confounded with seed variance.
    torch.manual_seed(args.seed)
    np.random.seed(args.seed)

    if args.device == "auto":
        if torch.backends.mps.is_available():
            device = torch.device("mps")
        elif torch.cuda.is_available():
            device = torch.device("cuda")
        else:
            device = torch.device("cpu")
    else:
        device = torch.device(args.device)
    print(f"device: {device}")

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

    splits, cache_dir = load_corpus(args.corpus)
    try:
        train_x, train_y = splits["train"]
        val_x, val_y = splits["val"]
        if not len(train_x) or not len(val_x):
            raise SystemExit("schema-v2 corpus produced an empty train or validation split")
        model = RankModel().to(device)
        optimizer = torch.optim.Adam(model.parameters(), lr=1e-3, weight_decay=1e-4)
        loader = DataLoader(TensorDataset(torch.from_numpy(train_x), torch.from_numpy(train_y)), batch_size=64, shuffle=True)
        best = float("inf")
        for epoch in range(args.epochs):
            model.train()
            for batch_x, batch_y in loader:
                batch_x = batch_x.to(device)
                batch_y = batch_y.to(device)
                optimizer.zero_grad()
                loss = combined_loss(model(batch_x), batch_y, args.tau, args.bce_weight)
                loss.backward()
                optimizer.step()
            model.eval()
            with torch.no_grad():
                rank_metric, selection_metric = validation_metrics(model, val_x, val_y, args.tau, args.bce_weight, device=device)
            print(f"epoch={epoch + 1} tier1_expected_soft_rank={rank_metric:.6f} selection_metric={selection_metric:.6f} [MODEL SELECTION ONLY — NOT A SHIP SIGNAL]")
            if selection_metric < best:
                best = selection_metric
                # Persist the seed alongside the weights, not just applied
                # to the in-process RNGs — once the command transcript that
                # launched this run is gone, a bare state_dict can't be
                # traced back to (or reproduced with) the RNG seed that
                # produced it. export_weights.py propagates this into the
                # exported provenance.json.
                torch.save({"state_dict": model.state_dict(), "seed": args.seed}, args.output)
    finally:
        # Close every memmap's underlying OS mapping before removing its
        # backing files. On Windows, an open memory-mapped file cannot be
        # unlinked at all (rmtree would silently leave the ~18.5 GB cache
        # behind via ignore_errors); closing first makes cleanup work
        # regardless of platform. Safe here specifically because nothing
        # reads train_x/val_x/loader again after this point, whether the
        # epoch loop ran to completion or the `try` block raised.
        for x, y in splits.values():
            if hasattr(x, "_mmap"):
                x._mmap.close()
            if hasattr(y, "_mmap"):
                y._mmap.close()
        # The memmap corpus cache is a training-run-scoped intermediate
        # (~18.5 GB on disk at 1M samples), not an artifact worth keeping —
        # `args.output`'s checkpoint is the actual deliverable and lives
        # outside cache_dir. Remove it regardless of how training exited so
        # a few reruns or a failed run can't exhaust the temp filesystem.
        shutil.rmtree(cache_dir, ignore_errors=True)


if __name__ == "__main__":
    main()
