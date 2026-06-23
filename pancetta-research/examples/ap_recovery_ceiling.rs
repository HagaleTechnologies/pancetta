//! ap_recovery_ceiling — hb-051 diagnostic
//!
//! Bounds the upper limit of what AP could ever contribute on hard-200.
//! For each WAV:
//!   (a) Extract truth callsigns from the jt9 baseline.
//!   (b) Decode pancetta with AP off (baseline).
//!   (c) Decode pancetta with ApContext.recent_calls populated from
//!       the truth callsigns (cheat — what would AP recover if we had
//!       perfect hints?).
//! Difference (c) - (b) is the AP-recovery upper bound on this corpus
//! under perfect-information conditions.
//!
//! Interpretation:
//!   - If ceiling is <0.5% of truth (~43 decodes on hard-200), hb-050
//!     (rolling-window data source) probably isn't worth the
//!     infrastructure investment.
//!   - If >2% (~170 decodes), invest in hb-050 with confidence —
//!     realistic rolling-window will deliver some fraction of the
//!     ceiling.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example ap_recovery_ceiling

use anyhow::Context;
use pancetta_ft8::ap::{ApContext, RecentCallAp};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn callsigns_in(message: &str) -> Vec<String> {
    let upper = message.to_uppercase();
    let tokens: Vec<&str> = upper.split_whitespace().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    if idx < tokens.len() && tokens[idx] == "CQ" {
        idx += 1;
        if idx < tokens.len() && is_cq_modifier(tokens[idx]) {
            idx += 1;
        }
    }
    for t in tokens.iter().skip(idx).take(2) {
        if looks_like_callsign(t) {
            out.push(t.to_string());
        }
    }
    out
}

fn is_cq_modifier(t: &str) -> bool {
    matches!(t, "DX" | "NA" | "SA" | "EU" | "AS" | "AF" | "OC" | "QRP")
        || t.chars().all(|c| c.is_ascii_digit())
        || (t.len() <= 3 && t.chars().all(|c| c.is_ascii_alphabetic()))
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

fn main() -> anyhow::Result<()> {
    let workspace = workspace_root()?;
    let baselines_dir = workspace.join("research/baselines/ft8");
    let manifest_path = workspace.join("research/corpus/curated/ft8/hard_200.manifest.json");

    // Load per-WAV jt9 truth.
    eprintln!("Loading jt9 baselines from {}...", baselines_dir.display());
    let mut per_wav_truth_msgs: HashMap<String, HashSet<String>> = HashMap::new();
    let mut per_wav_truth_calls: HashMap<String, Vec<String>> = HashMap::new();
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
        let mut truth_msgs = HashSet::new();
        let mut truth_calls: HashSet<String> = HashSet::new();
        for d in &decodes {
            if let Some(m) = d.get("message").and_then(|m| m.as_str()) {
                truth_msgs.insert(m.trim().to_string());
                for cs in callsigns_in(m) {
                    truth_calls.insert(cs);
                }
            }
        }
        per_wav_truth_msgs.insert(sha.clone(), truth_msgs);
        per_wav_truth_calls.insert(sha, truth_calls.into_iter().collect());
    }

    // Load hard_200 manifest.
    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: Value = serde_json::from_str(&manifest_str)?;
    let entries = manifest
        .get("entries")
        .and_then(|e| e.as_array())
        .context("manifest missing entries")?;
    eprintln!("Processing {} WAVs...", entries.len());

    let cfg = Ft8Config::default();
    let mut total_truth = 0usize;
    let mut total_ap_off_matched = 0usize;
    let mut total_ap_on_matched = 0usize;
    let mut total_ap_recovery = 0usize; // matched by AP-on but missed by AP-off
    let mut wavs_with_recovery = 0usize;

    for (i, entry) in entries.iter().enumerate() {
        if i % 20 == 0 {
            eprintln!(
                "  {}/{} (recovery so far: {})",
                i,
                entries.len(),
                total_ap_recovery
            );
        }
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
        let truth_calls = per_wav_truth_calls.get(&sha).cloned().unwrap_or_default();
        if truth_msgs.is_empty() {
            continue;
        }
        let samples = match load_wav_samples(&PathBuf::from(wav_path)) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // (b) AP off baseline
        let mut decoder_off =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decodes_off = decoder_off.decode_window(&samples).unwrap_or_default();
        let off_msgs: HashSet<String> = decodes_off
            .iter()
            .map(|d| d.text.trim().to_string())
            .collect();

        // (c) AP on with truth-derived recent_calls
        let recent: Vec<RecentCallAp> = truth_calls
            .iter()
            .filter_map(|c| RecentCallAp::new(c, 0.0))
            .collect();
        if recent.is_empty() {
            // No usable hints — count as zero recovery for this WAV
            let matched_off = truth_msgs
                .iter()
                .filter(|t| {
                    off_msgs
                        .iter()
                        .any(|d| d == *t || d.contains(t.as_str()) || t.contains(d.as_str()))
                })
                .count();
            total_truth += truth_msgs.len();
            total_ap_off_matched += matched_off;
            total_ap_on_matched += matched_off;
            continue;
        }
        let ctx = ApContext {
            my_call: None,
            recent_calls: recent,
            active_qso: None,
        };
        let mut decoder_on =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decodes_on = decoder_on
            .decode_window_with_ap(&samples, &ctx)
            .unwrap_or_default();
        let on_msgs: HashSet<String> = decodes_on
            .iter()
            .map(|d| d.text.trim().to_string())
            .collect();

        // Tally truth matches (loose: any decode that contains or is contained in truth string)
        let mut matched_off = 0usize;
        let mut matched_on = 0usize;
        let mut recovered_here = 0usize;
        for t in &truth_msgs {
            let in_off = off_msgs
                .iter()
                .any(|d| d == t || d.contains(t.as_str()) || t.contains(d.as_str()));
            let in_on = on_msgs
                .iter()
                .any(|d| d == t || d.contains(t.as_str()) || t.contains(d.as_str()));
            if in_off {
                matched_off += 1;
            }
            if in_on {
                matched_on += 1;
            }
            if in_on && !in_off {
                recovered_here += 1;
            }
        }
        total_truth += truth_msgs.len();
        total_ap_off_matched += matched_off;
        total_ap_on_matched += matched_on;
        total_ap_recovery += recovered_here;
        if recovered_here > 0 {
            wavs_with_recovery += 1;
        }
    }

    println!();
    println!("hb-051 — AP-recovery ceiling on hard-200 (perfect-information hints)");
    println!("====================================================================");
    println!("Total truth decodes:              {total_truth}");
    println!("Matched by pancetta AP-off:       {total_ap_off_matched}");
    println!("Matched by pancetta AP-on (truth):{total_ap_on_matched}");
    println!(
        "AP recovery (AP-on - AP-off):     {total_ap_recovery}  ({:.2}% of truth, {:.2}% lift over AP-off)",
        100.0 * total_ap_recovery as f64 / total_truth.max(1) as f64,
        100.0 * total_ap_recovery as f64 / total_ap_off_matched.max(1) as f64
    );
    println!("WAVs with at least one recovery:  {wavs_with_recovery} / 200");
    println!();
    println!("Interpretation:");
    println!("  <0.5% of truth: hb-050 (rolling-window) probably not worth infra investment.");
    println!("  0.5-2% of truth: marginal; depends on cost of hb-050 vs payoff.");
    println!("  >2% of truth: invest in hb-050 with confidence.");
    Ok(())
}
