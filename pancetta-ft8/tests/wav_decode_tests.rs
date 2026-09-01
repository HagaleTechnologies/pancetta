//! WAV file decoding tests
//!
//! Tests decoder against:
//! 1. Generated GFSK test signals (known-decodable by ft8_lib)
//! 2. Off-air recordings (best-effort — these may not decode with either
//!    our decoder or ft8_lib due to unknown signal characteristics)

use pancetta_ft8::{
    ft8_lib_ffi::ft8lib_decode_audio, DecodeBudget, DecodedMessage, Ft8Config, Ft8Decoder,
    SAMPLE_RATE, WINDOW_SAMPLES,
};

fn read_wav_file(path: &str) -> Vec<f32> {
    let reader =
        hound::WavReader::open(path).unwrap_or_else(|e| panic!("Failed to open {}: {}", path, e));
    let spec = reader.spec();
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.sample_rate, SAMPLE_RATE);
    reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

/// Read a fixture WAV and pad/truncate to exactly one decode window —
/// mirrors `decode_wav_file`'s buffer prep, but returns the raw samples
/// so the caller can drive `decode_window_budgeted` directly.
fn read_fixture_window(subpath: &str) -> Vec<f32> {
    let samples = read_wav_file(&fixture(subpath));
    if samples.len() >= WINDOW_SAMPLES {
        samples
    } else {
        let mut padded = samples;
        padded.resize(WINDOW_SAMPLES, 0.0);
        padded
    }
}

fn decode_wav_file(path: &str) -> Vec<DecodedMessage> {
    let samples = read_wav_file(path);
    let buffer: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
        samples
    } else {
        let mut padded = samples;
        padded.resize(WINDOW_SAMPLES, 0.0);
        padded
    };
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();
    decoder.decode_window(&buffer).unwrap_or_default()
}

fn fixture(subpath: &str) -> String {
    format!(
        "{}/tests/fixtures/wav/{}",
        env!("CARGO_MANIFEST_DIR"),
        subpath
    )
}

// =============================================================
// Generated GFSK test signals (known-good, decodable by ft8_lib)
// =============================================================

#[test]
fn test_decode_generated_cq() {
    let decoded = decode_wav_file(&fixture("generated/ft8_cq.wav"));
    // Our decoder may or may not decode GFSK signals from ft8_lib.
    // This test documents current capability.
    println!("generated/ft8_cq.wav: {} messages decoded", decoded.len());
    for m in &decoded {
        println!("  [{:6.1} dB] {}", m.snr_db, m.text);
    }
}

#[test]
fn test_decode_generated_report() {
    let decoded = decode_wav_file(&fixture("generated/ft8_report.wav"));
    println!(
        "generated/ft8_report.wav: {} messages decoded",
        decoded.len()
    );
    for m in &decoded {
        println!("  [{:6.1} dB] {}", m.snr_db, m.text);
    }
}

#[test]
fn test_decode_generated_rr73() {
    let decoded = decode_wav_file(&fixture("generated/ft8_rr73.wav"));
    println!("generated/ft8_rr73.wav: {} messages decoded", decoded.len());
    for m in &decoded {
        println!("  [{:6.1} dB] {}", m.snr_db, m.text);
    }
}

// =============================================================
// Off-air recordings (informational — ft8_lib also can't decode these)
// =============================================================

#[test]
fn test_decode_offair_summary() {
    let files = [
        "jtdx/000000_000001.wav",
        "jtdx/190227_155815.wav",
        "wsjt/210703_133430.wav",
        "wsjt/181201_180245.wav",
        "wsjt/170709_135615.wav",
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
        "basicft8/170923_082045.wav",
    ];

    let mut total = 0;
    let mut with_decodes = 0;

    for file in &files {
        let decoded = decode_wav_file(&fixture(file));
        if !decoded.is_empty() {
            with_decodes += 1;
        }
        total += decoded.len();
        println!("{}: {} messages", file, decoded.len());
        for m in &decoded {
            println!("  [{:6.1} dB] {}", m.snr_db, m.text);
        }
    }

    println!(
        "\nOff-air summary: {} messages from {}/{} files",
        total,
        with_decodes,
        files.len()
    );
    // Note: ft8_lib (kgoba/ft8_lib latest) also decodes 0 from these files.
    // These recordings may not contain standard FT8 or may require a different
    // decoder configuration. No assertion — this is informational only.
}

// =============================================================
// Cross-validation against ft8_lib reference implementation
// =============================================================

/// Compare our decoder's output against ft8_lib for each WAV file.
/// We assert that we decode at least 80% of what ft8_lib decodes.
#[test]
fn test_cross_validate_against_ft8lib() {
    let files = [
        "jtdx/000000_000001.wav",
        "jtdx/190227_155815.wav",
        "wsjt/210703_133430.wav",
        "wsjt/181201_180245.wav",
        "wsjt/170709_135615.wav",
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
        "basicft8/170923_082045.wav",
    ];

    let mut total_ours = 0usize;
    let mut total_ft8lib = 0usize;
    let mut files_below_threshold = Vec::new();
    let mut false_positive_candidates = 0usize;
    let mut unique_to_ours = 0usize;

    for file in &files {
        let path = fixture(file);
        let samples = read_wav_file(&path);
        let buffer: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
            samples.clone()
        } else {
            let mut padded = samples.clone();
            padded.resize(WINDOW_SAMPLES, 0.0);
            padded
        };

        // Our decoder
        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let our_decoded = decoder.decode_window(&buffer).unwrap_or_default();
        let our_count = our_decoded.len();

        // ft8_lib reference decoder
        let ft8lib_decoded = ft8lib_decode_audio(&samples);
        let ft8lib_count = ft8lib_decoded.len();

        total_ours += our_count;
        total_ft8lib += ft8lib_count;

        println!("{}: ours={}, ft8_lib={}", file, our_count, ft8lib_count);

        // Per-file check: if ft8_lib decodes messages, we should decode >= 80%
        if ft8lib_count > 0 {
            let ratio = our_count as f64 / ft8lib_count as f64;
            println!("  ratio: {:.1}%", ratio * 100.0);
            if ratio < 0.80 {
                files_below_threshold.push(format!(
                    "{}: ours={} vs ft8_lib={} ({:.0}%)",
                    file,
                    our_count,
                    ft8lib_count,
                    ratio * 100.0
                ));
            }
        }

        // Print decoded messages for comparison
        for m in &our_decoded {
            let ap_tag = if m.ap_level > 0 {
                format!(" [AP{}]", m.ap_level)
            } else {
                String::new()
            };
            println!("  [ours]   {:6.1} dB  {}{}", m.snr_db, m.text, ap_tag);
        }
        for (text, freq, _time, _ldpc, snr) in &ft8lib_decoded {
            println!("  [ft8lib] {:+6.1} dB  {:6.1} Hz  {}", snr, freq, text);
        }

        // Count decodes unique to our decoder that look like false positives
        let ft8lib_texts: std::collections::HashSet<&str> = ft8lib_decoded
            .iter()
            .map(|(text, _, _, _, _)| text.as_str())
            .collect();
        for m in &our_decoded {
            if !ft8lib_texts.contains(m.text.as_str()) {
                unique_to_ours += 1;
                // Flag suspect decodes: AP-assisted, or freetext with no
                // recognizable FT8 structure
                let is_suspect =
                    m.ap_level > 0 || m.message.message_type == pancetta_ft8::MessageType::FreeText;
                if is_suspect {
                    false_positive_candidates += 1;
                }
            }
        }
    }

    println!(
        "\nCross-validation totals: ours={}, ft8_lib={}",
        total_ours, total_ft8lib
    );
    println!(
        "False positive candidates: {}/{} unique to our decoder",
        false_positive_candidates, unique_to_ours
    );

    // Guard the ratio check below from passing vacuously: these 9 fixtures
    // are known-decodable real recordings (unlike the "off-air" section's
    // best-effort files), so ft8_lib decoding zero messages across all of
    // them means the reference decoder itself is broken — e.g. an FFI/link
    // regression that silently swapped in ft8lib_stub — not a legitimate
    // "nothing to compare" case. Without this, the ratio assertion below is
    // skipped entirely in that scenario and the test reports green (PR #278
    // review, round 3: this release-gate reuse of the test surfaced the gap).
    assert!(
        total_ft8lib > 0,
        "ft8_lib (the reference decoder) decoded zero messages across all {} fixtures — \
         the reference decoder itself is broken (stub swapped in? FFI regression?), \
         not a legitimate zero-signal case",
        files.len()
    );

    // Overall assertion: we should decode at least as many messages as ft8_lib.
    // Pancetta decoder achieves 129%+ of ft8_lib via:
    // spectrogram extraction, sum-product LDPC, TIME_OSR=2, OSD-3,
    // multi-pass signal subtraction, block detection, AP decoding,
    // parallel candidate decoding via rayon.
    // Regression floor: 100% means we decode at least as many as ft8_lib.
    // Previously 120%, but that counted CRC-14 false positives as "better".
    // After tightening confidence gates, we may decode fewer noise artifacts
    // which is correct behavior, not a regression.
    let overall_ratio = total_ours as f64 / total_ft8lib as f64;
    println!("Overall ratio: {:.1}%", overall_ratio * 100.0);

    assert!(
        overall_ratio >= 1.00,
        "REGRESSION: decode ratio {:.1}% dropped below 100% floor. Ours={}, ft8_lib={}.\nPer-file failures:\n{}",
        overall_ratio * 100.0,
        total_ours,
        total_ft8lib,
        files_below_threshold.join("\n")
    );
}

// =============================================================
// Performance: assert decode completes within real-time budget
// =============================================================

#[test]
fn test_decode_within_realtime_budget() {
    use std::time::Instant;

    let files = [
        "basicft8/170923_082000.wav",
        "basicft8/170923_082015.wav",
        "basicft8/170923_082030.wav",
    ];

    // In release mode: target 2x real-time (25.28s). In debug mode: allow 8x (101s).
    // CI runners are significantly slower than local hardware, so debug budget is generous.
    let max_decode_time = if cfg!(debug_assertions) {
        std::time::Duration::from_millis(101120) // 8x real-time for debug (CI)
    } else {
        std::time::Duration::from_millis(25280) // 2x real-time for release
    };

    for file in &files {
        let path = fixture(file);
        let samples = read_wav_file(&path);
        let buffer: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
            samples
        } else {
            let mut padded = samples;
            padded.resize(WINDOW_SAMPLES, 0.0);
            padded
        };

        // Single-pass decode for budget test — successive decoding is tested separately
        let config = Ft8Config {
            max_decode_passes: 1,
            ..Ft8Config::default()
        };
        let mut decoder = Ft8Decoder::new(config).unwrap();

        let start = Instant::now();
        let _decoded = decoder.decode_window(&buffer).unwrap_or_default();
        let elapsed = start.elapsed();

        println!("{}: decoded in {:?}", file, elapsed);
        assert!(
            elapsed < max_decode_time,
            "{}: decode took {:?}, exceeds real-time budget of {:?}",
            file,
            elapsed,
            max_decode_time
        );
    }
}

// Diagnostic tests below — require WAV fixtures not checked into git.
// Copy WAV files to tests/fixtures/wav/basicft8/ to enable.

#[test]
fn test_live_20m_wav() {
    let wav_path = format!(
        "{}/tests/fixtures/wav/basicft8/live_now.wav",
        env!("CARGO_MANIFEST_DIR")
    );
    if !std::path::Path::new(&wav_path).exists() {
        eprintln!("Skipping: {} not found", wav_path);
        return;
    }

    let mut reader = hound::WavReader::open(&wav_path).unwrap();
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / max)
                .collect()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
    };
    eprintln!(
        "WAV: {}ch {}Hz {} samples",
        spec.channels,
        spec.sample_rate,
        samples.len()
    );

    // ft8_lib
    let ft8lib = ft8lib_decode_audio(&samples);
    eprintln!("ft8_lib: {} decodes", ft8lib.len());
    for (msg, freq, _time, _ldpc, snr) in &ft8lib {
        eprintln!("  [ft8_lib] {:+.0} dB  {:.1} Hz  {}", snr, freq, msg);
    }

    // Ours — try with confidence floor disabled to isolate the issue
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();
    let ours = decoder.decode_window(&samples).unwrap_or_default();
    let metrics = decoder.get_last_metrics();
    eprintln!(
        "ours: {} decodes, sync_quality={:.3}",
        ours.len(),
        metrics.sync_quality
    );
    for msg in &ours {
        eprintln!(
            "  [ours]   {:+.0} dB  {:.1} Hz  conf={:.2}  {}",
            msg.snr_db, msg.frequency_offset, msg.confidence, msg.text
        );
    }

    eprintln!("\nft8_lib={}, ours={}", ft8lib.len(), ours.len());
}

#[test]
fn test_raw_vs_dsp_decimated() {
    let base = env!("CARGO_MANIFEST_DIR");

    // Test each audio source with ft8_lib
    for (label, filename) in &[
        ("raw-decimated", "raw_decimated_12khz.wav"),
        ("raw-subsampled", "raw_subsampled_12khz.wav"),
        ("dsp-decimated", "dsp_decimated.wav"),
    ] {
        let path = format!("{}/tests/fixtures/wav/basicft8/{}", base, filename);
        if !std::path::Path::new(&path).exists() {
            eprintln!("Skipping {}: not found", filename);
            continue;
        }

        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        let all_samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => {
                let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .filter_map(|s| s.ok())
                    .map(|s| s as f32 / max)
                    .collect()
            }
            hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
        };

        eprintln!(
            "\n=== {} ({}, {}Hz, {} samples, {:.1}s) ===",
            label,
            filename,
            spec.sample_rate,
            all_samples.len(),
            all_samples.len() as f64 / spec.sample_rate as f64
        );

        // Process in 12.64-second windows with 50% overlap
        let window_size = SAMPLE_RATE as usize * 1264 / 100; // 151680
        let step = window_size / 2;
        let mut total_ft8lib = 0;
        let mut total_ours = 0;
        let mut offset = 0;
        let mut window_num = 0;

        while offset + window_size <= all_samples.len() {
            let window = &all_samples[offset..offset + window_size];

            let ft8lib = ft8lib_decode_audio(window);
            let config = Ft8Config::default();
            let mut decoder = Ft8Decoder::new(config).unwrap();
            let ours = decoder.decode_window(window).unwrap_or_default();

            if !ft8lib.is_empty() || !ours.is_empty() {
                eprintln!(
                    "  window {} (offset {}): ft8_lib={}, ours={}",
                    window_num,
                    offset,
                    ft8lib.len(),
                    ours.len()
                );
                for (msg, freq, _time, _ldpc, snr) in &ft8lib {
                    eprintln!("    [ft8_lib] {:+.0} dB  {:.1} Hz  {}", snr, freq, msg);
                }
                for msg in &ours {
                    eprintln!(
                        "    [ours]   {:+.0} dB  {:.1} Hz  conf={:.2}  {}",
                        msg.snr_db, msg.frequency_offset, msg.confidence, msg.text
                    );
                }
            }

            total_ft8lib += ft8lib.len();
            total_ours += ours.len();
            offset += step;
            window_num += 1;
        }

        eprintln!("  TOTAL: ft8_lib={}, ours={}", total_ft8lib, total_ours);
    }
}

#[test]
fn test_python_fir_vs_naive() {
    let base = env!("CARGO_MANIFEST_DIR");
    for (label, filename) in &[
        ("python-FIR", "python_fir_decimated.wav"),
        ("naive-avg", "raw_decimated_12khz.wav"),
    ] {
        let path = format!("{}/tests/fixtures/wav/basicft8/{}", base, filename);
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        let all: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => {
                let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .filter_map(|s| s.ok())
                    .map(|s| s as f32 / max)
                    .collect()
            }
            _ => unreachable!(),
        };
        let window_size = SAMPLE_RATE as usize * 1264 / 100;
        let step = window_size / 2;
        let mut total = 0;
        let mut offset = 0;
        while offset + window_size <= all.len() {
            total += ft8lib_decode_audio(&all[offset..offset + window_size]).len();
            offset += step;
        }
        eprintln!("{}: ft8_lib decoded {} total", label, total);
    }
}

/// Regression test for the "every ft8_lib decode shows SNR +0" bug.
///
/// `decode_window_ft8lib` used to hard-code `snr_db = 0.0`. It now computes
/// a real SNR from the ft8_lib waterfall magnitudes (see
/// `ft8_lib_ffi::estimate_snr_from_waterfall`). On a real multi-signal
/// WSJT-X recording the SNRs must:
///   - not all be 0.0 (the bug),
///   - vary across decodes (a spread), and
///   - sit in a sane WSJT-X-like range (~ -24..+30 dB).
#[cfg(not(ft8lib_stub))]
#[test]
fn ft8lib_decode_snr_is_nonconstant_and_in_range() {
    let samples = read_wav_file(&fixture("wsjt/210703_133430.wav"));
    let decoded = Ft8Decoder::decode_window_ft8lib(&samples);
    assert!(
        decoded.len() >= 3,
        "expected several decodes from the WSJT-X test recording, got {}",
        decoded.len()
    );

    let snrs: Vec<f32> = decoded.iter().map(|m| m.snr_db).collect();

    // Not the old constant-0 bug.
    assert!(
        snrs.iter().any(|&s| s != 0.0),
        "every ft8_lib SNR was 0.0 — the constant-0 bug is back: {:?}",
        snrs
    );

    // The SNRs must actually vary (a spread), not be one repeated value.
    let min = snrs.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = snrs.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    assert!(
        max - min > 2.0,
        "ft8_lib SNRs show no meaningful spread (min={:.1} max={:.1}): {:?}",
        min,
        max,
        snrs
    );

    // Every value must land in a plausible WSJT-X-referenced range.
    for &s in &snrs {
        assert!(
            (-30.0..=35.0).contains(&s),
            "ft8_lib SNR {:.1} dB outside the plausible range: {:?}",
            s,
            snrs
        );
    }
}

// =============================================================
// Decoder-speed-overhaul Task 9: floor/budgeted candidate split
// =============================================================

/// With a budget that has already expired before decode starts, the
/// decoder must still decode the "floor" slice of ranked sync
/// candidates (the top `floor_candidates`, default 50) rather than
/// returning nothing. `report.budget_exhausted` must reflect that the
/// "rest" of the candidate list was skipped.
///
/// Unlimited-budget decode of this fixture produces 9 messages (see
/// `test_cross_validate_against_ft8lib`). The threshold here was
/// originally `>= 7`, back when only S1 (floor) / S2 (rest) were
/// budget-aware: cross-cycle averaging, coherent multipass, and
/// joint-pair-retry (S4/S5/S6, budget-checkpointed in decoder-speed-
/// overhaul Task 11) ran unconditionally regardless of budget and
/// padded the "floor-only" count above 7 even under an already-expired
/// budget. Now that Task 11 correctly gates those stages too, an
/// already-expired budget yields exactly the floor's own contribution
/// after dedup — measured at 4 on this fixture — so the threshold is
/// `>= 3` (small margin, still proving the floor ran a real decode
/// pass rather than an early-out no-op).
#[test]
fn budget_floor_still_decodes_top_candidates() {
    let samples = read_fixture_window("wsjt/210703_133430.wav");
    let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
    let (msgs, report) = d
        .decode_window_budgeted(&samples, DecodeBudget::until(std::time::Instant::now()))
        .unwrap();
    assert!(
        report.budget_exhausted,
        "expected an already-expired budget to report exhaustion"
    );
    assert!(
        msgs.len() >= 3,
        "floor (top-50 @ shallow BP) must still run, got {}",
        msgs.len()
    );
}

// =============================================================
// Decoder-speed-overhaul Task 11: budget checkpoints for S4-S7 +
// superset invariant
// =============================================================

/// Recursively collect every `.wav` fixture under
/// `tests/fixtures/wav/`, returned as paths relative to that root
/// (e.g. `"wsjt/210703_133430.wav"`) suitable for
/// `read_fixture_window`. The brief's literal example loops over
/// `wsjt/` only; we cover every fixture subdirectory (`basicft8`,
/// `generated`, `jtdx`, `wsjt`) to make the superset invariant as
/// strong as possible, since it's cheap and the whole point is "never
/// skip work under an unlimited budget" — that should hold everywhere,
/// not just in one subdirectory.
fn all_wav_fixtures() -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("wav") {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                out.push(rel);
            }
        }
    }
    let root =
        std::path::PathBuf::from(format!("{}/tests/fixtures/wav", env!("CARGO_MANIFEST_DIR")));
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out.sort();
    out
}

/// Message-set-equivalence check for the S4-S7 budget checkpoints
/// added in this task: `decode_window` is defined as
/// `decode_window_budgeted(samples, DecodeBudget::unlimited())`
/// discarding the report (see its doc comment), so this test proves
/// `decode_window_budgeted` under an unlimited budget produces a
/// message set that is a superset of (or equal to) whatever that
/// pre-existing entry point produces — i.e. the two call sites agree.
///
/// **Evidentiary scope (corrected 2026-07-06 per review finding):**
/// because both sides of this comparison invoke the *same*
/// checkpointed code path with the *same* `DecodeBudget::unlimited()`
/// value, this test CANNOT independently verify that the S4-S7
/// `has_time()` checkpoints are correctly conditional. A checkpoint
/// hardcoded to always skip (regardless of what `has_time()` actually
/// returns) would degrade both "baseline" and "budgeted" identically,
/// and this superset assertion would still pass — no fixture, however
/// rich, changes that, since it's a structural property of the
/// comparison, not a data-coverage gap. That conditional-skip
/// verification is what
/// `unlimited_budget_never_skips_s4_s7_stages_in_report` (below) is
/// for: it calls `decode_window_budgeted` directly and inspects
/// `DecodeBudgetReport::stages` for `skipped == 0`, which a hardcoded
/// always-skip bug would fail.
///
/// Runs over every WAV fixture under `tests/fixtures/wav/` (all four
/// subdirectories), not just the one example fixture in the task
/// brief.
#[test]
fn unlimited_budget_matches_baseline_decode_set_all_fixtures() {
    let fixtures = all_wav_fixtures();
    assert!(
        !fixtures.is_empty(),
        "expected to find WAV fixtures under tests/fixtures/wav/"
    );

    for fixture_path in fixtures {
        let samples = read_fixture_window(&fixture_path);

        let baseline: std::collections::BTreeSet<String> = {
            let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
            d.decode_window(&samples)
                .unwrap()
                .into_iter()
                .map(|m| m.text)
                .collect()
        };
        let budgeted: std::collections::BTreeSet<String> = {
            let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
            d.decode_window_budgeted(&samples, DecodeBudget::unlimited())
                .unwrap()
                .0
                .into_iter()
                .map(|m| m.text)
                .collect()
        };

        assert!(
            budgeted.is_superset(&baseline),
            "{}: budgeted decode set is missing baseline messages: {:?}",
            fixture_path,
            baseline.difference(&budgeted).collect::<Vec<_>>()
        );
    }
}

/// With a budget that has already expired before decode starts, every
/// S4-S7 stage checkpoint added in this task must record itself as
/// skipped in the report (rather than either silently running anyway,
/// or silently omitting itself from the report). `cross_cycle_averaging`
/// and `joint_pair_retry` are on by default (so S4/S6 are exercised and
/// must show `skipped == 1`); `coherent_multipass_iterations` defaults
/// to 3 (so S5 must show a single skip record with `skipped == 3`,
/// covering every remaining round in one entry since the round loop
/// breaks on the first budget check). `a7_enabled` defaults to `false`,
/// so S7 legitimately never enters its `if` at all — this is asserted
/// explicitly rather than assumed.
#[test]
fn expired_budget_skips_s4_s7_stages_in_report() {
    let samples = read_fixture_window("wsjt/210703_133430.wav");
    assert!(
        !Ft8Config::default().a7_enabled,
        "this test's S7 assertion assumes a7_enabled defaults to false"
    );
    let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
    let (_msgs, report) = d
        .decode_window_budgeted(&samples, DecodeBudget::until(std::time::Instant::now()))
        .unwrap();

    assert!(
        report.budget_exhausted,
        "expected an already-expired budget to report exhaustion"
    );

    let find = |label: &str| report.stages.iter().find(|(l, _, _, _)| *l == label);

    let s4 = find("S4-cross-cycle").expect("S4-cross-cycle must appear in the report");
    assert_eq!(
        s4.2, 0,
        "S4 must not have decoded anything under an expired budget"
    );
    assert_eq!(s4.3, 1, "S4 must record itself as fully skipped");

    let s5 = find("S5-multipass").expect("S5-multipass must appear in the report");
    assert_eq!(
        s5.2, 0,
        "S5 must not have decoded anything under an expired budget"
    );
    assert_eq!(
        s5.3, 3,
        "S5 must record all 3 default rounds as skipped in a single entry"
    );

    let s6 = find("S6-joint-pair").expect("S6-joint-pair must appear in the report");
    assert_eq!(
        s6.2, 0,
        "S6 must not have decoded anything under an expired budget"
    );
    assert_eq!(s6.3, 1, "S6 must record itself as fully skipped");

    assert!(
        find("S7-a7").is_none(),
        "a7_enabled defaults to false, so S7 should never enter its checkpoint"
    );
}

/// Review-gap closer (2026-07-06): `unlimited_budget_matches_baseline_decode_set_all_fixtures`
/// (above) cannot detect a checkpoint hardcoded to always skip
/// regardless of `has_time()`'s actual result, because its "baseline"
/// and "budgeted" both call `decode_window_budgeted` with the same
/// `DecodeBudget::unlimited()` value — an always-skip bug would
/// degrade both sides identically and the superset assertion would
/// still pass. `expired_budget_skips_s4_s7_stages_in_report` proves
/// the skip branch is reachable, but not that it's correctly
/// *conditional* (i.e. it doesn't distinguish "skips only when
/// expired" from "always skips").
///
/// This test closes that gap directly: run `decode_window_budgeted`
/// with `DecodeBudget::unlimited()` on fixtures known (from Task 11's
/// own debug notes, see `task-11-report.md`) to actually exercise
/// each checkpointed stage under current `Ft8Config` defaults
/// (`cross_cycle_averaging=true`, `coherent_multipass_iterations=3`,
/// `joint_pair_retry=true`, `a7_enabled=false`) —
/// `basicft8/live_now.wav` drives S4-cross-cycle to a nonzero decode
/// count, `jtdx/190227_155815.wav` drives S5-multipass to a nonzero
/// decode count across its rounds — and asserts every S4/S5/S6 stage
/// entry in the report shows `skipped == 0`. If any checkpoint were
/// hardcoded to always skip (`if true` instead of
/// `if !current_budget.has_time()`), the corresponding entry would
/// show `skipped > 0` (and, for S4/S6, `decoded == 0`) and this test
/// would fail — this is what the superset test structurally cannot
/// prove. (Manually verified: temporarily hardcoding the S4 checkpoint
/// to always skip makes this test fail while the superset test above
/// keeps passing; reverted before commit.)
#[test]
fn unlimited_budget_never_skips_s4_s7_stages_in_report() {
    assert!(
        !Ft8Config::default().a7_enabled,
        "this test's S7 absence assertion assumes a7_enabled defaults to false"
    );

    for fixture_path in ["basicft8/live_now.wav", "jtdx/190227_155815.wav"] {
        let samples = read_fixture_window(fixture_path);
        let mut d = Ft8Decoder::new(Ft8Config::default()).unwrap();
        let (_msgs, report) = d
            .decode_window_budgeted(&samples, DecodeBudget::unlimited())
            .unwrap();

        assert!(
            !report.budget_exhausted,
            "{fixture_path}: an unlimited budget must never report exhaustion"
        );

        // Every S4/S5/S6 entry (S5 may push multiple entries, one per
        // round) must show zero skipped work under an unlimited
        // budget. A hardcoded always-skip checkpoint would fail this.
        for label in ["S4-cross-cycle", "S5-multipass", "S6-joint-pair"] {
            let entries: Vec<_> = report
                .stages
                .iter()
                .filter(|(l, _, _, _)| *l == label)
                .collect();
            assert!(
                !entries.is_empty(),
                "{fixture_path}: {label} must appear in the report"
            );
            for entry in &entries {
                assert_eq!(
                    entry.3, 0,
                    "{fixture_path}: {label} must not skip any work under an unlimited budget \
                     (entry: {entry:?})"
                );
            }
        }

        let s4_decoded: u32 = report
            .stages
            .iter()
            .filter(|(l, _, _, _)| *l == "S4-cross-cycle")
            .map(|(_, _, decoded, _)| *decoded)
            .sum();
        let s5_decoded: u32 = report
            .stages
            .iter()
            .filter(|(l, _, _, _)| *l == "S5-multipass")
            .map(|(_, _, decoded, _)| *decoded)
            .sum();

        match fixture_path {
            "basicft8/live_now.wav" => assert!(
                s4_decoded > 0,
                "{fixture_path}: expected S4-cross-cycle to actually decode something \
                 (this fixture is known to exercise it); got 0 — the test fixture may have \
                 stopped exercising this stage"
            ),
            "jtdx/190227_155815.wav" => assert!(
                s5_decoded > 0,
                "{fixture_path}: expected S5-multipass to actually decode something \
                 (this fixture is known to exercise it); got 0 — the test fixture may have \
                 stopped exercising this stage"
            ),
            _ => unreachable!(),
        }

        assert!(
            report.stages.iter().all(|(l, _, _, _)| *l != "S7-a7"),
            "{fixture_path}: a7_enabled defaults to false, so S7 should never enter its checkpoint"
        );
    }
}
