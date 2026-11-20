"""Standard-library unit tests for scripts/pan213_manifests.py (PAN-213).

Run: python3 -m unittest discover -s scripts/tests -p test_pan213_manifests.py
"""

import contextlib
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import wave
from unittest import mock

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import pan213_manifests

SCRIPT = os.path.join(os.path.dirname(__file__), "..", "pan213_manifests.py")
SURVEY = "research/corpus/surveys/2026-05-30"
ALL_WAVS = f"{SURVEY}/all_wavs_scored.json"
TODAY = f"{SURVEY}/today_top100.entries.json"
KS = "research/corpus/curated/ft8/hb104_kill_switch.json"
REPORT = "research/corpus/PAN-213-recovery.json"


def wav_bytes(seed):
    payload = bytes([seed % 256]) * 64
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(12000)
        w.writeframes(payload)
    return buf.getvalue()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def write_file(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def load_json(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def row(path, digest, **extra):
    entry = {
        "wav_path": path,
        "wav_sha256": digest,
        "interest_score": 1.5,
        "slot_hhmmss": "000000",
    }
    entry.update(extra)
    return entry


def live_row(name, digest, **extra):
    return row(f"~/.pancetta/recordings/{name}", digest, **extra)


def ks_row(name, digest):
    return {
        "wav_path": f"~/.pancetta/recordings/{name}",
        "wav_sha256": digest,
        "n_pairs": 1,
        "pairs": [{"miss_message": "<...> WZ8DX EM79", "miss_freq_hz": 1321.875}],
    }


def write_json(path, obj, indent=2):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(obj, f, indent=indent)


def build_root(
    base, all_rows=(), existing_rows=(), today_rows=(), ks_entries=(), indent=2
):
    root = os.path.join(base, "repo")
    write_json(os.path.join(root, ALL_WAVS), list(all_rows), indent)
    write_json(
        os.path.join(root, f"{SURVEY}/existing_top100.entries.json"),
        list(existing_rows),
        indent,
    )
    write_json(os.path.join(root, TODAY), list(today_rows), indent)
    write_json(
        os.path.join(root, f"{SURVEY}/per_wav.json"),
        [{"wav_filename": "ft8_20260530_152356.wav", "file_bytes": 303404}],
        indent,
    )
    write_json(
        os.path.join(root, f"{SURVEY}/summary.json"), {"sampled_wav_count": 1}, indent
    )
    write_json(
        os.path.join(root, KS),
        {
            "schema_version": 1,
            "label": "hb104_kill_switch",
            "source": "test",
            "freq_gate_hz": 6.25,
            "time_gate_sec": 2.0,
            "entries": list(ks_entries),
        },
        indent=1,
    )
    return root


def run_cmd(argv):
    proc = subprocess.run(
        [sys.executable, SCRIPT, *argv], capture_output=True, text=True
    )
    return proc.returncode, proc.stdout + proc.stderr


def run_check(root, verify=False):
    argv = ["check", "--root", root]
    if verify:
        argv.append("--verify-files")
    return run_cmd(argv)


def run_repair(root, corpus, nas, out):
    return run_cmd(
        [
            "repair",
            "--root",
            root,
            "--corpus-dir",
            corpus,
            "--nas-dir",
            nas,
            "--output-dir",
            out,
        ]
    )


def run_repair_inprocess(root, corpus, nas, out):
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        code = pan213_manifests.main(
            [
                "repair",
                "--root",
                root,
                "--corpus-dir",
                corpus,
                "--nas-dir",
                nas,
                "--output-dir",
                out,
            ]
        )
    return code, buf.getvalue()


class Pan213TestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = self.tmp.name


class CheckRejectsLiveBufferPathsTests(Pan213TestCase):
    def test_check_rejects_live_buffer_paths(self):
        digest = sha256(b"seed")
        bad = [
            "~/.pancetta/recordings/tilde.wav",
            "/Users/other/.pancetta/recordings/other-home.wav",
            "~\\.pancetta\\recordings\\windows.wav",
            "~/.pancetta/corpus/../recordings/traversal.wav",
        ]
        good = [
            "/station/.pancetta/corpus/keep-abs.wav",
            "/station/.pancetta/recordings-old/keep-prefix.wav",
            "~/.pancetta/corpus/keep-tilde.wav",
        ]
        root = build_root(
            self.tmp.name,
            all_rows=[row(p, digest) for p in bad + good],
            today_rows=[row("corpus/relative.wav", digest)],
        )
        code, out = run_check(root)
        self.assertNotEqual(code, 0, out)
        for i, path in enumerate(bad):
            self.assertIn(f"{ALL_WAVS} row {i}: {path}: live-buffer", out)
        self.assertIn(f"{TODAY} row 0: corpus/relative.wav: path is not absolute", out)
        self.assertEqual(out.count("live-buffer path"), len(bad))
        self.assertNotIn("keep-abs.wav", out)
        self.assertNotIn("keep-prefix.wav", out)
        self.assertNotIn("keep-tilde.wav", out)

        clean = build_root(
            os.path.join(self.tmp.name, "clean"),
            all_rows=[row(p, digest) for p in good],
        )
        code, out = run_check(clean)
        self.assertEqual(code, 0, out)
        self.assertIn("passed", out)

        with self.subTest("missing input file"):
            broken = build_root(os.path.join(self.tmp.name, "missing"))
            os.remove(os.path.join(broken, f"{SURVEY}/summary.json"))
            code, out = run_check(broken)
            self.assertNotEqual(code, 0)
            self.assertIn("missing required input", out)
            self.assertIn("summary.json", out)

        with self.subTest("invalid json"):
            broken = build_root(os.path.join(self.tmp.name, "badjson"))
            with open(os.path.join(broken, f"{SURVEY}/per_wav.json"), "w") as f:
                f.write("{")
            code, out = run_check(broken)
            self.assertNotEqual(code, 0)
            self.assertIn("invalid JSON", out)
            self.assertIn("per_wav.json", out)

        with self.subTest("malformed wav row"):
            broken = build_root(
                os.path.join(self.tmp.name, "badrow"),
                all_rows=[{"wav_path": 7, "wav_sha256": digest}],
            )
            code, out = run_check(broken)
            self.assertNotEqual(code, 0)
            self.assertIn("wav_path must be a non-empty string", out)
            self.assertIn("row 0", out)

        with self.subTest("kill switch entries missing"):
            broken = build_root(os.path.join(self.tmp.name, "badks"))
            with open(os.path.join(broken, KS), "w") as f:
                json.dump({"schema_version": 1, "label": "hb104_kill_switch"}, f)
            code, out = run_check(broken)
            self.assertNotEqual(code, 0)
            self.assertIn('"entries" is not an array', out)


class CheckVerifyFilesTests(Pan213TestCase):
    def test_check_verify_files_opens_and_hashes_every_retained_recording(self):
        store = os.path.join(self.tmp.name, "store")
        ok1 = wav_bytes(1)
        ok2 = wav_bytes(2)
        changed = wav_bytes(3)
        write_file(os.path.join(store, "ok1.wav"), ok1)
        write_file(os.path.join(store, "ok2.wav"), ok2)
        write_file(os.path.join(store, "bad.wav"), changed)
        os.symlink(
            "/fake/.pancetta/recordings/gone.wav", os.path.join(store, "link.wav")
        )
        rows = [
            row(f"{store}/ok1.wav", sha256(ok1)),
            row(f"{store}/ok1.wav", sha256(ok1)),
            row(f"{store}/ok2.wav", sha256(ok2)),
            row(f"{store}/bad.wav", sha256(ok1)),
            row(f"{store}/link.wav", sha256(ok1)),
            row(f"{store}/missing.wav", sha256(ok1)),
        ]
        root = build_root(self.tmp.name, all_rows=rows)

        code, out = run_check(root)
        self.assertEqual(code, 0, out)

        code, out = run_check(root, verify=True)
        self.assertNotEqual(code, 0, out)
        self.assertIn(f"{ALL_WAVS} row 3: {store}/bad.wav: sha256 mismatch", out)
        self.assertIn("resolves into the live buffer", out)
        self.assertIn(f"{ALL_WAVS} row 5: {store}/missing.wav: file not found", out)
        self.assertNotIn("ok1.wav", out)
        self.assertNotIn("ok2.wav", out)

        clean = build_root(
            os.path.join(self.tmp.name, "clean"),
            all_rows=rows[:3],
        )
        code, out = run_check(clean, verify=True)
        self.assertEqual(code, 0, out)


class RepairPrefersLocalTests(Pan213TestCase):
    def test_repair_prefers_digest_matched_local_copy(self):
        data = wav_bytes(11)
        digest = sha256(data)
        corpus = os.path.join(self.tmp.name, "corpus")
        nas = os.path.join(self.tmp.name, "nas")
        write_file(os.path.join(corpus, "a.wav"), data)
        write_file(os.path.join(nas, "2026-05", "a.wav"), data)
        root = build_root(self.tmp.name, all_rows=[live_row("a.wav", digest)])
        out_dir = os.path.join(self.tmp.name, "stage")

        code, out = run_repair(root, corpus, nas, out_dir)
        self.assertEqual(code, 0, out)

        staged = load_json(os.path.join(out_dir, ALL_WAVS))
        expected = os.path.join(corpus, "a.wav")
        self.assertEqual(staged[0]["wav_path"], expected)
        with open(staged[0]["wav_path"], "rb") as f:
            self.assertEqual(f.read(), data)

        report = load_json(os.path.join(out_dir, REPORT))
        self.assertEqual(report["counts"]["distinct_wavs"]["resolved_local"], 1)
        self.assertEqual(report["counts"]["references"]["resolved_local"], 1)
        self.assertEqual(report["counts"]["distinct_wavs"]["dropped"], 0)
        self.assertEqual(report["month_shards_searched"], [])
        self.assertEqual(report["dropped"], [])

        with open(os.path.join(root, f"{SURVEY}/per_wav.json"), "rb") as f:
            source_per_wav = f.read()
        with open(os.path.join(out_dir, f"{SURVEY}/per_wav.json"), "rb") as f:
            self.assertEqual(f.read(), source_per_wav)


class RepairNasShardSearchTests(Pan213TestCase):
    def test_repair_searches_all_nas_shards_for_matching_bytes(self):
        true_bytes = wav_bytes(21)
        digest = sha256(true_bytes)
        corpus = os.path.join(self.tmp.name, "corpus")
        nas = os.path.join(self.tmp.name, "nas")
        write_file(os.path.join(corpus, "a.wav"), wav_bytes(22))
        write_file(os.path.join(nas, "2026-03", "a.wav"), wav_bytes(23))
        write_file(os.path.join(nas, "2026-05", "inner", "a.wav"), true_bytes)
        write_file(os.path.join(nas, "notes", "a.wav"), wav_bytes(24))
        root = build_root(self.tmp.name, all_rows=[live_row("a.wav", digest)])
        out_dir = os.path.join(self.tmp.name, "stage")

        code, out = run_repair(root, corpus, nas, out_dir)
        self.assertEqual(code, 0, out)

        staged = load_json(os.path.join(out_dir, ALL_WAVS))
        expected = os.path.abspath(os.path.join(nas, "2026-05", "inner", "a.wav"))
        self.assertEqual(staged[0]["wav_path"], expected)
        with open(staged[0]["wav_path"], "rb") as f:
            self.assertEqual(f.read(), true_bytes)

        report = load_json(os.path.join(out_dir, REPORT))
        self.assertEqual(report["counts"]["distinct_wavs"]["resolved_nas"], 1)
        self.assertEqual(report["counts"]["distinct_wavs"]["dropped"], 0)
        self.assertEqual(report["month_shards_searched"], ["2026-03", "2026-05"])
        self.assertNotIn("notes", json.dumps(report["month_shards_searched"]))


class RepairDropAccountingTests(Pan213TestCase):
    def test_repair_accounts_for_drops_and_preserves_other_data(self):
        keep = wav_bytes(31)
        stable = wav_bytes(32)
        mm_expected = wav_bytes(33)
        mm_corpus = wav_bytes(34)
        mm_nas = wav_bytes(35)
        dup_expected = wav_bytes(36)
        corpus = os.path.join(self.tmp.name, "corpus")
        nas = os.path.join(self.tmp.name, "nas")
        store = os.path.join(self.tmp.name, "store")
        write_file(os.path.join(corpus, "keep.wav"), keep)
        write_file(os.path.join(corpus, "mm.wav"), mm_corpus)
        write_file(os.path.join(nas, "2026-05", "mm.wav"), mm_nas)
        write_file(os.path.join(store, "stable.wav"), stable)
        all_rows = [
            live_row("dup.wav", sha256(dup_expected)),
            live_row("keep.wav", sha256(keep), slot_hhmmss="224413"),
            row(f"{store}/stable.wav", sha256(stable), error=None),
            live_row("mm.wav", sha256(mm_expected)),
        ]
        today_rows = [
            {"note": "path-free row"},
            live_row("dup.wav", sha256(dup_expected)),
        ]
        ks_entries = [
            ks_row("dup.wav", sha256(dup_expected)),
            ks_row("keep.wav", sha256(keep)),
        ]
        root = build_root(
            self.tmp.name,
            all_rows=all_rows,
            today_rows=today_rows,
            ks_entries=ks_entries,
        )
        out_dir = os.path.join(self.tmp.name, "stage")

        code, out = run_repair(root, corpus, nas, out_dir)
        self.assertEqual(code, 0, out)

        staged_all = load_json(os.path.join(out_dir, ALL_WAVS))
        self.assertEqual(len(staged_all), 2)
        with open(os.path.join(out_dir, ALL_WAVS)) as f:
            staged_text = f.read()
        self.assertTrue(staged_text.startswith('[\n  {\n    "wav_path"'))
        self.assertFalse(staged_text.endswith("\n"))
        with open(os.path.join(out_dir, KS)) as f:
            staged_ks_text = f.read()
        self.assertTrue(staged_ks_text.startswith('{\n "schema_version": 1,\n'))
        self.assertEqual(staged_all[0]["wav_path"], os.path.join(corpus, "keep.wav"))
        staged_keep = dict(staged_all[0])
        original_keep = dict(all_rows[1])
        staged_keep.pop("wav_path")
        original_keep.pop("wav_path")
        self.assertEqual(staged_keep, original_keep)
        self.assertEqual(staged_all[1]["wav_path"], f"{store}/stable.wav")
        self.assertEqual(staged_all[1]["error"], None)

        staged_today = load_json(os.path.join(out_dir, TODAY))
        self.assertEqual(staged_today, [{"note": "path-free row"}])

        staged_ks = load_json(os.path.join(out_dir, KS))
        for key in (
            "schema_version",
            "label",
            "source",
            "freq_gate_hz",
            "time_gate_sec",
        ):
            self.assertIn(key, staged_ks)
        self.assertEqual(len(staged_ks["entries"]), 1)
        self.assertEqual(
            staged_ks["entries"][0]["wav_path"], os.path.join(corpus, "keep.wav")
        )
        self.assertEqual(staged_ks["entries"][0]["pairs"], ks_entries[1]["pairs"])

        for name in ("per_wav.json", "summary.json"):
            with open(os.path.join(root, SURVEY, name), "rb") as f:
                source_bytes = f.read()
            with open(os.path.join(out_dir, SURVEY, name), "rb") as f:
                self.assertEqual(f.read(), source_bytes)

        report = load_json(os.path.join(out_dir, REPORT))
        drops = {d["old_wav_path"]: d for d in report["dropped"]}
        self.assertEqual(
            sorted(drops),
            sorted(
                [
                    "~/.pancetta/recordings/dup.wav",
                    "~/.pancetta/recordings/mm.wav",
                ]
            ),
        )
        self.assertEqual(
            drops["~/.pancetta/recordings/dup.wav"]["reason"], "no_candidate"
        )
        self.assertEqual(
            drops["~/.pancetta/recordings/dup.wav"]["expected_sha256"],
            sha256(dup_expected),
        )
        self.assertEqual(
            drops["~/.pancetta/recordings/dup.wav"]["occurrences"],
            [
                {"manifest": ALL_WAVS, "row_index": 0},
                {"manifest": TODAY, "row_index": 1},
                {"manifest": KS, "row_index": 0},
            ],
        )
        self.assertEqual(
            drops["~/.pancetta/recordings/mm.wav"]["reason"], "digest_mismatch"
        )
        rejected = {
            c["path"]: c["sha256"]
            for c in drops["~/.pancetta/recordings/mm.wav"]["rejected_candidates"]
        }
        self.assertEqual(rejected[os.path.join(corpus, "mm.wav")], sha256(mm_corpus))
        self.assertEqual(
            rejected[os.path.join(nas, "2026-05", "mm.wav")], sha256(mm_nas)
        )
        self.assertEqual(
            drops["~/.pancetta/recordings/mm.wav"]["occurrences"],
            [{"manifest": ALL_WAVS, "row_index": 3}],
        )

        self.assertEqual(
            report["counts"]["distinct_wavs"],
            {
                "referenced": 4,
                "resolved_local": 1,
                "resolved_nas": 0,
                "retained_stable": 1,
                "dropped": 2,
            },
        )
        self.assertEqual(
            report["counts"]["references"],
            {
                "total": 7,
                "resolved_local": 2,
                "resolved_nas": 0,
                "retained_stable": 1,
                "dropped": 4,
            },
        )

        code, out = run_check(out_dir, verify=True)
        self.assertEqual(code, 0, out)


class RepairInventoryFailureTests(Pan213TestCase):
    def test_repair_rejects_candidates_outside_supplied_stores(self):
        data = wav_bytes(43)
        corpus = os.path.join(self.tmp.name, "corpus")
        nas = os.path.join(self.tmp.name, "nas")
        external = os.path.join(self.tmp.name, "outside", "a.wav")
        write_file(external, data)
        os.makedirs(corpus)
        os.makedirs(os.path.join(nas, "2026-05"))
        root = build_root(self.tmp.name, all_rows=[live_row("a.wav", sha256(data))])

        os.symlink(external, os.path.join(corpus, "a.wav"))
        code, output = run_repair_inprocess(
            root, corpus, nas, os.path.join(self.tmp.name, "stage-local")
        )
        self.assertNotEqual(code, 0, output)
        self.assertIn("outside supplied store", output)

        os.unlink(os.path.join(corpus, "a.wav"))
        os.symlink(external, os.path.join(nas, "2026-05", "a.wav"))
        code, output = run_repair_inprocess(
            root, corpus, nas, os.path.join(self.tmp.name, "stage-nas")
        )
        self.assertNotEqual(code, 0, output)
        self.assertIn("outside supplied store", output)

    def test_repair_fails_on_unavailable_store_or_inventory_error(self):
        a = wav_bytes(41)
        gone = wav_bytes(42)
        corpus = os.path.join(self.tmp.name, "corpus")
        nas = os.path.join(self.tmp.name, "nas")
        write_file(os.path.join(corpus, "a.wav"), a)
        write_file(os.path.join(nas, "2026-05", "a.wav"), a)

        root = build_root(
            self.tmp.name,
            all_rows=[live_row("a.wav", sha256(a)), live_row("gone.wav", sha256(gone))],
        )
        with open(os.path.join(root, ALL_WAVS), "rb") as f:
            source_before = f.read()

        out_dir = os.path.join(self.tmp.name, "stage-a")
        code, out = run_repair_inprocess(
            root, os.path.join(self.tmp.name, "absent"), nas, out_dir
        )
        self.assertNotEqual(code, 0, out)
        self.assertIn("corpus not accessible", out)
        self.assertFalse(os.path.exists(out_dir))

        out_dir = os.path.join(self.tmp.name, "stage-b")
        code, out = run_repair_inprocess(
            root, corpus, os.path.join(self.tmp.name, "absent"), out_dir
        )
        self.assertNotEqual(code, 0, out)
        self.assertIn("NAS archive", out)
        self.assertFalse(os.path.exists(out_dir))

        empty_nas = os.path.join(self.tmp.name, "empty-nas")
        os.makedirs(empty_nas)
        with open(os.path.join(empty_nas, "readme.txt"), "w") as f:
            f.write("not a shard")
        out_dir = os.path.join(self.tmp.name, "stage-c")
        code, out = run_repair_inprocess(root, corpus, empty_nas, out_dir)
        self.assertNotEqual(code, 0, out)
        self.assertIn("month shards", out)
        self.assertFalse(os.path.exists(out_dir))

        out_dir = os.path.join(self.tmp.name, "stage-d")
        with mock.patch.object(
            pan213_manifests,
            "index_nas_shards",
            side_effect=pan213_manifests.InventoryError("traversal boom"),
        ):
            code, out = run_repair_inprocess(root, corpus, nas, out_dir)
        self.assertNotEqual(code, 0, out)
        self.assertIn("traversal boom", out)
        self.assertFalse(os.path.exists(out_dir))

        out_dir = os.path.join(self.tmp.name, "stage-e")
        with mock.patch.object(
            pan213_manifests,
            "file_digest",
            side_effect=pan213_manifests.InventoryError("read boom"),
        ):
            code, out = run_repair_inprocess(root, corpus, nas, out_dir)
        self.assertNotEqual(code, 0, out)
        self.assertIn("read boom", out)
        self.assertFalse(os.path.exists(out_dir))

        with open(os.path.join(root, ALL_WAVS), "rb") as f:
            self.assertEqual(f.read(), source_before)

        with self.subTest("contradictory digests for one path"):
            bad_root = build_root(
                os.path.join(self.tmp.name, "contradiction"),
                all_rows=[
                    live_row("a.wav", sha256(a)),
                    live_row("a.wav", sha256(gone)),
                ],
            )
            out_dir = os.path.join(self.tmp.name, "stage-f")
            code, out = run_repair_inprocess(bad_root, corpus, nas, out_dir)
            self.assertNotEqual(code, 0, out)
            self.assertIn("contradictory wav_sha256", out)
            self.assertFalse(os.path.exists(out_dir))

        with self.subTest("malformed digest"):
            bad_root = build_root(
                os.path.join(self.tmp.name, "baddigest"),
                all_rows=[live_row("a.wav", "not-a-digest")],
            )
            out_dir = os.path.join(self.tmp.name, "stage-g")
            code, out = run_repair_inprocess(bad_root, corpus, nas, out_dir)
            self.assertNotEqual(code, 0, out)
            self.assertIn("not a 64-hex digest", out)
            self.assertFalse(os.path.exists(out_dir))


if __name__ == "__main__":
    unittest.main()
