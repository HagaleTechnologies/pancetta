#!/usr/bin/env python3
"""Training data generator for Neural OSD.

Generates (LLR trajectory, error pattern) pairs for training a CNN that
predicts which LDPC info bits are wrong after belief propagation failure.

FT8 LDPC code: (174, 91) — 91 info bits, 83 parity bits.
"""

import argparse
import os
import re
import sys
from pathlib import Path

import numpy as np

# ---------------------------------------------------------------------------
# LDPC parity check matrix construction
# ---------------------------------------------------------------------------

# LDPC_NM from pancetta-ft8/src/ldpc.rs — sparse parity check matrix.
# Each row lists the 1-origin variable node indices connected to that check.
# 0 means unused (rows have 6 or 7 non-zero entries).
LDPC_NM: list[list[int]] = [
    [4, 31, 59, 91, 92, 96, 153],
    [5, 32, 60, 93, 115, 146, 0],
    [6, 24, 61, 94, 122, 151, 0],
    [7, 33, 62, 95, 96, 143, 0],
    [8, 25, 63, 83, 93, 96, 148],
    [6, 32, 64, 97, 126, 138, 0],
    [5, 34, 65, 78, 98, 107, 154],
    [9, 35, 66, 99, 139, 146, 0],
    [10, 36, 67, 100, 107, 126, 0],
    [11, 37, 67, 87, 101, 139, 158],
    [12, 38, 68, 102, 105, 155, 0],
    [13, 39, 69, 103, 149, 162, 0],
    [8, 40, 70, 82, 104, 114, 145],
    [14, 41, 71, 88, 102, 123, 156],
    [15, 42, 59, 106, 123, 159, 0],
    [1, 33, 72, 106, 107, 157, 0],
    [16, 43, 73, 108, 141, 160, 0],
    [17, 37, 74, 81, 109, 131, 154],
    [11, 44, 75, 110, 121, 166, 0],
    [45, 55, 64, 111, 130, 161, 173],
    [8, 46, 71, 112, 119, 166, 0],
    [18, 36, 76, 89, 113, 114, 143],
    [19, 38, 77, 104, 116, 163, 0],
    [20, 47, 70, 92, 138, 165, 0],
    [2, 48, 74, 113, 128, 160, 0],
    [21, 45, 78, 83, 117, 121, 151],
    [22, 47, 58, 118, 127, 164, 0],
    [16, 39, 62, 112, 134, 158, 0],
    [23, 43, 79, 120, 131, 145, 0],
    [19, 35, 59, 73, 110, 125, 161],
    [20, 36, 63, 94, 136, 161, 0],
    [14, 31, 79, 98, 132, 164, 0],
    [3, 44, 80, 124, 127, 169, 0],
    [19, 46, 81, 117, 135, 167, 0],
    [7, 49, 58, 90, 100, 105, 168],
    [12, 50, 61, 118, 119, 144, 0],
    [13, 51, 64, 114, 118, 157, 0],
    [24, 52, 76, 129, 148, 149, 0],
    [25, 53, 69, 90, 101, 130, 156],
    [20, 46, 65, 80, 120, 140, 170],
    [21, 54, 77, 100, 140, 171, 0],
    [35, 82, 133, 142, 171, 174, 0],
    [14, 30, 83, 113, 125, 170, 0],
    [4, 29, 68, 120, 134, 173, 0],
    [1, 4, 52, 57, 86, 136, 152],
    [26, 51, 56, 91, 122, 137, 168],
    [52, 84, 110, 115, 145, 168, 0],
    [7, 50, 81, 99, 132, 173, 0],
    [23, 55, 67, 95, 172, 174, 0],
    [26, 41, 77, 109, 141, 148, 0],
    [2, 27, 41, 61, 62, 115, 133],
    [27, 40, 56, 124, 125, 126, 0],
    [18, 49, 55, 124, 141, 167, 0],
    [6, 33, 85, 108, 116, 156, 0],
    [28, 48, 70, 85, 105, 129, 158],
    [9, 54, 63, 131, 147, 155, 0],
    [22, 53, 68, 109, 121, 174, 0],
    [3, 13, 48, 78, 95, 123, 0],
    [31, 69, 133, 150, 155, 169, 0],
    [12, 43, 66, 89, 97, 135, 159],
    [5, 39, 75, 102, 136, 167, 0],
    [2, 54, 86, 101, 135, 164, 0],
    [15, 56, 87, 108, 119, 171, 0],
    [10, 44, 82, 91, 111, 144, 149],
    [23, 34, 71, 94, 127, 153, 0],
    [11, 49, 88, 92, 142, 157, 0],
    [29, 34, 87, 97, 147, 162, 0],
    [30, 50, 60, 86, 137, 142, 162],
    [10, 53, 66, 84, 112, 128, 165],
    [22, 57, 85, 93, 140, 159, 0],
    [28, 32, 72, 103, 132, 166, 0],
    [28, 29, 84, 88, 117, 143, 150],
    [1, 26, 45, 80, 128, 147, 0],
    [17, 27, 89, 103, 116, 153, 0],
    [51, 57, 98, 163, 165, 172, 0],
    [21, 37, 73, 138, 152, 169, 0],
    [16, 47, 76, 130, 137, 154, 0],
    [3, 24, 30, 72, 104, 139, 0],
    [9, 40, 90, 106, 134, 151, 0],
    [15, 58, 60, 74, 111, 150, 163],
    [18, 42, 79, 144, 146, 152, 0],
    [25, 38, 65, 99, 122, 160, 0],
    [17, 42, 75, 129, 170, 172, 0],
]

N_CHECKS = 83
N_VARS = 174
N_INFO = 91
N_PARITY = 83
BP_ITERS = 25


def build_parity_check_matrix() -> np.ndarray:
    """Build the 83x174 parity check matrix H from LDPC_NM (sparse format)."""
    H = np.zeros((N_CHECKS, N_VARS), dtype=np.int8)
    for check_idx, row in enumerate(LDPC_NM):
        for var_1origin in row:
            if var_1origin > 0:
                H[check_idx, var_1origin - 1] = 1
    return H


def encode(info_bits: np.ndarray, H: np.ndarray) -> np.ndarray:
    """Encode 91 info bits into a 174-bit systematic codeword [info | parity].

    Parity bits are computed from the info-part of H (columns 0..91).
    parity[i] = sum(H[i, 0:91] * info_bits) mod 2
    """
    assert info_bits.shape == (N_INFO,)
    parity = (H[:, :N_INFO] @ info_bits) % 2
    return np.concatenate([info_bits, parity]).astype(np.int8)


def bpsk_modulate(codeword: np.ndarray) -> np.ndarray:
    """BPSK: 0 -> +1, 1 -> -1."""
    return 1.0 - 2.0 * codeword.astype(np.float64)


def add_awgn(signal: np.ndarray, snr_db: float) -> np.ndarray:
    """Add AWGN noise to signal at given SNR (dB)."""
    snr_lin = 10.0 ** (snr_db / 10.0)
    noise_var = 1.0 / (2.0 * snr_lin)
    noise = np.random.randn(len(signal)) * np.sqrt(noise_var)
    return signal + noise


def channel_llr(received: np.ndarray, snr_db: float) -> np.ndarray:
    """Compute initial channel LLRs for BPSK in AWGN.

    LLR = 2 * y * snr_linear * 2 = 4 * y * snr_linear
    Positive LLR favors bit 0, negative favors bit 1.
    """
    snr_lin = 10.0 ** (snr_db / 10.0)
    return 2.0 * received / (1.0 / (2.0 * snr_lin))


def bp_decode(
    channel_llrs: np.ndarray,
    H: np.ndarray,
    max_iter: int = BP_ITERS,
) -> tuple[np.ndarray, bool]:
    """Sum-product BP decoder, returning (trajectory, converged).

    trajectory: (max_iter, 174) — LLR values after each iteration.
    converged: True if all parity checks satisfied at any iteration.
    """
    # Pre-compute adjacency
    check_to_var: list[list[int]] = []
    for c in range(N_CHECKS):
        check_to_var.append(list(np.where(H[c] == 1)[0]))

    var_to_check: list[list[int]] = []
    for v in range(N_VARS):
        var_to_check.append(list(np.where(H[:, v] == 1)[0]))

    # Messages: check-to-variable
    # Use dictionaries for sparse message passing
    c2v = {}  # (check, var) -> message
    v2c = {}  # (var, check) -> message

    # Initialize v2c messages to channel LLRs
    for v in range(N_VARS):
        for c in var_to_check[v]:
            v2c[(v, c)] = channel_llrs[v]

    trajectory = np.zeros((max_iter, N_VARS), dtype=np.float32)
    converged = False

    for it in range(max_iter):
        # Check-to-variable update (sum-product / tanh rule)
        for c in range(N_CHECKS):
            vars_in_check = check_to_var[c]
            # Collect incoming v2c messages
            incoming = [v2c[(v, c)] for v in vars_in_check]

            for idx, v in enumerate(vars_in_check):
                product = 1.0
                for j in range(len(incoming)):
                    if j != idx:
                        x = np.clip(incoming[j] / 2.0, -10, 10)
                        product *= np.tanh(x)
                product = np.clip(product, -0.9999999, 0.9999999)
                c2v[(c, v)] = 2.0 * np.arctanh(product)

        # Variable-to-check update
        for v in range(N_VARS):
            checks_for_var = var_to_check[v]
            # Total LLR = channel + sum of all c2v
            total = channel_llrs[v] + sum(c2v.get((c, v), 0.0) for c in checks_for_var)
            trajectory[it, v] = total

            for c in checks_for_var:
                v2c[(v, c)] = total - c2v.get((c, v), 0.0)

        # Check convergence: hard-decision satisfies all parity checks?
        hard = (trajectory[it] < 0).astype(np.int8)
        syndrome = (H @ hard) % 2
        if np.all(syndrome == 0):
            # Fill remaining iterations with final values
            for remaining in range(it + 1, max_iter):
                trajectory[remaining] = trajectory[it]
            converged = True
            break

    return trajectory, converged


def generate_samples(
    n_samples: int,
    H: np.ndarray,
    snr_range: tuple[float, float] = (5.0, 14.0),
    seed: int = 42,
    verbose: bool = True,
) -> tuple[np.ndarray, np.ndarray]:
    """Generate training samples (BP failures only).

    Returns:
        X: (n_samples, BP_ITERS, N_VARS) — LLR trajectories
        Y: (n_samples, N_INFO) — error patterns (which info bits are wrong)
    """
    rng = np.random.RandomState(seed)
    X = np.zeros((n_samples, BP_ITERS, N_VARS), dtype=np.float32)
    Y = np.zeros((n_samples, N_INFO), dtype=np.float32)

    collected = 0
    total_tried = 0

    while collected < n_samples:
        # Random info bits
        info = rng.randint(0, 2, size=N_INFO).astype(np.int8)
        codeword = encode(info, H)

        # Random SNR in range
        snr_db = rng.uniform(snr_range[0], snr_range[1])

        # Modulate + noise
        signal = bpsk_modulate(codeword)
        received = add_awgn(signal, snr_db)
        llrs = channel_llr(received, snr_db)

        # Run BP
        trajectory, converged = bp_decode(llrs, H)
        total_tried += 1

        if converged:
            # Skip: BP succeeded, no training value
            continue

        # Hard decision on final iteration
        hard_decision = (trajectory[-1] < 0).astype(np.int8)
        info_errors = (hard_decision[:N_INFO] != info).astype(np.float32)

        X[collected] = trajectory
        Y[collected] = info_errors
        collected += 1

        if verbose and collected % 100 == 0:
            print(
                f"  collected {collected}/{n_samples} "
                f"(tried {total_tried}, failure rate "
                f"{collected/total_tried:.1%})"
            )

    if verbose:
        print(
            f"  Done: {collected} failures from {total_tried} trials "
            f"(failure rate {collected/total_tried:.1%})"
        )

    return X, Y


def main():
    parser = argparse.ArgumentParser(
        description="Generate neural OSD training data for FT8 LDPC"
    )
    parser.add_argument(
        "--n-train", type=int, default=100_000, help="Number of training samples"
    )
    parser.add_argument(
        "--n-val", type=int, default=10_000, help="Number of validation samples"
    )
    parser.add_argument(
        "--output-dir",
        type=str,
        default="training/neural_osd/data",
        help="Output directory",
    )
    parser.add_argument(
        "--snr-low", type=float, default=5.0, help="Low end of Eb/N0 range (dB)"
    )
    parser.add_argument(
        "--snr-high", type=float, default=14.0, help="High end of Eb/N0 range (dB)"
    )
    parser.add_argument("--seed", type=int, default=42, help="Random seed")
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)

    print("Building parity check matrix...")
    H = build_parity_check_matrix()
    print(f"  H shape: {H.shape}, nnz: {H.sum()}")

    # Verify H: each check should have 6 or 7 connections
    row_weights = H.sum(axis=1)
    print(f"  Row weights: min={row_weights.min()}, max={row_weights.max()}")

    snr_range = (args.snr_low, args.snr_high)

    print(f"\nGenerating {args.n_train} training samples...")
    train_X, train_Y = generate_samples(
        args.n_train, H, snr_range=snr_range, seed=args.seed
    )

    print(f"\nGenerating {args.n_val} validation samples...")
    val_X, val_Y = generate_samples(
        args.n_val, H, snr_range=snr_range, seed=args.seed + 1
    )

    # Save
    print(f"\nSaving to {args.output_dir}/...")
    np.save(os.path.join(args.output_dir, "train_X.npy"), train_X)
    np.save(os.path.join(args.output_dir, "train_Y.npy"), train_Y)
    np.save(os.path.join(args.output_dir, "val_X.npy"), val_X)
    np.save(os.path.join(args.output_dir, "val_Y.npy"), val_Y)

    print(f"\nShapes:")
    print(f"  train_X: {train_X.shape}")
    print(f"  train_Y: {train_Y.shape}")
    print(f"  val_X:   {val_X.shape}")
    print(f"  val_Y:   {val_Y.shape}")
    print("\nDone!")


if __name__ == "__main__":
    main()
