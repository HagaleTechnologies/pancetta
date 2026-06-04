//! hb-092 — Codeword-based NMS dedup (post-decode) — diagnostic
//!
//! Question: among `unique_decoded` outputs (currently keyed on text), how
//! many are codeword-binary duplicates that the text-level dedup misses
//! because the displayed `(freq, dt)` differ by ≥ 5 Hz OR ≥ 50 ms?
//!
//! Kill-switch (per bank entry hb-092):
//!   PROCEED if ≥ 5% of novel decodes (pancetta-only, not in jt9 baseline)
//!   on top-20 hard-200 are codeword-duplicates.
//!
//! Method:
//!   1. Load `hard_200.manifest.json`, take top-20 by interest score.
//!   2. Decode each WAV with `Ft8Config::default()`.
//!   3. Group resulting `DecodedMessage`s within each WAV by
//!      `message.payload_bits` (the canonical 91-bit FT8 payload).
//!   4. Within each group of size > 1, count pairs where
//!      |Δfreq_hz| ≥ 5 OR |Δdt_s| ≥ 0.050.
//!      Those are the "codeword-duplicate, TF-distinct" decodes that hb-092
//!      would collapse.
//!   5. Cross-reference each duplicate's text against the jt9 baseline to
//!      classify as recovered (in jt9) or novel (pancetta-only).
//!   6. Report fractions; PROCEED at ≥ 5% novel-duplicates / total novels.
//!
//! Run:
//!   cargo run --release -p pancetta-research \
//!     --example hb092_codeword_dedup_diagnostic
//!
//! Output: a per-WAV table + overall summary line ending with PROCEED or
//! SHELVE. No persistent artifact written; the journal captures the result.

use anyhow::{Context, Result};
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashMap;
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
    anyhow::ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "expected 12 kHz mono"
    );
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

#[derive(Debug, Clone)]
struct BaselineDecode {
    message: String,
}

fn load_baseline(ws: &Path, sha: &str) -> Option<Vec<BaselineDecode>> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{}.json", sha));
    let txt = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    let arr = v["decodes"].as_array()?;
    Some(
        arr.iter()
            .filter_map(|d| {
                Some(BaselineDecode {
                    message: d["message"].as_str()?.to_string(),
                })
            })
            .collect(),
    )
}

#[derive(Default, Debug)]
struct WavStats {
    sha8: String,
    total_decodes: usize,
    distinct_payloads: usize,
    /// Decodes that share their payload with another decode in the same WAV
    /// AND the pair differs by ≥5 Hz or ≥50 ms in (freq, dt).
    codeword_dup_tf_distinct: usize,
    /// Of those duplicates, how many are novel (not in jt9 baseline).
    novel_dups: usize,
    /// Total novels (pancetta-only) on this WAV.
    novel_total: usize,
    /// Total recovered (matches jt9) on this WAV.
    recovered_total: usize,
}

fn main() -> Result<()> {
    let ws = workspace_root()?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let entries = manifest["entries"].as_array().context("no entries")?;

    let top_n: usize = std::env::var("HB092_TOP_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    println!(
        "## hb-092 codeword-dedup diagnostic — top-{} hard-200 WAVs",
        top_n
    );
    println!("  WAV          decodes  payloads  dup_tf  novel  rec  novel_dups");
    println!("  ----------   -------  --------  ------  -----  ---  ----------");

    let mut all_stats: Vec<WavStats> = Vec::new();
    let cfg = Ft8Config::default();

    for entry in entries.iter().take(top_n) {
        let wav_path = entry["wav_path"].as_str().context("wav_path")?;
        let sha = entry["wav_sha256"].as_str().context("wav_sha256")?;
        let samples = load_wav(Path::new(wav_path))?;
        let mut decoder =
            Ft8Decoder::new(cfg.clone()).map_err(|e| anyhow::anyhow!("Ft8Decoder::new: {e}"))?;
        let decoded = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        let baseline = load_baseline(&ws, sha).unwrap_or_default();
        let baseline_msgs: std::collections::HashSet<&str> =
            baseline.iter().map(|d| d.message.as_str()).collect();

        // Group by payload_bits (91-bit FT8 canonical payload).
        // BitVec doesn't impl Hash; convert to Vec<bool> for the HashMap key.
        let mut groups: HashMap<Vec<bool>, Vec<usize>> = HashMap::new();
        for (i, d) in decoded.iter().enumerate() {
            let key: Vec<bool> = d.message.payload_bits.iter().map(|b| *b).collect();
            groups.entry(key).or_default().push(i);
        }

        let mut codeword_dup_tf_distinct = 0usize;
        let mut novel_dups = 0usize;

        for idxs in groups.values() {
            if idxs.len() < 2 {
                continue;
            }
            // For each decode in this group, check if it has at least one
            // partner that is TF-distinct (Δfreq ≥ 5 Hz OR Δdt ≥ 50 ms).
            for &i in idxs {
                let di = &decoded[i];
                let mut has_tf_distinct_partner = false;
                for &j in idxs {
                    if i == j {
                        continue;
                    }
                    let dj = &decoded[j];
                    if (di.frequency_offset - dj.frequency_offset).abs() >= 5.0
                        || (di.time_offset - dj.time_offset).abs() >= 0.050
                    {
                        has_tf_distinct_partner = true;
                        break;
                    }
                }
                if has_tf_distinct_partner {
                    codeword_dup_tf_distinct += 1;
                    if !baseline_msgs.contains(di.text.as_str()) {
                        novel_dups += 1;
                    }
                }
            }
        }

        let novel_total = decoded
            .iter()
            .filter(|d| !baseline_msgs.contains(d.text.as_str()))
            .count();
        let recovered_total = decoded.len() - novel_total;

        let stats = WavStats {
            sha8: sha[..8].to_string(),
            total_decodes: decoded.len(),
            distinct_payloads: groups.len(),
            codeword_dup_tf_distinct,
            novel_dups,
            novel_total,
            recovered_total,
        };

        println!(
            "  {:8}    {:>7}  {:>8}  {:>6}  {:>5}  {:>3}  {:>10}",
            stats.sha8,
            stats.total_decodes,
            stats.distinct_payloads,
            stats.codeword_dup_tf_distinct,
            stats.novel_total,
            stats.recovered_total,
            stats.novel_dups,
        );
        all_stats.push(stats);
    }

    // Overall summary.
    let total_decodes: usize = all_stats.iter().map(|s| s.total_decodes).sum();
    let total_payloads: usize = all_stats.iter().map(|s| s.distinct_payloads).sum();
    let total_codeword_dups: usize = all_stats.iter().map(|s| s.codeword_dup_tf_distinct).sum();
    let total_novel: usize = all_stats.iter().map(|s| s.novel_total).sum();
    let total_recovered: usize = all_stats.iter().map(|s| s.recovered_total).sum();
    let total_novel_dups: usize = all_stats.iter().map(|s| s.novel_dups).sum();

    println!();
    println!("## Summary (top-{} hard-200)", top_n);
    println!(
        "  Total decodes:                                     {}",
        total_decodes
    );
    println!(
        "  Distinct payloads:                                 {}",
        total_payloads
    );
    println!(
        "  Codeword-duplicate decodes (TF-distinct, Δf≥5 or Δt≥50ms): {}",
        total_codeword_dups
    );
    println!(
        "  Recovered (pancetta ∩ jt9):                         {}",
        total_recovered
    );
    println!(
        "  Novel (pancetta-only):                              {}",
        total_novel
    );
    println!(
        "  Novel codeword-duplicates:                          {}",
        total_novel_dups
    );

    let frac_novel_dups = if total_novel == 0 {
        0.0
    } else {
        total_novel_dups as f64 / total_novel as f64
    };
    let frac_overall_dups = if total_decodes == 0 {
        0.0
    } else {
        total_codeword_dups as f64 / total_decodes as f64
    };

    println!();
    println!(
        "  novel_dups / novel_total:      {:.2}%",
        frac_novel_dups * 100.0
    );
    println!(
        "  codeword_dups / total_decodes: {:.2}%",
        frac_overall_dups * 100.0
    );

    println!();
    if frac_novel_dups >= 0.05 {
        println!(
            "## Verdict: PROCEED  ({:.2}% ≥ 5% threshold)",
            frac_novel_dups * 100.0
        );
    } else {
        println!(
            "## Verdict: SHELVE   ({:.2}% < 5% threshold; text-level dedup already catches the bulk)",
            frac_novel_dups * 100.0
        );
    }

    Ok(())
}
