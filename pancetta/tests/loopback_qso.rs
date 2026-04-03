//! Loopback QSO integration test: two simulated stations complete a full FT8 QSO
//! via encode → modulate → decode → state machine → generate response → encode → ...
//!
//! No audio hardware, no coordinator, no async runtime for the core loop.
//! Tests the pure FT8 + QSO pipeline.

use pancetta_ft8::{
    DecodedMessage, Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, WINDOW_SAMPLES,
};

/// Station identity for the loopback test
#[allow(dead_code)]
struct Station {
    callsign: String,
    grid: String,
    encoder: Ft8Encoder,
    modulator: Ft8Modulator,
    decoder: Ft8Decoder,
}

impl Station {
    fn new(callsign: &str, grid: &str) -> Self {
        Self {
            callsign: callsign.to_string(),
            grid: grid.to_string(),
            encoder: Ft8Encoder::new(),
            modulator: Ft8Modulator::new_default().unwrap(),
            decoder: Ft8Decoder::new(Ft8Config::default()).unwrap(),
        }
    }

    /// Encode a message text into audio samples (padded to WINDOW_SAMPLES)
    fn encode_and_modulate(&mut self, text: &str, freq_offset: f64) -> Vec<f32> {
        let symbols = self
            .encoder
            .encode_message(text, None)
            .unwrap_or_else(|e| panic!("Failed to encode '{}': {}", text, e));
        let mut audio = self
            .modulator
            .modulate_symbols(&symbols, freq_offset)
            .unwrap();
        audio.resize(WINDOW_SAMPLES, 0.0);
        audio
    }

    /// Decode audio samples and return decoded messages
    fn decode(&mut self, audio: &[f32]) -> Vec<DecodedMessage> {
        self.decoder.decode_window(audio).unwrap_or_default()
    }
}

#[test]
fn test_loopback_cq_decode() {
    // Station A sends CQ
    let mut station_a = Station::new("W1ABC", "FN42");
    let cq_text = "CQ W1ABC FN42";
    let audio = station_a.encode_and_modulate(cq_text, 500.0);

    // Station B decodes it
    let mut station_b = Station::new("K2DEF", "FM18");
    let decoded = station_b.decode(&audio);

    assert!(
        !decoded.is_empty(),
        "Station B should decode at least one message"
    );
    assert_eq!(
        decoded[0].text, cq_text,
        "Decoded message should match CQ text"
    );
}
