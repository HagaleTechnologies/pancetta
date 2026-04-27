#!/usr/bin/env python3
"""Export trained DIA model weights to a packed binary blob.

Writes a flat little-endian f32 file at
``pancetta-ft8/assets/neural_osd_weights.bin``. The Rust loader
(``pancetta-ft8/src/neural_osd_weights.rs``) splits the blob back into
the eight named tensors using the dimensions baked into the schema —
both files MUST agree, byte-for-byte.

The previous version of this script wrote a 19,953-line Rust source
file with the weights as inline ``pub const X: &[f32]`` arrays. That
worked but was a 12 MB blob in source form that bloated every workspace
build. The packed binary takes ~80 KB.
"""
import argparse
import os
import struct


# Layout (concatenated, no header). Must stay in lockstep with
# pancetta-ft8/src/neural_osd_weights.rs::TOTAL_LEN and the per-tensor
# `*_LEN` constants there.
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
    parser.add_argument("--model", type=str, default="model.pt")
    parser.add_argument(
        "--output",
        type=str,
        default="../../pancetta-ft8/assets/neural_osd_weights.bin",
    )
    args = parser.parse_args()

    import torch
    from train import DIAModel

    model = DIAModel()
    model.load_state_dict(torch.load(args.model, map_location="cpu", weights_only=True))
    model.eval()

    weights = {}
    for name, param in model.named_parameters():
        weights[name] = param.detach().numpy().flatten().tolist()

    # Validate shapes against the schema before writing — catches a
    # model topology change that would otherwise silently corrupt the
    # blob layout.
    for name, expected_len in TENSOR_ORDER:
        actual = weights.get(name)
        if actual is None:
            raise SystemExit(f"model is missing parameter '{name}'")
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
            for v in weights[name]:
                f.write(struct.pack("<f", v))
                total_floats += 1

    file_size = os.path.getsize(args.output)
    print(f"Exported {total_floats:,} f32 parameters to {args.output}")
    print(f"File size: {file_size:,} bytes ({file_size/1024:.1f} KB)")
    print(
        "Sentinel checksum: "
        f"conv1.weight[0]={weights['conv1.weight'][0]:.8e} "
        f"conv1.bias[0]={weights['conv1.bias'][0]:.8e} "
        f"linear.bias[0]={weights['linear.bias'][0]:.8e}"
    )


if __name__ == "__main__":
    main()
