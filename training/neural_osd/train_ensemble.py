#!/usr/bin/env python3
"""hb-194 — Bayesian deep ensembles over the existing 20K neural-OSD CNN.

Trains N independent copies of the DIA model (same architecture as
`pancetta-ft8/src/neural_osd.rs` and `train_session2.py`) with:
  - different RNG seeds (default {42..49})
  - per-seed bootstrap-sampled training subsets (replace=True over the
    deterministic 80/10/10 split's train fold, so val/test are held out
    identically across all N for a fair ensemble eval)

For each seed we save its best-val-loss checkpoint as
`ensemble_seed_NN.pt`. A companion `train_ensemble.log` captures the
per-epoch progress per seed.

Defaults match `train_session2.py` so the single-seed=42 run reproduces
Session 2's baseline.

Outputs:
  ensemble_seed_42.pt ... ensemble_seed_49.pt
  ensemble_seed_42.metrics.json ... ensemble_seed_49.metrics.json
  (held-out test split is identical across seeds — fixed seed=42 split)
"""
import argparse
import copy
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

# Reuse the data-loading and metric helpers from train_session2 to keep
# the ensemble strictly comparable to Session 2's single-model baseline.
sys.path.insert(0, str(Path(__file__).parent))
from train_session2 import (  # noqa: E402
    BP_ITERS,
    DIAModel,
    K_INFO,
    N_CODEWORD,
    focal_loss,
    load_jsonl,
    per_sample_metrics,
    split_dataset,
)


def train_one_seed(seed, splits, args, device, log_fh):
    """Train one ensemble member. Returns (best_val_loss, best_epoch,
    test_metrics_dict).

    Bootstrap-samples the training fold (replace=True, size = original
    train fold size) with `seed`. Reinitializes the model with `seed`
    so each member has different weights at step 0 in addition to a
    different data sample.
    """
    torch.manual_seed(seed)
    np.random.seed(seed)
    rng = np.random.default_rng(seed)

    train_X, train_Y, train_p = splits["train"]
    val_X, val_Y, val_p = splits["val"]
    test_X, test_Y, test_p = splits["test"]

    n_train = len(train_X)
    boot_idx = rng.integers(low=0, high=n_train, size=n_train)
    boot_X = train_X[boot_idx]
    boot_Y = train_Y[boot_idx]

    train_ds = TensorDataset(torch.from_numpy(boot_X), torch.from_numpy(boot_Y))
    val_ds = TensorDataset(torch.from_numpy(val_X), torch.from_numpy(val_Y))
    train_loader = DataLoader(train_ds, batch_size=args.batch_size, shuffle=True)
    val_loader = DataLoader(val_ds, batch_size=args.batch_size, shuffle=False)

    model = DIAModel().to(device)
    optimizer = optim.Adam(model.parameters(), lr=args.lr, weight_decay=1e-4)
    scheduler = optim.lr_scheduler.CosineAnnealingLR(optimizer, T_max=args.epochs)

    best_val_loss = float("inf")
    best_epoch = -1
    best_state = None
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
        val_preds, val_targets = [], []
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
        line = (
            f"[seed={seed}] epoch {epoch+1:3d}/{args.epochs}: "
            f"train={train_loss:.5f} val={val_loss:.5f} "
            f"sample_rec={m['sample_recovery_rate']:.3f} "
            f"gate_rec={m['gate_restricted_recall']:.3f} "
            f"lr={optimizer.param_groups[0]['lr']:.1e}{marker}"
        )
        print(line, flush=True)
        log_fh.write(line + "\n")
        log_fh.flush()

        if improved:
            best_val_loss = val_loss
            best_epoch = epoch + 1
            best_state = copy.deepcopy(model.state_dict())
            patience_counter = 0
        else:
            patience_counter += 1
            if patience_counter >= args.patience:
                print(
                    f"[seed={seed}] early-stop at epoch {epoch+1} "
                    f"(no improve for {args.patience} epochs)",
                    flush=True,
                )
                break

    ckpt_path = Path(args.outdir) / f"ensemble_seed_{seed}.pt"
    torch.save(best_state, ckpt_path)

    # Test-eval the best checkpoint for this member
    model.load_state_dict(best_state)
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
    test_metrics = per_sample_metrics(
        test_pred_t, test_target_t, torch.from_numpy(test_p)
    )
    test_metrics["seed"] = int(seed)
    test_metrics["best_val_loss"] = float(best_val_loss)
    test_metrics["best_epoch"] = int(best_epoch)
    test_metrics["n_bootstrap_unique"] = int(np.unique(boot_idx).size)
    test_metrics["bootstrap_size"] = int(n_train)

    metrics_path = Path(args.outdir) / f"ensemble_seed_{seed}.metrics.json"
    with open(metrics_path, "w") as f:
        json.dump(test_metrics, f, indent=2)

    return best_val_loss, best_epoch, test_metrics, str(ckpt_path)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--data",
        type=str,
        default=str(
            Path(__file__).parent.parent.parent
            / ".claude/worktrees/agent-a3b4a0fbd479ad9c9/research/experiments/"
            "2026-05-31-hb-064-dia-osd-session1/trajectories.jsonl"
        ),
    )
    parser.add_argument("--outdir", type=str, default=str(Path(__file__).parent))
    parser.add_argument(
        "--seeds",
        type=str,
        default="42,43,44,45,46,47,48,49",
        help="Comma-separated RNG seeds (default {42..49} — 8 members)",
    )
    parser.add_argument("--epochs", type=int, default=60)
    parser.add_argument("--batch-size", type=int, default=64)
    parser.add_argument("--lr", type=float, default=1e-3)
    parser.add_argument("--patience", type=int, default=15)
    parser.add_argument("--focal-gamma", type=float, default=2.0)
    parser.add_argument(
        "--split-seed",
        type=int,
        default=42,
        help="Train/val/test split seed — same across all members for "
        "fair ensemble eval (default 42 — matches Session 2)",
    )
    parser.add_argument(
        "--device", type=str, default="auto",
        choices=["auto", "cpu", "mps", "cuda"],
    )
    parser.add_argument(
        "--max-samples", type=int, default=None,
        help="Cap JSONL line scan (debug only)",
    )
    args = parser.parse_args()

    seeds = [int(s) for s in args.seeds.split(",") if s.strip()]
    print("=" * 70)
    print(f"hb-194 — Bayesian deep ensembles, N={len(seeds)} members, seeds={seeds}")
    print("=" * 70)

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

    print(f"Loading JSONL: {args.data}")
    X, Y, parity_final = load_jsonl(args.data, max_samples=args.max_samples)
    if len(X) == 0:
        print("ERROR: zero recovered samples — bailing.", file=sys.stderr)
        sys.exit(1)

    splits = split_dataset(X, Y, parity_final, seed=args.split_seed)
    for name, (Xs, Ys, ps) in splits.items():
        gate_n = int((ps <= 6).sum())
        print(f"  {name:5}: N={len(Xs):5d}  parity<=6 N={gate_n}")

    Path(args.outdir).mkdir(parents=True, exist_ok=True)
    log_path = Path(args.outdir) / "train_ensemble.log"

    started = time.time()
    summary = []
    with open(log_path, "w") as log_fh:
        log_fh.write(f"hb-194 ensemble training, seeds={seeds}, "
                     f"device={device}, started={time.strftime('%Y-%m-%dT%H:%M:%S')}\n")
        for seed in seeds:
            t0 = time.time()
            best_val_loss, best_epoch, test_metrics, ckpt = train_one_seed(
                seed, splits, args, device, log_fh
            )
            dt = time.time() - t0
            line = (
                f"[seed={seed}] DONE: best_val_loss={best_val_loss:.5f} "
                f"epoch={best_epoch} sample_rec={test_metrics['sample_recovery_rate']:.3f} "
                f"({dt:.0f}s) -> {ckpt}"
            )
            print(line, flush=True)
            log_fh.write(line + "\n")
            log_fh.flush()
            summary.append({
                "seed": seed,
                "best_val_loss": best_val_loss,
                "best_epoch": best_epoch,
                "elapsed_s": dt,
                "ckpt": ckpt,
                "test_metrics": test_metrics,
            })

    total_dt = time.time() - started
    print("=" * 70)
    print(f"Total elapsed: {total_dt:.0f}s ({total_dt/60:.1f} min) for "
          f"{len(seeds)} members")
    print("Per-member test sample_recovery_rate:")
    for s in summary:
        print(f"  seed={s['seed']}: {s['test_metrics']['sample_recovery_rate']:.4f} "
              f"(bit_acc={s['test_metrics']['bit_accuracy']:.4f}, "
              f"bit_prec={s['test_metrics']['bit_precision']:.4f}, "
              f"bit_rec={s['test_metrics']['bit_recall']:.4f})")

    summary_path = Path(args.outdir) / "ensemble_train_summary.json"
    with open(summary_path, "w") as f:
        json.dump({
            "seeds": seeds,
            "total_elapsed_s": total_dt,
            "device": str(device),
            "args": {
                "epochs": args.epochs,
                "batch_size": args.batch_size,
                "lr": args.lr,
                "patience": args.patience,
                "focal_gamma": args.focal_gamma,
                "split_seed": args.split_seed,
            },
            "members": summary,
        }, f, indent=2)
    print(f"Summary saved to {summary_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
