//! Neural OSD model weights, loaded from a packed binary blob.
//!
//! Previously this file was 19,953 lines of inline `pub const X: &[f32] = &[…];`
//! constants — a 12 MB source file that bloated compile times for every
//! workspace build. The weights are now stored as a single
//! `assets/neural_osd_weights.bin` file in flat little-endian f32 layout
//! and parsed once at first access via `OnceLock`.
//!
//! Regenerate the binary after retraining with:
//!
//! ```bash
//! cd training/neural_osd && python export_weights.py
//! ```
//!
//! The dimensions below must match the example dumper byte-for-byte —
//! a mismatch is caught at first call and panics with a descriptive
//! message rather than reading garbage.

use std::sync::OnceLock;

const CONV1_WEIGHT_LEN: usize = 32 * 26 * 3; // 2496
const CONV1_BIAS_LEN: usize = 32;
const CONV2_WEIGHT_LEN: usize = 16 * 32 * 3; // 1536
const CONV2_BIAS_LEN: usize = 16;
const CONV3_WEIGHT_LEN: usize = 16; // 1 * 16 * 1
const CONV3_BIAS_LEN: usize = 1;
const LINEAR_WEIGHT_LEN: usize = 91 * 174; // 15834
const LINEAR_BIAS_LEN: usize = 91;
const TOTAL_LEN: usize = CONV1_WEIGHT_LEN
    + CONV1_BIAS_LEN
    + CONV2_WEIGHT_LEN
    + CONV2_BIAS_LEN
    + CONV3_WEIGHT_LEN
    + CONV3_BIAS_LEN
    + LINEAR_WEIGHT_LEN
    + LINEAR_BIAS_LEN; // 20022

const RAW_BYTES: &[u8] = include_bytes!("../assets/neural_osd_weights.bin");

struct Weights {
    floats: Vec<f32>,
}

fn weights() -> &'static Weights {
    static W: OnceLock<Weights> = OnceLock::new();
    W.get_or_init(|| {
        assert_eq!(
            RAW_BYTES.len(),
            TOTAL_LEN * 4,
            "neural_osd_weights.bin size {} does not match the {} f32 = {} byte schema. \
             Did the model dimensions change without regenerating the blob?",
            RAW_BYTES.len(),
            TOTAL_LEN,
            TOTAL_LEN * 4,
        );
        let floats: Vec<f32> = RAW_BYTES
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        debug_assert_eq!(floats.len(), TOTAL_LEN);
        Weights { floats }
    })
}

fn slice(start: usize, len: usize) -> &'static [f32] {
    &weights().floats[start..start + len]
}

/// First convolutional layer weight tensor (output channels × in × kernel).
pub fn conv1_weight() -> &'static [f32] {
    slice(0, CONV1_WEIGHT_LEN)
}

/// First convolutional layer bias.
pub fn conv1_bias() -> &'static [f32] {
    slice(CONV1_WEIGHT_LEN, CONV1_BIAS_LEN)
}

/// Second convolutional layer weight tensor.
pub fn conv2_weight() -> &'static [f32] {
    slice(CONV1_WEIGHT_LEN + CONV1_BIAS_LEN, CONV2_WEIGHT_LEN)
}

/// Second convolutional layer bias.
pub fn conv2_bias() -> &'static [f32] {
    slice(
        CONV1_WEIGHT_LEN + CONV1_BIAS_LEN + CONV2_WEIGHT_LEN,
        CONV2_BIAS_LEN,
    )
}

/// Third convolutional layer weight tensor.
pub fn conv3_weight() -> &'static [f32] {
    slice(
        CONV1_WEIGHT_LEN + CONV1_BIAS_LEN + CONV2_WEIGHT_LEN + CONV2_BIAS_LEN,
        CONV3_WEIGHT_LEN,
    )
}

/// Third convolutional layer bias.
pub fn conv3_bias() -> &'static [f32] {
    slice(
        CONV1_WEIGHT_LEN + CONV1_BIAS_LEN + CONV2_WEIGHT_LEN + CONV2_BIAS_LEN + CONV3_WEIGHT_LEN,
        CONV3_BIAS_LEN,
    )
}

/// Final linear layer weight matrix.
pub fn linear_weight() -> &'static [f32] {
    slice(
        CONV1_WEIGHT_LEN
            + CONV1_BIAS_LEN
            + CONV2_WEIGHT_LEN
            + CONV2_BIAS_LEN
            + CONV3_WEIGHT_LEN
            + CONV3_BIAS_LEN,
        LINEAR_WEIGHT_LEN,
    )
}

/// Final linear layer bias.
pub fn linear_bias() -> &'static [f32] {
    slice(
        CONV1_WEIGHT_LEN
            + CONV1_BIAS_LEN
            + CONV2_WEIGHT_LEN
            + CONV2_BIAS_LEN
            + CONV3_WEIGHT_LEN
            + CONV3_BIAS_LEN
            + LINEAR_WEIGHT_LEN,
        LINEAR_BIAS_LEN,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn weights_load_with_expected_dimensions() {
        assert_eq!(conv1_weight().len(), CONV1_WEIGHT_LEN);
        assert_eq!(conv1_bias().len(), CONV1_BIAS_LEN);
        assert_eq!(conv2_weight().len(), CONV2_WEIGHT_LEN);
        assert_eq!(conv2_bias().len(), CONV2_BIAS_LEN);
        assert_eq!(conv3_weight().len(), CONV3_WEIGHT_LEN);
        assert_eq!(conv3_bias().len(), CONV3_BIAS_LEN);
        assert_eq!(linear_weight().len(), LINEAR_WEIGHT_LEN);
        assert_eq!(linear_bias().len(), LINEAR_BIAS_LEN);
    }

    #[test]
    fn provenance_matches_compiled_blob_schema_and_hash() {
        let provenance: serde_json::Value =
            serde_json::from_str(include_str!("../assets/neural_osd_weights.provenance.json"))
                .expect("valid neural OSD provenance JSON");
        assert_eq!(provenance["total_len"], TOTAL_LEN);
        assert_eq!(provenance["byte_length"], RAW_BYTES.len());
        let digest = format!("{:x}", Sha256::digest(RAW_BYTES));
        assert_eq!(provenance["sha256"], digest);
        let lengths: Vec<usize> = provenance["tensors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["length"].as_u64().unwrap() as usize)
            .collect();
        assert_eq!(
            lengths,
            vec![
                CONV1_WEIGHT_LEN,
                CONV1_BIAS_LEN,
                CONV2_WEIGHT_LEN,
                CONV2_BIAS_LEN,
                CONV3_WEIGHT_LEN,
                CONV3_BIAS_LEN,
                LINEAR_WEIGHT_LEN,
                LINEAR_BIAS_LEN,
            ]
        );
    }
}
