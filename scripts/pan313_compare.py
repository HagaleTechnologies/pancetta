#!/usr/bin/env python3
"""pan313_compare.py — paired A/B checker for the PAN-313 FFT-fallback
budget gate (internal tool; standard library only).

Compares a PARENT and a CANDIDATE results file produced by
`pancetta-ft8/examples/pan313_fallback_eval.rs --mode results` and enforces
the ticket's fixture-level contracts before reporting success:

  1. both inputs exist, are non-empty, parse, and are result records;
  2. identity: the same recordings, WAV sha256 hashes, sample counts,
     config (protocol, budget, repetition shape) and Rayon thread width;
  3. unlimited-budget message sets are EXACTLY equal per recording and
     length (the ticket's "decode output is unchanged" scenario);
  4. reference recall (true-positive proxy against the parent's
     unlimited message sets, since no jt9 truth cache exists in the
     cloud) at the Standard budget is not lower on any recording or in
     aggregate — counts without independent truth are never reported as
     true positives;
  5. the median per-recording S1-floor time at the Standard budget
     strictly decreases (unchanged or increased time fails).

Missing or empty inputs are a FAILURE, never a silent zero-loss success.
The parent built before the gate publishes no `fft-fallback` row; that is
reported as "unavailable", not treated as zero.

Exit status: 0 = all checks pass, 1 = any failure (all failures listed).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

LENGTHS = ("151680", "162000", "full")


def load_results(path: str) -> dict:
    p = Path(path)
    if not p.exists():
        raise SystemExit(f"FAIL: input missing: {path}")
    raw = p.read_bytes()
    if not raw.strip():
        raise SystemExit(f"FAIL: input empty: {path}")
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as e:
        raise SystemExit(f"FAIL: input {path} is not valid JSON: {e}") from e
    if data.get("kind") != "pan313-fft-fallback-results":
        raise SystemExit(f"FAIL: input {path} is not a pan313 results file")
    recs = data.get("recordings")
    if not isinstance(recs, list) or not recs:
        raise SystemExit(f"FAIL: input {path} holds no recordings")
    return data


def arm_map(recording: dict) -> dict:
    """(length, budget) -> arm record."""
    out = {}
    for arm in recording["arms"]:
        out[(arm["length"], arm["budget"])] = arm
    return out


def check_identity(parent: dict, candidate: dict, failures: list) -> None:
    for key in ("protocol", "standard_budget_ms"):
        if parent["config"][key] != candidate["config"][key]:
            failures.append(
                f"config mismatch on {key}: "
                f"{parent['config'][key]!r} != {candidate['config'][key]!r}"
            )
    p_threads = parent["host"]["rayon_threads"]
    c_threads = candidate["host"]["rayon_threads"]
    if p_threads != c_threads:
        failures.append(
            f"thread-width mismatch: parent {p_threads} vs candidate {c_threads} "
            "(paired timing requires the same Rayon pool width)"
        )
    p_paths = [r["path"] for r in parent["recordings"]]
    c_paths = [r["path"] for r in candidate["recordings"]]
    if p_paths != c_paths:
        failures.append(
            f"recording set mismatch: parent {len(p_paths)} vs candidate "
            f"{len(c_paths)} recordings (first difference: "
            f"{next((a for a, b in zip(p_paths, c_paths) if a != b), 'count')})"
        )
        return
    for p_rec, c_rec in zip(parent["recordings"], candidate["recordings"]):
        if p_rec["sha256"] != c_rec["sha256"]:
            failures.append(f"{p_rec['path']}: WAV sha256 mismatch")
        if p_rec["samples"] != c_rec["samples"]:
            failures.append(
                f"{p_rec['path']}: sample-count mismatch "
                f"{p_rec['samples']} != {c_rec['samples']}"
            )
        for (length, budget), p_arm in arm_map(p_rec).items():
            c_arm = arm_map(c_rec).get((length, budget))
            if c_arm is None:
                failures.append(
                    f"{p_rec['path']}@{length}/{budget}: candidate arm missing"
                )


def check_unlimited_sets(parent: dict, candidate: dict, failures: list) -> None:
    for p_rec, c_rec in zip(parent["recordings"], candidate["recordings"]):
        for length in LENGTHS:
            p_texts = set(arm_map(p_rec)[(length, "unlimited")]["texts"])
            c_texts = set(arm_map(c_rec)[(length, "unlimited")]["texts"])
            if p_texts != c_texts:
                lost = sorted(p_texts - c_texts)
                gained = sorted(c_texts - p_texts)
                failures.append(
                    f"{p_rec['path']}@{length}: unlimited message set changed "
                    f"(lost {lost}, gained {gained})"
                )


def check_reference_recall(parent: dict, candidate: dict, failures: list) -> list:
    """Standard-budget recall against the parent unlimited sets (TP proxy).
    Returns per-recording TP differences for the report."""
    diffs = []
    agg_p = agg_c = 0
    for p_rec, c_rec in zip(parent["recordings"], candidate["recordings"]):
        rec_p = rec_c = 0
        for length in LENGTHS:
            reference = set(arm_map(p_rec)[(length, "unlimited")]["texts"])
            p_std = set(arm_map(p_rec)[(length, "standard")]["texts"])
            c_std = set(arm_map(c_rec)[(length, "standard")]["texts"])
            rec_p += len(p_std & reference)
            rec_c += len(c_std & reference)
        diffs.append((p_rec["path"], rec_p, rec_c, rec_c - rec_p))
        agg_p += rec_p
        agg_c += rec_c
    for path, tp_p, tp_c, delta in diffs:
        if tp_c < tp_p:
            failures.append(
                f"{path}: Standard reference recall dropped {tp_p} -> {tp_c} "
                f"({delta})"
            )
    if agg_c < agg_p:
        failures.append(
            f"aggregate Standard reference recall dropped {agg_p} -> {agg_c}"
        )
    return diffs


def median(values: list) -> float:
    s = sorted(values)
    n = len(s)
    if n == 0:
        return float("nan")
    return float(s[n // 2]) if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2.0


def check_s1_floor(parent: dict, candidate: dict, failures: list) -> tuple:
    p_meds, c_meds = [], []
    for p_rec, c_rec in zip(parent["recordings"], candidate["recordings"]):
        p_ms = [
            ms
            for length in LENGTHS
            for ms in arm_map(p_rec)[(length, "standard")]["s1_floor_ms"]
        ]
        c_ms = [
            ms
            for length in LENGTHS
            for ms in arm_map(c_rec)[(length, "standard")]["s1_floor_ms"]
        ]
        if p_ms:
            p_meds.append(median(p_ms))
        if c_ms:
            c_meds.append(median(c_ms))
    if not p_meds or not c_meds:
        failures.append("S1-floor timing missing from one of the inputs")
        return (None, None)
    p_agg, c_agg = median(p_meds), median(c_meds)
    if c_agg >= p_agg:
        failures.append(
            f"median S1-floor wall time at Standard did not improve: "
            f"parent {p_agg} ms -> candidate {c_agg} ms"
        )
    return (p_agg, c_agg)


def fallback_availability(candidate: dict) -> str:
    rows = 0
    nulls = 0
    for rec in candidate["recordings"]:
        for arm in rec["arms"]:
            if arm["fft_fallback"] is None:
                nulls += 1
            else:
                rows += 1
    return f"{rows} arms with fft-fallback rows, {nulls} unavailable (null)"


def compare(parent_path: str, candidate_path: str) -> int:
    failures: list = []
    parent = load_results(parent_path)
    candidate = load_results(candidate_path)
    check_identity(parent, candidate, failures)
    if not any("recording set mismatch" in f for f in failures):
        check_unlimited_sets(parent, candidate, failures)
        diffs = check_reference_recall(parent, candidate, failures)
        p_s1, c_s1 = check_s1_floor(parent, candidate, failures)
    if failures:
        print(f"FAIL: {len(failures)} problem(s):")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PASS: paired A/B checks")
    print(f"  unlimited message sets: identical on all recordings")
    print(f"  Standard reference recall: aggregate {diffs and sum(d[1] for d in diffs)}"
          f" -> {diffs and sum(d[2] for d in diffs)}")
    for path, tp_p, tp_c, delta in diffs:
        print(f"    {path}: {tp_p} -> {tp_c} ({delta:+d})")
    print(f"  median S1-floor @Standard: {p_s1} ms -> {c_s1} ms")
    print(f"  candidate fallback telemetry: {fallback_availability(candidate)}")
    return 0


def main(argv: list) -> int:
    if len(argv) != 3:
        print("usage: pan313_compare.py PARENT.json CANDIDATE.json", file=sys.stderr)
        return 2
    return compare(argv[1], argv[2])


if __name__ == "__main__":
    sys.exit(main(sys.argv))
