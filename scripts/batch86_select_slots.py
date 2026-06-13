#!/usr/bin/env python3
"""Batch 86 — select the top-N slots for the hb-104 kill-switch experiment.

Ranks 5/30 slots by the number of (miss, decoded-signal) pairs with
|freq delta| < 6.25 Hz and |dt delta| < 2 s, using the Batch 66 scan
(pancetta decodes) and the Batch 85-refreshed ft8_lib truth (real
freq/time). Emits research/corpus/curated/ft8/hb104_kill_switch.json
with per-slot pair coordinates.
"""

import json
from pathlib import Path

WS = Path(__file__).resolve().parent.parent
TOP_N = 20
FREQ_GATE = 6.25
TIME_GATE = 2.0

manifest = json.loads(
    (WS / "research/corpus/curated/ft8/raw_530_full.manifest.json").read_text()
)
path_to_sha = {e["wav_path"]: e["wav_sha256"] for e in manifest["entries"]}


def truth_for(sha):
    p = WS / f"research/baselines/ft8/{sha}.ft8lib.json"
    if not p.exists():
        return []
    return [
        (d["message"], d["freq_hz"], d.get("time_sec", 0.0))
        for d in json.loads(p.read_text())["decodes"]
        if d.get("message")
    ]


slots = []
scan = WS / "research/corpus/scans/raw_20260530_scan.jsonl"
for line in scan.open():
    rec = json.loads(line)
    sha = path_to_sha.get(rec["path"])
    if sha is None:
        continue
    truth = truth_for(sha)
    dec_by_text = {d["text"]: (d["freq"], d["dt"]) for d in rec["decodes"]}
    pairs = []
    for msg, tf, tt in truth:
        if msg in dec_by_text:
            continue  # not a miss
        for ntext, (nf, ndt) in dec_by_text.items():
            if abs(nf - tf) < FREQ_GATE and abs(ndt - tt) < TIME_GATE:
                pairs.append(
                    {
                        "miss_message": msg,
                        "miss_freq_hz": tf,
                        "miss_time_sec": tt,
                        "neighbor_text": ntext,
                        "neighbor_freq_hz": nf,
                        "neighbor_dt_sec": ndt,
                    }
                )
    if pairs:
        slots.append(
            {
                "wav_path": rec["path"],
                "wav_sha256": sha,
                "n_pairs": len(pairs),
                "pairs": pairs,
            }
        )

slots.sort(key=lambda s: -s["n_pairs"])
out = {
    "schema_version": 1,
    "label": "hb104_kill_switch",
    "source": "batch86_select_slots.py (scan=raw_20260530, truth=ft8_lib batch85)",
    "freq_gate_hz": FREQ_GATE,
    "time_gate_sec": TIME_GATE,
    "entries": slots[:TOP_N],
}
dest = WS / "research/corpus/curated/ft8/hb104_kill_switch.json"
dest.write_text(json.dumps(out, indent=1))
total_pairs = sum(s["n_pairs"] for s in slots[:TOP_N])
print(
    f"{len(slots)} slots with >=1 co-channel miss-pair; "
    f"top {TOP_N} hold {total_pairs} pairs -> {dest}"
)
for s in slots[:5]:
    print(f"  {Path(s['wav_path']).name}: {s['n_pairs']} pairs")
