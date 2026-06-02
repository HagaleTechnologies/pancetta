#!/usr/bin/env python3
"""hb-064 Session 3 — Wortsman-compliant shared-init ensemble fine-tune.

Per hb-194 Session 2 SHELVE: weight-space averaging (Wortsman 2022
"model soups") requires shared-init basin (Wortsman §3.1). The from-
scratch seed-diverse N=8 ensemble of Session 1's recommendation does
NOT satisfy that assumption — averaging produces the chord between
basin modes, typically worse than either endpoint.

Session 3 fixes that: load the production single-model weights as
INITIALIZATION for every member, then fine-tune N=8 copies with
different (seed, bootstrap) perturbations for a short run that keeps
them in the shared basin. Wortsman's uniform-soup recipe then applies
because all members descend from the same start.

Defaults:
  - 8 seeds (42..49)
  - 30 epochs per member (fewer than Session 2's 60; keep in basin)
  - LR = 1e-4 (10x smaller than Session 2; basin-local fine-tune)
  - focal loss γ=2 (Session 2 strategy)
  - bootstrap sampling enabled (Lakshminarayanan 2017)

Produces:
  session3/ensemble_si_seed_42.pt ... ensemble_si_seed_49.pt
  session3/ensemble_si_seed_NN.metrics.json
  session3/ensemble_si_train_summary.json
"""
import argparse
import copy
import json
import struct
import sys
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.optim as optim
from torch.utils.data import DataLoader, TensorDataset

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


# Tensor packing order — MUST match export_weights.py and
# pancetta-ft8/src/neural_osd_weights.rs byte-for-byte.
TENSOR_ORDER = [
    ("conv1.weight", (32, BP_ITERS, 3)),
    ("conv1.bias", (32,)),
    ("conv2.weight", (16, 32, 3)),
    ("conv2.bias", (16,)),
    ("conv3.weight", (1, 16, 1)),
    ("conv3.bias", (1,)),
    ("linear.weight", (K_INFO, N_CODEWORD)),
    ("linear.bias", (K_INFO,)),
]


def load_packed_weights_into_model(model: DIAModel, packed_path: Path) -> None:
    """Read the production packed-binary blob and stuff it into ``model``.

    The blob is little-endian f32 sequence in TENSOR_ORDER. We reshape
    each chunk and copy into the matching named parameter.
    """
    with open(packed_path, "rb") as f:
        raw = f.read()
    expected = sum(int(np.prod(s)) for _, s in TENSOR_ORDER)
    actual_floats = len(raw) // 4
    if actual_floats != expected:
        raise SystemExit(
            f"packed-blob size mismatch: got {actual_floats} f32s, "
            f"schema expects {expected} ({packed_path})"
        )

    floats = struct.unpack(f"<{expected}f", raw)
    offset = 0
    state_dict = model.state_dict()
    for name, shape in TENSOR_ORDER:
        nelem = int(np.prod(shape))
        chunk = np.asarray(floats[offset : offset + nelem], dtype=np.float32).reshape(shape)
        state_dict[name].copy_(torch.from_numpy(chunk))
        offset += nelem
    assert offset == expected
    model.load_state_dict(state_dict)


def train_one_seed(
    seed: int,
    splits,
    args,
    device: torch.device,
    log_fh,
    production_weights: Path,
):
    """Train one ensemble member starting from the production-shared
    init. Returns (best_val_loss, best_epoch, test_metrics, ckpt_path).
    """
    torch.manual_seed(seed)
    np.random.seed(seed)
    rng = np.random.default_rng(seed)

    train_X, train_Y, train_p = splits["train"]
    val_X, val_Y, val_p = splits["val"]
    test_X, test_Y, test_p = splits["test"]

    n_train = len(train_X)
    if args.no_bootstrap:
        boot_idx = np.arange(n_train)
        boot_X = train_X
        boot_Y = train_Y
    else:
        boot_idx = rng.integers(low=0, high=n_train, size=n_train)
        boot_X = train_X[boot_idx]
        boot_Y = train_Y[boot_idx]

    train_ds = TensorDataset(torch.from_numpy(boot_X), torch.from_numpy(boot_Y))
    val_ds = TensorDataset(torch.from_numpy(val_X), torch.from_numpy(val_Y))
    train_loader = DataLoader(train_ds, batch_size=args.batch_size, shuffle=True)
    val_loader = DataLoader(val_ds, batch_size=args.batch_size, shuffle=False)

    # Shared init: every seed starts from production weights, then
    # perturbed by the bootstrap sample + seeded SGD trajectory.
    model = DIAModel().to(device)
    load_packed_weights_into_model(model, production_weights)
    optimizer = optim.Adam(model.parameters(), lr=args.lr, weight_decay=1e-4)
    scheduler = optim.lr_scheduler.CosineAnnealingLR(optimizer, T_max=args.epochs)

    # Pre-training eval to log shared-init starting point
    model.eval()
    val_p_tensor = torch.from_numpy(val_p)
    with torch.no_grad():
        val_preds = []
        val_targets = []
        for X_batch, Y_batch in val_loader:
            X_batch = X_batch.to(device)
            Y_batch = Y_batch.to(device)
            val_preds.append(model(X_batch).cpu())
            val_targets.append(Y_batch.cpu())
        m_init = per_sample_metrics(
            torch.cat(val_preds, dim=0),
            torch.cat(val_targets, dim=0),
            val_p_tensor,
        )
    init_line = (
        f"[seed={seed}] init: val sample_rec={m_init['sample_recovery_rate']:.3f} "
        f"bit_prec={m_init['bit_precision']:.3f} bit_rec={m_init['bit_recall']:.3f}"
    )
    print(init_line, flush=True)
    log_fh.write(init_line + "\n")

    best_val_loss = float("inf")
    best_epoch = -1
    best_state = None
    patience_counter = 0

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
                early_line = (
                    f"[seed={seed}] early-stop at epoch {epoch+1} "
                    f"(no improve for {args.patience} epochs)"
                )
                print(early_line, flush=True)
                log_fh.write(early_line + "\n")
                break

    prefix = args.ckpt_prefix
    ckpt_path = Path(args.outdir) / f"{prefix}_seed_{seed}.pt"
    if best_state is None:
        best_state = model.state_dict()
    torch.save(best_state, ckpt_path)

    # Test eval
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
    test_metrics["init_val_sample_recovery_rate"] = float(m_init["sample_recovery_rate"])
    metrics_path = Path(args.outdir) / f"{prefix}_seed_{seed}.metrics.json"
    with open(metrics_path, "w") as f:
        json.dump(test_metrics, f, indent=2)

    return best_val_loss, best_epoch, test_metrics, str(ckpt_path)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--data", type=str, required=True,
                        help="Path to Session 3 trajectories JSONL")
    parser.add_argument("--production-weights", type=str,
                        default=str(Path(__file__).parent.parent.parent
                                    / "pancetta-ft8/assets/neural_osd_weights.bin"))
    parser.add_argument("--outdir", type=str,
                        default=str(Path(__file__).parent / "session3"))
    parser.add_argument("--seeds", type=str, default="42,43,44,45,46,47,48,49")
    parser.add_argument("--epochs", type=int, default=30,
                        help="Per-member epoch budget. Keep small so members stay in basin.")
    parser.add_argument("--batch-size", type=int, default=64)
    parser.add_argument("--lr", type=float, default=1e-4,
                        help="LR — 10x smaller than Session 2 for basin-local fine-tune.")
    parser.add_argument("--patience", type=int, default=10)
    parser.add_argument("--focal-gamma", type=float, default=2.0)
    parser.add_argument("--ckpt-prefix", type=str, default="ensemble_si")
    parser.add_argument("--split-seed", type=int, default=42)
    parser.add_argument("--device", type=str, default="auto",
                        choices=["auto", "cpu", "mps", "cuda"])
    parser.add_argument("--max-samples", type=int, default=None)
    parser.add_argument("--no-bootstrap", action="store_true")
    args = parser.parse_args()

    seeds = [int(s) for s in args.seeds.split(",") if s.strip()]
    print("=" * 72)
    print(f"hb-064 S3 shared-init ensemble, N={len(seeds)} members, seeds={seeds}")
    print(f"shared init <- {args.production_weights}")
    print("=" * 72)

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
        print("ERROR: zero recovered samples.", file=sys.stderr)
        sys.exit(1)

    splits = split_dataset(X, Y, parity_final, seed=args.split_seed)
    for name, (Xs, Ys, ps) in splits.items():
        gate_n = int((ps <= 6).sum())
        print(f"  {name:5}: N={len(Xs):5d}  parity<=6 N={gate_n}")

    Path(args.outdir).mkdir(parents=True, exist_ok=True)
    log_path = Path(args.outdir) / "train_session3_shared_init.log"
    started = time.time()
    summary = []
    with open(log_path, "w") as log_fh:
        log_fh.write(
            f"hb-064 S3 shared-init ensemble seeds={seeds} "
            f"device={device} started={time.strftime('%Y-%m-%dT%H:%M:%S')}\n"
        )
        for seed in seeds:
            t0 = time.time()
            best_val_loss, best_epoch, test_metrics, ckpt = train_one_seed(
                seed,
                splits,
                args,
                device,
                log_fh,
                Path(args.production_weights),
            )
            dt = time.time() - t0
            line = (
                f"[seed={seed}] DONE: best_val_loss={best_val_loss:.5f} "
                f"epoch={best_epoch} sample_rec={test_metrics['sample_recovery_rate']:.3f} "
                f"init_sample_rec={test_metrics['init_val_sample_recovery_rate']:.3f} "
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
    print("=" * 72)
    print(f"Total elapsed: {total_dt:.0f}s ({total_dt/60:.1f} min) for {len(seeds)} members")
    print("Per-member test sample_recovery_rate (Session 3 / shared-init basin):")
    for s in summary:
        print(
            f"  seed={s['seed']}: {s['test_metrics']['sample_recovery_rate']:.4f} "
            f"(init: {s['test_metrics']['init_val_sample_recovery_rate']:.4f}, "
            f"bit_acc={s['test_metrics']['bit_accuracy']:.4f}, "
            f"bit_prec={s['test_metrics']['bit_precision']:.4f}, "
            f"bit_rec={s['test_metrics']['bit_recall']:.4f})"
        )

    summary_path = Path(args.outdir) / "ensemble_si_train_summary.json"
    with open(summary_path, "w") as f:
        json.dump(
            {
                "seeds": seeds,
                "total_elapsed_s": total_dt,
                "device": str(device),
                "production_weights": str(args.production_weights),
                "args": {
                    "epochs": args.epochs,
                    "batch_size": args.batch_size,
                    "lr": args.lr,
                    "patience": args.patience,
                    "focal_gamma": args.focal_gamma,
                    "split_seed": args.split_seed,
                    "no_bootstrap": bool(args.no_bootstrap),
                },
                "members": summary,
            },
            f,
            indent=2,
        )
    print(f"Summary saved to {summary_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
