#!/usr/bin/env python3
"""hb-194 Session 2 — weight-space ensemble averaging (Wortsman 2022 "model
soups").

Loads N independently trained ensemble member checkpoints, computes the
element-wise mean of each layer's parameters, and exports a single packed
binary blob to `pancetta-ft8/assets/`.

Rationale
---------
Output-space ensembling (run all N models, average logits) is the gold
standard for Bayesian deep ensembles (Lakshminarayanan et al. 2017), but
costs N× inference. The OSD CNN is in the FT8 decoder hot path; 8×
inference is too expensive for production.

Wortsman et al. 2022 ("Model soups: averaging weights of multiple fine-
tuned models improves accuracy without increasing inference time", ICML)
showed that averaging the WEIGHTS of multiple fine-tuned models often
captures most of the ensemble benefit at zero inference cost, provided
the members all start from a similar initialization basin. Members
trained from scratch with different seeds may NOT satisfy this
assumption — different seeds can land in different loss-landscape modes,
in which case averaging produces a degraded model.

This script implements the "uniform soup" formulation (eq. 1 in
Wortsman 2022): θ_soup = mean(θ_1, …, θ_N). The Rust runtime is
UNCHANGED — the single 80KB weight blob is just an arithmetic mean of
the N members' weights, drop-in replaceable.

If the loss-basin assumption fails for this N=8 ensemble trained from
scratch with seed-only diversity, the A/B test will show a regression
and we shelve. If it passes, we get the N=8 ensemble benefit at 1×
inference cost — best-case outcome.

Usage
-----
    python average_ensemble_weights.py \\
        --inputs session2/ensemble_nb_seed_42.pt session2/ensemble_nb_seed_43.pt ... \\
        --output ../../pancetta-ft8/assets/neural_osd_weights_ensemble.bin
"""
import argparse
import os
import struct
import sys
from pathlib import Path

import torch

# Must agree with pancetta-ft8/src/neural_osd_weights.rs byte-for-byte.
# Copied verbatim from export_weights.py to keep this script standalone.
TENSOR_ORDER = [
    ("conv1.weight", 32 * 25 * 3),
    ("conv1.bias", 32),
    ("conv2.weight", 16 * 32 * 3),
    ("conv2.bias", 16),
    ("conv3.weight", 1 * 16 * 1),
    ("conv3.bias", 1),
    ("linear.weight", 91 * 174),
    ("linear.bias", 91),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--inputs", nargs="+", required=True,
        help="N ensemble member .pt checkpoints to average",
    )
    parser.add_argument(
        "--output", type=str, required=True,
        help="Output path for the packed-binary ensemble-mean weights blob",
    )
    args = parser.parse_args()

    if len(args.inputs) < 2:
        print("ERROR: need at least 2 checkpoints to average", file=sys.stderr)
        sys.exit(1)

    print(f"Averaging {len(args.inputs)} checkpoints:")
    for p in args.inputs:
        print(f"  {p}")

    # Load all checkpoints. state_dict format from train_ensemble.py.
    state_dicts = []
    for path in args.inputs:
        sd = torch.load(path, map_location="cpu", weights_only=True)
        state_dicts.append(sd)
        print(f"  loaded {path}: keys={list(sd.keys())}")

    # Verify all members have the same parameter set.
    ref_keys = set(state_dicts[0].keys())
    for i, sd in enumerate(state_dicts[1:], start=1):
        if set(sd.keys()) != ref_keys:
            raise SystemExit(
                f"checkpoint {i} key set differs from member 0:\n"
                f"  member 0: {sorted(ref_keys)}\n"
                f"  member {i}: {sorted(sd.keys())}"
            )

    # Element-wise mean per parameter (Wortsman 2022 uniform soup).
    avg_sd = {}
    for key in state_dicts[0].keys():
        stacked = torch.stack([sd[key].float() for sd in state_dicts], dim=0)
        avg_sd[key] = stacked.mean(dim=0)
        print(
            f"  {key}: shape={tuple(avg_sd[key].shape)}, "
            f"mean={avg_sd[key].mean().item():+.4e}, "
            f"member-stds-of-means={stacked.mean(dim=tuple(range(1, stacked.ndim))).std().item():+.4e}"
        )

    # Flatten in TENSOR_ORDER and validate against the Rust schema.
    weights_flat = {}
    for key, tensor in avg_sd.items():
        weights_flat[key] = tensor.detach().cpu().numpy().flatten().tolist()

    for name, expected_len in TENSOR_ORDER:
        actual = weights_flat.get(name)
        if actual is None:
            raise SystemExit(f"averaged state_dict is missing parameter '{name}'")
        if len(actual) != expected_len:
            raise SystemExit(
                f"parameter '{name}' has {len(actual)} elements, "
                f"schema expects {expected_len}. Update both this script "
                f"and pancetta-ft8/src/neural_osd_weights.rs together."
            )

    os.makedirs(os.path.dirname(os.path.abspath(args.output)), exist_ok=True)
    total_floats = 0
    with open(args.output, "wb") as f:
        for name, _ in TENSOR_ORDER:
            for v in weights_flat[name]:
                f.write(struct.pack("<f", v))
                total_floats += 1

    file_size = os.path.getsize(args.output)
    print()
    print(f"Exported {total_floats:,} f32 parameters to {args.output}")
    print(f"File size: {file_size:,} bytes ({file_size/1024:.1f} KB)")
    print(
        "Sentinel checksum: "
        f"conv1.weight[0]={weights_flat['conv1.weight'][0]:.8e} "
        f"conv1.bias[0]={weights_flat['conv1.bias'][0]:.8e} "
        f"linear.bias[0]={weights_flat['linear.bias'][0]:.8e}"
    )


if __name__ == "__main__":
    main()
