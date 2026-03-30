/// Benchmark tests for the decoder comparison harness.
///
/// These tests verify the structured output of `decode_wav_to_results` and
/// `ComparisonSummary` serialization.

use pancetta_ft8::benchmark::{BenchmarkResult, ComparisonSummary, DecodeResult, decode_wav_to_results, compare_results};

/// Verify that `decode_wav_to_results` returns structured output with the
/// expected fields populated for a known fixture WAV file.
#[test]
fn test_decode_wav_to_results_returns_structured_output() {
    let wav_path = "tests/fixtures/wav/generated/ft8_cq.wav";

    let result = decode_wav_to_results(wav_path)
        .expect("decode_wav_to_results should succeed on a valid WAV file");

    // file_path should match what was passed in
    assert_eq!(result.file_path, wav_path);

    // processing_time_ms must be positive (decoding takes measurable time)
    assert!(
        result.processing_time_ms > 0.0,
        "processing_time_ms should be > 0, got {}",
        result.processing_time_ms
    );

    // The struct fields must be accessible (type-level assertion)
    let _pancetta: &Vec<DecodeResult> = &result.pancetta_decodes;
    let _ft8lib: &Vec<DecodeResult> = &result.ft8lib_decodes;
}

/// Verify that `DecodeResult` and `BenchmarkResult` serialize to valid JSON
/// with the expected field names and values.
#[test]
fn test_decode_result_serializes_to_json() {
    let decode = DecodeResult {
        message: "CQ W1ABC FN42".to_string(),
        frequency_hz: 1500.0,
        time_offset_s: 0.5,
        snr_db: -10.0,
    };

    let json = serde_json::to_string(&decode).expect("DecodeResult should serialize to JSON");

    assert!(json.contains("\"message\""), "JSON should contain 'message' key");
    assert!(json.contains("CQ W1ABC FN42"), "JSON should contain the message text");
    assert!(json.contains("\"frequency_hz\""), "JSON should contain 'frequency_hz' key");
    assert!(json.contains("\"time_offset_s\""), "JSON should contain 'time_offset_s' key");
    assert!(json.contains("\"snr_db\""), "JSON should contain 'snr_db' key");

    // Round-trip: deserialize and check values
    let decoded: DecodeResult =
        serde_json::from_str(&json).expect("JSON should deserialize back to DecodeResult");
    assert_eq!(decoded.message, "CQ W1ABC FN42");
    assert!((decoded.frequency_hz - 1500.0).abs() < 1e-6);
    assert!((decoded.time_offset_s - 0.5).abs() < 1e-6);
    assert!((decoded.snr_db - (-10.0_f32)).abs() < 1e-4);
}

/// Verify that `BenchmarkResult` serializes correctly.
#[test]
fn test_benchmark_result_serializes_to_json() {
    let result = BenchmarkResult {
        file_path: "some/file.wav".to_string(),
        pancetta_decodes: vec![DecodeResult {
            message: "CQ DX K1ABC".to_string(),
            frequency_hz: 1234.5,
            time_offset_s: 0.0,
            snr_db: -5.0,
        }],
        ft8lib_decodes: vec![],
        processing_time_ms: 42.0,
    };

    let json = serde_json::to_string(&result).expect("BenchmarkResult should serialize to JSON");

    assert!(json.contains("\"file_path\""));
    assert!(json.contains("some/file.wav"));
    assert!(json.contains("\"pancetta_decodes\""));
    assert!(json.contains("\"ft8lib_decodes\""));
    assert!(json.contains("\"processing_time_ms\""));
    assert!(json.contains("42.0"));
}

/// Verify that `compare_results` correctly computes set-based statistics.
#[test]
fn test_compare_results_computes_summary() {
    let r1 = BenchmarkResult {
        file_path: "a.wav".to_string(),
        pancetta_decodes: vec![
            DecodeResult { message: "CQ W1ABC FN42".to_string(), frequency_hz: 1500.0, time_offset_s: 0.0, snr_db: -10.0 },
            DecodeResult { message: "K1DEF W1ABC -12".to_string(), frequency_hz: 1600.0, time_offset_s: 0.0, snr_db: -8.0 },
        ],
        ft8lib_decodes: vec![
            DecodeResult { message: "CQ W1ABC FN42".to_string(), frequency_hz: 1500.0, time_offset_s: 0.0, snr_db: -10.0 },
        ],
        processing_time_ms: 100.0,
    };

    let summary = compare_results(&[r1]);

    assert_eq!(summary.total_files, 1);
    assert_eq!(summary.pancetta_total, 2);
    assert_eq!(summary.ft8lib_total, 1);
    assert_eq!(summary.both_decoded, 1, "one message decoded by both");
    assert_eq!(summary.pancetta_only, 1, "one message decoded only by pancetta");
    assert_eq!(summary.ft8lib_only, 0, "ft8lib had no exclusive decodes");
    assert_eq!(summary.per_file.len(), 1);

    // parity_percent: both / max(pancetta, ft8lib) * 100
    // = 1 / 2 * 100 = 50.0
    assert!(
        (summary.parity_percent - 50.0).abs() < 0.01,
        "parity_percent should be 50.0, got {}",
        summary.parity_percent
    );
}

/// Verify that `ComparisonSummary` serializes to JSON.
#[test]
fn test_comparison_summary_serializes_to_json() {
    let summary = ComparisonSummary {
        total_files: 3,
        pancetta_total: 10,
        ft8lib_total: 9,
        both_decoded: 8,
        pancetta_only: 2,
        ft8lib_only: 1,
        parity_percent: 88.88,
        per_file: vec![],
    };

    let json = serde_json::to_string(&summary).expect("ComparisonSummary should serialize to JSON");

    assert!(json.contains("\"total_files\""));
    assert!(json.contains("\"parity_percent\""));
    assert!(json.contains("88.88"));
}
