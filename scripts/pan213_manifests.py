#!/usr/bin/env python3
"""PAN-213 survey and kill-switch manifest guard and digest-verified repair.

check:
    Reject WAV references that point into the live capture buffer
    (~/.pancetta/recordings, any home, POSIX or Windows separators, dot
    segments) and non-absolute WAV paths in the 2026-05-30 survey and the
    hb-104 kill switch. --verify-files additionally opens every retained
    reference and compares its SHA-256 with the stored wav_sha256. The
    default check performs no WAV I/O.

repair:
    Rewrite live-buffer references to a digest-verified copy found by
    basename in the local corpus or in the YYYY-MM month shards of the NAS
    archive, stage the rewritten manifests plus byte-identical path-free
    inputs, audit the staged tree with the check, and record every dropped
    recording in research/corpus/PAN-213-recovery.json.

Standard library only. Run from the repository root:

    python3 scripts/pan213_manifests.py check --root .
    python3 scripts/pan213_manifests.py check --root . --verify-files
    python3 scripts/pan213_manifests.py repair --root . \
        --corpus-dir ~/.pancetta/corpus \
        --nas-dir /Users/Shared/maersk/offload/pancetta-wav-corpus \
        --output-dir /tmp/pan213-stage
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
from collections import OrderedDict
from datetime import datetime, timezone

SURVEY_DIR = "research/corpus/surveys/2026-05-30"
KILL_SWITCH = "research/corpus/curated/ft8/hb104_kill_switch.json"
ARRAY_FILES = (
    "all_wavs_scored.json",
    "existing_top100.entries.json",
    "today_top100.entries.json",
)
PATH_FREE_FILES = ("per_wav.json", "summary.json")
REQUIRED_FILES = tuple(f"{SURVEY_DIR}/{name}" for name in ARRAY_FILES + PATH_FREE_FILES) + (KILL_SWITCH,)
RECOVERY_REPORT = "research/corpus/PAN-213-recovery.json"
HEX64 = re.compile(r"^[0-9a-fA-F]{64}$")
MONTH_SHARD = re.compile(r"^\d{4}-\d{2}$")
LIVE_REASON = "live-buffer path (inside .pancetta/recordings, the capture buffer)"
RELATIVE_REASON = "path is not absolute"


class ManifestError(Exception):
    pass


class InventoryError(Exception):
    pass


def _split_root_parts(path):
    s = path.replace("\\", "/")
    root = None
    m = re.match(r"^([A-Za-z]:)", s)
    if m:
        root = m.group(1)
        s = s[m.end():]
    elif s.startswith("~"):
        m = re.match(r"^(~[^/]*)", s)
        root = m.group(1)
        s = s[m.end():]
    elif s.startswith("/"):
        root = "/"
    parts = [seg for seg in s.split("/") if seg]
    return root, parts


def _resolve_parts(root, parts):
    resolved = []
    for seg in parts:
        if seg == ".":
            continue
        if seg == "..":
            if resolved:
                resolved.pop()
            elif root is None:
                resolved.append("..")
            continue
        resolved.append(seg)
    return resolved


def _has_live_pair(parts):
    return any(a == ".pancetta" and b == "recordings" for a, b in zip(parts, parts[1:]))


def classify_path(path):
    root, raw_parts = _split_root_parts(path)
    norm_parts = _resolve_parts(root, raw_parts)
    if _has_live_pair(raw_parts) or _has_live_pair(norm_parts):
        return "live", LIVE_REASON
    if root is None:
        return "relative", RELATIVE_REASON
    return "stable", ""


def path_key(path):
    root, parts = _split_root_parts(path)
    return (root, tuple(_resolve_parts(root, parts)))


def store_basename(path):
    root, parts = _split_root_parts(path)
    resolved = _resolve_parts(root, parts)
    return resolved[-1] if resolved else ""


class Reference:
    __slots__ = ("rel", "index", "path", "digest")

    def __init__(self, rel, index, path, digest):
        self.rel = rel
        self.index = index
        self.path = path
        self.digest = digest


def _validate_rows(rel, rows):
    for i, row in enumerate(rows):
        if not isinstance(row, dict):
            raise ManifestError(f"{rel} row {i}: entry is not a JSON object")
        if "wav_path" in row and row["wav_path"] is not None:
            if not isinstance(row["wav_path"], str) or not row["wav_path"]:
                raise ManifestError(f"{rel} row {i}: wav_path must be a non-empty string")


def _read_json(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def load_required(root):
    missing = [rel for rel in REQUIRED_FILES if not os.path.isfile(os.path.join(root, rel))]
    if missing:
        raise ManifestError(
            "missing required input(s): " + ", ".join(missing) + " (all six are required)"
        )
    data = OrderedDict()
    for rel in REQUIRED_FILES:
        try:
            data[rel] = _read_json(os.path.join(root, rel))
        except json.JSONDecodeError as e:
            raise ManifestError(f"invalid JSON in {rel}: {e}")
        except OSError as e:
            raise ManifestError(f"cannot read {rel}: {e}")
    _validate_shapes(data)
    return data


def _validate_shapes(data):
    for rel in REQUIRED_FILES:
        if rel == KILL_SWITCH:
            obj = data[rel]
            if not isinstance(obj, dict):
                raise ManifestError(f"{rel}: top level is not a JSON object")
            entries = obj.get("entries")
            if not isinstance(entries, list):
                raise ManifestError(f'{rel}: "entries" is not an array')
            _validate_rows(rel, entries)
        elif rel == f"{SURVEY_DIR}/summary.json":
            if not isinstance(data[rel], dict):
                raise ManifestError(f"{rel}: top level is not a JSON object")
        else:
            arr = data[rel]
            if not isinstance(arr, list):
                raise ManifestError(f"{rel}: top level is not a JSON array")
            _validate_rows(rel, arr)


def collect_references(data):
    refs = []
    for name in ARRAY_FILES:
        rel = f"{SURVEY_DIR}/{name}"
        for i, row in enumerate(data[rel]):
            if "wav_path" in row and row["wav_path"] is not None:
                refs.append(Reference(rel, i, row["wav_path"], row.get("wav_sha256")))
    entries = data[KILL_SWITCH]["entries"]
    for i, row in enumerate(entries):
        if "wav_path" in row and row["wav_path"] is not None:
            refs.append(Reference(KILL_SWITCH, i, row["wav_path"], row.get("wav_sha256")))
    return refs


def file_digest(path):
    h = hashlib.sha256()
    try:
        with open(path, "rb") as f:
            for chunk in iter(lambda: f.read(1 << 20), b""):
                h.update(chunk)
    except OSError as e:
        raise InventoryError(f"cannot read recording {path}: {e}")
    return h.hexdigest()


def _verify_reference(ref, cache):
    resolved = os.path.realpath(os.path.expanduser(ref.path))
    status, _ = classify_path(resolved)
    if status == "live":
        return [f"{ref.rel} row {ref.index}: {ref.path}: resolves into the live buffer: {resolved}"]
    if not os.path.exists(resolved):
        return [f"{ref.rel} row {ref.index}: {ref.path}: file not found: {resolved}"]
    if not os.path.isfile(resolved):
        return [f"{ref.rel} row {ref.index}: {ref.path}: not a regular file: {resolved}"]
    digest = ref.digest
    if not isinstance(digest, str) or not HEX64.match(digest):
        return [f"{ref.rel} row {ref.index}: {ref.path}: wav_sha256 is missing or not a 64-hex digest"]
    if resolved not in cache:
        cache[resolved] = file_digest(resolved)
    actual = cache[resolved]
    if actual != digest.lower():
        return [
            f"{ref.rel} row {ref.index}: {ref.path}: sha256 mismatch: file {actual}, manifest {digest}"
        ]
    return []


def run_check(root, verify_files=False):
    data = load_required(root)
    refs = collect_references(data)
    failures = []
    for ref in refs:
        status, reason = classify_path(ref.path)
        if status != "stable":
            failures.append(f"{ref.rel} row {ref.index}: {ref.path}: {reason}")
    if verify_files:
        cache = {}
        for ref in refs:
            if classify_path(ref.path)[0] != "stable":
                continue
            failures.extend(_verify_reference(ref, cache))
    return refs, failures


def cmd_check(args):
    try:
        refs, failures = run_check(args.root, args.verify_files)
    except ManifestError as e:
        print(f"pan213 check: {e}")
        return 2
    except InventoryError as e:
        print(f"pan213 check: {e}")
        return 3
    for line in failures:
        print(f"pan213 check: {line}")
    if failures:
        print(f"pan213 check: FAILED, {len(failures)} problem reference(s) under {args.root}")
        return 1
    mode = ", every file hashed" if args.verify_files else ", no WAV I/O"
    print(
        f"pan213 check: passed, {len(refs)} reference(s) across {len(REQUIRED_FILES)} files{mode}"
    )
    return 0


def index_nas_shards(nas_dir):
    try:
        with os.scandir(nas_dir) as it:
            shards = sorted(
                e.name for e in it if e.is_dir(follow_symlinks=False) and MONTH_SHARD.match(e.name)
            )
    except OSError as e:
        raise InventoryError(f"cannot list NAS archive {nas_dir}: {e}")
    if not shards:
        raise InventoryError(f"NAS archive {nas_dir} has no YYYY-MM month shards")
    index = {}
    for name in shards:
        for file_path in _walk_shard(os.path.join(nas_dir, name)):
            index.setdefault(os.path.basename(file_path), []).append(file_path)
    for name in index:
        index[name].sort()
    return shards, index


def _walk_shard(shard_root):
    files = []

    def rec(dir_path):
        try:
            with os.scandir(dir_path) as it:
                entries = sorted(it, key=lambda e: e.name)
        except OSError as e:
            raise InventoryError(f"cannot walk NAS shard {dir_path}: {e}")
        for entry in entries:
            try:
                if entry.is_dir(follow_symlinks=False):
                    rec(entry.path)
                elif entry.is_file():
                    files.append(entry.path)
            except OSError as e:
                raise InventoryError(f"cannot inspect NAS entry {entry.path}: {e}")

    rec(shard_root)
    return files


def _decide(data, corpus_dir, nas_dir):
    refs = collect_references(data)
    groups = OrderedDict()
    occurrences = {}
    for ref in refs:
        digest = ref.digest
        if not isinstance(digest, str) or not HEX64.match(digest):
            raise ManifestError(
                f"{ref.rel} row {ref.index}: {ref.path}: wav_sha256 is missing or not a 64-hex digest"
            )
        lowered = digest.lower()
        key = path_key(ref.path)
        if key in groups and groups[key]["digest"] != lowered:
            raise ManifestError(
                f"{ref.rel} row {ref.index}: contradictory wav_sha256 values for {ref.path}"
            )
        group = groups.setdefault(
            key, {"old_path": ref.path, "digest": lowered, "occurrences": []}
        )
        group["occurrences"].append(ref)
        occurrences[(ref.rel, ref.index)] = key

    nas_state = {"shards": [], "index": None}

    def nas_lookup(name):
        if nas_state["index"] is None:
            shards, index = index_nas_shards(nas_dir)
            nas_state["shards"] = shards
            nas_state["index"] = index
        return nas_state["index"].get(name, [])

    for group in groups.values():
        old = group["old_path"]
        digest = group["digest"]
        status, _ = classify_path(old)
        rejected = []

        def try_candidate(path):
            actual = file_digest(path)
            if actual == digest:
                return True
            rejected.append({"path": path, "sha256": actual})
            return False

        if status == "stable":
            resolved = os.path.realpath(os.path.expanduser(old))
            if os.path.isfile(resolved):
                actual = file_digest(resolved)
                if actual == digest:
                    group["outcome"] = "retained_stable"
                    group["new_path"] = old
                    continue
                rejected.append({"path": resolved, "sha256": actual})

        name = store_basename(old)
        matched = None
        if name:
            local_path = os.path.join(corpus_dir, name)
            if os.path.isfile(local_path) and try_candidate(local_path):
                matched = local_path
            if matched is None:
                for candidate in nas_lookup(name):
                    if try_candidate(candidate):
                        matched = candidate
                        break
        if matched is not None:
            group["outcome"] = (
                "resolved_nas" if matched.startswith(nas_dir + os.sep) else "resolved_local"
            )
            group["new_path"] = matched
        else:
            group["outcome"] = "dropped"
            group["reason"] = "digest_mismatch" if rejected else "no_candidate"
            group["rejected_candidates"] = rejected
    return groups, occurrences, nas_state


def _counts(groups):
    distinct = {
        "referenced": len(groups),
        "resolved_local": 0,
        "resolved_nas": 0,
        "retained_stable": 0,
        "dropped": 0,
    }
    references = {
        "total": 0,
        "resolved_local": 0,
        "resolved_nas": 0,
        "retained_stable": 0,
        "dropped": 0,
    }
    for group in groups.values():
        n = len(group["occurrences"])
        distinct[group["outcome"]] += 1
        references[group["outcome"]] += n
        references["total"] += n
    return {"distinct_wavs": distinct, "references": references}


def _detect_indent(text):
    m = re.search(r"\n([ \t]+)", text)
    return m.group(1) if m else None


def _dump_like_source(obj, source_text, path):
    text = json.dumps(obj, indent=_detect_indent(source_text))
    if source_text.endswith("\n"):
        text += "\n"
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def _rewrite_rows(rows, rel, groups, occurrences):
    rewritten = []
    for i, row in enumerate(rows):
        key = occurrences.get((rel, i))
        if key is None:
            rewritten.append(row)
            continue
        group = groups[key]
        if group["outcome"] == "dropped":
            continue
        if group["outcome"] in ("resolved_local", "resolved_nas"):
            row = dict(row)
            row["wav_path"] = group["new_path"]
        rewritten.append(row)
    return rewritten


def cmd_repair(args):
    root = os.path.abspath(args.root)
    corpus_dir = os.path.abspath(os.path.expanduser(args.corpus_dir))
    nas_dir = os.path.abspath(os.path.expanduser(args.nas_dir))
    out_dir = os.path.abspath(os.path.expanduser(args.output_dir))
    try:
        data = load_required(root)
    except ManifestError as e:
        print(f"pan213 repair: {e}")
        return 2
    if os.path.exists(out_dir) and os.listdir(out_dir):
        print(f"pan213 repair: staging directory is not empty: {out_dir}")
        return 2
    try:
        if not os.path.isdir(corpus_dir):
            raise InventoryError(f"local corpus not accessible: {corpus_dir}")
        groups, occurrences, nas_state = _decide(data, corpus_dir, nas_dir)
    except ManifestError as e:
        print(f"pan213 repair: {e}")
        return 2
    except InventoryError as e:
        print(f"pan213 repair: {e}")
        return 3

    sources = {}
    for rel in REQUIRED_FILES:
        with open(os.path.join(root, rel), "r", encoding="utf-8") as f:
            sources[rel] = f.read()

    os.makedirs(out_dir, exist_ok=True)
    try:
        for name in ARRAY_FILES + PATH_FREE_FILES:
            rel = f"{SURVEY_DIR}/{name}"
            staged_path = os.path.join(out_dir, rel)
            os.makedirs(os.path.dirname(staged_path), exist_ok=True)
            if name in PATH_FREE_FILES:
                shutil.copyfile(os.path.join(root, rel), staged_path)
                continue
            _dump_like_source(
                _rewrite_rows(data[rel], rel, groups, occurrences), sources[rel], staged_path
            )
        kill_switch = dict(data[KILL_SWITCH])
        kill_switch["entries"] = _rewrite_rows(
            kill_switch["entries"], KILL_SWITCH, groups, occurrences
        )
        staged_path = os.path.join(out_dir, KILL_SWITCH)
        os.makedirs(os.path.dirname(staged_path), exist_ok=True)
        _dump_like_source(kill_switch, sources[KILL_SWITCH], staged_path)

        try:
            staged_refs, failures = run_check(out_dir, verify_files=True)
        except (ManifestError, InventoryError) as e:
            print(f"pan213 repair: staged audit failed: {e}")
            shutil.rmtree(out_dir, ignore_errors=True)
            return 3
        if failures:
            for line in failures[:20]:
                print(f"pan213 repair: staged audit: {line}")
            print(
                "pan213 repair: staged audit failed; incomplete staged output discarded "
                f"({len(failures)} problem reference(s))"
            )
            shutil.rmtree(out_dir, ignore_errors=True)
            return 3

        counts = _counts(groups)
        drops = []
        for group in groups.values():
            if group["outcome"] != "dropped":
                continue
            drops.append(
                {
                    "old_wav_path": group["old_path"],
                    "expected_sha256": group["digest"],
                    "reason": group["reason"],
                    "rejected_candidates": group["rejected_candidates"],
                    "occurrences": [
                        {"manifest": ref.rel, "row_index": ref.index}
                        for ref in group["occurrences"]
                    ],
                }
            )
        report = {
            "ticket": "PAN-213",
            "generated_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "roots": {"repository": root, "corpus_dir": corpus_dir, "nas_dir": nas_dir},
            "month_shards_searched": nas_state["shards"],
            "counts": counts,
            "dropped": drops,
        }
        report_path = os.path.join(out_dir, RECOVERY_REPORT)
        os.makedirs(os.path.dirname(report_path), exist_ok=True)
        with open(report_path, "w", encoding="utf-8") as f:
            json.dump(report, f, indent=2)
            f.write("\n")
    except OSError as e:
        shutil.rmtree(out_dir, ignore_errors=True)
        print(f"pan213 repair: cannot write staged output: {e}")
        return 3

    distinct = counts["distinct_wavs"]
    references = counts["references"]
    print(
        f"pan213 repair: {distinct['referenced']} distinct WAV(s), "
        f"{references['total']} reference(s)"
    )
    print(
        f"  resolved from local corpus: {distinct['resolved_local']} / {references['resolved_local']}"
    )
    print(
        f"  recovered from NAS archive: {distinct['resolved_nas']} / {references['resolved_nas']}"
    )
    print(
        f"  verified at stable path:    {distinct['retained_stable']} / {references['retained_stable']}"
    )
    print(f"  dropped (see report):       {distinct['dropped']} / {references['dropped']}")
    if nas_state["shards"]:
        print(f"  month shards searched: {', '.join(nas_state['shards'])}")
    print(f"pan213 repair: staged tree: {out_dir}")
    print(
        f"pan213 repair: staged audit passed, {len(staged_refs)} reference(s) verified"
    )
    print(f"pan213 repair: drop report staged at {RECOVERY_REPORT}")
    return 0


def main(argv=None):
    if argv is None:
        argv = sys.argv[1:]
    parser = argparse.ArgumentParser(
        prog="pan213_manifests.py",
        description="PAN-213: guard survey/kill-switch WAV paths against the live capture buffer and stage a digest-verified repair",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    check = sub.add_parser("check", help="reject live-buffer and non-absolute WAV paths")
    check.add_argument("--root", required=True, help="repository root holding the six input files")
    check.add_argument(
        "--verify-files",
        action="store_true",
        help="also open every retained WAV and compare its SHA-256 with the stored digest",
    )
    check.set_defaults(func=cmd_check)

    repair = sub.add_parser("repair", help="stage a digest-verified rewrite of broken references")
    repair.add_argument("--root", required=True, help="repository root holding the six input files")
    repair.add_argument("--corpus-dir", required=True, help="local corpus directory (~/.pancetta/corpus)")
    repair.add_argument("--nas-dir", required=True, help="NAS archive root holding YYYY-MM month shards")
    repair.add_argument("--output-dir", required=True, help="fresh staging directory for the repaired tree")
    repair.set_defaults(func=cmd_repair)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
