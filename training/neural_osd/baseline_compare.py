#!/usr/bin/env python3
"""Sanity-check the trained model against a |LLR|-ordering baseline on
the same test split.

For each test sample:
  - Baseline ranks info bits by ascending |final_llr| (smallest |LLR| = most
    likely wrong). Take top-T as "predicted errors" where T = true error count.
  - Model ranks info bits by descending predicted-error probability.

Report sample-level top-T precision >= 0.5 recovery for both.
"""
import json
import os
import sys
import numpy as np
import torch
from train_session2 import (
    DIAModel, load_jsonl, split_dataset, per_sample_metrics,
    N_CODEWORD, K_INFO, BP_ITERS,
)

JSONL = "../../.claude/worktrees/agent-a3b4a0fbd479ad9c9/research/experiments/2026-05-31-hb-064-dia-osd-session1/trajectories.jsonl"
CKPT = "session2_best.pt"


def llr_baseline_recovery(final_llrs_test, Y_test):
    """For each sample: predict top-T bits as the T smallest-|LLR| bits.
    Recovery = (top-T precision >= 0.5).
    """
    n = len(Y_test)
    recovered = 0
    for i in range(n):
        true_errs = np.where(Y_test[i] == 1)[0]
        T = max(len(true_errs), 1)
        # rank ASCENDING |LLR| (smallest first); take top T
        info_abs = np.abs(final_llrs_test[i][:K_INFO])
        bottom_idx = np.argpartition(info_abs, T - 1)[:T]
        correct = sum(1 for j in bottom_idx if j in set(true_errs.tolist()))
        if correct / T >= 0.5:
            recovered += 1
    return recovered / max(n, 1)


def main():
    print("Reloading dataset + final_llrs for baseline comparison...")
    # Read JSONL again to also capture final_llrs for recovered samples
    X_list, Y_list, parity_list, fll_list = [], [], [], []
    with open(JSONL, "r") as f:
        for line in f:
            d = json.loads(line)
            if not d.get("osd_recovered"):
                continue
            codeword = d.get("osd_codeword")
            if codeword is None:
                continue
            traj = np.asarray(d["trajectory_flat"], dtype=np.float32).reshape(BP_ITERS, N_CODEWORD)
            final_llrs = np.asarray(d["final_llrs"], dtype=np.float32)
            osd_cw = np.asarray(codeword, dtype=np.int8)
            hard_dec = (final_llrs < 0).astype(np.int8)
            err_full = (osd_cw != hard_dec).astype(np.float32)
            Y = err_full[:K_INFO]
            X_list.append(traj)
            Y_list.append(Y)
            fll_list.append(final_llrs)
            parity_list.append(int(d["features"].get("parity_errors_final", -1)))
    X = np.stack(X_list)
    Y = np.stack(Y_list)
    fll = np.stack(fll_list)
    p = np.asarray(parity_list, dtype=np.int32)
    print(f"recovered N={len(X)}, mean true errors/sample: {Y.sum(axis=1).mean():.2f}")

    # Same deterministic split
    rng = np.random.default_rng(42)
    idx = rng.permutation(len(X))
    n_train = int(0.8 * len(X))
    n_val = int(0.1 * len(X))
    test_idx = idx[n_train + n_val:]
    X_test = X[test_idx]
    Y_test = Y[test_idx]
    fll_test = fll[test_idx]
    p_test = p[test_idx]
    print(f"test N={len(X_test)} (parity<=6: {(p_test <= 6).sum()})")

    # |LLR| baseline
    llr_rate = llr_baseline_recovery(fll_test, Y_test)
    print(f"\n|LLR|-ordering baseline (production OSD's current ranker):")
    print(f"  sample top-T recovery rate: {llr_rate:.3f}")

    # Model
    device = torch.device("mps") if torch.backends.mps.is_available() else torch.device("cpu")
    model = DIAModel().to(device)
    model.load_state_dict(torch.load(CKPT, map_location=device, weights_only=True))
    model.eval()
    with torch.no_grad():
        pred_t = model(torch.from_numpy(X_test).to(device)).cpu()
    Y_test_t = torch.from_numpy(Y_test)
    p_test_t = torch.from_numpy(p_test)
    m = per_sample_metrics(pred_t, Y_test_t, p_test_t)
    print(f"\nModel (session2_best.pt) on same test split:")
    print(f"  sample top-T recovery rate: {m['sample_recovery_rate']:.3f}")
    print(f"  bit precision: {m['bit_precision']:.3f}")
    print(f"  bit recall: {m['bit_recall']:.3f}")
    print(f"  bit F1: {m['bit_f1']:.3f}")

    # Decision interpretation
    print(f"\nInterpretation:")
    if m['sample_recovery_rate'] > llr_rate * 1.10:
        print(f"  Model beats |LLR| baseline by {(m['sample_recovery_rate']/llr_rate - 1)*100:.0f}% — proceed.")
    else:
        print(f"  Model {m['sample_recovery_rate']:.3f} vs |LLR| {llr_rate:.3f}: "
              f"{'tie' if abs(m['sample_recovery_rate']-llr_rate)<0.02 else 'model loses'} — SHELVE.")
        print(f"  Production OSD already uses |LLR| ordering. No reason to swap weights.")


if __name__ == "__main__":
    main()
