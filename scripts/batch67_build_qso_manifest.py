#!/usr/bin/env python3
"""Batch 67 — build a QSO-continuous manifest from a slot scan.

A slot qualifies if AT LEAST ONE of its decoded callsigns also appears
in the NEXT slot (within 30 seconds). Output entries include both the
qualifying slot AND the next slot so the cross-sequence A7 measurement
has the (N, N+1) pairs to consume.

Usage:
    python3 scripts/batch67_build_qso_manifest.py \\
        --scan research/corpus/scans/raw_20260530_scan.jsonl \\
        --out research/corpus/curated/ft8/qso_continuous_530.manifest.json \\
        --name qso_continuous_530 \\
        --max-pairs 250
"""
import argparse
import json
import os
import hashlib
import sys
from pathlib import Path


def sha256_of(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--scan", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--name", required=True)
    p.add_argument(
        "--max-pairs",
        type=int,
        default=250,
        help="Cap on (slot N, slot N+1) pairs; default 250 = 500 slots total",
    )
    p.add_argument(
        "--pair-window-seconds",
        type=int,
        default=30,
        help="Max wall-clock between adjacent slots to count as a pair (default 30)",
    )
    args = p.parse_args()

    slots = []
    with open(args.scan) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            slots.append(json.loads(line))
    slots.sort(key=lambda s: s.get("timestamp", 0))
    print(f"loaded {len(slots)} slots", file=sys.stderr)

    # Find qualifying (N, N+1) pairs: same callsign in both, within window.
    pairs = []
    for i in range(len(slots) - 1):
        a = slots[i]
        b = slots[i + 1]
        ats = a.get("timestamp")
        bts = b.get("timestamp")
        if not ats or not bts:
            continue
        if abs(bts - ats) > args.pair_window_seconds:
            continue
        a_calls = set(a.get("callsigns_in_slot", []))
        b_calls = set(b.get("callsigns_in_slot", []))
        if a_calls & b_calls:
            pairs.append((i, i + 1))
    print(f"found {len(pairs)} qualifying QSO-continuous pairs", file=sys.stderr)

    if args.max_pairs and len(pairs) > args.max_pairs:
        step = len(pairs) / args.max_pairs
        pairs = [pairs[int(k * step)] for k in range(args.max_pairs)]
        print(f"capped to {len(pairs)} pairs (every {step:.2f}th)", file=sys.stderr)

    # Materialise both slots of each pair as manifest entries.
    seen = set()
    entries = []
    pair_count = 0
    for pair_idx, (i, j) in enumerate(pairs):
        for slot_local in (i, j):
            if slot_local in seen:
                continue
            seen.add(slot_local)
            path = slots[slot_local]["path"]
            if not os.path.exists(path):
                continue
            sha = sha256_of(path)
            entries.append(
                {
                    "wav_path": path,
                    "wav_sha256": sha,
                    "slot_index": slot_local,
                    "wav_timestamp": slots[slot_local].get("timestamp"),
                    "pair_index": pair_idx,
                }
            )
        pair_count += 1
        if (pair_count) % 50 == 0:
            print(f"  hashed pairs {pair_count}/{len(pairs)}", file=sys.stderr)

    out = {
        "schema_version": 1,
        "label": args.name,
        "generated_at": "batch67",
        "source": args.scan,
        "criterion": (
            f"qso_continuous (adjacent pair within {args.pair_window_seconds}s "
            f"with shared callsign)"
        ),
        "entries": entries,
    }

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as fh:
        json.dump(out, fh, indent=2)
    print(f"wrote {args.out} ({len(entries)} entries)", file=sys.stderr)


if __name__ == "__main__":
    main()
