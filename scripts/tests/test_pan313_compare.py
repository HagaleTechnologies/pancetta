"""Standard-library unit tests for scripts/pan313_compare.py.

Run: python3 -m unittest discover -s scripts/tests -p test_pan313_compare.py
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import pan313_compare  # noqa: E402

SCRIPT = os.path.join(os.path.dirname(__file__), "..", "pan313_compare.py")


def recording(path="a.wav", sha="h1", samples=180000, texts=("W9ABC K1ZZ FN20",)):
    arms = []
    for length in pan313_compare.LENGTHS:
        for budget in ("unlimited", "standard"):
            arms.append(
                {
                    "length": length,
                    "budget": budget,
                    "texts": sorted(texts),
                    "window_wall_ms": [100],
                    "s1_floor_ms": [300 if budget == "standard" else 400],
                    "fft_fallback": None
                    if budget == "standard"
                    else {"elapsed_ms": 5, "attempts": 3, "budget_skips": 0},
                    "stages": [],
                }
            )
    return {"path": path, "sha256": sha, "samples": samples, "arms": arms}


def results(recordings, threads=2, revision="x"):
    return {
        "schema": 1,
        "kind": "pan313-fft-fallback-results",
        "revision": revision,
        "config": {"protocol": "ft8", "standard_budget_ms": 250,
                   "repetitions": 1, "warmups": 1},
        "host": {"rayon_threads": threads},
        "recordings": recordings,
    }


class TempPair:
    def __init__(self, parent, candidate):
        self.dir = tempfile.TemporaryDirectory()
        self.ppath = os.path.join(self.dir.name, "parent.json")
        self.cpath = os.path.join(self.dir.name, "candidate.json")
        for path, data in ((self.ppath, parent), (self.cpath, candidate)):
            with open(path, "w") as f:
                json.dump(data, f)

    def run(self):
        proc = subprocess.run(
            [sys.executable, SCRIPT, self.ppath, self.cpath],
            capture_output=True,
            text=True,
        )
        return proc.returncode, proc.stdout + proc.stderr

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.dir.cleanup()


class CompareTests(unittest.TestCase):
    def test_matching_pair_passes(self):
        # Identical inputs except the required S1-floor improvement (a
        # byte-identical pair can never satisfy the strict-decrease rule).
        cand = recording()
        for arm in cand["arms"]:
            if arm["budget"] == "standard":
                arm["s1_floor_ms"] = [200]
        with TempPair(results([recording()]), results([cand])) as pair:
            code, out = pair.run()
        self.assertEqual(code, 0, out)

    def test_missing_input_fails(self):
        rec = recording()
        with TempPair(results([rec]), results([rec])) as pair:
            os.remove(pair.cpath)
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("missing", out)

    def test_empty_input_fails_not_zero_loss_success(self):
        rec = recording()
        with TempPair(results([rec]), results([rec])) as pair:
            open(pair.cpath, "w").close()
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("empty", out)

    def test_identity_mismatch_fails(self):
        with TempPair(
            results([recording(sha="h1")]), results([recording(sha="h2")])
        ) as pair:
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("sha256 mismatch", out)

    def test_sample_length_mismatch_fails(self):
        with TempPair(
            results([recording(samples=180000)]),
            results([recording(samples=162000)]),
        ) as pair:
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("sample-count mismatch", out)

    def test_thread_identity_mismatch_fails(self):
        with TempPair(
            results([recording()], threads=2), results([recording()], threads=4)
        ) as pair:
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("thread-width mismatch", out)

    def test_unlimited_message_set_loss_fails(self):
        parent_rec = recording(texts=("A", "B"))
        cand_rec = recording(texts=("A",))
        with TempPair(results([parent_rec]), results([cand_rec])) as pair:
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("unlimited message set changed", out)
        self.assertIn("lost ['B']", out)

    def test_tp_loss_fails(self):
        # Standard arm loses the reference message the parent decoded.
        parent_rec = recording(texts=("A", "B"))
        cand_rec = recording(texts=("A", "B"))
        for arm in cand_rec["arms"]:
            if arm["budget"] == "standard":
                arm["texts"] = ["A"]
        with TempPair(results([parent_rec]), results([cand_rec])) as pair:
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("reference recall dropped", out)

    def test_unchanged_s1_floor_fails(self):
        rec = recording()
        slow = recording()
        for arm in slow["arms"]:
            if arm["budget"] == "standard":
                arm["s1_floor_ms"] = [300]
        with TempPair(results([slow]), results([rec])) as pair:
            # both 300 -> unchanged
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("did not improve", out)

    def test_increased_s1_floor_fails(self):
        cand = recording()
        for arm in cand["arms"]:
            if arm["budget"] == "standard":
                arm["s1_floor_ms"] = [500]
        with TempPair(results([recording()]), results([cand])) as pair:
            code, out = pair.run()
        self.assertNotEqual(code, 0)
        self.assertIn("did not improve", out)

    def test_improved_s1_floor_with_equal_sets_passes(self):
        cand = recording()
        for arm in cand["arms"]:
            if arm["budget"] == "standard":
                arm["s1_floor_ms"] = [100]
        with TempPair(results([recording()]), results([cand])) as pair:
            code, out = pair.run()
        self.assertEqual(code, 0, out)

    def test_faster_candidate_standard_recall_gain_passes(self):
        parent_rec = recording(texts=("A", "B"))
        cand_rec = recording(texts=("A", "B"))
        for arm in parent_rec["arms"]:
            if arm["budget"] == "standard":
                arm["texts"] = ["A"]
        cand = recording(texts=("A", "B"))
        for arm in cand["arms"]:
            if arm["budget"] == "standard":
                arm["s1_floor_ms"] = [100]
        cand_rec = cand
        for arm in cand_rec["arms"]:
            if arm["budget"] == "standard":
                arm["s1_floor_ms"] = [100]
        with TempPair(results([parent_rec]), results([cand_rec])) as pair:
            code, out = pair.run()
        self.assertEqual(code, 0, out)


if __name__ == "__main__":
    unittest.main()
