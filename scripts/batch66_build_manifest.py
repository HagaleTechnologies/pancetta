#!/usr/bin/env python3
"""Batch 66 — build a curated manifest from a JSONL slot-scan.

Reads batch66_slot_scan output (one JSON line per slot), selects slots
that match a criterion (repeat-heavy by default), and writes a
manifest at research/corpus/curated/ft8/<name>.manifest.json.

Repeat-heavy criterion: a slot qualifies if at least one of its
decoded callsigns appears in >= K total slots within the same scan
within a 45-minute window centered on this slot.

Usage:
    python3 scripts/batch66_build_manifest.py \\
        --scan research/corpus/scans/raw_20260530_scan.jsonl \\
        --out research/corpus/curated/ft8/repeat_heavy_530.manifest.json \\
        --name repeat_heavy_530 \\
        --min-callsign-repeats 3 \\
        --window-seconds 2700 \\
        --max-slots 500
"""
import argparse
import json
import os
import hashlib
import sys
from collections import defaultdict
from pathlib import Path


def sha256_of(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--scan", required=True, help="JSONL slot scan input")
    p.add_argument("--out", required=True, help="Manifest output path")
    p.add_argument("--name", required=True, help="Manifest label")
    p.add_argument(
        "--min-callsign-repeats",
        type=int,
        default=3,
        help="Minimum total slots containing the same callsign within window",
    )
    p.add_argument(
        "--window-seconds",
        type=int,
        default=2700,
        help="Sliding window (default 2700 = 45 min)",
    )
    p.add_argument(
        "--max-slots", type=int, default=500, help="Cap selected slot count"
    )
    args = p.parse_args()

    slots = []
    with open(args.scan) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            slots.append(json.loads(line))
    print(f"loaded {len(slots)} slots", file=sys.stderr)

    # Sort by timestamp.
    slots.sort(key=lambda s: s.get("timestamp", 0))

    # For each callsign, tally appearances by slot index. Then for each
    # slot, count how many of its callsigns appear in >=K slots within
    # the 45-min window. The slot qualifies if any callsign hits K.
    n = len(slots)
    callsign_to_slot_indices: defaultdict = defaultdict(list)
    for i, s in enumerate(slots):
        for c in s.get("callsigns_in_slot", []):
            callsign_to_slot_indices[c].append(i)

    # For each slot, find qualifying callsigns.
    selected_indices = []
    for i, s in enumerate(slots):
        ts = s.get("timestamp", 0)
        if ts == 0:
            continue
        for c in s.get("callsigns_in_slot", []):
            indices = callsign_to_slot_indices.get(c, [])
            # Count indices whose timestamp is within window of ts.
            count_in_window = 0
            for j in indices:
                jts = slots[j].get("timestamp", 0)
                if jts and abs(jts - ts) <= args.window_seconds:
                    count_in_window += 1
            if count_in_window >= args.min_callsign_repeats:
                selected_indices.append(i)
                break
    print(f"selected {len(selected_indices)} qualifying slots", file=sys.stderr)

    if args.max_slots and len(selected_indices) > args.max_slots:
        # Sample evenly across the qualifying set.
        step = len(selected_indices) / args.max_slots
        selected_indices = [
            selected_indices[int(k * step)] for k in range(args.max_slots)
        ]
        print(
            f"capped to {len(selected_indices)} slots (every {step:.2f}th)",
            file=sys.stderr,
        )

    entries = []
    for idx, i in enumerate(selected_indices):
        path = slots[i]["path"]
        if not os.path.exists(path):
            continue
        sha = sha256_of(path)
        entries.append(
            {
                "wav_path": path,
                "wav_sha256": sha,
                "slot_index": idx,
                "wav_timestamp": slots[i].get("timestamp"),
            }
        )
        if (idx + 1) % 100 == 0:
            print(f"  hashed {idx + 1}/{len(selected_indices)}", file=sys.stderr)

    out = {
        "schema_version": 1,
        "label": args.name,
        "generated_at_utc": "batch66",
        "source": args.scan,
        "criterion": f"repeat_heavy (min_repeats={args.min_callsign_repeats}, "
        f"window={args.window_seconds}s)",
        "entries": entries,
    }

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as fh:
        json.dump(out, fh, indent=2)
    print(f"wrote {args.out} ({len(entries)} entries)", file=sys.stderr)


if __name__ == "__main__":
    main()
