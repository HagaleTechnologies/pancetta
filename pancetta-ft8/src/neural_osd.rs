//! Neural OSD: CNN-guided bit-flip ordering for OSD decoding.
//!
//! Uses a trained DIA (Decoding Information Aggregation) model to predict
//! which LDPC info bits are most likely wrong after BP failure. The predicted
//! probabilities replace |LLR|-based ordering in OSD, reducing trials from
//! ~125K to ~200.

// rationale: CNN forward-pass loops index conv/linear weight and activation
// tensors by position; the index is load-bearing for the tensor layout.
#![allow(clippy::needless_range_loop)]

use crate::neural_osd_weights::{
    conv1_bias, conv1_weight, conv2_bias, conv2_weight, conv3_bias, conv3_weight, linear_bias,
    linear_weight,
};

const N_CODEWORD: usize = 174;
const K_INFO: usize = 91;
const BP_ITERS: usize = 25;
const CONV1_OUT: usize = 32;
const CONV2_OUT: usize = 16;

/// Predict which info bits are most likely wrong after BP failure.
///
/// Input: LLR trajectory from 25 BP iterations (trajectory[iter][bit]).
/// Output: 91 probabilities — higher means more likely wrong.
pub fn predict_error_bits(trajectory: &[[f32; N_CODEWORD]; BP_ITERS]) -> [f32; K_INFO] {
    // Borrow the weight slices once at the top so the inner loops avoid
    // re-resolving the OnceLock on every access. The OnceLock load itself
    // is cheap, but the inner loop runs hundreds of millions of times per
    // failed-BP frame.
    let conv1_w = conv1_weight();
    let conv1_b = conv1_bias();
    let conv2_w = conv2_weight();
    let conv2_b = conv2_bias();
    let conv3_w = conv3_weight();
    let conv3_b = conv3_bias();
    let linear_w = linear_weight();
    let linear_b = linear_bias();

    // Layer 1: Conv1D(25→32, kernel=3, padding=1) + ReLU
    let mut h1 = [[0.0f32; N_CODEWORD]; CONV1_OUT];
    for out_ch in 0..CONV1_OUT {
        for pos in 0..N_CODEWORD {
            let mut sum = conv1_b[out_ch];
            for in_ch in 0..BP_ITERS {
                for k in 0..3usize {
                    let p = pos as isize + k as isize - 1;
                    if p >= 0 && (p as usize) < N_CODEWORD {
                        sum += trajectory[in_ch][p as usize]
                            * conv1_w[out_ch * BP_ITERS * 3 + in_ch * 3 + k];
                    }
                }
            }
            h1[out_ch][pos] = sum.max(0.0); // ReLU
        }
    }

    // Layer 2: Conv1D(32→16, kernel=3, padding=1) + ReLU
    let mut h2 = [[0.0f32; N_CODEWORD]; CONV2_OUT];
    for out_ch in 0..CONV2_OUT {
        for pos in 0..N_CODEWORD {
            let mut sum = conv2_b[out_ch];
            for in_ch in 0..CONV1_OUT {
                for k in 0..3usize {
                    let p = pos as isize + k as isize - 1;
                    if p >= 0 && (p as usize) < N_CODEWORD {
                        sum +=
                            h1[in_ch][p as usize] * conv2_w[out_ch * CONV1_OUT * 3 + in_ch * 3 + k];
                    }
                }
            }
            h2[out_ch][pos] = sum.max(0.0); // ReLU
        }
    }

    // Layer 3: Conv1D(16→1, kernel=1) → squeeze
    let mut h3 = [0.0f32; N_CODEWORD];
    for pos in 0..N_CODEWORD {
        let mut sum = conv3_b[0];
        for in_ch in 0..CONV2_OUT {
            sum += h2[in_ch][pos] * conv3_w[in_ch];
        }
        h3[pos] = sum;
    }

    // Layer 4: Linear(174→91) + sigmoid
    let mut output = [0.0f32; K_INFO];
    for i in 0..K_INFO {
        let mut sum = linear_b[i];
        for j in 0..N_CODEWORD {
            sum += h3[j] * linear_w[i * N_CODEWORD + j];
        }
        output[i] = 1.0 / (1.0 + (-sum).exp()); // sigmoid
    }

    output
}

#[cfg(test)]
// rationale: test-only builder structs assigned field-by-field after
// default(); sequential assignment reads clearer than a struct-update splat.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_predict_error_bits_runs() {
        let mut trajectory = [[0.0f32; N_CODEWORD]; BP_ITERS];
        for iter in 0..BP_ITERS {
            for bit in 0..N_CODEWORD {
                trajectory[iter][bit] = (iter as f32 - 12.0) * 0.1;
            }
        }
        let probs = predict_error_bits(&trajectory);

        assert_eq!(probs.len(), K_INFO);
        for &p in &probs {
            assert!((0.0..=1.0).contains(&p), "Probability {} out of range", p);
        }
    }

    #[test]
    fn test_predict_deterministic() {
        let trajectory = [[1.0f32; N_CODEWORD]; BP_ITERS];
        let p1 = predict_error_bits(&trajectory);
        let p2 = predict_error_bits(&trajectory);
        assert_eq!(p1, p2, "Forward pass should be deterministic");
    }
}
