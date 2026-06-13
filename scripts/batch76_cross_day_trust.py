#!/usr/bin/env python3
"""Batch 76 — cross-day callsign trust: DB build + FP-separation diagnostic.

Iteration 3 of the user plan (Batch 72 journal): build a multi-day callsign
trust database from the ft8_lib truth labels covering all 25k raw slots,
then test whether "callsign seen on >= K distinct days" separates pancetta
TPs from FPs.

Phase A: walk research/baselines/ft8/*.ft8lib.json, attribute each truth
decode to its capture day (from the wav filename), build callsign -> day-set.

Phase B: replay the Batch 66 slot scan (research/corpus/scans/
raw_20260530_scan.jsonl, full pancetta decode output for 2066 slots of
2026-05-30 under the pre-Batch-72 osd=Some(2) default), label each decode
TP/FP against ft8_lib truth, and measure trust coverage per class using a
LEAVE-5/30-OUT trust set (trust built only from other days; no leakage).

Output: research/notes/2026-06-11-batch76-cross-day-trust.md
"""

import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

WS = Path(__file__).resolve().parent.parent
BASELINES = WS / "research/baselines/ft8"
SCAN = WS / "research/corpus/scans/raw_20260530_scan.jsonl"
MANIFEST = WS / "research/corpus/curated/ft8/raw_530_full.manifest.json"
NOTE = WS / "research/notes/2026-06-11-batch76-cross-day-trust.md"

DAY_RE = re.compile(r"ft8_(\d{8})_\d{6}\.wav$")


def extract_callsigns(text: str):
    """Mirror of batch66_slot_scan.rs::extract_callsigns."""
    out = []
    for token in text.split():
        if token.upper() == "CQ" or token.startswith("<"):
            continue
        # strip non-alphanumeric (except '/') from both ends, uppercase
        i, j = 0, len(token)
        while i < j and not (token[i].isalnum() or token[i] == "/"):
            i += 1
        while j > i and not (token[j - 1].isalnum() or token[j - 1] == "/"):
            j -= 1
        cleaned = token[i:j].upper()
        canonical = cleaned.split("/")[0]
        if not (3 <= len(canonical) <= 7):
            continue
        m = re.fullmatch(r"([A-Z]{1,2})([0-9]{1,4})([A-Z]{1,3})", canonical)
        if not m:
            continue
        out.append(canonical)
    return out


def day_of(wav_path: str):
    m = DAY_RE.search(wav_path)
    return m.group(1) if m else None


def main() -> None:
    # ---- Phase A: trust DB from all truth files ----
    call_days = defaultdict(set)
    day_files = Counter()
    day_decodes = Counter()
    n_files = 0
    for f in BASELINES.glob("*.ft8lib.json"):
        d = json.loads(f.read_text())
        day = day_of(d.get("wav_path", ""))
        if day is None:
            continue
        n_files += 1
        day_files[day] += 1
        for dec in d.get("decodes", []):
            msg = dec.get("message")
            if not msg:
                continue
            day_decodes[day] += 1
            for c in extract_callsigns(msg):
                call_days[c].add(day)

    days_hist = Counter(len(v) for v in call_days.values())
    print(f"truth files with day attribution: {n_files}")
    print(f"distinct days: {len(day_files)}")
    print(f"unique callsigns: {len(call_days)}")
    for k in sorted(days_hist):
        print(f"  callsigns seen on exactly {k} day(s): {days_hist[k]}")
    for kmin in (1, 2, 3, 4, 5):
        n = sum(1 for v in call_days.values() if len(v) >= kmin)
        print(f"trust set size at K>={kmin}: {n}")

    # ---- Phase B: TP/FP separation on the 5/30 scan, leave-5/30-out ----
    test_day = "20260530"
    trust_sets = {
        k: {c for c, v in call_days.items() if len(v - {test_day}) >= k}
        for k in (1, 2, 3, 4, 5)
    }

    manifest = json.loads(MANIFEST.read_text())
    path_to_sha = {e["wav_path"]: e["wav_sha256"] for e in manifest["entries"]}

    def truth_for(sha):
        p = BASELINES / f"{sha}.ft8lib.json"
        if not p.exists():
            return set()
        return {
            d["message"]
            for d in json.loads(p.read_text()).get("decodes", [])
            if d.get("message")
        }

    # counts[k][cls][bucket]; cls in {TP, FP}; bucket in {all_trusted, some, none, no_callsign}
    counts = {k: {"TP": Counter(), "FP": Counter()} for k in trust_sets}
    totals = Counter()
    missing_sha = 0
    with SCAN.open() as fh:
        for line in fh:
            rec = json.loads(line)
            sha = path_to_sha.get(rec["path"])
            if sha is None:
                missing_sha += 1
                continue
            truth = truth_for(sha)
            for dec in rec["decodes"]:
                cls = "TP" if dec["text"] in truth else "FP"
                totals[cls] += 1
                calls = extract_callsigns(dec["text"])
                for k, ts in trust_sets.items():
                    if not calls:
                        bucket = "no_callsign"
                    else:
                        hits = sum(1 for c in calls if c in ts)
                        bucket = (
                            "all_trusted"
                            if hits == len(calls)
                            else ("some_trusted" if hits else "none_trusted")
                        )
                    counts[k][cls][bucket] += 1

    lines = []
    lines.append("# Batch 76 — cross-day callsign trust (iteration 3)\n")
    lines.append("## Phase A: trust DB from ft8_lib truth over all raw days\n")
    lines.append(f"- truth files: {n_files}, distinct days: {len(day_files)}, "
                 f"unique callsigns: {len(call_days)}")
    for kmin in (1, 2, 3, 4, 5):
        n = sum(1 for v in call_days.values() if len(v) >= kmin)
        lines.append(f"- trust set at K>={kmin}: {n} callsigns")
    lines.append("\nDays-per-callsign histogram (first 10):")
    for k in sorted(days_hist)[:10]:
        lines.append(f"- {k} day(s): {days_hist[k]}")

    lines.append(f"\n## Phase B: separation on 5/30 scan (leave-5/30-out trust)\n")
    lines.append(f"Scan decodes: TP={totals['TP']}, FP={totals['FP']} "
                 f"(pre-Batch-72 osd=Some(2) scan; {missing_sha} paths unmatched)\n")
    for k in sorted(trust_sets):
        lines.append(f"### K>={k} (trust set {len(trust_sets[k])} callsigns)\n")
        lines.append("| Class | all_trusted | some_trusted | none_trusted | no_callsign |")
        lines.append("|---|---:|---:|---:|---:|")
        for cls in ("TP", "FP"):
            c = counts[k][cls]
            tot = totals[cls] or 1
            lines.append(
                f"| {cls} | {c['all_trusted']} ({c['all_trusted']/tot:.1%}) "
                f"| {c['some_trusted']} ({c['some_trusted']/tot:.1%}) "
                f"| {c['none_trusted']} ({c['none_trusted']/tot:.1%}) "
                f"| {c['no_callsign']} ({c['no_callsign']/tot:.1%}) |"
            )
        lines.append("")
        # hypothetical filter: drop decodes with >=1 callsign and none trusted
        tp_lost = counts[k]["TP"]["none_trusted"]
        fp_killed = counts[k]["FP"]["none_trusted"]
        lines.append(
            f"Hypothetical gate 'reject if has callsigns and none trusted': "
            f"kills {fp_killed} FPs ({fp_killed/(totals['FP'] or 1):.1%}), "
            f"loses {tp_lost} TPs ({tp_lost/(totals['TP'] or 1):.1%})\n"
        )

    NOTE.write_text("\n".join(lines) + "\n")
    print(f"\nwrote {NOTE}")
    print("\n".join(lines[-40:]))


if __name__ == "__main__":
    main()
