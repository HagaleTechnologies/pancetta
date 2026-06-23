//! hb-087 callsign-priors-on-residual — feasibility / kill-switch diagnostic.
//!
//! Spawned 2026-05-31 from the hb-086 V3 SHELVE (Costas-relaxation surfaces
//! noise, not signal). The remaining hard-200 wall is sub-Costas-threshold
//! weak signals. hb-086 V3 closed Costas-relaxation as a route; this
//! hypothesis attacks the same wall via a structurally different mechanism:
//! BYPASS Costas pre-gate entirely and decode the residual at a position
//! chosen by geometric proximity to a subtracted decode, using a *callsign
//! prior* (one of the operator's known/recent/spotted callsigns) injected
//! at the appropriate 28-bit window of the LLR vector (AP1/AP2-style).
//!
//! **Mechanism premise.** Unconstrained LDPC BP on garbage LLRs converges
//! 100% of the time at the production iteration count — that was the V3
//! finding. With a *prior* injected (28 bits pinned at ±15.0 LLR), the
//! valid-codeword space collapses dramatically; only LLR patterns consistent
//! with a real signal carrying callsign `C` produce a CRC-passing
//! convergence. The structural test for this hypothesis is therefore:
//!
//!   How often is a missed truth's callsign actually in the union of the
//!   prior sources we'd have at runtime?
//!
//! No prior coverage → no leverage → SHELVE pre-implementation.
//!
//! ## What this diagnostic measures
//!
//! On the refreshed top-20 worst hard-200 WAVs, for every truth pancetta
//! misses, check whether either of its callsigns appears in:
//!
//!   1. **Operator callsign** — single fixed call (K5ARH); the safest
//!      prior (zero FP-injection risk because we ALWAYS know our own call).
//!   2. **Recent callsigns (this WAV)** — every callsign mentioned in any
//!      pancetta decode from this WAV. Simulates the "rolling 15-30 min
//!      window of recently-heard callsigns" prior that hb-027/hb-052/
//!      callsign_continuity.rs already maintain in production. We
//!      approximate per-session-window with per-WAV here because hard-200
//!      WAVs are disjoint 15-second slots.
//!   3. **Bundled-common-active** — a small hand-picked list of ~100 very
//!      active stations (DXpedition + permanent-active). Approximates the
//!      cqdx.io spots prior (`spotted_callsigns()`). This bundled list is
//!      used only for the diagnostic; production would pull from cqdx
//!      cache instead.
//!
//! For each missed truth we count whether **either** of its extracted
//! callsigns hits at least one source. The headline number is overall
//! prior-coverage rate, broken down by source so we can see which one
//! contributes most.
//!
//! Kill switch: PROCEED if overall prior coverage ≥ 20% of missed
//! truths in the top-20 hard-200; SHELVE otherwise.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example hb087_callsign_priors_feasibility

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const OPERATOR_CALLSIGN: &str = "K5ARH";
const PROCEED_THRESHOLD_PCT: f64 = 20.0;
const SLOT_S: f64 = 15.0;

/// Hand-picked sample of very active / DXpedition stations heard frequently
/// on FT8. Approximates the cqdx-spots prior available at runtime. Drawn
/// from common LotW activity + a few DXpedition staples; deliberately
/// under-100 to avoid pretending we have a bigger universe than cqdx
/// realistically delivers in a single live-spots window.
const BUNDLED_COMMON_ACTIVE: &[&str] = &[
    // North America active
    "K1JT", "K9AN", "W1AW", "WB2FKO", "W2NRA", "K3LR", "N4YDU", "W5KFT", "K6ND", "W7RN", "K8GP",
    "W9RE", "N0NI", "K0PC", "VE3EJ", "VE5SF", "VE9AA", "K2LE", "N3RS", "K4ZW", "K5ZD", "W6YA",
    "N7AT", "K8AZ", "W9KKN", "K0RF", "VE2IM", "VY2ZM", "XE2X", // South America
    "CE3CT", "CW5W", "LU8YE", "PY5EG", "PY2NY", "ZP6CW", "HC8N", // EU active
    "DL1IAO", "DL6FBL", "DJ5IW", "DR1A", "G4PIQ", "G3SXW", "GW3YDX", "EI7M", "F5IN", "F6BEE",
    "I4VEQ", "IK2QEI", "ON4UN", "OZ4UN", "PA0LSK", "S52ZW", "SK3W", "SM5AJV", "9A1A", "OK1RF",
    "OM3RM", "YU1ZZ", "Z32U", "Z37M", // EU/AF border + AF active
    "EA6NB", "CT3MD", "CN2AA", "ZS6Y", "CN8KD", "EA8RM", "S01WS", // Asia active
    "JA1NLX", "JA7QVI", "JE1JKL", "JH4ADV", "BG2AUE", "BY1RX", "HL5IVL", "VR2BG", "9V1YC", "BV9G",
    // OC active
    "VK3ER", "VK6IR", "VK9NI", "ZL3IX", "ZL1ANH", "ZL2IFB", "FK8GM", "T88AT",
    // Recent / current DXpedition staples
    "VP6R", "TX5S", "VK0EK", "FT5ZM", "K9W", "ZL9A", "VP8STI", "VP8SGI", "5W1SA", "TI9A", "3Y0Z",
    "T31EU",
];

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn load_wav(path: &PathBuf) -> anyhow::Result<Vec<f32>> {
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

/// Extract bare-callsign tokens from an FT8 message string. Mirrors
/// pancetta-qso::callsign_continuity::callsigns_in but inlined here so the
/// diagnostic has no inter-crate test dependency.
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
            let bare = t.split('/').next().unwrap_or(t);
            out.push(bare.to_string());
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

#[derive(Default, Clone)]
struct CoverageCounts {
    operator: usize,
    recent: usize,
    bundled: usize,
    any: usize,
}

struct WavStats {
    sha_short: String,
    truth_total: usize,
    recovered: usize,
    missed: usize,
    missed_with_extractable_call: usize,
    covered_by_operator: usize,
    covered_by_recent: usize,
    covered_by_bundled: usize,
    covered_any: usize,
}

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;

    let main_json: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/scorecards/main.json"),
    )?)?;
    let top20_hashes: Vec<String> = main_json["tiers"]["curated-hard-200"]["per_wav_top_failures"]
        .as_array()
        .context("per_wav_top_failures not array")?
        .iter()
        .take(20)
        .map(|f| f["wav_hash"].as_str().unwrap().to_string())
        .collect();

    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        ws.join("research/corpus/curated/ft8/hard_200.manifest.json"),
    )?)?;
    let mut path_by_sha: HashMap<String, String> = HashMap::new();
    for e in manifest["entries"].as_array().context("no entries")? {
        path_by_sha.insert(
            e["wav_sha256"].as_str().unwrap().to_string(),
            e["wav_path"].as_str().unwrap().to_string(),
        );
    }

    let operator_set: HashSet<String> = [OPERATOR_CALLSIGN.to_string()].into_iter().collect();
    let bundled_set: HashSet<String> = BUNDLED_COMMON_ACTIVE
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut wav_stats: Vec<WavStats> = Vec::new();
    let mut total_missed: usize = 0;
    let mut total_missed_with_call: usize = 0;
    let mut total_coverage: CoverageCounts = CoverageCounts::default();

    eprintln!(
        "Decoding top-20 hard-200 WAVs (production config — multipass N=3 + V1 ON), \
         then checking prior coverage on missed truths..."
    );
    let cfg = Ft8Config::default();

    for (idx, sha) in top20_hashes.iter().enumerate() {
        let Some(wav_path_str) = path_by_sha.get(sha) else {
            eprintln!("  [{idx:2}] sha {} not in manifest — skip", &sha[..8]);
            continue;
        };
        let wav_path = PathBuf::from(wav_path_str);
        let baseline_path = ws.join(format!("research/baselines/ft8/{sha}.json"));

        let baseline: Value = serde_json::from_str(&std::fs::read_to_string(&baseline_path)?)?;
        let truths: Vec<(f64, f64, String)> = baseline["decodes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|d| {
                        Some((
                            d.get("freq_hz")?.as_f64()?,
                            d.get("dt_s")?.as_f64()?,
                            d.get("message")?.as_str()?.trim().to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let samples = match load_wav(&wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx:2}] WAV load failed for {}: {e}", &sha[..8]);
                continue;
            }
        };
        let mut decoder = Ft8Decoder::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let pancetta_decodes = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        let pancetta: Vec<(f64, f64, String)> = pancetta_decodes
            .iter()
            .map(|d| {
                (
                    d.frequency_offset,
                    d.time_offset.rem_euclid(SLOT_S),
                    d.text.trim().to_string(),
                )
            })
            .collect();

        // Build the "recent callsigns (this WAV)" prior set: every callsign
        // mentioned by any pancetta decode from this WAV. Stand-in for the
        // rolling 15-30 min window pancetta maintains in production.
        let mut recent_set: HashSet<String> = HashSet::new();
        for (_, _, pm) in &pancetta {
            for c in callsigns_in(pm) {
                recent_set.insert(c);
            }
        }

        let mut recovered = 0usize;
        let mut missed_truths: Vec<String> = Vec::new();
        for (_, _, tm) in &truths {
            let matched = pancetta
                .iter()
                .any(|(_, _, pm)| pm.contains(tm) || tm.contains(pm));
            if matched {
                recovered += 1;
            } else {
                missed_truths.push(tm.clone());
            }
        }

        let mut wav_cov = CoverageCounts::default();
        let mut wav_missed_with_call = 0usize;

        for tm in &missed_truths {
            let calls = callsigns_in(tm);
            if calls.is_empty() {
                continue;
            }
            wav_missed_with_call += 1;

            let hit_operator = calls.iter().any(|c| operator_set.contains(c));
            let hit_recent = calls.iter().any(|c| recent_set.contains(c));
            let hit_bundled = calls.iter().any(|c| bundled_set.contains(c));

            if hit_operator {
                wav_cov.operator += 1;
            }
            if hit_recent {
                wav_cov.recent += 1;
            }
            if hit_bundled {
                wav_cov.bundled += 1;
            }
            if hit_operator || hit_recent || hit_bundled {
                wav_cov.any += 1;
            }
        }

        total_missed += missed_truths.len();
        total_missed_with_call += wav_missed_with_call;
        total_coverage.operator += wav_cov.operator;
        total_coverage.recent += wav_cov.recent;
        total_coverage.bundled += wav_cov.bundled;
        total_coverage.any += wav_cov.any;

        wav_stats.push(WavStats {
            sha_short: sha[..8].to_string(),
            truth_total: truths.len(),
            recovered,
            missed: missed_truths.len(),
            missed_with_extractable_call: wav_missed_with_call,
            covered_by_operator: wav_cov.operator,
            covered_by_recent: wav_cov.recent,
            covered_by_bundled: wav_cov.bundled,
            covered_any: wav_cov.any,
        });
        eprintln!(
            "  [{idx:2}] {} truth={} rec={} missed={} miss-w-call={} cov-any={} \
             (op={} recent={} bundled={})",
            &sha[..8],
            truths.len(),
            recovered,
            missed_truths.len(),
            wav_missed_with_call,
            wav_cov.any,
            wav_cov.operator,
            wav_cov.recent,
            wav_cov.bundled,
        );
    }

    println!("\n=== hb-087 callsign-priors feasibility (top-20 hard-200) ===\n");
    println!("Per-WAV breakdown:");
    println!(
        "  {:>9} {:>6} {:>5} {:>7} {:>10} {:>4} {:>6} {:>7} {:>4}",
        "sha", "truth", "rec", "missed", "miss-w-cal", "op", "recent", "bundled", "any",
    );
    for w in &wav_stats {
        println!(
            "  {:>9} {:>6} {:>5} {:>7} {:>10} {:>4} {:>6} {:>7} {:>4}",
            w.sha_short,
            w.truth_total,
            w.recovered,
            w.missed,
            w.missed_with_extractable_call,
            w.covered_by_operator,
            w.covered_by_recent,
            w.covered_by_bundled,
            w.covered_any,
        );
    }

    println!(
        "\nAggregate: total_missed={}  missed_with_extractable_call={}",
        total_missed, total_missed_with_call,
    );

    if total_missed_with_call == 0 {
        println!("No extractable-call missed truths — SHELVE (insufficient signal to evaluate).");
        return Ok(());
    }

    let pct_of_with_call = |n: usize| 100.0 * n as f64 / total_missed_with_call as f64;
    let pct_of_all = |n: usize| 100.0 * n as f64 / total_missed.max(1) as f64;

    println!(
        "\nPrior coverage by source (denominator = missed truths with an extractable callsign):"
    );
    println!(
        "  operator (K5ARH):           {:>5} ({:>5.1}% of with-call, {:>5.1}% of all missed)",
        total_coverage.operator,
        pct_of_with_call(total_coverage.operator),
        pct_of_all(total_coverage.operator),
    );
    println!(
        "  recent (this-WAV decodes):  {:>5} ({:>5.1}% of with-call, {:>5.1}% of all missed)",
        total_coverage.recent,
        pct_of_with_call(total_coverage.recent),
        pct_of_all(total_coverage.recent),
    );
    println!(
        "  bundled-common-active:      {:>5} ({:>5.1}% of with-call, {:>5.1}% of all missed)",
        total_coverage.bundled,
        pct_of_with_call(total_coverage.bundled),
        pct_of_all(total_coverage.bundled),
    );
    println!(
        "  ANY of the above:           {:>5} ({:>5.1}% of with-call, {:>5.1}% of all missed)",
        total_coverage.any,
        pct_of_with_call(total_coverage.any),
        pct_of_all(total_coverage.any),
    );

    let headline_pct = pct_of_all(total_coverage.any);
    println!(
        "\nDecision threshold: PROCEED if ANY-source coverage of all missed truths \
         is ≥{:.0}%.\n  Headline = {:.1}%",
        PROCEED_THRESHOLD_PCT, headline_pct,
    );

    let verdict = if headline_pct >= PROCEED_THRESHOLD_PCT {
        format!(
            "PROCEED — {:.1}% of missed truths have a callsign already covered by one of \
             (operator / recent-decodes / bundled-common). The callsign-prior mechanism has \
             real leverage; specify implementation and proceed to AP-constrained residual \
             decode design.",
            headline_pct,
        )
    } else {
        format!(
            "SHELVE — only {:.1}% of missed truths have a callsign covered by any of our \
             prior sources. Even with perfect AP-constrained-residual decoding, the upper \
             bound on recovery is too small. The mechanism does not have enough leverage on \
             this corpus.",
            headline_pct,
        )
    };
    println!("\nVerdict: {verdict}");

    Ok(())
}
