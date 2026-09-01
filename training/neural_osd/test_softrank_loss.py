import json
import shutil
import tempfile
import unittest
from pathlib import Path

from train_rank import (
    apply_mrb_permutation,
    invert_permutation,
    load_corpus,
    soft_rank_loss,
    soft_rank_loss_tensor,
)


class SoftRankLossTests(unittest.TestCase):
    def test_moving_true_error_up_strictly_decreases_loss(self):
        labels = [[0.0, 1.0, 0.0]]
        low = soft_rank_loss([[2.0, -1.0, 0.0]], labels, tau=0.2)
        high = soft_rank_loss([[0.0, 2.0, -1.0]], labels, tau=0.2)
        self.assertLess(high, low)

    def test_constant_score_shift_is_invariant(self):
        scores = [[0.1, 0.7, -0.2, 1.1]]
        labels = [[1.0, 0.0, 1.0, 0.0]]
        self.assertAlmostEqual(
            soft_rank_loss(scores, labels, tau=0.3),
            soft_rank_loss([[v + 123.0 for v in scores[0]]], labels, tau=0.3),
            places=5,
        )

    def test_small_temperature_approaches_hard_expected_rank(self):
        scores = [[3.0, 1.0, 2.0]]
        labels = [[0.0, 1.0, 1.0]]
        # The design's all-j formulation is half-offset: [0.5, 2.5, 1.5].
        self.assertAlmostEqual(soft_rank_loss(scores, labels, tau=1e-3), 2.0, places=3)

    def test_tensor_loss_matches_reference_direction_and_value(self):
        try:
            import torch
        except ImportError:
            self.skipTest("PyTorch is not installed")
        scores = [[3.0, 1.0, 2.0]]
        labels = [[0.0, 1.0, 1.0]]
        expected = soft_rank_loss(scores, labels, tau=0.25)
        actual = soft_rank_loss_tensor(
            torch.tensor(scores), torch.tensor(labels), tau=0.25
        ).item()
        self.assertAlmostEqual(actual, expected, places=6)

    def test_loader_preserves_natural_systematic_output_indices(self):
        try:
            import numpy  # noqa: F401
        except ImportError:
            self.skipTest("NumPy is not installed")
        codeword = [0] * 174
        codeword[0] = 1
        row = {
            "schema_version": 2,
            "osd_recovered": True,
            "mrb_perm": list(range(173, -1, -1)),
            "osd_codeword": codeword,
            "final_llrs": [1.0] * 174,
            "trajectory_flat": [0.0] * (25 * 174),
            "syndrome_counts": [0] * 174,
            "split_key": "natural-index-contract",
        }
        with tempfile.TemporaryDirectory() as directory:
            corpus = Path(directory) / "capture.jsonl"
            corpus.write_text(json.dumps(row) + "\n")
            splits, cache_dir = load_corpus(corpus)
        try:
            labels = next(values[1] for values in splits.values() if len(values[1]))
            self.assertEqual(labels[0, 0], 1.0)
            self.assertEqual(labels[0].sum(), 1.0)
        finally:
            shutil.rmtree(cache_dir, ignore_errors=True)

    def test_loader_rejects_unsupported_schema_versions(self):
        try:
            import numpy  # noqa: F401
        except ImportError:
            self.skipTest("NumPy is not installed")
        # A schema-v1 row must abort the load rather than be skipped: silently
        # dropping legacy captures trains on an unnoticed subset of the corpus.
        row = {
            "schema_version": 1,
            "osd_recovered": True,
            "mrb_perm": list(range(174)),
            "osd_codeword": [0] * 174,
            "final_llrs": [1.0] * 174,
            "trajectory_flat": [0.0] * (25 * 174),
            "syndrome_counts": [0] * 174,
            "split_key": "legacy-row",
        }
        with tempfile.TemporaryDirectory() as directory:
            corpus = Path(directory) / "capture.jsonl"
            corpus.write_text(json.dumps(row) + "\n")
            with self.assertRaises(ValueError) as caught:
                load_corpus(corpus)
        self.assertIn("schema_version", str(caught.exception))

    def test_loader_tolerates_blank_trailing_lines(self):
        try:
            import numpy  # noqa: F401
        except ImportError:
            self.skipTest("NumPy is not installed")
        codeword = [0] * 174
        codeword[0] = 1
        row = {
            "schema_version": 2,
            "osd_recovered": True,
            "mrb_perm": list(range(174)),
            "osd_codeword": codeword,
            "final_llrs": [1.0] * 174,
            "trajectory_flat": [0.0] * (25 * 174),
            "syndrome_counts": [0] * 174,
            "split_key": "blank-line-tolerance",
        }
        with tempfile.TemporaryDirectory() as directory:
            corpus = Path(directory) / "capture.jsonl"
            corpus.write_text(json.dumps(row) + "\n\n")
            splits, cache_dir = load_corpus(corpus)
        try:
            self.assertTrue(any(len(values[1]) for values in splits.values()))
        finally:
            shutil.rmtree(cache_dir, ignore_errors=True)

    def test_permutation_then_inverse_is_identity(self):
        values = list(range(174))
        perm = list(range(173, -1, -1))
        permuted = apply_mrb_permutation(values, perm)
        inverse = invert_permutation(perm)
        self.assertEqual(apply_mrb_permutation(permuted, inverse), values)

    def test_python_contract_matches_committed_schema(self):
        path = Path(__file__).resolve().parents[2] / "pancetta-ft8/assets/neural_osd_weights.provenance.json"
        contract = json.loads(path.read_text())
        self.assertEqual(contract["input_channels"], 26)
        self.assertEqual(contract["syndrome_normalization_divisor"], 3)
        self.assertEqual(contract["total_len"], sum(t["length"] for t in contract["tensors"]))
        self.assertEqual([t["name"] for t in contract["tensors"]], [
            "conv1.weight", "conv1.bias", "conv2.weight", "conv2.bias",
            "conv3.weight", "conv3.bias", "linear.weight", "linear.bias",
        ])


if __name__ == "__main__":
    unittest.main()
