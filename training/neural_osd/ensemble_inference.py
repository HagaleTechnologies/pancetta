#!/usr/bin/env python3
"""hb-194 — ensemble inference + variance/uncertainty analysis.

Loads N ensemble member checkpoints, runs forward-pass on the held-out
test split (same deterministic split as `train_session2.py` and
`train_ensemble.py`), and reports:

  1. Per-member sample_recovery_rate (single-model baselines)
  2. Ensemble-mean prediction sample_recovery_rate
  3. Per-sample predictive variance (mean of per-bit variance over the
     91 info bits) — distribution stats (mean, p10, p50, p90)
  4. Variance-vs-correctness calibration:
     - Split test set into low-/high-variance halves by ensemble variance
     - Compare sample_recovery_rate of each half
     - Spearman/Pearson correlation: per-sample variance vs per-sample
       "is recovered correctly" indicator
  5. Decision per the brief:
     - ensemble-mean beats single-model by >5% accuracy → GRADUATE
     - else if variance calibration |r| >= 0.2 (and high-var subset
       LOWER acc than low-var) → PROCEED to Session 2 (wire variance as
       a flag)
     - else → SHELVE

Outputs:
  ensemble_eval.json (structured metrics)
  prints a human-readable summary

Reuses `train_session2.load_jsonl`, `split_dataset`, `per_sample_metrics`
verbatim so the split is byte-identical to Session 2.
"""
import argparse
import json
import sys
from pathlib import Path

import numpy as np
import torch
from torch.utils.data import DataLoader, TensorDataset

sys.path.insert(0, str(Path(__file__).parent))
from train_session2 import (  # noqa: E402
    DIAModel,
    K_INFO,
    load_jsonl,
    per_sample_metrics,
    split_dataset,
)


def _per_sample_correct(pred_np, Y_np):
    """Per-sample top-T recovered indicator (same logic as
    per_sample_metrics' sample_recovered, returned as an array)."""
    n = pred_np.shape[0]
    correct = np.zeros(n, dtype=bool)
    n_true_errors = Y_np.sum(axis=1).astype(int)
    for i in range(n):
        T = max(n_true_errors[i], 1)
        top_idx = np.argpartition(pred_np[i], -T)[-T:]
        true_set = set(np.where(Y_np[i] == 1)[0].tolist())
        hits = sum(1 for j in top_idx if j in true_set)
        if T > 0 and hits / T >= 0.5:
            correct[i] = True
    return correct


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
        "--seeds", type=str, default="42,43,44,45,46,47,48,49",
    )
    parser.add_argument("--split-seed", type=int, default=42)
    parser.add_argument("--batch-size", type=int, default=64)
    parser.add_argument(
        "--device", type=str, default="auto",
        choices=["auto", "cpu", "mps", "cuda"],
    )
    parser.add_argument("--max-samples", type=int, default=None)
    parser.add_argument(
        "--output-json", type=str, default="ensemble_eval.json",
    )
    args = parser.parse_args()

    seeds = [int(s) for s in args.seeds.split(",") if s.strip()]

    if args.device == "auto":
        if torch.backends.mps.is_available():
            device = torch.device("mps")
        elif torch.cuda.is_available():
            device = torch.device("cuda")
        else:
            device = torch.device("cpu")
    else:
        device = torch.device(args.device)

    print(f"hb-194 ensemble inference — N={len(seeds)} members, device={device}")
    print(f"Loading data: {args.data}")
    X, Y, parity_final = load_jsonl(args.data, max_samples=args.max_samples)
    splits = split_dataset(X, Y, parity_final, seed=args.split_seed)
    test_X, test_Y, test_p = splits["test"]

    test_ds = TensorDataset(torch.from_numpy(test_X), torch.from_numpy(test_Y))
    test_loader = DataLoader(test_ds, batch_size=args.batch_size, shuffle=False)
    Y_np_test = test_Y  # already np
    print(f"test N={len(test_X)}  parity<=6 N={int((test_p <= 6).sum())}")

    # Run forward-pass for each member; stack to (N_seeds, N_samples, K_INFO)
    all_preds = []
    member_metrics = []
    for seed in seeds:
        ckpt_path = Path(args.outdir) / f"ensemble_seed_{seed}.pt"
        if not ckpt_path.exists():
            print(f"ERROR: missing checkpoint {ckpt_path}", file=sys.stderr)
            sys.exit(1)
        model = DIAModel().to(device)
        state = torch.load(ckpt_path, map_location=device, weights_only=True)
        model.load_state_dict(state)
        model.eval()
        preds_chunks = []
        with torch.no_grad():
            for X_batch, _ in test_loader:
                p = model(X_batch.to(device))
                preds_chunks.append(p.cpu())
        preds = torch.cat(preds_chunks, dim=0).numpy()  # (N_samples, 91)
        all_preds.append(preds)
        m = per_sample_metrics(
            torch.from_numpy(preds),
            torch.from_numpy(Y_np_test),
            torch.from_numpy(test_p),
        )
        m["seed"] = int(seed)
        member_metrics.append(m)
        print(f"  seed={seed}: sample_rec={m['sample_recovery_rate']:.4f} "
              f"bit_acc={m['bit_accuracy']:.4f} "
              f"bit_prec={m['bit_precision']:.4f} "
              f"bit_rec={m['bit_recall']:.4f}")

    preds_stack = np.stack(all_preds, axis=0)  # (N_seeds, N_samples, 91)

    # Ensemble mean prediction
    ens_mean = preds_stack.mean(axis=0)  # (N_samples, 91)
    # Per-bit variance across seeds, then mean over bits -> per-sample
    per_bit_var = preds_stack.var(axis=0, ddof=0)  # (N_samples, 91)
    per_sample_var = per_bit_var.mean(axis=1)      # (N_samples,)

    # Predictive entropy of the ensemble mean (per-bit Bernoulli entropy
    # averaged over bits) — alternative disagreement signal.
    eps = 1e-9
    H_bits = -(ens_mean * np.log(ens_mean + eps)
               + (1 - ens_mean) * np.log(1 - ens_mean + eps))
    per_sample_entropy = H_bits.mean(axis=1)  # (N_samples,)

    ens_metrics = per_sample_metrics(
        torch.from_numpy(ens_mean),
        torch.from_numpy(Y_np_test),
        torch.from_numpy(test_p),
    )
    print()
    print(f"ENSEMBLE-MEAN sample_recovery_rate: {ens_metrics['sample_recovery_rate']:.4f}")
    print(f"               bit_accuracy:        {ens_metrics['bit_accuracy']:.4f}")
    print(f"               bit_precision:       {ens_metrics['bit_precision']:.4f}")
    print(f"               bit_recall:          {ens_metrics['bit_recall']:.4f}")

    # Single-model baseline = MEAN of per-member sample_recovery_rate
    # (apples-to-apples vs ensemble on same test set)
    single_rec_mean = float(np.mean([m["sample_recovery_rate"] for m in member_metrics]))
    single_rec_std = float(np.std([m["sample_recovery_rate"] for m in member_metrics]))
    print(f"SINGLE-MODEL mean sample_recovery_rate: {single_rec_mean:.4f} "
          f"(std {single_rec_std:.4f})")

    delta_abs = ens_metrics["sample_recovery_rate"] - single_rec_mean
    delta_rel = delta_abs / max(single_rec_mean, 1e-9)
    print(f"Δ ensemble vs single-model mean: {delta_abs:+.4f} ({delta_rel*100:+.1f}%)")

    # Variance distribution
    var_stats = {
        "mean": float(per_sample_var.mean()),
        "std": float(per_sample_var.std()),
        "p10": float(np.percentile(per_sample_var, 10)),
        "p50": float(np.percentile(per_sample_var, 50)),
        "p90": float(np.percentile(per_sample_var, 90)),
        "min": float(per_sample_var.min()),
        "max": float(per_sample_var.max()),
    }
    entropy_stats = {
        "mean": float(per_sample_entropy.mean()),
        "p10": float(np.percentile(per_sample_entropy, 10)),
        "p50": float(np.percentile(per_sample_entropy, 50)),
        "p90": float(np.percentile(per_sample_entropy, 90)),
    }
    print()
    print(f"Per-sample variance distribution: mean={var_stats['mean']:.5f} "
          f"p10={var_stats['p10']:.5f} p50={var_stats['p50']:.5f} "
          f"p90={var_stats['p90']:.5f}")
    print(f"Per-sample entropy distribution:  mean={entropy_stats['mean']:.5f} "
          f"p10={entropy_stats['p10']:.5f} p50={entropy_stats['p50']:.5f} "
          f"p90={entropy_stats['p90']:.5f}")

    # Variance-vs-correctness calibration
    ens_correct = _per_sample_correct(ens_mean, Y_np_test)  # bool (N,)
    n = len(ens_correct)
    half = n // 2
    # Sort by variance ascending
    sort_idx = np.argsort(per_sample_var)
    low_var_idx = sort_idx[:half]
    high_var_idx = sort_idx[half:]
    low_var_acc = float(ens_correct[low_var_idx].mean()) if half else 0.0
    high_var_acc = float(ens_correct[high_var_idx].mean()) if (n - half) else 0.0
    # Pearson correlation: variance vs (1 - correct) i.e. variance vs error
    err = 1 - ens_correct.astype(np.float64)
    # Manual Pearson (avoids scipy dep)
    if per_sample_var.std() > 0 and err.std() > 0:
        pearson = float(np.corrcoef(per_sample_var, err)[0, 1])
    else:
        pearson = 0.0
    # Spearman via rank
    var_rank = np.argsort(np.argsort(per_sample_var))
    err_rank = np.argsort(np.argsort(err))
    if var_rank.std() > 0 and err_rank.std() > 0:
        spearman = float(np.corrcoef(var_rank, err_rank)[0, 1])
    else:
        spearman = 0.0

    print()
    print(f"Calibration: low-var half acc={low_var_acc:.4f}, "
          f"high-var half acc={high_var_acc:.4f}, "
          f"Δ={(low_var_acc - high_var_acc):+.4f}")
    print(f"  Pearson(variance, error)  = {pearson:+.4f}")
    print(f"  Spearman(variance, error) = {spearman:+.4f}")

    # Decision logic per the brief
    # 1. ensemble mean beats single-model by >5% (relative) → GRADUATE
    # 2. variance-calibration |r| >= 0.2 AND low-var-acc > high-var-acc → PROCEED
    # 3. else → SHELVE
    if delta_rel > 0.05:
        decision = "GRADUATE"
        rationale = (
            f"ensemble mean sample_rec beats single-model mean by "
            f"{delta_rel*100:+.1f}% (>5%) — ensemble for production"
        )
    elif (abs(spearman) >= 0.2) and (low_var_acc > high_var_acc):
        decision = "PROCEED"
        rationale = (
            f"ensemble mean is flat vs single-model ({delta_rel*100:+.1f}%) "
            f"but variance is calibrated (Spearman={spearman:+.3f}, "
            f"low-var acc {low_var_acc:.3f} > high-var acc {high_var_acc:.3f}) "
            f"— wire variance as a flag into production decoder (Session 2)"
        )
    else:
        decision = "SHELVE"
        rationale = (
            f"ensemble mean delta {delta_rel*100:+.1f}% < 5% AND variance "
            f"not calibrated (Spearman={spearman:+.3f}, low-var acc "
            f"{low_var_acc:.3f} vs high-var acc {high_var_acc:.3f}) — "
            f"no operational lever"
        )
    print()
    print(f"DECISION: {decision}")
    print(f"  {rationale}")

    out = {
        "seeds": seeds,
        "n_test": int(n),
        "single_model": {
            "per_seed_sample_recovery": [m["sample_recovery_rate"] for m in member_metrics],
            "mean_sample_recovery": single_rec_mean,
            "std_sample_recovery": single_rec_std,
            "per_member_metrics": member_metrics,
        },
        "ensemble_mean": {
            "metrics": ens_metrics,
        },
        "delta_vs_single_model": {
            "absolute": float(delta_abs),
            "relative_pct": float(delta_rel * 100),
        },
        "variance": var_stats,
        "entropy": entropy_stats,
        "calibration": {
            "low_var_half_acc": low_var_acc,
            "high_var_half_acc": high_var_acc,
            "delta_low_minus_high": float(low_var_acc - high_var_acc),
            "pearson_var_vs_error": pearson,
            "spearman_var_vs_error": spearman,
        },
        "decision": decision,
        "rationale": rationale,
    }
    outpath = Path(args.outdir) / args.output_json
    with open(outpath, "w") as f:
        json.dump(out, f, indent=2)
    print(f"\nResults saved to {outpath}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
