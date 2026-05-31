#!/usr/bin/env python3
"""Train the DIA (Decoding Information Aggregation) neural-OSD model on
the Session 1 production-config trajectory dataset (hb-064).

Input: JSONL emitted by `pancetta-research/examples/hb064_generate_trajectory_dataset.rs`.
       Each line:
         {
           "wav": str, "tier": str,
           "channel_llrs": [174 f32],
           "trajectory_flat": [25*174 f32],      # iteration-major
           "final_llrs": [174 f32],
           "osd_recovered": bool,
           "osd_codeword": [174 0/1] or None,    # only on recovered
           "bp_iters_run": int,
           "features": {parity_errors_final: int, ...}
         }

Architecture: matches pancetta-ft8/src/neural_osd.rs EXACTLY:
    Conv1d(25 -> 32, k=3, pad=1) -> ReLU
    Conv1d(32 -> 16, k=3, pad=1) -> ReLU
    Conv1d(16 -> 1,  k=1)
    Linear(174 -> 91) -> Sigmoid

Output: best-val-loss checkpoint at args.output (default session2_best.pt).

Imbalance strategy: focal loss (gamma=2). Rationale: 545 positives vs
~28k negatives = ~51:1. We tried two alternatives in design:
  - Weighted BCE with pos_weight=51: amplifies gradient on positives
    uniformly but doesn't downweight already-easy negatives, which
    dominate the loss.
  - Oversampling 545 positives ~50x: tends to overfit on a small
    pool (only ~436 train positives after split).
  - Focal loss (gamma=2): downweights well-classified samples on BOTH
    classes, focusing learning on hard examples. Standard pick for
    51:1 imbalance with modest minority count.

Gate-restricted recall: separately reported on the
parity_errors_final <= 6 subset (the production parity-gate). This is
THE headline metric for the Session 2 decision: if the model only
beats trivial accuracy by tracking parity_errors_final, it carries no
signal beyond the existing gate.

Per-info-bit error label:
    Y[b] = osd_codeword[b] XOR (final_llrs[b] < 0)    for b in 0..91
Only OSD-recovered samples carry a usable Y. Failed samples are
SKIPPED for training (no ground truth). At inference time the model
sees every BP-failure trajectory, so a separate "should we bother
running OSD at all" signal would come from a classifier-style head;
that's out of scope here — Session 2 retrains the per-bit error
predictor.
"""
import argparse
import json
import os
import sys
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.optim as optim
from torch.utils.data import DataLoader, TensorDataset


# Match pancetta-ft8/src/neural_osd.rs constants exactly.
N_CODEWORD = 174
K_INFO = 91
BP_ITERS = 25


class DIAModel(nn.Module):
    """Decoding Information Aggregation model — matches train.py/neural_osd.rs."""

    def __init__(self):
        super().__init__()
        self.conv1 = nn.Conv1d(BP_ITERS, 32, kernel_size=3, padding=1)
        self.conv2 = nn.Conv1d(32, 16, kernel_size=3, padding=1)
        self.conv3 = nn.Conv1d(16, 1, kernel_size=1)
        self.linear = nn.Linear(N_CODEWORD, K_INFO)
        self.relu = nn.ReLU()

    def forward(self, x):
        # x: (batch, 25, 174)
        h = self.relu(self.conv1(x))   # (batch, 32, 174)
        h = self.relu(self.conv2(h))   # (batch, 16, 174)
        h = self.conv3(h).squeeze(1)   # (batch, 174)
        h = torch.sigmoid(self.linear(h))  # (batch, 91)
        return h


def focal_loss(pred, target, gamma=2.0, eps=1e-6):
    """Focal loss for binary cross-entropy.

    L = - [ (1 - p_t)^gamma * log(p_t) ]
    where p_t = pred if target==1 else 1-pred.

    Per-element, then mean-reduced.
    """
    pred = pred.clamp(eps, 1.0 - eps)
    p_t = pred * target + (1 - pred) * (1 - target)
    loss = -((1 - p_t) ** gamma) * torch.log(p_t)
    return loss.mean()


def load_jsonl(path, max_samples=None):
    """Load the Session 1 JSONL, returning recovered-only samples with
    per-info-bit error labels.

    Returns:
        X: float32 (N, 25, 174) trajectory tensor
        Y: float32 (N, 91)      per-info-bit error label
        parity_final: int32 (N,) parity_errors_final per sample
    """
    X_list, Y_list, parity_list = [], [], []
    n_recovered = 0
    n_failed = 0
    n_codeword_missing = 0
    started = time.time()
    with open(path, "r") as f:
        for i, line in enumerate(f):
            if max_samples and i >= max_samples:
                break
            d = json.loads(line)
            if not d.get("osd_recovered"):
                n_failed += 1
                continue
            n_recovered += 1
            codeword = d.get("osd_codeword")
            if codeword is None or len(codeword) != N_CODEWORD:
                n_codeword_missing += 1
                continue

            traj = np.asarray(d["trajectory_flat"], dtype=np.float32).reshape(BP_ITERS, N_CODEWORD)
            final_llrs = np.asarray(d["final_llrs"], dtype=np.float32)
            osd_cw = np.asarray(codeword, dtype=np.int8)

            # Per-info-bit error label:
            #   hard_decision_bit = (final_llrs < 0)  (per common LLR convention:
            #     positive LLR -> bit 0, negative LLR -> bit 1)
            #   Y[b] = (osd_codeword[b] != hard_decision_bit[b])
            hard_dec = (final_llrs < 0).astype(np.int8)
            err_full = (osd_cw != hard_dec).astype(np.float32)
            Y = err_full[:K_INFO]  # first 91 are info bits

            X_list.append(traj)
            Y_list.append(Y)
            parity_list.append(int(d["features"].get("parity_errors_final", -1)))

            if (n_recovered % 100) == 0:
                pass  # quiet; was for debug

    X = np.stack(X_list) if X_list else np.zeros((0, BP_ITERS, N_CODEWORD), dtype=np.float32)
    Y = np.stack(Y_list) if Y_list else np.zeros((0, K_INFO), dtype=np.float32)
    parity_final = np.asarray(parity_list, dtype=np.int32)

    dt = time.time() - started
    print(f"loaded {n_recovered:,} recovered + {n_failed:,} failed = "
          f"{n_recovered + n_failed:,} total in {dt:.1f}s "
          f"(codeword missing: {n_codeword_missing})")
    print(f"X shape: {X.shape}, Y shape: {Y.shape}, "
          f"Y positive rate (per-bit): {Y.mean():.4f}")
    return X, Y, parity_final


def split_dataset(X, Y, parity_final, seed=42):
    """Deterministic 80/10/10 train/val/test split."""
    n = len(X)
    rng = np.random.default_rng(seed)
    idx = rng.permutation(n)
    n_train = int(0.8 * n)
    n_val = int(0.1 * n)
    train_idx = idx[:n_train]
    val_idx = idx[n_train:n_train + n_val]
    test_idx = idx[n_train + n_val:]
    splits = {}
    for name, sel in [("train", train_idx), ("val", val_idx), ("test", test_idx)]:
        splits[name] = (X[sel], Y[sel], parity_final[sel])
    return splits


def per_sample_metrics(pred, Y, parity_final, threshold=0.5):
    """Compute sample-level recovery metrics.

    Strategy: this model predicts a *per-info-bit* error probability. The
    paper-style application is to ORDER bits by descending p — OSD then
    enumerates flip patterns over the top-k. For Session 2 gating we want a
    sample-level signal: "did the model correctly identify the sample's error
    pattern?" Operationally we use the top-T predicted bits (T = ceil(actual
    #errors)) and compute precision of that top-T pick. A sample is
    counted as "recovered correctly" if precision-at-T >= 0.5 — i.e. the
    model's top-T captures at least half the true errors. This is a coarse
    rolllup; the eventual production-time test is the A/B run.

    We also compute a per-bit accuracy at threshold for reference.
    """
    pred = pred.detach().cpu().numpy()  # (N, 91)
    Y_np = Y.detach().cpu().numpy()      # (N, 91)
    parity_np = parity_final.cpu().numpy() if torch.is_tensor(parity_final) else parity_final

    pred_binary = (pred > threshold).astype(np.float32)
    tp = ((pred_binary == 1) & (Y_np == 1)).sum()
    fp = ((pred_binary == 1) & (Y_np == 0)).sum()
    fn = ((pred_binary == 0) & (Y_np == 1)).sum()
    tn = ((pred_binary == 0) & (Y_np == 0)).sum()

    precision = tp / max(tp + fp, 1)
    recall = tp / max(tp + fn, 1)
    f1 = (2 * precision * recall) / max(precision + recall, 1e-9)
    bit_acc = (tp + tn) / max(tp + fp + fn + tn, 1)

    # Sample-level: per-sample top-T precision >= 0.5
    n = len(pred)
    sample_recovered = np.zeros(n, dtype=bool)
    n_true_errors = Y_np.sum(axis=1).astype(int)
    for i in range(n):
        T = max(n_true_errors[i], 1)
        # take indices of top-T predicted-error bits
        top_idx = np.argpartition(pred[i], -T)[-T:]
        true_set = set(np.where(Y_np[i] == 1)[0].tolist())
        correct = sum(1 for j in top_idx if j in true_set)
        if T > 0 and correct / T >= 0.5:
            sample_recovered[i] = True

    sample_recovery_rate = float(sample_recovered.mean()) if n else 0.0

    # Gate-restricted: parity_errors_final <= 6 (production gate)
    gate_mask = parity_np <= 6
    gate_n = int(gate_mask.sum())
    gate_recall = float(sample_recovered[gate_mask].mean()) if gate_n else 0.0

    return {
        "bit_accuracy": float(bit_acc),
        "bit_precision": float(precision),
        "bit_recall": float(recall),
        "bit_f1": float(f1),
        "sample_recovery_rate": sample_recovery_rate,
        "n_samples": n,
        "gate_n": gate_n,
        "gate_restricted_recall": gate_recall,
    }


def train(args):
    print("=" * 70)
    print("hb-064 Session 2 — neural OSD retraining on production-config")
    print("  trajectories (Session 1 JSONL).")
    print("=" * 70)

    print(f"Loading JSONL: {args.data}")
    X, Y, parity_final = load_jsonl(args.data, max_samples=args.max_samples)
    n_total = len(X)
    if n_total == 0:
        print("ERROR: zero recovered samples — bailing.", file=sys.stderr)
        sys.exit(1)

    splits = split_dataset(X, Y, parity_final, seed=args.seed)
    for name, (Xs, Ys, ps) in splits.items():
        gate_n = int((ps <= 6).sum())
        print(f"  {name:5}: N={len(Xs):5d}  parity<=6 N={gate_n}")

    train_X, train_Y, train_p = splits["train"]
    val_X, val_Y, val_p = splits["val"]
    test_X, test_Y, test_p = splits["test"]

    # Device pick
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

    train_ds = TensorDataset(torch.from_numpy(train_X), torch.from_numpy(train_Y))
    val_ds = TensorDataset(torch.from_numpy(val_X), torch.from_numpy(val_Y))

    train_loader = DataLoader(train_ds, batch_size=args.batch_size, shuffle=True)
    val_loader = DataLoader(val_ds, batch_size=args.batch_size, shuffle=False)

    model = DIAModel().to(device)
    print(f"model params: {sum(p.numel() for p in model.parameters()):,}")

    optimizer = optim.Adam(model.parameters(), lr=args.lr, weight_decay=1e-4)
    scheduler = optim.lr_scheduler.CosineAnnealingLR(optimizer, T_max=args.epochs)

    best_val_loss = float("inf")
    best_epoch = -1
    patience_counter = 0

    val_p_tensor = torch.from_numpy(val_p)

    for epoch in range(args.epochs):
        model.train()
        train_loss = 0.0
        n_train_samples = 0
        for X_batch, Y_batch in train_loader:
            X_batch = X_batch.to(device)
            Y_batch = Y_batch.to(device)
            optimizer.zero_grad()
            pred = model(X_batch)
            loss = focal_loss(pred, Y_batch, gamma=args.focal_gamma)
            loss.backward()
            optimizer.step()
            train_loss += loss.item() * X_batch.size(0)
            n_train_samples += X_batch.size(0)
        train_loss /= max(n_train_samples, 1)

        model.eval()
        val_loss = 0.0
        n_val_samples = 0
        val_preds = []
        val_targets = []
        with torch.no_grad():
            for X_batch, Y_batch in val_loader:
                X_batch = X_batch.to(device)
                Y_batch = Y_batch.to(device)
                pred = model(X_batch)
                loss = focal_loss(pred, Y_batch, gamma=args.focal_gamma)
                val_loss += loss.item() * X_batch.size(0)
                n_val_samples += X_batch.size(0)
                val_preds.append(pred.cpu())
                val_targets.append(Y_batch.cpu())
        val_loss /= max(n_val_samples, 1)
        scheduler.step()

        val_pred_t = torch.cat(val_preds, dim=0)
        val_target_t = torch.cat(val_targets, dim=0)
        m = per_sample_metrics(val_pred_t, val_target_t, val_p_tensor)

        improved = val_loss < best_val_loss
        marker = " *" if improved else ""
        print(f"epoch {epoch+1:3d}/{args.epochs}: train={train_loss:.5f} "
              f"val={val_loss:.5f} sample_rec={m['sample_recovery_rate']:.3f} "
              f"gate_rec={m['gate_restricted_recall']:.3f} "
              f"(N_gate={m['gate_n']}) lr={optimizer.param_groups[0]['lr']:.1e}{marker}",
              flush=True)

        if improved:
            best_val_loss = val_loss
            best_epoch = epoch + 1
            torch.save(model.state_dict(), args.output)
            patience_counter = 0
        else:
            patience_counter += 1
            if patience_counter >= args.patience:
                print(f"early-stop at epoch {epoch+1} (no improve for "
                      f"{args.patience} epochs)")
                break

    # Final test eval using best checkpoint
    model.load_state_dict(torch.load(args.output, map_location=device, weights_only=True))
    model.eval()

    test_ds = TensorDataset(torch.from_numpy(test_X), torch.from_numpy(test_Y))
    test_loader = DataLoader(test_ds, batch_size=args.batch_size, shuffle=False)
    test_preds, test_targets = [], []
    with torch.no_grad():
        for X_batch, Y_batch in test_loader:
            pred = model(X_batch.to(device))
            test_preds.append(pred.cpu())
            test_targets.append(Y_batch)
    test_pred_t = torch.cat(test_preds, dim=0)
    test_target_t = torch.cat(test_targets, dim=0)
    metrics = per_sample_metrics(test_pred_t, test_target_t, torch.from_numpy(test_p))

    print("\n" + "=" * 70)
    print(f"Best val loss: {best_val_loss:.5f} at epoch {best_epoch}")
    print(f"Test metrics (best checkpoint):")
    for k, v in metrics.items():
        if isinstance(v, float):
            print(f"  {k}: {v:.4f}")
        else:
            print(f"  {k}: {v}")
    print("=" * 70)

    # Save metrics for journal
    metrics_path = Path(args.output).with_suffix(".metrics.json")
    metrics["best_val_loss"] = float(best_val_loss)
    metrics["best_epoch"] = int(best_epoch)
    metrics["n_recovered_total"] = int(n_total)
    metrics["train_size"] = int(len(train_X))
    metrics["val_size"] = int(len(val_X))
    metrics["test_size"] = int(len(test_X))
    metrics["focal_gamma"] = float(args.focal_gamma)
    metrics["seed"] = int(args.seed)
    with open(metrics_path, "w") as f:
        json.dump(metrics, f, indent=2)
    print(f"metrics saved to {metrics_path}")

    # Decision gate
    gate_rec = metrics["gate_restricted_recall"]
    if gate_rec >= 0.55:
        print(f"\n*** PROCEED: gate-restricted recall {gate_rec:.3f} >= 0.55 ***")
        return 0
    else:
        print(f"\n*** SHELVE: gate-restricted recall {gate_rec:.3f} < 0.55 ***")
        return 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--data",
        type=str,
        default="../../.claude/worktrees/agent-a3b4a0fbd479ad9c9/research/experiments/2026-05-31-hb-064-dia-osd-session1/trajectories.jsonl",
        help="Path to Session 1 trajectories JSONL",
    )
    parser.add_argument("--output", type=str, default="session2_best.pt")
    parser.add_argument("--epochs", type=int, default=60)
    parser.add_argument("--batch-size", type=int, default=64)
    parser.add_argument("--lr", type=float, default=1e-3)
    parser.add_argument("--patience", type=int, default=15)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--focal-gamma", type=float, default=2.0)
    parser.add_argument("--device", type=str, default="auto",
                        choices=["auto", "cpu", "mps", "cuda"])
    parser.add_argument("--max-samples", type=int, default=None,
                        help="Cap JSONL line scan (for debugging)")
    args = parser.parse_args()
    sys.exit(train(args))
