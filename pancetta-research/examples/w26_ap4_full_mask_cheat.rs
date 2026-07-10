//! w26_ap4_full_mask_cheat — Task W2.6 diagnostic
//!
//! The eval harness's stock CLI (`--ap-my-call`/`--ap-recent-calls`) can
//! set `ApContext.my_call`/`recent_calls` for a whole corpus run, but has
//! NO way to construct `ApContext.active_qso` (every CLI-built context
//! hardcodes `active_qso: None`) — the same harness-measurability gap
//! flagged for AP3/AP4 in Tasks W1.1/W1.7. AP4's full message-content
//! mask (RR73/RRR/73) only ever fires when `active_qso` is
//! `Some(QsoApProgress::WaitingForConfirmation)`, so it cannot be
//! exercised via `eval`'s stock corpus-wide flags at all.
//!
//! This mirrors `ap_recovery_ceiling.rs`'s (hb-051) established
//! cheat-informed methodology for exactly this class of problem: for
//! each WAV whose jt9-verified truth IS an RR73/RRR/73 completion
//! message, construct the "perfect information" `ApContext` a real
//! operator mid-QSO would have (my_call = the truth's to_callsign,
//! active_qso = the truth's from_callsign awaiting confirmation) and
//! compare:
//!   (a) plain AP4 (i3-only, `ap4_full_message_mask_enabled: false`)
//!   (b) AP4 + full message-content mask (`ap4_full_message_mask_enabled: true`)
//! against that EXACT truth message, on hard-200.
//!
//! Separately, on noise_1000 (which has no truth messages at all — every
//! WAV is band noise), the SAME fixed `ApContext` (my_call=K5ARH,
//! active_qso=W1AW awaiting confirmation) is applied to every noise WAV
//! to measure whether the full mask increases the false-decode rate
//! GIVEN that AP4 is already active (the fair comparison: does the EXTRA
//! content bias make false positives more likely, conditional on AP4
//! already firing in production for a real mid-QSO operator).
//!
//! Run:
//!   cargo run --release -p pancetta-research --example w26_ap4_full_mask_cheat

use anyhow::Context;
use pancetta_ft8::ap::{ApContext, MyCallAp, QsoAp, QsoApProgress};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn load_wav_samples(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening WAV {}", path.display()))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "WAV {} not 12kHz mono",
        path.display()
    );
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    };
    Ok(samples)
}

fn looks_like_callsign(t: &str) -> bool {
    let len = t.len();
    if !(3..=10).contains(&len) {
        return false;
    }
    let mut has_digit = false;
    let mut has_alpha = false;
    for c in t.chars() {
        if c.is_ascii_digit() {
            has_digit = true;
        } else if c.is_ascii_alphabetic() {
            has_alpha = true;
        } else if c != '/' {
            return false;
        }
    }
    has_digit && has_alpha
}

/// A confirmation-message truth: "<to_call> <from_call> RRR|RR73|73",
/// exactly 3 tokens, both callsign-shaped, last token one of the three
/// canonical tokens.
struct ConfirmationTruth {
    to_call: String,
    from_call: String,
}

fn parse_confirmation_truth(text: &str) -> Option<ConfirmationTruth> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.len() != 3 {
        return None;
    }
    if !matches!(tokens[2], "RRR" | "RR73" | "73") {
        return None;
    }
    if !looks_like_callsign(tokens[0]) || !looks_like_callsign(tokens[1]) {
        return None;
    }
    Some(ConfirmationTruth {
        to_call: tokens[0].to_string(),
        from_call: tokens[1].to_string(),
    })
}

fn run_hard200(workspace: &std::path::Path) -> anyhow::Result<()> {
    let baselines_dir = workspace.join("research/baselines/ft8");
    let manifest_path = workspace.join("research/corpus/curated/ft8/hard_200.manifest.json");

    eprintln!("Loading jt9 baselines from {}...", baselines_dir.display());
    let mut per_wav_truth_msgs: HashMap<String, Vec<String>> = HashMap::new();
    for entry in std::fs::read_dir(&baselines_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let sha = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let s = std::fs::read_to_string(&path)?;
        let v: Value = serde_json::from_str(&s)?;
        let decodes = v
            .get("decodes")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let msgs: Vec<String> = decodes
            .iter()
            .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
            .map(|s| s.trim().to_string())
            .collect();
        per_wav_truth_msgs.insert(sha, msgs);
    }

    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&manifest_str)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;

    // NOTE (post-flip pin): `Ft8Config::default().ap4_full_message_mask_enabled`
    // was flipped to `true` in commit e777fdf4 based on THIS harness's
    // measurement. `baseline_cfg` must therefore explicitly pin the flag
    // back to `false` rather than relying on `Ft8Config::default()` — the
    // whole point of "baseline" here is "plain AP4, no full mask", and
    // since the flip, plain `default()` no longer means that. The
    // `post_norm` variant pins it too, so it isolates the post-norm
    // ordering effect alone rather than silently also picking up the full
    // mask via the (now-true) crate default. See the mirrored fix in
    // `pancetta-ft8/tests/w26_ap_coverage_tests.rs::ap4_full_mask_rescues_signal_that_plain_ap4_cannot_decode`,
    // which caught this exact class of problem for the unit test.
    let baseline_cfg = Ft8Config {
        ap4_full_message_mask_enabled: false,
        ..Ft8Config::default()
    };
    let variants: [(&str, Ft8Config); 2] = [
        (
            "full_mask",
            Ft8Config {
                ap4_full_message_mask_enabled: true,
                ..Ft8Config::default()
            },
        ),
        (
            "post_norm",
            Ft8Config {
                ap4_full_message_mask_enabled: false,
                ap_injection_post_normalization: true,
                ..Ft8Config::default()
            },
        ),
    ];

    let mut confirmation_truths_found = 0usize;
    let mut baseline_matched = 0usize;
    let mut variant_matched = [0usize; 2];
    let mut variant_recovered = [0usize; 2]; // variant matched, baseline did not
    let mut variant_regressed = [0usize; 2]; // baseline matched, variant did not

    for entry in entries.iter() {
        let wav_path = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("entry missing wav_path")?;
        let sha = entry
            .get("wav_sha256")
            .and_then(|s| s.as_str())
            .context("entry missing wav_sha256")?
            .to_string();
        let truth_msgs = per_wav_truth_msgs.get(&sha).cloned().unwrap_or_default();

        for text in &truth_msgs {
            let Some(truth) = parse_confirmation_truth(text) else {
                continue;
            };
            confirmation_truths_found += 1;

            let Some(my_call) = MyCallAp::new(&truth.to_call) else {
                continue;
            };
            let Some(qso) = QsoAp::new(&truth.from_call, QsoApProgress::WaitingForConfirmation)
            else {
                continue;
            };
            let ctx = ApContext {
                my_call: Some(my_call),
                recent_calls: vec![],
                active_qso: Some(qso),
            };

            let samples = match load_wav_samples(&PathBuf::from(wav_path)) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let mut baseline_decoder = Ft8Decoder::new(baseline_cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let baseline_decodes = baseline_decoder
                .decode_window_with_ap(&samples, &ctx)
                .unwrap_or_default();
            let baseline_hit = baseline_decodes.iter().any(|m| m.text.trim() == text);
            if baseline_hit {
                baseline_matched += 1;
            }

            for (i, (_name, cfg)) in variants.iter().enumerate() {
                let mut variant_decoder = Ft8Decoder::new(cfg.clone())
                    .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
                let variant_decodes = variant_decoder
                    .decode_window_with_ap(&samples, &ctx)
                    .unwrap_or_default();
                let variant_hit = variant_decodes.iter().any(|m| m.text.trim() == text);
                if variant_hit {
                    variant_matched[i] += 1;
                }
                if variant_hit && !baseline_hit {
                    variant_recovered[i] += 1;
                }
                if baseline_hit && !variant_hit {
                    variant_regressed[i] += 1;
                }
            }
        }
    }

    println!();
    println!(
        "W2.6 — cheat-informed hard-200 (perfect-information QSO context, RR73/RRR/73 truths only)"
    );
    println!("=============================================================================================");
    println!("RR73/RRR/73 confirmation truths found in hard-200: {confirmation_truths_found}");
    println!("Matched by plain AP4 (i3-only, baseline):           {baseline_matched}");
    for (i, (name, _cfg)) in variants.iter().enumerate() {
        println!(
            "[{name}] matched: {}  recovered(variant hit, baseline miss): {}  regressed(baseline hit, variant miss): {}",
            variant_matched[i], variant_recovered[i], variant_regressed[i]
        );
    }
    Ok(())
}

fn run_noise(workspace: &std::path::Path) -> anyhow::Result<()> {
    let manifest_path = workspace.join("research/corpus/curated/noise/noise_1000.manifest.json");
    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&manifest_str)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;

    // NOTE (post-flip pin): see the matching comment in `run_hard200` —
    // `Ft8Config::default()` now has `ap4_full_message_mask_enabled: true`
    // post-e777fdf4, so `baseline_cfg` and the `post_norm` variant must
    // explicitly pin it back to `false` to keep measuring what their names
    // claim (plain AP4 / post-norm-ordering-only).
    let baseline_cfg = Ft8Config {
        ap4_full_message_mask_enabled: false,
        ..Ft8Config::default()
    };
    let variants: [(&str, Ft8Config); 2] = [
        (
            "full_mask",
            Ft8Config {
                ap4_full_message_mask_enabled: true,
                ..Ft8Config::default()
            },
        ),
        (
            "post_norm",
            Ft8Config {
                ap4_full_message_mask_enabled: false,
                ap_injection_post_normalization: true,
                ..Ft8Config::default()
            },
        ),
    ];

    // Fixed synthetic mid-QSO context applied to every noise WAV — models
    // a real operator with an active QSO awaiting confirmation, since
    // that's the only state that gates the full mask / exercises AP4 at
    // all in production.
    let my_call = MyCallAp::new("K5ARH").expect("K5ARH should encode");
    let qso =
        QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).expect("W1AW should encode");
    let ctx = ApContext {
        my_call: Some(my_call),
        recent_calls: vec![],
        active_qso: Some(qso),
    };

    let mut baseline_fp = 0usize;
    let mut baseline_fp_wavs = 0usize;
    let mut variant_fp = [0usize; 2];
    let mut variant_fp_wavs = [0usize; 2];
    let mut n = 0usize;

    for entry in entries.iter() {
        let wav_path = entry
            .get("wav_path")
            .and_then(|p| p.as_str())
            .context("entry missing wav_path")?;
        let samples = match load_wav_samples(&PathBuf::from(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        n += 1;
        if n.is_multiple_of(100) {
            eprintln!("  {n}/1000  (baseline_fp={baseline_fp} variant_fp={variant_fp:?})");
        }

        let mut baseline_decoder = Ft8Decoder::new(baseline_cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let baseline_decodes = baseline_decoder
            .decode_window_with_ap(&samples, &ctx)
            .unwrap_or_default();
        if !baseline_decodes.is_empty() {
            baseline_fp += baseline_decodes.len();
            baseline_fp_wavs += 1;
        }

        for (i, (name, cfg)) in variants.iter().enumerate() {
            let mut variant_decoder = Ft8Decoder::new(cfg.clone())
                .map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
            let variant_decodes = variant_decoder
                .decode_window_with_ap(&samples, &ctx)
                .unwrap_or_default();
            if !variant_decodes.is_empty() {
                variant_fp[i] += variant_decodes.len();
                variant_fp_wavs[i] += 1;
                for d in &variant_decodes {
                    eprintln!(
                        "    [{name}] FP on {wav_path}: {:?} (ap_level={})",
                        d.text, d.ap_level
                    );
                }
            }
        }
    }

    println!();
    println!("W2.6 — noise_1000 false-positive check (fixed mid-QSO context: K5ARH awaiting W1AW confirmation)");
    println!("=====================================================================================================");
    println!("Noise WAVs processed:                         {n}");
    println!(
        "False positives, plain AP4 (baseline):        {baseline_fp}  ({baseline_fp_wavs} WAVs)"
    );
    for (i, (name, _cfg)) in variants.iter().enumerate() {
        println!(
            "[{name}] false positives: {}  ({} WAVs)",
            variant_fp[i], variant_fp_wavs[i]
        );
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let workspace = workspace_root()?;
    let args: Vec<String> = std::env::args().collect();
    let which = args.get(1).map(|s| s.as_str()).unwrap_or("both");

    if which == "hard200" || which == "both" {
        run_hard200(&workspace)?;
    }
    if which == "noise" || which == "both" {
        run_noise(&workspace)?;
    }
    Ok(())
}
