//! PAN-313 internal evaluation collector: fixture A/B evidence for the
//! budget-gated legacy per-symbol FFT fallback (`decoder.rs`).
//!
//! Two modes:
//!
//! * `--mode baseline` — captures the FROZEN unlimited-budget decode sets
//!   for every committed fixture WAV at exactly 151,680, 162,000 and
//!   full-recording sample lengths. Run this against the PARENT revision
//!   before changing decoder behavior; `wav_decode_tests.rs::
//!   unlimited_fft_fallback_matches_frozen_baseline` compares the live
//!   decoder against the committed JSON forever after.
//! * `--mode results` — per-recording paired A/B records (message sets,
//!   window wall time, S1-floor wall time, `fft-fallback` telemetry when
//!   the running binary publishes it, and the full stage rows) for the
//!   unlimited and 250 ms Standard arms, one warmup plus N alternating
//!   measured repetitions per arm. `scripts/pan313_compare.py` consumes
//!   a parent/candidate pair of these files.
//!
//! The same source builds against the parent revision (which publishes no
//! `fft-fallback` row): the fallback counters are then recorded as
//! `null` ("unavailable") rather than invented zeros.
//!
//! Everything here is internal tooling: no customer CLI, no research
//! scorecard schema, no public API.

use std::time::{Duration, Instant};

use pancetta_ft8::{DecodeBudget, Ft8Config, Ft8Decoder, SAMPLE_RATE, WINDOW_SAMPLES};

const STANDARD_BUDGET_MS: u64 = 250;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode = "results".to_string();
    let mut manifest: Option<String> = None;
    let mut out_path: Option<String> = None;
    let mut repetitions = 1usize;
    let mut warmups = 1usize;
    let mut threads = 2usize;
    let mut revision = std::env::var("PAN313_REVISION").unwrap_or_else(|_| "unknown".into());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                i += 1;
                manifest = Some(args.get(i).expect("--manifest needs a path").clone());
            }
            "--mode" => {
                i += 1;
                mode = args.get(i).expect("--mode needs a value").clone();
            }
            "--out" => {
                i += 1;
                out_path = Some(args.get(i).expect("--out needs a value").clone());
            }
            "--repetitions" => {
                i += 1;
                repetitions = args
                    .get(i)
                    .expect("--repetitions needs a value")
                    .parse()
                    .expect("--repetitions must be an integer");
            }
            "--warmups" => {
                i += 1;
                warmups = args
                    .get(i)
                    .expect("--warmups needs a value")
                    .parse()
                    .expect("--warmups must be an integer");
            }
            "--threads" => {
                i += 1;
                threads = args
                    .get(i)
                    .expect("--threads needs a value")
                    .parse()
                    .expect("--threads must be an integer");
            }
            "--revision" => {
                i += 1;
                revision = args.get(i).expect("--revision needs a value").clone();
            }
            other => panic!("unknown argument: {other}"),
        }
        i += 1;
    }
    if let Ok(head) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
    {
        if head.status.success() {
            let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
            if revision == "unknown" {
                revision = head;
            }
        }
    }
    if revision == "unknown" {
        eprintln!("warning: revision is unknown (pass --revision or PAN313_REVISION)");
    }

    assert!(
        repetitions == 1 && threads > 0,
        "use one measured repetition per invocation; alternate parent/candidate externally"
    );

    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .expect("rayon pool");

    let fixtures = match manifest {
        Some(path) => {
            let data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).expect("read manifest"))
                    .expect("parse manifest");
            data["entries"]
                .as_array()
                .expect("manifest entries")
                .iter()
                .map(|entry| {
                    let path = entry["wav_path"].as_str().expect("wav_path");
                    let bytes = std::fs::read(path).expect("manifest WAV missing");
                    assert_eq!(
                        sha256_hex(&bytes),
                        entry["wav_sha256"].as_str().expect("wav_sha256"),
                        "manifest WAV hash mismatch: {path}"
                    );
                    std::fs::canonicalize(path)
                        .expect("WAV path")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        }
        None => all_wav_fixtures(),
    };
    assert!(
        !fixtures.is_empty(),
        "expected fixture WAVs under tests/fixtures/wav/"
    );

    let json = match mode.as_str() {
        "baseline" => run_baseline(&fixtures, &revision),
        "results" => run_results(&fixtures, &revision, repetitions, warmups),
        other => panic!("unknown --mode {other:?} (expected baseline|results)"),
    };
    let text = serde_json::to_string_pretty(&json).expect("serialize");
    match out_path {
        Some(path) => {
            std::fs::write(&path, &text).expect("write --out");
            eprintln!("wrote {} bytes to {path}", text.len());
        }
        None => println!("{text}"),
    }
}

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

fn wav_path(subpath: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wav")
        .join(subpath)
}

fn read_wav(subpath: &str) -> Vec<f32> {
    let path = wav_path(subpath).display().to_string();
    let reader =
        hound::WavReader::open(&path).unwrap_or_else(|e| panic!("failed to open {path}: {e}"));
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "{path}: expected mono");
    assert_eq!(spec.sample_rate, SAMPLE_RATE, "{path}: expected 12 kHz");
    reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// One unlimited-budget decode at an exact window length with a FRESH
/// decoder (no cross-window history drift), returning the sorted unique
/// message texts.
fn decode_texts_at_length(
    samples_full: &[f32],
    length: usize,
    budget: DecodeBudget,
) -> Vec<String> {
    let window = &samples_full[..length.min(samples_full.len())];
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).expect("decoder");
    let (msgs, _report) = decoder
        .decode_window_budgeted(window, budget)
        .expect("decode");
    let mut texts: Vec<String> = msgs.into_iter().map(|m| m.text).collect();
    texts.sort();
    texts.dedup();
    texts
}

fn window_lengths(samples_full: &[f32]) -> Vec<(String, usize)> {
    let full = samples_full.len();
    assert!(full >= 162_000, "recording too short for requested lengths");
    vec![
        ("151680".into(), WINDOW_SAMPLES.min(full)),
        ("162000".into(), 162_000.min(full)),
        ("full".into(), full),
    ]
}

fn run_baseline(fixtures: &[String], revision: &str) -> serde_json::Value {
    let mut recordings = Vec::new();
    for path in fixtures {
        let bytes = std::fs::read(wav_path(path)).expect("read wav bytes");
        let samples = read_wav(path);
        let mut lengths = serde_json::Map::new();
        for (label, len) in window_lengths(&samples) {
            let texts = decode_texts_at_length(&samples, len, DecodeBudget::unlimited());
            if texts.is_empty() {
                eprintln!("baseline: note: {path}@{label} decoded no messages");
            }
            lengths.insert(label, serde_json::json!(texts));
        }
        recordings.push(serde_json::json!({
            "path": path,
            "sha256": sha256_hex(&bytes),
            "samples": samples.len(),
            "lengths": lengths,
        }));
        eprintln!("baseline: {path} ({} samples)", samples.len());
    }
    serde_json::json!({
        "schema": 1,
        "kind": "pan313-fft-fallback-baseline",
        "source_revision": revision,
        "generated_at": now_iso(),
        "recordings": recordings,
    })
}

fn now_iso() -> String {
    // std-only: format the system time as best-effort ISO-8601.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch");
    format!("{}.{:03}Z", unix_to_iso(now.as_secs()), now.subsec_millis())
}

/// Days-from-civil inverse (Howard Hinnant's algorithm) for a
/// dependency-free UTC timestamp.
fn unix_to_iso(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}")
}

struct ArmRecord {
    length_label: String,
    budget_label: String,
    texts: Vec<String>,
    window_wall_ms: Vec<u128>,
    s1_floor_ms: Vec<u128>,
    fft_fallback_seen: bool,
    fft_fallback_elapsed_ms: u128,
    fft_fallback_attempts: u64,
    fft_fallback_skipped: u64,
    stages: Vec<serde_json::Value>,
}

fn run_arm(
    samples_full: &[f32],
    length: usize,
    standard: bool,
    reps: usize,
    warmups: usize,
) -> ArmRecord {
    let mut rec = ArmRecord {
        length_label: String::new(),
        budget_label: if standard {
            "standard".into()
        } else {
            "unlimited".into()
        },
        texts: Vec::new(),
        window_wall_ms: Vec::new(),
        s1_floor_ms: Vec::new(),
        fft_fallback_seen: false,
        fft_fallback_elapsed_ms: 0,
        fft_fallback_attempts: 0,
        fft_fallback_skipped: 0,
        stages: Vec::new(),
    };
    for rep in 0..(warmups + reps) {
        let mut decoder = Ft8Decoder::new(Ft8Config::default()).expect("decoder");
        let budget = if standard {
            DecodeBudget::until(Instant::now() + Duration::from_millis(STANDARD_BUDGET_MS))
        } else {
            DecodeBudget::unlimited()
        };
        let window = &samples_full[..length.min(samples_full.len())];
        let start = Instant::now();
        let (msgs, report) = decoder
            .decode_window_budgeted(window, budget)
            .expect("decode");
        let wall = start.elapsed().as_millis();
        if rep >= warmups {
            let mut texts: Vec<String> = msgs.into_iter().map(|m| m.text).collect();
            texts.sort();
            texts.dedup();
            if !standard && !rec.window_wall_ms.is_empty() {
                assert_eq!(
                    rec.texts, texts,
                    "unlimited sets drifted between repetitions"
                );
            }
            rec.texts = texts;
            rec.window_wall_ms.push(wall);
            rec.s1_floor_ms.push(
                report
                    .stages
                    .iter()
                    .filter(|(l, _, _, _)| *l == "S1-floor")
                    .map(|(_, ms, _, _)| *ms as u128)
                    .sum(),
            );
            if let Some((_, ms, attempts, skipped)) = report
                .stages
                .iter()
                .find(|(l, _, _, _)| *l == "fft-fallback")
            {
                rec.fft_fallback_seen = true;
                rec.fft_fallback_elapsed_ms = *ms as u128;
                rec.fft_fallback_attempts = *attempts as u64;
                rec.fft_fallback_skipped = *skipped as u64;
            }
            rec.stages = report
                .stages
                .iter()
                .map(|(l, ms, done, skipped)| serde_json::json!([l, ms, done, skipped]))
                .collect();
        }
    }
    rec
}

fn cpu_model() -> String {
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in info.lines() {
            if let Some(rest) = line.strip_prefix("model name") {
                if let Some(v) = rest.split(':').nth(1) {
                    return v.trim().to_string();
                }
            }
        }
    }
    "unknown".into()
}

fn cgroup_cpu_quota() -> serde_json::Value {
    if let Ok(v2) = std::fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let v2 = v2.trim().to_string();
        return serde_json::json!({"version": 2, "cpu.max": v2});
    }
    if let (Ok(q), Ok(p)) = (
        std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us"),
        std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us"),
    ) {
        return serde_json::json!({
            "version": 1,
            "quota_us": q.trim().to_string(),
            "period_us": p.trim().to_string(),
        });
    }
    serde_json::json!("unavailable")
}

fn rustc_version() -> String {
    std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn arm_json(rec: &ArmRecord) -> serde_json::Value {
    let fallback = if rec.fft_fallback_seen {
        serde_json::json!({
            "elapsed_ms": rec.fft_fallback_elapsed_ms,
            "attempts": rec.fft_fallback_attempts,
            "budget_skips": rec.fft_fallback_skipped,
        })
    } else {
        serde_json::Value::Null
    };
    serde_json::json!({
        "length": rec.length_label,
        "budget": rec.budget_label,
        "texts": rec.texts,
        "window_wall_ms": rec.window_wall_ms,
        "s1_floor_ms": rec.s1_floor_ms,
        "fft_fallback": fallback,
        "stages": rec.stages,
    })
}

fn run_results(
    fixtures: &[String],
    revision: &str,
    reps: usize,
    warmups: usize,
) -> serde_json::Value {
    let mut recordings = Vec::new();
    for path in fixtures {
        let bytes = std::fs::read(wav_path(path)).expect("read wav bytes");
        let samples = read_wav(path);
        let mut arms = Vec::new();
        for (label, len) in window_lengths(&samples) {
            // Budgets are separate runs. Alternate parent/candidate binaries
            // externally with --repetitions 1 for paired timing.
            for standard in [false, true] {
                let mut rec = run_arm(&samples, len, standard, reps, warmups);
                rec.length_label = label.clone();
                arms.push(arm_json(&rec));
            }
        }
        recordings.push(serde_json::json!({
            "path": path,
            "sha256": sha256_hex(&bytes),
            "samples": samples.len(),
            "arms": arms,
        }));
        eprintln!("results: {path} done",);
    }
    serde_json::json!({
        "schema": 1,
        "kind": "pan313-fft-fallback-results",
        "revision": revision,
        "generated_at": now_iso(),
        "host": {
            "cpu_model": cpu_model(),
            "cgroup_cpu": cgroup_cpu_quota(),
            "rayon_threads": rayon::current_num_threads(),
            "rustc": rustc_version(),
        },
        "config": {
            "protocol": "ft8",
            "standard_budget_ms": STANDARD_BUDGET_MS,
            "repetitions": reps,
            "warmups": warmups,
            "budget_anchor": "after-decoder-construction-before-decode",
        },
        "recordings": recordings,
    })
}
