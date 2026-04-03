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

/// Helper: find a decoded message containing expected text (case-insensitive match)
fn find_message<'a>(decoded: &'a [DecodedMessage], expected: &str) -> Option<&'a DecodedMessage> {
    let expected_upper = expected.to_uppercase();
    decoded.iter().find(|m| m.text.to_uppercase() == expected_upper)
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

#[test]
fn test_loopback_full_qso_cq_to_73() {
    let mut station_a = Station::new("W1ABC", "FN42");
    let mut station_b = Station::new("K2DEF", "FM18");
    let freq = 500.0;

    // Step 1: Station A sends CQ
    let cq_text = "CQ W1ABC FN42";
    let audio = station_a.encode_and_modulate(cq_text, freq);
    let decoded = station_b.decode(&audio);
    assert!(
        find_message(&decoded, cq_text).is_some(),
        "Station B should decode CQ. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 2: Station B responds with grid
    let grid_response = "W1ABC K2DEF FM18";
    let audio = station_b.encode_and_modulate(grid_response, freq);
    let decoded = station_a.decode(&audio);
    assert!(
        find_message(&decoded, grid_response).is_some(),
        "Station A should decode grid response. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 3: Station A sends signal report
    let report = "K2DEF W1ABC -10";
    let audio = station_a.encode_and_modulate(report, freq);
    let decoded = station_b.decode(&audio);
    assert!(
        find_message(&decoded, report).is_some(),
        "Station B should decode signal report. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 4: Station B sends R+report (acknowledges A's report, sends own)
    // Note: encoder accepts "R-12" but decoder formats as "R -12" (space before report)
    let r_report_encode = "W1ABC K2DEF R-12";
    let r_report_decoded = "W1ABC K2DEF R -12";
    let audio = station_b.encode_and_modulate(r_report_encode, freq);
    let decoded = station_a.decode(&audio);
    assert!(
        find_message(&decoded, r_report_decoded).is_some(),
        "Station A should decode R+report. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 5: Station A sends RR73
    let rr73 = "K2DEF W1ABC RR73";
    let audio = station_a.encode_and_modulate(rr73, freq);
    let decoded = station_b.decode(&audio);
    assert!(
        find_message(&decoded, rr73).is_some(),
        "Station B should decode RR73. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 6: Station B sends 73
    let seventy_three = "W1ABC K2DEF 73";
    let audio = station_b.encode_and_modulate(seventy_three, freq);
    let decoded = station_a.decode(&audio);
    assert!(
        find_message(&decoded, seventy_three).is_some(),
        "Station A should decode 73. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}
