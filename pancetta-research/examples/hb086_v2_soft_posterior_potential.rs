//! hb086_v2_soft_posterior_potential — hb-086 V2 kill-switch diagnostic.
//!
//! V1 (`joint_pair_retry`, GRADUATED 2026-05-28) caught +12 hard-200 by
//! retrying failed original sync_candidates on the (post-multipass)
//! residual spectrogram. The V1 journal documented two remaining leaks:
//!
//!   (a) candidates whose pass-1 sync_search never found them (residual
//!       can't recover what sync missed);
//!   (b) candidates whose residual LLRs are *still* corrupted — by C/D/...
//!       neighbors beyond the single subtracted A, or by imperfect ML
//!       projection on A's modulation phase.
//!
//! V2 attacks (b): for V1-failed candidates that have multiple nearby
//! decoded neighbors, replace the single hard ML subtract of the nearest
//! neighbor with probability-weighted (soft) cancellation of ALL nearby
//! decoded neighbors. The mechanism only pays off when there's a
//! multi-neighbor leak to clean.
//!
//! Kill-switch question: of V1-failed candidates (proxied by missed
//! truths with ≥1 nearby pancetta decode — the V1 pair-likely subset),
//! what fraction have 2+ nearby decoded neighbors (the multi-neighbor
//! leak V2 targets)? Below 20% → SHELVE (mechanism doesn't fit). Above
//! 20% → PROCEED.
//!
//! Secondary read: SNR distribution of those neighbors. Marginal-SNR
//! neighbors mean the rotor estimate is noisy and the hard ML projection
//! over-fits to the chosen tone, leaving residual energy at the true tone
//! that soft cancellation would preserve.
//!
//! We report results at BOTH a strict overlap window (±25 Hz ≈ 4 FFT
//! bins — the symbol-bin overlap range) AND a relaxed adjacent-band
//! window (±50 Hz — V1's diagnostic shape, captures sidelobe leakage).
//!
//! Run: cargo run --release -p pancetta-research --example hb086_v2_soft_posterior_potential

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

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

const STRICT_FREQ_HZ: f64 = 25.0;
const RELAXED_FREQ_HZ: f64 = 50.0;
const NEIGHBOR_DT_S: f64 = 2.0;
const SLOT_S: f64 = 15.0;
const MARGINAL_SNR_DB: f32 = -15.0;
const MULTI_NEIGHBOR_THRESHOLD_PCT: f64 = 20.0;

struct WindowAgg {
    label: &'static str,
    freq_hz: f64,
    v1_failed_proxy: usize,
    multi_neighbor: usize,
    multi_neighbor_with_marginal: usize,
    neighbor_count_hist: HashMap<usize, usize>,
    neighbor_snr_samples: Vec<f32>,
}

impl WindowAgg {
    fn new(label: &'static str, freq_hz: f64) -> Self {
        Self {
            label,
            freq_hz,
            v1_failed_proxy: 0,
            multi_neighbor: 0,
            multi_neighbor_with_marginal: 0,
            neighbor_count_hist: HashMap::new(),
            neighbor_snr_samples: Vec::new(),
        }
    }
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

    struct WavStats {
        sha_short: String,
        truth_total: usize,
        recovered: usize,
        missed: usize,
        v1_failed_strict: usize,
        multi_strict: usize,
        v1_failed_relaxed: usize,
        multi_relaxed: usize,
    }

    let mut wav_stats: Vec<WavStats> = Vec::new();
    let mut strict = WindowAgg::new("strict (±25 Hz overlap)", STRICT_FREQ_HZ);
    let mut relaxed = WindowAgg::new("relaxed (±50 Hz adjacent)", RELAXED_FREQ_HZ);
    let mut total_missed = 0usize;

    eprintln!("Decoding top-20 hard-200 WAVs (production config — V1 ON)...");
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

        let pancetta: Vec<(f64, f64, String, f32)> = pancetta_decodes
            .iter()
            .map(|d| {
                (
                    d.frequency_offset,
                    d.time_offset.rem_euclid(SLOT_S),
                    d.text.trim().to_string(),
                    d.snr_db,
                )
            })
            .collect();

        let mut recovered = 0usize;
        let mut missed_locally: Vec<(f64, f64, String)> = Vec::new();
        for (tf, td, tm) in &truths {
            let matched = pancetta
                .iter()
                .any(|(_, _, pm, _)| pm.contains(tm) || tm.contains(pm));
            if matched {
                recovered += 1;
            } else {
                missed_locally.push((*tf, *td, tm.clone()));
            }
        }

        let mut wav_v1_strict = 0usize;
        let mut wav_multi_strict = 0usize;
        let mut wav_v1_relaxed = 0usize;
        let mut wav_multi_relaxed = 0usize;

        for (tf, td, _tm) in &missed_locally {
            for agg in [&mut strict, &mut relaxed] {
                let neighbors: Vec<&(f64, f64, String, f32)> = pancetta
                    .iter()
                    .filter(|(pf, pdt, _, _)| {
                        let df = (pf - tf).abs();
                        let dd = (pdt - td).abs();
                        df <= agg.freq_hz && dd <= NEIGHBOR_DT_S
                    })
                    .collect();
                let n = neighbors.len();
                *agg.neighbor_count_hist.entry(n).or_insert(0) += 1;

                if n >= 1 {
                    agg.v1_failed_proxy += 1;
                    if agg.freq_hz == STRICT_FREQ_HZ {
                        wav_v1_strict += 1;
                    } else {
                        wav_v1_relaxed += 1;
                    }
                    if n >= 2 {
                        agg.multi_neighbor += 1;
                        if agg.freq_hz == STRICT_FREQ_HZ {
                            wav_multi_strict += 1;
                        } else {
                            wav_multi_relaxed += 1;
                        }
                        let any_marginal = neighbors
                            .iter()
                            .any(|(_, _, _, snr)| *snr < MARGINAL_SNR_DB);
                        if any_marginal {
                            agg.multi_neighbor_with_marginal += 1;
                        }
                    }
                    for (_, _, _, snr) in &neighbors {
                        agg.neighbor_snr_samples.push(*snr);
                    }
                }
            }
            total_missed += 1;
        }

        wav_stats.push(WavStats {
            sha_short: sha[..8].to_string(),
            truth_total: truths.len(),
            recovered,
            missed: missed_locally.len(),
            v1_failed_strict: wav_v1_strict,
            multi_strict: wav_multi_strict,
            v1_failed_relaxed: wav_v1_relaxed,
            multi_relaxed: wav_multi_relaxed,
        });
        eprintln!(
            "  [{idx:2}] {} truth={} rec={} missed={} strict[v1={} multi={}] relaxed[v1={} multi={}]",
            &sha[..8],
            truths.len(),
            recovered,
            missed_locally.len(),
            wav_v1_strict,
            wav_multi_strict,
            wav_v1_relaxed,
            wav_multi_relaxed,
        );
    }

    println!("\n=== hb-086 V2 soft-posterior-potential diagnostic (top-20 hard-200) ===\n");
    println!("Per-WAV breakdown (counts of missed truths classified by neighbor window):");
    println!(
        "  {:>9} {:>6} {:>6} {:>7} | {:>10} {:>10} | {:>10} {:>10}",
        "sha", "truth", "rec", "missed", "v1f-strict", "mult-str", "v1f-relax", "mult-rel"
    );
    for w in &wav_stats {
        println!(
            "  {:>9} {:>6} {:>6} {:>7} | {:>10} {:>10} | {:>10} {:>10}",
            w.sha_short,
            w.truth_total,
            w.recovered,
            w.missed,
            w.v1_failed_strict,
            w.multi_strict,
            w.v1_failed_relaxed,
            w.multi_relaxed,
        );
    }

    println!("\nAggregate (total missed truths = {})\n", total_missed);

    for agg in [&strict, &relaxed] {
        println!("--- Window: {} ---", agg.label);
        if agg.v1_failed_proxy == 0 {
            println!("  (no V1-failed-proxy candidates in this window)\n");
            continue;
        }
        let pct_multi = 100.0 * agg.multi_neighbor as f64 / agg.v1_failed_proxy as f64;
        let pct_marg = 100.0 * agg.multi_neighbor_with_marginal as f64 / agg.v1_failed_proxy as f64;
        println!(
            "  V1-failed proxy (missed + ≥1 nearby decode):              {}",
            agg.v1_failed_proxy
        );
        println!(
            "  ├─ with 2+ nearby decoded neighbors (V2 target subset):   {} ({:.1}% of v1-fail)",
            agg.multi_neighbor, pct_multi
        );
        println!(
            "  └─ AND ≥1 marginal-SNR neighbor (<{:.0} dB):              {} ({:.1}% of v1-fail)",
            MARGINAL_SNR_DB, agg.multi_neighbor_with_marginal, pct_marg
        );

        println!("\n  Nearby-neighbor count distribution:");
        let mut counts: Vec<(usize, usize)> = agg
            .neighbor_count_hist
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        counts.sort_by_key(|&(k, _)| k);
        for (k, v) in &counts {
            let pct = 100.0 * *v as f64 / total_missed as f64;
            println!(
                "    {:>2} neighbors:  {:>4} missed truths ({:.1}%)",
                k, v, pct
            );
        }

        if !agg.neighbor_snr_samples.is_empty() {
            let mut sorted = agg.neighbor_snr_samples.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let n = sorted.len();
            let median = sorted[n / 2];
            let p25 = sorted[n / 4];
            let p10 = sorted[n / 10];
            let n_marginal = sorted.iter().filter(|s| **s < MARGINAL_SNR_DB).count();
            let pct_marginal = 100.0 * n_marginal as f64 / n as f64;
            println!(
                "\n  Neighbor-decode SNR distribution (n={}): p10={:.1} p25={:.1} median={:.1} dB",
                n, p10, p25, median
            );
            println!(
                "    marginal (< {:.0} dB): {} ({:.1}% of neighbor samples)\n",
                MARGINAL_SNR_DB, n_marginal, pct_marginal
            );
        } else {
            println!();
        }
    }

    println!("Decision thresholds (V2 spec):");
    println!("  Primary: ≥20% of V1-failed candidates must have neighbors with meaningfully-soft");
    println!("           posteriors (i.e., neighbors with non-trivial tone uncertainty).");
    println!(
        "           Proxied by neighbor-decode SNR < {:.0} dB (where LLR magnitudes shrink",
        MARGINAL_SNR_DB
    );
    println!("           enough that soft posteriors differ meaningfully from hard).");
    println!(
        "  Secondary: ≥{:.0}% of V1-failed candidates have 2+ nearby neighbors (multi-neighbor",
        MULTI_NEIGHBOR_THRESHOLD_PCT
    );
    println!("           target population for V2's soft cancellation pass).");

    let strict_pct = if strict.v1_failed_proxy > 0 {
        100.0 * strict.multi_neighbor as f64 / strict.v1_failed_proxy as f64
    } else {
        0.0
    };
    let relaxed_pct = if relaxed.v1_failed_proxy > 0 {
        100.0 * relaxed.multi_neighbor as f64 / relaxed.v1_failed_proxy as f64
    } else {
        0.0
    };
    let marg_strict_pct = if strict.v1_failed_proxy > 0 {
        100.0 * strict.multi_neighbor_with_marginal as f64 / strict.v1_failed_proxy as f64
    } else {
        0.0
    };
    let marg_relaxed_pct = if relaxed.v1_failed_proxy > 0 {
        100.0 * relaxed.multi_neighbor_with_marginal as f64 / relaxed.v1_failed_proxy as f64
    } else {
        0.0
    };

    println!(
        "\n  strict  (±{:.0} Hz overlap):   multi-neighbor {:.1}%, marginal-SNR-among-multi {:.1}%",
        STRICT_FREQ_HZ, strict_pct, marg_strict_pct
    );
    println!(
        "  relaxed (±{:.0} Hz adjacent):  multi-neighbor {:.1}%, marginal-SNR-among-multi {:.1}%",
        RELAXED_FREQ_HZ, relaxed_pct, marg_relaxed_pct
    );

    let primary_pass = marg_strict_pct >= MULTI_NEIGHBOR_THRESHOLD_PCT
        || marg_relaxed_pct >= MULTI_NEIGHBOR_THRESHOLD_PCT;
    let secondary_pass =
        strict_pct >= MULTI_NEIGHBOR_THRESHOLD_PCT || relaxed_pct >= MULTI_NEIGHBOR_THRESHOLD_PCT;

    let verdict = match (primary_pass, secondary_pass) {
        (true, _) => "PROCEED — neighbors have meaningful tone uncertainty AND multi-neighbor leak exists.",
        (false, true) => "SHELVE — multi-neighbor count clears the threshold, but neighbor SNRs are uniformly strong (LLR magnitudes large → soft posteriors ≈ hard projections). The spec's primary kill criterion (soft-meaningfully-differs-from-hard) fails: there is nothing for soft cancellation to preserve. hb-081 (per-decode subtract scaling) regressed precisely because dropping subtract energy on strong decodes was wrong; soft cancellation is the same family of move.",
        (false, false) => "SHELVE — neither the multi-neighbor count nor the marginal-SNR fraction clears the V2 threshold.",
    };
    println!("\nVerdict: {verdict}");

    Ok(())
}
