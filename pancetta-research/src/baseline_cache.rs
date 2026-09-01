//! Shared schema for the jt9 baseline decode cache (`research/baselines/<mode>/<sha>.json`),
//! written by `bin/baseline.rs` and read by `bin/eval.rs` and [`crate::curated`]'s preflight.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BaselineDecode {
    pub message: String,
    pub freq_hz: f64,
    pub dt_s: f64,
    pub snr_db: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BaselineCache {
    pub schema_version: u32,
    pub wav_path: String,
    pub wav_sha256: String,
    pub decoder_identity: String,
    pub decodes: Vec<BaselineDecode>,
    pub elapsed_seconds: f64,
}

impl BaselineCache {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;
}
