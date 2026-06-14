#!/usr/bin/env python3
"""Build a per-day curated manifest from station captures + ft8_lib truth.

Walks ~/.pancetta/recordings for WAVs whose filename matches a capture day
(ft8_<YYYYMMDD>_<HHMMSS>[_<band>].wav), and emits
research/corpus/curated/ft8/day_<YYYYMMDD>.manifest.json containing the slots
that have >= 1 ft8_lib truth decode (the same selection used for the 6/13 day
manifest in Batch 102/103). Truth labels must already exist under
research/baselines/ft8/<sha>.ft8lib.json (run batch71_ft8lib_truth_all first).

Usage:
    python3 scripts/build_day_manifest.py 20260614
    python3 scripts/build_day_manifest.py 20260614 --recordings /path/to/recordings
"""

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

WS = Path(__file__).resolve().parent.parent
BASELINES = WS / "research/baselines/ft8"
OUT_DIR = WS / "research/corpus/curated/ft8"
# Tolerant of the optional _<band> suffix (Batch 103 band-stamped recordings).
DAY_RE = re.compile(r"ft8_(\d{8})_\d{6}")


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def truth_decode_count(sha: str) -> int:
    p = BASELINES / f"{sha}.ft8lib.json"
    if not p.exists():
        return 0
    try:
        return len(json.loads(p.read_text()).get("decodes", []))
    except (json.JSONDecodeError, OSError):
        return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("day", help="capture day, YYYYMMDD")
    ap.add_argument(
        "--recordings",
        default=str(Path.home() / ".pancetta/recordings"),
        help="recordings directory",
    )
    args = ap.parse_args()

    rec_dir = Path(args.recordings)
    wavs = sorted(
        p
        for p in rec_dir.glob(f"ft8_{args.day}_*.wav")
        if DAY_RE.search(p.name)
    )
    if not wavs:
        print(f"no WAVs for day {args.day} under {rec_dir}", file=sys.stderr)
        return 1

    entries = []
    no_truth = 0
    empty = 0
    for wav in wavs:
        sha = sha256_of(wav)
        n = truth_decode_count(sha)
        if not (BASELINES / f"{sha}.ft8lib.json").exists():
            no_truth += 1
            continue
        if n < 1:
            empty += 1
            continue
        entries.append({"wav_path": str(wav), "wav_sha256": sha})

    manifest = {
        "schema_version": 1,
        "label": f"day_{args.day}",
        "source": (
            f"station capture {args.day[:4]}-{args.day[4:6]}-{args.day[6:]} "
            "(full day), slots with >=1 ft8_lib decode"
        ),
        "entries": entries,
    }
    out = OUT_DIR / f"day_{args.day}.manifest.json"
    out.write_text(json.dumps(manifest, indent=1) + "\n")

    print(
        f"day {args.day}: {len(wavs)} WAVs -> {len(entries)} slots with >=1 decode "
        f"({empty} empty, {no_truth} unlabeled) -> {out}"
    )
    if no_truth:
        print(
            f"WARNING: {no_truth} WAVs have no truth label; run batch71 first",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
