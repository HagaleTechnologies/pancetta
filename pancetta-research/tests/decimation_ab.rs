//! PAN-320 A/B harness tests (AC5). Local-only like the whole research
//! crate; `pancetta-research` is never built in CI.
//!
//! * `stereo_uses_channel_zero` — channel 0 alone feeds both arms; a
//!   right-channel-only signal must decode like silence (catches any
//!   accidental channel averaging).
//! * `paired_decode_and_statistics` — deterministic dedup, SNR
//!   distributions (nearest-rank), paired deltas, fir/sub-only messages,
//!   nonfinite-SNR handling, and the tie-keeps-subsample rule end to end.
//! * `rejects_invalid_corpus` — malformed manifests and WAVs are refused.
//! * `cli_errors_do_not_replace_report` — the binary's exit codes, stderr
//!   contract, and output preservation on failure.
//! * `strict_count_decision` — win/tie/loss/missing decisions.
//! * `encoder_roundtrip_both_arms` — a real encoder→modulator→decimator→
//!   decoder paired test: in-band parity, and the FIR never loses.

use pancetta_research::decimation_ab::{
    self, compare_captures, decode_prefix_samples, dedupe_by_text, load_capture,
    nearest_rank_quantile, strict_count_decision as decide, validate_manifest, ArmResult,
    CaptureEntry, CaptureManifest, DecimationFilter, DecodeArm, DecodeRecord, LoadedCapture,
    SnrDistribution, CAPTURE_FRAMES, CAPTURE_RATE_HZ, CONDITIONS, MANIFEST_VERSION,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Lcg(u64);
impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64)
    }
    fn next_f32(&mut self) -> f32 {
        self.next_f64() as f32
    }
    fn gaussian(&mut self) -> f32 {
        // Box-Muller with a fixed second sample.
        let u1 = self.next_f64().max(1e-12);
        let u2 = self.next_f64();
        ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
    }
}

/// Complex amplitude of a tone at `hz` in `samples` at sample rate `fs`.
fn tone_amplitude(samples: &[f32], hz: f64, fs: f64) -> f64 {
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    let w = 2.0 * std::f64::consts::PI * hz / fs;
    for (n, &s) in samples.iter().enumerate() {
        let ph = w * n as f64;
        re += s as f64 * ph.cos();
        im += s as f64 * ph.sin();
    }
    2.0 * (re * re + im * im).sqrt() / samples.len() as f64
}

fn rms(samples: &[f32]) -> f64 {
    ((samples
        .iter()
        .map(|s| (*s as f64) * (*s as f64))
        .sum::<f64>()
        / samples.len().max(1) as f64)
        .sqrt())
}

/// Write a PCM16 WAV with per-channel sample producers; returns its path
/// and sha256. `frame_count` frames at 48 kHz.
fn write_wav(
    dir: &std::path::Path,
    name: &str,
    channels: usize,
    frame_count: usize,
    sample_rate: u32,
    make: impl Fn(usize, usize) -> f32,
) -> (PathBuf, String) {
    let path = dir.join(name);
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for frame in 0..frame_count {
        for ch in 0..channels {
            let s = (make(ch, frame) * i16::MAX as f32).clamp(-32768.0, 32767.0) as i16;
            writer.write_sample(s).unwrap();
        }
    }
    writer.finalize().unwrap();
    let sha = decimation_ab::sha256_file(&path).unwrap();
    (path, sha)
}

fn entry(path: &str, sha256: &str, condition: &str) -> CaptureEntry {
    CaptureEntry {
        path: path.to_string(),
        sha256: sha256.to_string(),
        condition: condition.to_string(),
        lead_seconds: 0.5,
    }
}

fn manifest(captures: Vec<CaptureEntry>) -> CaptureManifest {
    CaptureManifest {
        version: MANIFEST_VERSION,
        captures,
    }
}

/// One valid manifest per condition writing distinct quiet WAVs into
/// `dir` (distinct tone per condition so the hashes are unique).
fn valid_manifest(dir: &std::path::Path) -> CaptureManifest {
    let mut captures = Vec::new();
    for (idx, cond) in CONDITIONS.iter().enumerate() {
        let tone_hz = 700.0 + 130.0 * idx as f64;
        let (_, sha) = write_wav(
            dir,
            &format!("{cond}.wav"),
            1,
            CAPTURE_FRAMES,
            CAPTURE_RATE_HZ,
            {
                let tone_hz = tone_hz;
                move |_, i| {
                    0.001
                        * ((2.0 * std::f64::consts::PI * tone_hz * i as f64 / 48000.0).sin()) as f32
                }
            },
        );
        captures.push(entry(&format!("{cond}.wav"), &sha, cond));
    }
    manifest(captures)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A stereo capture with signal only in the right channel must decode
/// like a silent channel 0 in *both* arms (channel 0 alone feeds both).
#[test]
fn stereo_uses_channel_zero() {
    let tmp = tempfile::tempdir().unwrap();
    // Left: near-silence. Right: a loud 1 kHz tone. A mono mixdown (or an
    // average) would put amplitude ~0.25 into both arms.
    let (path, sha) = write_wav(
        tmp.path(),
        "busy.wav",
        2,
        CAPTURE_FRAMES,
        CAPTURE_RATE_HZ,
        |ch, i| {
            let t = i as f64 / 48000.0;
            if ch == 0 {
                0.0
            } else {
                (0.5 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as f32
            }
        },
    );
    let e = entry("busy.wav", &sha, "busy");
    let loaded = load_capture(tmp.path(), &e).unwrap();
    assert_eq!(loaded.channel_count, 2);
    assert!(
        rms(&loaded.channel0) < 0.01,
        "channel 0 must be silent, got rms {}",
        rms(&loaded.channel0)
    );

    struct RmsDecode;
    impl DecodeArm for RmsDecode {
        fn decode(&mut self, window: &[f32]) -> Vec<DecodeRecord> {
            if rms(window) > 0.05 {
                vec![DecodeRecord {
                    text: "LOUD RIGHT CHANNEL".into(),
                    snr_db: 10.0,
                    frequency_hz: 1000.0,
                    time_offset_s: 0.0,
                }]
            } else {
                Vec::new()
            }
        }
    }
    let mut decode = RmsDecode;
    let report = compare_captures(std::slice::from_ref(&loaded), &mut decode, true);
    assert_eq!(report.captures.len(), 1);
    let c = &report.captures[0];
    assert_eq!(
        c.subsample.merged_decodes, 0,
        "subsample arm uses channel 0"
    );
    assert_eq!(c.fir31.merged_decodes, 0, "fir31 arm uses channel 0");
    let _ = &path;
}

/// Deterministic statistics: dedup keeps the highest SNR per text, SNR
/// quantiles are nearest-rank, nonfinite SNR is counted but excluded,
/// fir/sub-only messages are separated, and a decode-count tie keeps
/// subsample regardless of a better paired SNR.
#[test]
fn paired_decode_and_statistics() {
    // Channel 0 content: a 500 Hz pilot (passes both arms) + a 9.5 kHz
    // tone (folds to 2.5 kHz only under subsampling) — the callback keys
    // its arm identity off the folded tone's amplitude.
    let mut lcg = Lcg(0xC0FFEE);
    let mut channel0 = vec![0.0f32; CAPTURE_FRAMES];
    for (i, s) in channel0.iter_mut().enumerate() {
        let t = i as f64 / 48000.0;
        *s = (0.3 * (2.0 * std::f64::consts::PI * 500.0 * t).sin()
            + 0.8 * (2.0 * std::f64::consts::PI * 9500.0 * t).sin()) as f32
            + 0.002 * lcg.gaussian();
    }
    let loaded = LoadedCapture {
        entry: entry("busy.wav", &"0".repeat(64), "busy"),
        channel_count: 1,
        channel0,
    };

    struct ArmProbe;
    impl DecodeArm for ArmProbe {
        fn decode(&mut self, window: &[f32]) -> Vec<DecodeRecord> {
            let folded = tone_amplitude(window, 2500.0, 12_000.0);
            let shared_snr = if folded > 0.3 { -10.0 } else { -5.0 };
            let mut recs = vec![
                DecodeRecord {
                    text: "BOTH ARMS".into(),
                    snr_db: shared_snr,
                    frequency_hz: 500.0,
                    time_offset_s: 0.1,
                },
                // Duplicate text in the same arm: lower SNR must be dropped.
                DecodeRecord {
                    text: "BOTH ARMS".into(),
                    snr_db: shared_snr - 7.0,
                    frequency_hz: 300.0,
                    time_offset_s: 0.2,
                },
                // Nonfinite SNR: recorded, excluded from quantiles.
                DecodeRecord {
                    text: "NAN SNR".into(),
                    snr_db: f32::NAN,
                    frequency_hz: 900.0,
                    time_offset_s: 0.3,
                },
            ];
            if folded > 0.3 {
                // Subsample arm (folded tone present).
                recs.push(DecodeRecord {
                    text: "SUBONLY".into(),
                    snr_db: 1.0,
                    frequency_hz: 2500.0,
                    time_offset_s: 0.4,
                });
            } else {
                // FIR arm.
                recs.push(DecodeRecord {
                    text: "FIRONLY".into(),
                    snr_db: 2.0,
                    frequency_hz: 600.0,
                    time_offset_s: 0.4,
                });
            }
            recs
        }
    }

    let mut probe = ArmProbe;
    let report = compare_captures(std::slice::from_ref(&loaded), &mut probe, true);
    assert_eq!(report.captures.len(), 1);
    let c = &report.captures[0];

    // Dedup: 3 records -> 2 unique texts per arm ("BOTH ARMS", plus the
    // arm-only one; the NaN record is its own text and counts once).
    assert_eq!(c.subsample.merged_decodes, 3); // BOTH ARMS, NAN SNR, SUBONLY
    assert_eq!(c.fir31.merged_decodes, 3); // BOTH ARMS, NAN SNR, FIRONLY
    assert_eq!(c.subsample_only, vec!["SUBONLY".to_string()]);
    assert_eq!(c.fir_only, vec!["FIRONLY".to_string()]);
    assert_eq!(c.paired_n, 2); // BOTH ARMS + NAN SNR present in both arms
                               // Paired delta median over [-5.0 - (-10.0), NaN]: finite part +5 dB,
                               // the NaN pair contributes NaN which nearest-rank sorts last.
    let delta = c.paired_snr_delta.unwrap();
    assert!((delta - 5.0).abs() < 1e-6, "paired delta {delta}");

    // Nonfinite SNR: counted separately, excluded from quantiles.
    assert_eq!(c.subsample.snr.nonfinite, 1);
    assert_eq!(c.subsample.snr.n, 2);

    // Totals tie (3 vs 3): subsample stays the default even though the
    // FIR's paired SNR is 5 dB better — the ticket requires MORE decodes.
    assert_eq!(
        report.decision.selected_default, "subsample",
        "tie must keep subsample regardless of SNR"
    );
    assert_eq!(report.decision.decode_delta, Some(0));
    assert!(report.decision.reason.contains("tie"));

    // The report carries the filter coefficients and channel convention.
    assert_eq!(report.filter_coefficients.len(), 31);
    assert_eq!(report.decimation["channel"], "channel_0");
}

#[test]
fn rejects_invalid_corpus() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Baseline valid manifest.
    let ok = valid_manifest(dir);
    validate_manifest(&ok).unwrap();

    // Wrong version.
    let mut m = valid_manifest(dir);
    m.version = 2;
    assert!(validate_manifest(&m).is_err(), "wrong version");

    // Empty.
    assert!(
        validate_manifest(&manifest(Vec::new())).is_err(),
        "empty corpus"
    );

    // Missing condition.
    let mut m = valid_manifest(dir);
    m.captures.retain(|c| c.condition != "quiet");
    assert!(validate_manifest(&m).is_err(), "missing condition");

    // Duplicate paths and duplicate hashes.
    let mut m = valid_manifest(dir);
    let clone = m.captures[0].clone();
    m.captures.push(clone);
    assert!(validate_manifest(&m).is_err(), "duplicate path+hash");

    let mut m = valid_manifest(dir);
    m.captures[1].sha256 = m.captures[0].sha256.clone();
    assert!(validate_manifest(&m).is_err(), "duplicate hash");

    // Malformed hash.
    let mut m = valid_manifest(dir);
    m.captures[0].sha256 = "xyz".into();
    assert!(validate_manifest(&m).is_err(), "malformed hash");

    // Bad lead and unknown condition.
    let mut m = valid_manifest(dir);
    m.captures[0].lead_seconds = 0.7;
    assert!(validate_manifest(&m).is_err(), "bad lead_seconds");
    let mut m = valid_manifest(dir);
    m.captures[0].condition = "sunny".into();
    assert!(validate_manifest(&m).is_err(), "unknown condition");

    // WAV-level rejections through load_capture:
    let (path, sha) = write_wav(dir, "wrongrate.wav", 1, CAPTURE_FRAMES, 44_100, |_, i| {
        0.1 * (i as f32 / 64.0).sin()
    });
    let err = load_capture(dir, &entry("wrongrate.wav", &sha, "quiet")).unwrap_err();
    assert!(err.to_string().contains("44100"), "{err}");
    let _ = path;

    let (path, sha) = write_wav(
        dir,
        "short.wav",
        1,
        CAPTURE_FRAMES - 480,
        CAPTURE_RATE_HZ,
        |_, i| 0.1 * (i as f32 / 64.0).sin(),
    );
    let err = load_capture(dir, &entry("short.wav", &sha, "quiet")).unwrap_err();
    assert!(err.to_string().contains("frames"), "{err}");
    let _ = path;

    // Non-PCM16 (float) WAV.
    let float_path = dir.join("float.wav");
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: CAPTURE_RATE_HZ,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&float_path, spec).unwrap();
        for i in 0..CAPTURE_FRAMES {
            w.write_sample(0.1f32 * (i as f32 / 64.0).sin()).unwrap();
        }
        w.finalize().unwrap();
    }
    let sha = decimation_ab::sha256_file(&float_path).unwrap();
    let err = load_capture(dir, &entry("float.wav", &sha, "quiet")).unwrap_err();
    assert!(err.to_string().contains("PCM16"), "{err}");

    // Hash mismatch: rejected before either arm decodes anything.
    let (path, sha) = write_wav(
        dir,
        "hashy.wav",
        1,
        CAPTURE_FRAMES,
        CAPTURE_RATE_HZ,
        |_, i| 0.1 * (i as f32 / 64.0).sin(),
    );
    let mut bad = entry("hashy.wav", &sha, "quiet");
    bad.sha256 = "f".repeat(64);
    assert!(load_capture(dir, &bad).is_err(), "hash mismatch");
    let _ = path;
}

/// The binary's CLI contract: exit codes, stderr lines, and — critically —
/// a failed run never creates or replaces the output report.
#[test]
fn cli_errors_do_not_replace_report() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let exe = env!("CARGO_BIN_EXE_decimation-ab");
    let out_path = dir.join("result.json");
    std::fs::write(&out_path, b"sentinel").unwrap();

    let run = |args: &[&str]| std::process::Command::new(exe).args(args).output().unwrap();

    // Missing manifest file.
    let out = run(&[
        "--manifest",
        dir.join("missing.json").to_str().unwrap(),
        "--output",
        out_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("decimation-ab: cannot read capture manifest"),
        "stderr: {stderr}"
    );
    assert_eq!(std::fs::read(&out_path).unwrap(), b"sentinel");

    // Invalid manifest JSON.
    let bad_json = dir.join("bad.json");
    std::fs::write(&bad_json, "{ not json").unwrap();
    let out = run(&[
        "--manifest",
        bad_json.to_str().unwrap(),
        "--output",
        out_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("decimation-ab: invalid capture input"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("bad.json"),
        "offending path must follow: {stderr}"
    );
    assert_eq!(std::fs::read(&out_path).unwrap(), b"sentinel");

    // Structurally invalid manifest (missing condition) — same class.
    let one = json!({"version": 1, "captures": [{
        "path": "quiet.wav", "sha256": "0".repeat(64),
        "condition": "quiet", "lead_seconds": 0.5
    }]});
    std::fs::write(&bad_json, one.to_string()).unwrap();
    let out = run(&[
        "--manifest",
        bad_json.to_str().unwrap(),
        "--output",
        out_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid capture input"));
    assert_eq!(std::fs::read(&out_path).unwrap(), b"sentinel");
}

/// The strict count decision: FIR only on a strictly larger total; ties
/// and losses keep subsample; missing evidence keeps subsample pending.
#[test]
fn strict_count_decision() {
    // Win.
    let d = decide(Some(10), Some(11));
    assert_eq!(d.selected_default, "fir31");
    assert_eq!(d.decode_delta, Some(1));

    // Tie keeps subsample even though SNR is not part of the rule.
    let d = decide(Some(11), Some(11));
    assert_eq!(d.selected_default, "subsample");
    assert_eq!(d.decode_delta, Some(0));
    assert!(d.reason.contains("tie"));

    // Loss.
    let d = decide(Some(12), Some(11));
    assert_eq!(d.selected_default, "subsample");
    assert_eq!(d.decode_delta, Some(-1));

    // Missing input (either side).
    for (s, f) in [(None, Some(5)), (Some(5), None), (None, None)] {
        let d = decide(s, f);
        assert_eq!(d.selected_default, "subsample", "missing evidence");
        assert!(d.subsample_total.is_none() && d.fir31_total.is_none());
        assert!(d.reason.contains("pending"));
    }
}

// ---------------------------------------------------------------------------
// Real encoder → modulator → decimator → decoder paired test
// ---------------------------------------------------------------------------

/// Upsample 12 kHz → 48 kHz by zero-insertion + a 63-tap windowed-sinc
/// low-pass (cutoff 6 kHz), then round-trip i16 like the raw48 recorder.
fn upsample_to_48k(input: &[f32]) -> Vec<f32> {
    let n = input.len();
    let taps: Vec<f64> = {
        let m = 31.0f64;
        let fc = 2.0 * 6_000.0f64 / 48_000.0;
        (0..63)
            .map(|k| {
                let t = k as f64 - m;
                let sinc = if t.abs() < 1e-9 {
                    fc
                } else {
                    (std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t)
                };
                // Hann window: enough for a synthetic fixture (this is
                // test signal generation, not a shipped filter).
                let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * k as f64 / 62.0).cos());
                sinc * w * 4.0
            })
            .collect()
    };
    let mut out = vec![0.0f64; n * 4];
    for (k, &s) in input.iter().enumerate() {
        let base = k * 4;
        for (j, &h) in taps.iter().enumerate() {
            let idx = base as isize + j as isize - 31;
            if idx >= 0 && (idx as usize) < out.len() {
                out[idx as usize] += h * s as f64;
            }
        }
    }
    // i16 round trip, exactly like the raw48 writer.
    out.iter()
        .map(|&s| ((s * i16::MAX as f64).clamp(-32768.0, 32767.0) as i16) as f32 / 32768.0)
        .collect()
}

/// Real encoder/modulator-to-decoder paired test: an in-band FT8 signal
/// decodes identically through both arms; adding a strong 9.5 kHz
/// interferer never makes the FIR lose to subsampling.
#[test]
fn encoder_roundtrip_both_arms() {
    use pancetta_ft8::{Ft8Decoder, Ft8Encoder, Ft8Modulator};

    let build_capture = |interferer: bool| -> LoadedCapture {
        let mut encoder = Ft8Encoder::new();
        let symbols = encoder
            .encode_message("K1ABC W9XYZ EM17", None)
            .expect("encode");
        // Base 1500 + offset 1000 = 2500 Hz: with the interferer, the
        // 9.5 kHz tone folds *onto* the signal under subsampling — the
        // TX-desense mechanism this ticket is about.
        let mut modulator = Ft8Modulator::new(12_000, 1500.0, 1.0).expect("modulator");
        let base = modulator
            .modulate_symbols(&symbols, 1000.0)
            .expect("modulate");
        // Up to 48 kHz (with the i16 round trip inside), like raw48 files.
        let up = upsample_to_48k(&base);

        let mut lcg = Lcg(0x9A11_3201);
        let mut channel0 = vec![0.0f32; CAPTURE_FRAMES];
        // Signal starts at the 0.5 s lead boundary.
        for (k, slot) in channel0.iter_mut().enumerate().skip(24_000) {
            let i = k - 24_000;
            // Signal at 0.3 amplitude: with the 0.8 fold-on-signal tone,
            // the subsample arm's in-band SNR collapses below the FT8
            // floor; the FIR arm sees the same signal almost clean.
            let mut s = if i < up.len() { 0.3 * up[i] } else { 0.0 };
            if interferer {
                s += (0.8 * (2.0 * std::f64::consts::PI * 9_500.0 * k as f64 / 48_000.0).sin())
                    as f32;
            }
            *slot = s + 0.003 * lcg.gaussian();
        }
        LoadedCapture {
            entry: entry(
                if interferer {
                    "desense.wav"
                } else {
                    "busy.wav"
                },
                &"0".repeat(64),
                if interferer { "tx_desense" } else { "busy" },
            ),
            channel_count: 1,
            channel0,
        }
    };

    let prefix = decode_prefix_samples(0.5);
    for (interferer, expect_sub, expect_fir_min) in [(false, 1usize, 1usize), (true, 0, 1)] {
        let capture = build_capture(interferer);
        let sub = decimation_ab::subsample_arm(&capture.channel0);
        let mut fir = DecimationFilter::new();
        let mut fir_stream = Vec::new();
        fir.process(&capture.channel0, &mut fir_stream);

        let decode = |window: &[f32]| -> usize {
            let mut decoder = Ft8Decoder::new(pancetta_ft8::Ft8Config::default()).unwrap();
            decoder
                .decode_window(&window[..prefix])
                .unwrap()
                .iter()
                .map(|m| m.text.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };

        let sub_n = decode(&sub);
        let fir_n = decode(&fir_stream);
        assert_eq!(
            sub_n, expect_sub,
            "interferer={interferer}: subsample arm decode count"
        );
        assert!(
            fir_n >= expect_fir_min,
            "interferer={interferer}: fir31 must decode (got {fir_n})"
        );
        assert!(
            fir_n >= sub_n,
            "interferer={interferer}: fir31 must never trail subsample"
        );
    }
}

// ---------------------------------------------------------------------------
// Unit checks the above rely on
// ---------------------------------------------------------------------------

#[test]
fn quantile_helpers() {
    // Nearest-rank on 10 values: p10 = 1st, median = 5th/6th boundary.
    let v: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
    assert_eq!(nearest_rank_quantile(&v, 10.0), Some(1.0));
    assert_eq!(nearest_rank_quantile(&v, 50.0), Some(5.0));
    assert_eq!(nearest_rank_quantile(&v, 90.0), Some(9.0));
    assert_eq!(nearest_rank_quantile(&v, 100.0), Some(10.0));
    assert_eq!(nearest_rank_quantile(&[], 50.0), None);

    // Dedup keeps highest SNR; frequency breaks exact ties.
    let recs = vec![
        DecodeRecord {
            text: "A".into(),
            snr_db: 1.0,
            frequency_hz: 100.0,
            time_offset_s: 0.0,
        },
        DecodeRecord {
            text: "A".into(),
            snr_db: 5.0,
            frequency_hz: 100.0,
            time_offset_s: 0.0,
        },
        DecodeRecord {
            text: "A".into(),
            snr_db: 5.0,
            frequency_hz: 900.0,
            time_offset_s: 0.0,
        },
    ];
    let kept = dedupe_by_text(recs);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].snr_db, 5.0);
    assert_eq!(kept[0].frequency_hz, 900.0);

    let dist = SnrDistribution::from_snrs(&[1.0, f32::NAN, 3.0]);
    assert_eq!(dist.n, 2);
    assert_eq!(dist.nonfinite, 1);
    assert_eq!(dist.min, Some(1.0));
    assert_eq!(dist.max, Some(3.0));
}

/// Compile-time use of otherwise test-only imports (report struct fields
/// referenced by the qualification guard in the main crate's docs).
#[test]
fn report_struct_shape() {
    let pending = decimation_ab::pending_report("no station captures supplied");
    assert_eq!(pending.status, "pending_station_captures");
    assert!(pending.captures.is_empty());
    assert_eq!(pending.decision.selected_default, "subsample");
    assert_eq!(pending.decision.subsample_total, None);
    let _arm: ArmResult = ArmResult {
        merged_decodes: 0,
        snr: SnrDistribution::default(),
        messages: vec![],
    };
    let _map: BTreeMap<String, serde_json::Value> = BTreeMap::new();
}
