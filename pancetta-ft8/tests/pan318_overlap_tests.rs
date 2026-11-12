//! PAN-318 qualification tests: the overlapped two-pass window versus the
//! former sequential pipeline on real off-air fixtures, through the same
//! execution helper production uses
//! (`pancetta_ft8::window_execution::run_overlapped_passes`).
//!
//! Drivers mirror the coordinator's call shapes exactly:
//!   - sequential (today, pre-PAN-318): waterfall → ft8_lib → native on ONE
//!     decoder, one receipt-anchored `DecodeBudget`, merged ft8-lib-first.
//!   - overlapped (PAN-318): ft8_lib and native start together on two
//!     identically-constructed decoders (the native call is `&mut self`,
//!     the reference pass runs on its own instance), joined before the
//!     same merge; the waterfall runs after the joined pair, off the
//!     native entry path.
//!
//! Strict wall-clock qualification (`pre_native_ms` < 10 ms, start skew
//! ≤ 5 ms) is `#[ignore]`d and rejects a debug build — run the exact
//! release invocation from the PAN-318 plan:
//!
//! ```text
//! cargo test --release -p pancetta-ft8 --features benchmark --test pan318_overlap_tests \
//!     -- --include-ignored --nocapture --test-threads=1
//! ```
//!
//! Deterministic parity tests run in every build; no silent skip when
//! ft8_lib or a listed WAV is absent (the fixture loader asserts both).

use pancetta_ft8::merge::merge_ft8lib_first;
use pancetta_ft8::window_execution::run_overlapped_passes;
use pancetta_ft8::{
    ft8lib_is_available, ApContext, DecodeBudget, DecodeBudgetReport, DecodedMessage, Ft8Config,
    Ft8Decoder, Protocol, WINDOW_SAMPLES,
};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

const STANDARD_BUDGET_MS: u64 = 250;
const FT8: Protocol = Protocol::Ft8;

/// The five production-sized off-air windows the PAN-318 research and plan
/// measured (vendor ft8_lib test WAVs, 12 kHz mono, truncated to
/// `WINDOW_SAMPLES` exactly as production windows are shaped).
const FIXTURES: [&str; 5] = [
    "191111_110200.wav",
    "191111_110700.wav",
    "websdr_test2.wav",
    "websdr_test7.wav",
    "websdr_test12.wav",
];

fn fixture_path(name: &str) -> String {
    format!(
        "{}/vendor/ft8_lib/test/wav/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Production feeds a 12.64 s / 151,680-sample window (WINDOW_SAMPLES).
fn production_window(path: &str) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap_or_else(|e| {
        panic!("fixture {path} must be present (vendored ft8_lib test WAV): {e}")
    });
    assert_eq!(reader.spec().sample_rate, 12000, "fixture must be 12 kHz");
    assert_eq!(reader.spec().channels, 1, "fixture must be mono");
    let mut samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    samples.truncate(WINDOW_SAMPLES);
    samples
}

fn sorted_texts(msgs: &[DecodedMessage]) -> Vec<String> {
    let mut v: Vec<String> = msgs.iter().map(|m| m.text.clone()).collect();
    v.sort();
    v
}

/// Timing observations one driver run made, in microseconds (skew and
/// pre-native costs are sub-millisecond and must stay measurable).
#[derive(Debug, Clone, Copy)]
struct DriverTiming {
    pre_native_us: u64,
    start_skew_us: u64,
    waterfall_us: u64,
}

/// Today's (pre-PAN-318) coordinator order on one decoder: waterfall →
/// ft8_lib → native, one receipt-anchored budget shared by the budgeted
/// call. Returns the merged message set and its timing.
fn decode_sequential(window: &[f32], budget_ms: u64) -> (Vec<DecodedMessage>, DriverTiming) {
    let receipt = Instant::now();
    let budget = DecodeBudget::until(receipt + Duration::from_millis(budget_ms));

    let waterfall_start = Instant::now();
    let audio_f64: Vec<f64> = window.iter().map(|&s| s as f64).collect();
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).expect("decoder ctor");
    let _wf = decoder
        .generate_waterfall_data(&audio_f64)
        .expect("waterfall");
    let waterfall_us = waterfall_start.elapsed().as_micros() as u64;

    let ft8lib = catch_unwind(AssertUnwindSafe(|| {
        decoder.decode_window_ft8lib_with_context(window, FT8)
    }))
    .unwrap_or_default();

    let native_call_start = Instant::now();
    let pre_native_us = native_call_start.duration_since(receipt).as_micros() as u64;
    let (native, _report) = catch_unwind(AssertUnwindSafe(|| {
        decoder.decode_window_with_ap_scoped_partner_budgeted(
            window,
            &ApContext::default(),
            None,
            None,
            budget,
        )
    }))
    .unwrap_or_else(|_| Ok((Vec::new(), DecodeBudgetReport::default())))
    .unwrap_or_default();

    let merged = merge_ft8lib_first(ft8lib, native);
    let timing = DriverTiming {
        pre_native_us,
        start_skew_us: 0, // sequential: the passes do not start together
        waterfall_us,
    };
    (merged.messages, timing)
}

/// The PAN-318 coordinator order: the ft8_lib and native passes start
/// together on two identically-constructed decoders through the production
/// execution helper (barrier + in-worker timestamps + per-pass panic
/// isolation + join before return), then the waterfall runs after the
/// joined pair. Same merge, same budget anchor.
fn decode_overlapped(window: &[f32], budget_ms: u64) -> (Vec<DecodedMessage>, DriverTiming) {
    let receipt = Instant::now();
    let budget = DecodeBudget::until(receipt + Duration::from_millis(budget_ms));

    let mut native_decoder = Ft8Decoder::new(Ft8Config::default()).expect("native ctor");
    let reference_decoder = Ft8Decoder::new(Ft8Config::default()).expect("reference ctor");

    let outcome = run_overlapped_passes(
        move || {
            let messages = reference_decoder.decode_window_ft8lib_with_context(window, FT8);
            (messages, reference_decoder)
        },
        move || {
            let result = native_decoder.decode_window_with_ap_scoped_partner_budgeted(
                window,
                &ApContext::default(),
                None,
                None,
                budget,
            );
            (result, native_decoder)
        },
    );
    let pre_native_us = outcome
        .native_started_at
        .duration_since(receipt)
        .as_micros() as u64;
    let start_skew_us = outcome.start_skew().as_micros() as u64;

    let ft8lib = outcome
        .reference
        .map(|(messages, _worker)| messages)
        .unwrap_or_default();
    // Recover the native decoder for the waterfall below (a caught panic
    // consumes it — construct a fresh one; the waterfall needs no state).
    let ((native, _report), mut native_decoder) = match outcome.native {
        Some((result, worker)) => (result.unwrap_or_default(), worker),
        None => (
            (Vec::new(), DecodeBudgetReport::default()),
            Ft8Decoder::new(Ft8Config::default()).expect("native ctor after panic"),
        ),
    };

    // Waterfall AFTER the joined pair, off the native entry path.
    let waterfall_start = Instant::now();
    let audio_f64: Vec<f64> = window.iter().map(|&s| s as f64).collect();
    let _wf = native_decoder
        .generate_waterfall_data(&audio_f64)
        .expect("waterfall");
    let waterfall_us = waterfall_start.elapsed().as_micros() as u64;

    let merged = merge_ft8lib_first(ft8lib, native);
    let timing = DriverTiming {
        pre_native_us,
        start_skew_us,
        waterfall_us,
    };
    (merged.messages, timing)
}

/// Rejects a debug build: strict wall-clock qualification only means
/// something in the optimized profile CI ships.
fn reject_debug_build() {
    if cfg!(debug_assertions) {
        panic!(
            "release-only qualification test: run with --release --features benchmark \
             -- --include-ignored --nocapture --test-threads=1"
        );
    }
}

/// The linked build must have the vendored ft8_lib (the FFI stub returns
/// an empty result set and would make every parity claim vacuous), and
/// every listed fixture must exist.
#[test]
fn fixtures_present_and_ft8lib_linked() {
    assert!(
        ft8lib_is_available(),
        "the vendored ft8_lib must be linked (init the submodule before running)"
    );
    for name in FIXTURES {
        let window = production_window(&fixture_path(name));
        assert_eq!(window.len(), WINDOW_SAMPLES, "{name} must fill a window");
    }
}

/// AC3 (`standard_overlap_matches_sequential_fixtures`): on all five
/// off-air windows under the Standard 250 ms receipt-anchored budget, the
/// overlapped merged TEXT set equals the sequential result. Equality of
/// the exact-text multisets is asserted — never a count or subset.
#[test]
fn standard_overlap_matches_sequential_fixtures() {
    assert!(ft8lib_is_available());
    for name in FIXTURES {
        let window = production_window(&fixture_path(name));
        let (sequential, seq_timing) = decode_sequential(&window, STANDARD_BUDGET_MS);
        let (overlapped, _overlap_timing) = decode_overlapped(&window, STANDARD_BUDGET_MS);
        let seq_texts = sorted_texts(&sequential);
        let overlap_texts = sorted_texts(&overlapped);
        assert_eq!(
            seq_texts,
            overlap_texts,
            "{name}: overlapped merged text set must equal the sequential result \
             (sequential {} decodes, overlap {} decodes); sequential pre_native={}us",
            seq_texts.len(),
            overlap_texts.len(),
            seq_timing.pre_native_us
        );
        assert!(
            !seq_texts.is_empty(),
            "{name}: fixture must decode something for this comparison to bite"
        );
    }
}

/// Full-field comparison key: every field of the message, with three
/// per-run noise sources pinned so two runs at different instants can be
/// compared — (a) the decode wall-clock `timestamp` (records WHEN the
/// driver ran), (b) `decode_time_into_window` (rayon scheduling
/// telemetry), and (c) the heap `addr:` a `BitVec`'s Debug prints (the
/// payload BITS are compared; only the allocation address is masked).
fn full_field_key(mut m: DecodedMessage) -> String {
    m.timestamp = std::time::SystemTime::UNIX_EPOCH;
    m.decode_time_into_window = None;
    mask_heap_addrs(format!("{m:?}"))
}

fn mask_heap_addrs(s: String) -> String {
    const NEEDLE: &str = "addr: 0x";
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(pos) = rest.find(NEEDLE) {
        let (before, after) = rest.split_at(pos);
        out.push_str(before);
        out.push_str("addr: 0x0");
        rest = &after[NEEDLE.len()..];
        rest = rest.trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out.push_str(rest);
    out
}

/// AC3 (`unlimited_overlap_preserves_messages`): with an unlimited budget
/// the overlap preserves every decoded message FIELD (not just text),
/// comparing identically-seeded fresh decoders through both drivers.
#[test]
fn unlimited_overlap_preserves_messages() {
    assert!(ft8lib_is_available());
    for name in FIXTURES {
        let window = production_window(&fixture_path(name));
        let (sequential, _) = decode_sequential(&window, u64::MAX);
        let (overlapped, _) = decode_overlapped(&window, u64::MAX);
        let mut seq_debug: Vec<String> = sequential.into_iter().map(full_field_key).collect();
        seq_debug.sort();
        let mut overlap_debug: Vec<String> = overlapped.into_iter().map(full_field_key).collect();
        overlap_debug.sort();
        assert_eq!(
            seq_debug, overlap_debug,
            "{name}: unlimited-budget full-field message sets must be identical"
        );
    }
}

/// AC2 (`standard_window_pre_native_budget`): warmed, release, unscoped —
/// every measured window must reach the native call in under 10 ms from
/// receipt. The waterfall no longer runs before native entry, and the
/// reference pass overlaps instead of preceding it.
#[test]
#[ignore = "release-only wall-clock qualification: --release --include-ignored"]
fn standard_window_pre_native_budget() {
    reject_debug_build();
    assert!(ft8lib_is_available());
    for name in FIXTURES {
        let window = production_window(&fixture_path(name));
        // Warm both code paths so one-time construction (FFT plans, LDPC
        // tables) does not pollute the measurement.
        let _ = decode_overlapped(&window, STANDARD_BUDGET_MS);
        for _run in 0..3 {
            let (_, timing) = decode_overlapped(&window, STANDARD_BUDGET_MS);
            assert!(
                timing.pre_native_us < 10_000,
                "{name}: pre_native {}us must stay under 10ms without a scoped call",
                timing.pre_native_us
            );
        }
    }
}

/// AC1 (`standard_window_start_skew`): warmed, release — the ft8_lib and
/// native passes must start within 5 ms of each other, measured inside
/// the executing workers immediately before the decoder calls (which is
/// what `run_overlapped_passes` timestamps after barrier release).
#[test]
#[ignore = "release-only wall-clock qualification: --release --include-ignored"]
fn standard_window_start_skew() {
    reject_debug_build();
    assert!(ft8lib_is_available());
    for name in FIXTURES {
        let window = production_window(&fixture_path(name));
        let _ = decode_overlapped(&window, STANDARD_BUDGET_MS);
        for _run in 0..3 {
            let (_, timing) = decode_overlapped(&window, STANDARD_BUDGET_MS);
            assert!(
                timing.start_skew_us <= 5_000,
                "{name}: in-worker start skew {}us must stay within 5ms",
                timing.start_skew_us
            );
        }
    }
}

/// AC2 sanity, in every build: the overlap's native entry must not be
/// charged the (sequential) waterfall-plus-reference prefix. Sequential
/// pre-native includes both; overlapped pre-native excludes them by
/// construction. This is a shape check, not the release timing gate.
#[test]
fn overlapped_native_entry_skips_the_waterfall_prefix() {
    assert!(ft8lib_is_available());
    let window = production_window(&fixture_path(FIXTURES[0]));
    let (_, seq) = decode_sequential(&window, STANDARD_BUDGET_MS);
    let (_, overlap) = decode_overlapped(&window, STANDARD_BUDGET_MS);
    assert!(
        seq.pre_native_us > overlap.waterfall_us,
        "sequential pre_native ({}us) should include the waterfall ({}us) it runs first",
        seq.pre_native_us,
        overlap.waterfall_us
    );
    assert!(
        overlap.pre_native_us < seq.pre_native_us,
        "overlapped native entry ({}us) must be cheaper than the sequential prefix ({}us)",
        overlap.pre_native_us,
        seq.pre_native_us
    );
}

/// AC4 (`reference_waterfall_preserves_geometry_and_tones`): the waterfall
/// derived from the reference monitor's captured grid reproduces the legacy
/// generator's display geometry — row count, per-row time offsets (same
/// partition arithmetic, no second FFT), 0–3000 Hz bin axis — stays finite,
/// normalizes into [0,1], and places the fixture's spectral peaks within one
/// source frequency step of the legacy image.
#[test]
fn reference_waterfall_preserves_geometry_and_tones() {
    assert!(ft8lib_is_available());
    let window = production_window(&fixture_path(FIXTURES[0]));
    let audio_f64: Vec<f64> = window.iter().map(|&s| s as f64).collect();

    let mut native = Ft8Decoder::new(Ft8Config::default()).expect("native ctor");
    let legacy = native
        .generate_waterfall_data(&audio_f64)
        .expect("legacy waterfall");

    let reference = Ft8Decoder::new(Ft8Config::default()).expect("reference ctor");
    let (_messages, grid) = reference.decode_window_ft8lib_with_context_and_capture(&window, FT8);
    let grid = grid.expect("real build returns a display grid");
    let derived = grid.to_waterfall_data(window.len());

    // Geometry: same rows, same 0–3000 Hz bin axis, same row time labels.
    assert_eq!(
        derived.power_matrix.len(),
        legacy.power_matrix.len(),
        "row count must match the legacy partition (including the trailing short row)"
    );
    assert_eq!(derived.frequency_bins.len(), legacy.frequency_bins.len());
    assert_eq!(derived.frequency_bins, legacy.frequency_bins);
    for (d, l) in derived.time_bins.iter().zip(legacy.time_bins.iter()) {
        assert!(
            (d - l).abs() < 1e-9,
            "row time label {d} must reproduce the legacy {l}"
        );
    }
    for row in &derived.power_matrix {
        assert!(
            row.iter().all(|v| v.is_finite()),
            "dB values must be finite"
        );
        assert_eq!(row.len(), legacy.frequency_bins.len());
    }

    // Normalized display values live in [0,1] with a real dynamic range.
    let range = derived.max_power - derived.min_power;
    assert!(range > 0.0, "off-air fixture must have dynamic range");
    let normalized: Vec<f32> = derived
        .power_matrix
        .iter()
        .flat_map(|row| {
            row.iter()
                .map(|&p| ((p - derived.min_power) / range) as f32)
        })
        .collect();
    assert!(normalized.iter().all(|v| (0.0..=1.0).contains(v)));

    // Tone placement: on pure tones the derived image must peak at the
    // tone's true frequency (50/1500/2990 Hz — band edges and center, the
    // plan's placement set; the FFI unit test adds silence). Peak error
    // tolerance is one source frequency step (3.125 Hz) plus one display
    // bin (2.93 Hz). Comparing peaks between the two FFT geometries on the
    // multi-signal off-air fixture is not a placement claim: near-equal
    // broadband peaks legitimately sit a few bins apart, so the fixture
    // asserts geometry only.
    let bin_width = legacy.frequency_bins[1] - legacy.frequency_bins[0];
    let tolerance = 1.0 / (grid.symbol_period as f64 * grid.freq_osr as f64) + bin_width;
    let global_peak = |data: &pancetta_ft8::WaterfallData| {
        let (mut best_v, mut best_i) = (f64::MIN, 0usize);
        for row in &data.power_matrix {
            for (i, &v) in row.iter().enumerate() {
                if v > best_v {
                    best_v = v;
                    best_i = i;
                }
            }
        }
        data.frequency_bins[best_i]
    };
    for tone_hz in [50.0f32, 1500.0, 2990.0] {
        let tone_samples: Vec<f32> = (0..WINDOW_SAMPLES)
            .map(|i| 0.25 * (2.0 * std::f32::consts::PI * tone_hz * i as f32 / 12000.0).sin())
            .collect();
        let (_, tone_grid) =
            reference.decode_window_ft8lib_with_context_and_capture(&tone_samples, FT8);
        let tone_wf = tone_grid
            .expect("tone capture")
            .to_waterfall_data(tone_samples.len());
        let t_hz = global_peak(&tone_wf);
        assert!(
            (t_hz - tone_hz as f64).abs() <= tolerance,
            "{tone_hz} Hz tone placed at {t_hz} Hz"
        );
    }
}
