#!/usr/bin/env python3
"""Migrate the 25-channel neural-OSD blob to 26 channels without changing output."""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
from pathlib import Path

OLD_CHANNELS = 25
NEW_CHANNELS = 26
CONV1_OUT = 32
KERNEL = 3
OLD_CONV1_LEN = CONV1_OUT * OLD_CHANNELS * KERNEL
NEW_CONV1_LEN = CONV1_OUT * NEW_CHANNELS * KERNEL
TAIL_LENGTHS = [32, 16 * 32 * 3, 16, 16, 1, 91 * 174, 91]
TENSOR_NAMES = [
    "conv1.weight", "conv1.bias", "conv2.weight", "conv2.bias",
    "conv3.weight", "conv3.bias", "linear.weight", "linear.bias",
]


def migrate(values: list[float]) -> list[float]:
    expected = OLD_CONV1_LEN + sum(TAIL_LENGTHS)
    if len(values) != expected:
        raise ValueError(f"expected {expected} floats, got {len(values)}")
    old_conv1 = values[:OLD_CONV1_LEN]
    new_conv1: list[float] = []
    for out_channel in range(CONV1_OUT):
        start = out_channel * OLD_CHANNELS * KERNEL
        new_conv1.extend(old_conv1[start:start + OLD_CHANNELS * KERNEL])
        new_conv1.extend([0.0] * KERNEL)
    return new_conv1 + values[OLD_CONV1_LEN:]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    default = Path(__file__).resolve().parents[2] / "pancetta-ft8/assets/neural_osd_weights.bin"
    parser.add_argument("--input", type=Path, default=default)
    parser.add_argument("--output", type=Path, default=default)
    parser.add_argument("--provenance", type=Path, default=default.with_suffix(".provenance.json"))
    args = parser.parse_args()
    raw = args.input.read_bytes()
    values = list(struct.unpack(f"<{len(raw) // 4}f", raw))
    migrated = migrate(values)
    output = struct.pack(f"<{len(migrated)}f", *migrated)
    args.output.write_bytes(output)
    lengths = [NEW_CONV1_LEN, *TAIL_LENGTHS]
    provenance = {
        "schema_version": 1,
        "sha256": hashlib.sha256(output).hexdigest(),
        "byte_length": len(output),
        "total_len": len(migrated),
        "input_channels": NEW_CHANNELS,
        "syndrome_normalization_divisor": 3,
        "derivation": "25-channel production blob with a zero-initialized syndrome channel inserted per conv1 output channel",
        "tensors": [
            {"name": name, "length": length}
            for name, length in zip(TENSOR_NAMES, lengths, strict=True)
        ],
    }
    args.provenance.write_text(json.dumps(provenance, indent=2) + "\n")
    print(f"migrated {len(values)} -> {len(migrated)} f32; sha256={provenance['sha256']}")


if __name__ == "__main__":
    main()
