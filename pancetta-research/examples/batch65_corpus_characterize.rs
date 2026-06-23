//! Batch 65 — corpus characterization framework.
//!
//! Standardized mechanism-relevant metrics computed purely from decoder
//! output (no truth labels required). Tells us, for any corpus
//! (curated manifest OR a directory of raw recordings), which
//! mechanisms it actually exercises.
//!
//! For each corpus, the report covers:
//!
//! 1. **Volume** — slot count, time span (when timestamps are
//!    extractable from filenames), avg decodes / slot.
//! 2. **Signal density** (auto_passband regime) — distribution of
//!    decodes-per-slot (mean / p50 / p90 / p99).
//! 3. **Message-type distribution** (FDR regime) — per-type counts:
//!    Standard / FreeText / Telemetry / DXpedition / NonStdCall /
//!    Contest / Unknown.
//! 4. **SNR distribution** (LLR whitening / Slow-tier regimes) —
//!    histogram of decoded SNRs across coarse bands.
//! 5. **QSO continuity index** (hb-237 regime) — for each (slot N,
//!    slot N+1) pair where filenames imply sequential timing,
//!    fraction of slot-N callsigns appearing in slot-N+1 decodes.
//! 6. **Repeat-heavy index** (hb-244 regime) — distribution of
//!    how many times each unique callsign appears across the corpus;
//!    top-N repeat callers list.
//! 7. **Drift index** (Batch 50 freq tracker regime) — for repeated
//!    callsigns, std-dev of their decoded audio frequency across
//!    receptions.
//! 8. **Per-slot timestamp** when extractable from filename
//!    (ft8_YYYYMMDD_HHMMSS.wav pattern).
//!
//! Inputs:
//!   - Manifest file path (`research/corpus/curated/ft8/*.manifest.json`)
//!     OR a directory of WAVs.
//!
//! Outputs:
//!   - Markdown report to stdout
//!   - JSON sidecar with all metrics for cross-corpus comparison
//!
//! Run:
//!   cargo run --release -p pancetta-research --example batch65_corpus_characterize -- \
//!     --manifest research/corpus/curated/ft8/hard_200.manifest.json
//!
//!   cargo run --release -p pancetta-research --example batch65_corpus_characterize -- \
//!     --dir ~/.pancetta/recordings --filter ft8_20260425_

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder, MessageType};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    anyhow::ensure!(spec.channels == 1 && spec.sample_rate == 12000);
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

/// Extract ham-callsign-shaped tokens from a decoded text (best-effort).
/// A ham callsign is structurally: 1-2 letters (prefix), 1-4 digits
/// (region), then 1-3 letters (suffix). E.g. K1ABC, W9XYZ, JA1ABC.
/// Grid squares (EM75, FN42), reports (R-5, +03), and special tokens
/// (RR73, 73) are filtered out — they pass the letter+digit test but
/// don't have the prefix-digit-suffix structure.
fn extract_callsigns(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        if token.eq_ignore_ascii_case("CQ") || token.starts_with('<') {
            continue;
        }
        let cleaned: String = token
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '/')
            .to_uppercase();
        let canonical = cleaned.split('/').next().unwrap_or(&cleaned).to_string();
        if canonical.len() < 3 || canonical.len() > 7 {
            continue;
        }
        // Walk the canonical token: 1-2 leading letters, 1-4 digits, 1-3 trailing letters.
        let chars: Vec<char> = canonical.chars().collect();
        let mut i = 0;
        let mut prefix_letters = 0;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            prefix_letters += 1;
            i += 1;
            if prefix_letters > 2 {
                break;
            }
        }
        if !(1..=2).contains(&prefix_letters) {
            continue;
        }
        let mut digits = 0;
        while i < chars.len() && chars[i].is_ascii_digit() {
            digits += 1;
            i += 1;
            if digits > 4 {
                break;
            }
        }
        if digits == 0 || digits > 4 {
            continue;
        }
        let mut suffix_letters = 0;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            suffix_letters += 1;
            i += 1;
            if suffix_letters > 3 {
                break;
            }
        }
        if !(1..=3).contains(&suffix_letters) {
            continue;
        }
        if i != chars.len() {
            // trailing junk → not a clean callsign
            continue;
        }
        out.push(canonical);
    }
    out
}

fn timestamp_from_filename(path: &Path) -> Option<i64> {
    // ft8_YYYYMMDD_HHMMSS.wav → epoch seconds (UTC)
    let stem = path.file_stem()?.to_str()?;
    let stripped = stem.strip_prefix("ft8_")?;
    let parts: Vec<&str> = stripped.split('_').collect();
    if parts.len() < 2 {
        return None;
    }
    let date = parts[0]; // YYYYMMDD
    let time = parts[1]; // HHMMSS
    if date.len() != 8 || time.len() != 6 {
        return None;
    }
    let year: i64 = date[0..4].parse().ok()?;
    let month: i64 = date[4..6].parse().ok()?;
    let day: i64 = date[6..8].parse().ok()?;
    let hour: i64 = time[0..2].parse().ok()?;
    let min: i64 = time[2..4].parse().ok()?;
    let sec: i64 = time[4..6].parse().ok()?;
    // Naive epoch — ignores leap years; sufficient for ordering and
    // adjacency detection within a single corpus.
    let approx_epoch = (year - 1970) * 365 * 86400
        + (month - 1) * 30 * 86400
        + (day - 1) * 86400
        + hour * 3600
        + min * 60
        + sec;
    Some(approx_epoch)
}

#[derive(Default)]
struct CorpusStats {
    slot_count: usize,
    total_decodes: usize,
    per_slot_decode_counts: Vec<usize>,
    snr_buckets: BTreeMap<i32, usize>, // SNR floor (dB) → count
    type_counts: HashMap<String, usize>,
    callsign_appearances: HashMap<String, usize>,
    callsign_freqs: HashMap<String, Vec<f64>>,
    sequential_pairs_examined: usize,
    sequential_continuity_hits: usize,
    earliest_ts: Option<i64>,
    latest_ts: Option<i64>,
}

fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * p / 100.0).floor() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn type_label(t: MessageType) -> &'static str {
    match t {
        MessageType::Standard => "Standard",
        MessageType::Extended => "Extended",
        MessageType::Contest => "Contest",
        MessageType::FieldDay => "FieldDay",
        MessageType::Telemetry => "Telemetry",
        MessageType::FreeText => "FreeText",
        MessageType::DXpedition => "DXpedition",
        MessageType::RTTYRoundup => "RTTYRoundup",
        MessageType::NonStdCall => "NonStdCall",
        MessageType::Unknown => "Unknown",
    }
}

fn characterize_corpus(name: &str, paths: &[PathBuf], cfg: &Ft8Config) -> Result<CorpusStats> {
    let mut stats = CorpusStats::default();

    // Sort paths by filename so chronological filenames cluster
    // sequentially (most ft8_ filenames are timestamp-ordered).
    let mut sorted_paths: Vec<PathBuf> = paths.to_vec();
    sorted_paths.sort();

    // Track prior slot's callsign set for continuity index.
    let mut prior_callsigns: HashSet<String> = HashSet::new();
    let mut prior_ts: Option<i64> = None;

    eprintln!("[{name}] characterizing {} slots…", sorted_paths.len());
    let t0 = std::time::Instant::now();
    for (i, path) in sorted_paths.iter().enumerate() {
        if let Some(ts) = timestamp_from_filename(path) {
            stats.earliest_ts = Some(stats.earliest_ts.map_or(ts, |e| e.min(ts)));
            stats.latest_ts = Some(stats.latest_ts.map_or(ts, |e| e.max(ts)));
        }
        let samples = match load_wav(path) {
            Ok(s) => s,
            Err(_) => continue, // skip unreadable
        };
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;
        stats.slot_count += 1;
        stats.total_decodes += decoded.len();
        stats.per_slot_decode_counts.push(decoded.len());

        let mut this_callsigns: HashSet<String> = HashSet::new();
        for d in &decoded {
            // SNR bucket — coarse (5 dB bins, floored).
            let snr_floor = ((d.snr_db / 5.0).floor() as i32) * 5;
            *stats.snr_buckets.entry(snr_floor).or_insert(0) += 1;
            // Type counts.
            *stats
                .type_counts
                .entry(type_label(d.message.message_type).to_string())
                .or_insert(0) += 1;
            // Callsigns.
            for c in extract_callsigns(&d.text) {
                *stats.callsign_appearances.entry(c.clone()).or_insert(0) += 1;
                stats
                    .callsign_freqs
                    .entry(c.clone())
                    .or_default()
                    .push(d.frequency_offset);
                this_callsigns.insert(c);
            }
        }

        // QSO continuity index — pairs of sequential (≤30 s apart) slots
        // where prior callsigns reappear.
        let cur_ts = timestamp_from_filename(path);
        if let (Some(prior), Some(cur)) = (prior_ts, cur_ts) {
            if (cur - prior).abs() <= 30 && !prior_callsigns.is_empty() {
                stats.sequential_pairs_examined += 1;
                if this_callsigns
                    .intersection(&prior_callsigns)
                    .next()
                    .is_some()
                {
                    stats.sequential_continuity_hits += 1;
                }
            }
        }
        prior_callsigns = this_callsigns;
        prior_ts = cur_ts;

        if (i + 1) % 250 == 0 {
            eprintln!(
                "    [{}/{}] decodes so far: {}",
                i + 1,
                sorted_paths.len(),
                stats.total_decodes
            );
        }
    }
    eprintln!(
        "[{name}] done in {:.1}s; {} decodes",
        t0.elapsed().as_secs_f64(),
        stats.total_decodes
    );
    Ok(stats)
}

fn stats_to_json(name: &str, s: &CorpusStats) -> Value {
    let mut sorted_decode_counts = s.per_slot_decode_counts.clone();
    sorted_decode_counts.sort();
    let mean_decodes = if s.slot_count > 0 {
        s.total_decodes as f64 / s.slot_count as f64
    } else {
        0.0
    };
    let unique_callsigns = s.callsign_appearances.len();
    let max_repeats = s.callsign_appearances.values().copied().max().unwrap_or(0);
    let pairs = s.sequential_pairs_examined.max(1);
    let continuity = s.sequential_continuity_hits as f64 / pairs as f64;

    // Top-10 repeat callsigns
    let mut by_count: Vec<(&String, &usize)> = s.callsign_appearances.iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(a.1));
    let top10: Vec<Value> = by_count
        .iter()
        .take(10)
        .map(|(c, n)| json!({"call": c, "count": n}))
        .collect();

    // Drift: for callsigns with ≥3 receptions, compute std-dev of decoded freq.
    let mut drift_samples: Vec<f64> = Vec::new();
    for (_call, freqs) in s.callsign_freqs.iter() {
        if freqs.len() < 3 {
            continue;
        }
        let n = freqs.len() as f64;
        let mean = freqs.iter().sum::<f64>() / n;
        let var = freqs.iter().map(|f| (f - mean).powi(2)).sum::<f64>() / n;
        drift_samples.push(var.sqrt());
    }
    let median_drift = if drift_samples.is_empty() {
        0.0
    } else {
        let mut ds = drift_samples.clone();
        ds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        ds[ds.len() / 2]
    };

    json!({
        "name": name,
        "slot_count": s.slot_count,
        "total_decodes": s.total_decodes,
        "mean_decodes_per_slot": mean_decodes,
        "decodes_p50": percentile(&sorted_decode_counts, 50.0),
        "decodes_p90": percentile(&sorted_decode_counts, 90.0),
        "decodes_p99": percentile(&sorted_decode_counts, 99.0),
        "type_counts": s.type_counts,
        "snr_buckets": s.snr_buckets,
        "unique_callsigns": unique_callsigns,
        "max_repeats_one_callsign": max_repeats,
        "top10_repeat_callers": top10,
        "median_freq_drift_hz": median_drift,
        "sequential_pairs_examined": s.sequential_pairs_examined,
        "sequential_continuity_hits": s.sequential_continuity_hits,
        "qso_continuity_index": continuity,
        "earliest_ts": s.earliest_ts,
        "latest_ts": s.latest_ts,
        "time_span_seconds": s.latest_ts.zip(s.earliest_ts).map(|(l, e)| l - e),
    })
}

fn print_report(name: &str, s: &CorpusStats) {
    let j = stats_to_json(name, s);
    println!("\n# {name}\n");
    println!("- slots: {}", j["slot_count"]);
    println!("- total decodes: {}", j["total_decodes"]);
    println!(
        "- mean decodes/slot: {:.2}",
        j["mean_decodes_per_slot"].as_f64().unwrap_or(0.0)
    );
    println!(
        "- decode density p50 / p90 / p99: {} / {} / {}",
        j["decodes_p50"], j["decodes_p90"], j["decodes_p99"]
    );
    println!("- unique callsigns: {}", j["unique_callsigns"]);
    println!(
        "- max repeats by one callsign: {}",
        j["max_repeats_one_callsign"]
    );
    println!(
        "- median freq drift (for callsigns ≥3 receptions): {:.2} Hz",
        j["median_freq_drift_hz"].as_f64().unwrap_or(0.0)
    );
    println!(
        "- QSO-continuity index (frac of seq pairs where prior callsigns reappear): {:.3} ({}/{} pairs)",
        j["qso_continuity_index"].as_f64().unwrap_or(0.0),
        j["sequential_continuity_hits"],
        j["sequential_pairs_examined"]
    );
    if let Some(span) = j["time_span_seconds"].as_i64() {
        println!(
            "- time span: {} seconds ({:.1} hours)",
            span,
            span as f64 / 3600.0
        );
    }
    println!("- message-type distribution:");
    let mut tlist: Vec<(&String, &Value)> = j["type_counts"].as_object().unwrap().iter().collect();
    tlist.sort_by(|a, b| b.1.as_u64().cmp(&a.1.as_u64()));
    for (t, n) in tlist.iter().take(10) {
        println!("    {t}: {n}");
    }
    println!("- SNR distribution (5 dB bins):");
    let mut snr: Vec<(&String, &Value)> = j["snr_buckets"].as_object().unwrap().iter().collect();
    snr.sort_by_key(|(k, _v)| k.parse::<i32>().unwrap_or(0));
    for (s, n) in snr.iter() {
        println!("    {s} dB: {n}");
    }
    println!("- top 5 repeat callers:");
    for entry in j["top10_repeat_callers"].as_array().unwrap().iter().take(5) {
        println!(
            "    {}: {}",
            entry["call"].as_str().unwrap_or(""),
            entry["count"]
        );
    }
}

fn parse_args() -> Result<(String, Vec<PathBuf>)> {
    let args: Vec<String> = std::env::args().collect();
    let mut manifest: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut name_override: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                manifest = Some(args[i + 1].clone());
                i += 2;
            }
            "--dir" => {
                dir = Some(args[i + 1].clone());
                i += 2;
            }
            "--filter" => {
                filter = Some(args[i + 1].clone());
                i += 2;
            }
            "--limit" => {
                limit = Some(args[i + 1].parse()?);
                i += 2;
            }
            "--name" => {
                name_override = Some(args[i + 1].clone());
                i += 2;
            }
            _ => i += 1,
        }
    }

    let (name, mut paths): (String, Vec<PathBuf>) = if let Some(mp) = manifest {
        let manifest_path = if mp.starts_with('/') {
            PathBuf::from(&mp)
        } else {
            workspace_root()?.join(&mp)
        };
        let n = name_override.unwrap_or_else(|| {
            manifest_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "corpus".into())
        });
        let manifest_json: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
        let entries: Vec<PathBuf> = manifest_json["entries"]
            .as_array()
            .context("manifest has no 'entries'")?
            .iter()
            .filter_map(|e| e["wav_path"].as_str().map(PathBuf::from))
            .collect();
        (n, entries)
    } else if let Some(d) = dir {
        let dir_path = PathBuf::from(&d);
        let n = name_override.unwrap_or_else(|| {
            dir_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "corpus".into())
        });
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir_path)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("wav") {
                continue;
            }
            if let Some(f) = &filter {
                if !p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .contains(f)
                {
                    continue;
                }
            }
            entries.push(p);
        }
        (n, entries)
    } else {
        anyhow::bail!(
            "usage: --manifest <path> | --dir <path> [--filter <substr>] [--limit N] [--name X]"
        );
    };

    if let Some(lim) = limit {
        paths.sort();
        paths.truncate(lim);
    }

    Ok((name, paths))
}

fn main() -> Result<()> {
    let (name, paths) = parse_args()?;
    let cfg = Ft8Config {
        max_decode_passes: 2,
        ldpc_iterations: 200,
        ..Ft8Config::default()
    };

    let stats = characterize_corpus(&name, &paths, &cfg)?;
    print_report(&name, &stats);

    // Write JSON sidecar.
    let ws = workspace_root()?;
    let out_dir = ws.join("research/corpus/characterizations");
    std::fs::create_dir_all(&out_dir)?;
    let out_path = out_dir.join(format!("{name}.json"));
    std::fs::write(
        &out_path,
        serde_json::to_string_pretty(&stats_to_json(&name, &stats))?,
    )?;
    eprintln!("\nWrote JSON to {}", out_path.display());

    Ok(())
}
