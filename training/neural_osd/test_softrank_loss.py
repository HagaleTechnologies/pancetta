import json
import unittest
from pathlib import Path

from train_rank import apply_mrb_permutation, invert_permutation, soft_rank_loss


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
