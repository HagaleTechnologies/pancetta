//! Neural OSD: CNN-guided bit-flip ordering for OSD decoding.
//!
//! Uses a trained DIA (Decoding Information Aggregation) model to predict
//! which LDPC info bits are most likely wrong after BP failure. The predicted
//! probabilities replace |LLR|-based ordering in OSD, reducing trials from
//! ~125K to ~200.

use crate::neural_osd_weights::*;

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
    // Layer 1: Conv1D(25→32, kernel=3, padding=1) + ReLU
    let mut h1 = [[0.0f32; N_CODEWORD]; CONV1_OUT];
    for out_ch in 0..CONV1_OUT {
        for pos in 0..N_CODEWORD {
            let mut sum = CONV1_BIAS[out_ch];
            for in_ch in 0..BP_ITERS {
                for k in 0..3usize {
                    let p = pos as isize + k as isize - 1;
                    if p >= 0 && (p as usize) < N_CODEWORD {
                        sum += trajectory[in_ch][p as usize]
                            * CONV1_WEIGHT[out_ch * BP_ITERS * 3 + in_ch * 3 + k];
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
            let mut sum = CONV2_BIAS[out_ch];
            for in_ch in 0..CONV1_OUT {
                for k in 0..3usize {
                    let p = pos as isize + k as isize - 1;
                    if p >= 0 && (p as usize) < N_CODEWORD {
                        sum += h1[in_ch][p as usize]
                            * CONV2_WEIGHT[out_ch * CONV1_OUT * 3 + in_ch * 3 + k];
                    }
                }
            }
            h2[out_ch][pos] = sum.max(0.0); // ReLU
        }
    }

    // Layer 3: Conv1D(16→1, kernel=1) → squeeze
    let mut h3 = [0.0f32; N_CODEWORD];
    for pos in 0..N_CODEWORD {
        let mut sum = CONV3_BIAS[0];
        for in_ch in 0..CONV2_OUT {
            sum += h2[in_ch][pos] * CONV3_WEIGHT[in_ch];
        }
        h3[pos] = sum;
    }

    // Layer 4: Linear(174→91) + sigmoid
    let mut output = [0.0f32; K_INFO];
    for i in 0..K_INFO {
        let mut sum = LINEAR_BIAS[i];
        for j in 0..N_CODEWORD {
            sum += h3[j] * LINEAR_WEIGHT[i * N_CODEWORD + j];
        }
        output[i] = 1.0 / (1.0 + (-sum).exp()); // sigmoid
    }

    output
}

#[cfg(test)]
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
            assert!(p >= 0.0 && p <= 1.0, "Probability {} out of range", p);
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
