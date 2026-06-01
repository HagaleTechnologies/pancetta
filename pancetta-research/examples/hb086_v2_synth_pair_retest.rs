//! hb086_v2_synth_pair_retest — hb-086 V2 third-corpus re-test on the
//! hb-146 synth-pair-200 adversarial corpus.
//!
//! Context: V2's soft-cancellation kill-switch (primary criterion:
//! ≥20% of V1-failed candidates have a marginal-SNR neighbor with
//! decode SNR < -15 dB) failed at 0% on BOTH the original hard-200
//! corpus (2026-05-30) and the refreshed hard-200 corpus (2026-05-31).
//!
//! Per the Phase A audit re-label, V2 is "SHELVED across two corpora —
//! re-test gate: hb-146 synth-pair-200 (which by construction contains
//! marginal-SNR weak signals beside decoded strong neighbors) or a
//! fundamentally new mechanism variant." This example re-runs the
//! diagnostic against synth-pair-200.
//!
//! Construction of synth-pair-200 (per hb-146 SHIPPED journal):
//!   - 180 WAVs sweeping (ΔSNR ∈ {0,3,6,9,12} dB, Δf ∈ {6,12,25,50} Hz,
//!     Δt ∈ {0, 0.1, 0.25} s, 6 message templates)
//!   - strong signal at 1500 Hz with strong_snr_db = -8 dB
//!   - weak signal at 1500 + Δf Hz, scaled by 10^(-ΔSNR/20)
//!   - AWGN at strong_snr_db reference noise floor
//!   - Pancetta baseline: 177/180 strong-decode, 92/180 weak-decode
//!     (49% weak miss — 0% weak recovery at ΔSNR≥9 ∧ Δf≤12 Hz, 88 WAVs)
//!
//! For each WAV where pancetta misses the weak truth (synth-pair's
//! V1-failed-candidate proxy is much cleaner than hard-200's: the truth
//! is KNOWN by construction at (1500+Δf Hz, lead_in+Δt s)), examine the
//! SNR of pancetta's nearby decoded neighbors at the same strict
//! (±25 Hz × ±2 s) and relaxed (±50 Hz × ±2 s) windows the original
//! diagnostic used. Report marginal-SNR (< -15 dB) fraction and the
//! full SNR distribution, compare to the hard-200 (0%) numbers.
//!
//! Decision per Phase A label convention (no "DEFINITIVELY" language):
//!   - marginal-SNR rate ≥ 20%  → PROCEED to V2 Session 2 (implementation)
//!   - 10% ≤ rate < 20%         → MARGINAL — write up partial result
//!   - rate < 10%               → SHELVE on 3rd corpus
//!
//! Run: cargo run --release -p pancetta-research --example hb086_v2_synth_pair_retest

use anyhow::Context;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use pancetta_research::synth::SynthPairManifest;
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

// Match the original hb-086 V2 diagnostic windows exactly so cross-corpus
// comparison is apples-to-apples.
const STRICT_FREQ_HZ: f64 = 25.0;
const RELAXED_FREQ_HZ: f64 = 50.0;
const NEIGHBOR_DT_S: f64 = 2.0;
const MARGINAL_SNR_DB: f32 = -15.0;
const MULTI_NEIGHBOR_THRESHOLD_PCT: f64 = 20.0;

// hard-200 reference numbers (from V2 SHELVE journal 2026-05-30 +
// recheck 2026-05-31). Used for the 3-corpus comparison table.
const HARD200_OLD_MULTI_STRICT_PCT: f64 = 14.8;
const HARD200_OLD_MULTI_RELAXED_PCT: f64 = 34.8;
const HARD200_OLD_MARGINAL_STRICT_PCT: f64 = 0.0;
const HARD200_OLD_MARGINAL_RELAXED_PCT: f64 = 0.0;
const HARD200_OLD_NBR_MEDIAN_RELAXED: f64 = -1.5;
const HARD200_OLD_NBR_P10_RELAXED: f64 = -5.7;

const HARD200_REFRESHED_MULTI_STRICT_PCT: f64 = 16.7;
const HARD200_REFRESHED_MULTI_RELAXED_PCT: f64 = 33.8;
const HARD200_REFRESHED_MARGINAL_STRICT_PCT: f64 = 0.0;
const HARD200_REFRESHED_MARGINAL_RELAXED_PCT: f64 = 0.0;
const HARD200_REFRESHED_NBR_MEDIAN_RELAXED: f64 = -1.6;
const HARD200_REFRESHED_NBR_P10_RELAXED: f64 = -5.1;

struct WindowAgg {
    label: &'static str,
    freq_hz: f64,
    v1_failed_proxy: usize,
    with_any_neighbor: usize,
    with_marginal_neighbor: usize,
    multi_neighbor: usize,
    neighbor_count_hist: HashMap<usize, usize>,
    neighbor_snr_samples: Vec<f32>,
}

impl WindowAgg {
    fn new(label: &'static str, freq_hz: f64) -> Self {
        Self {
            label,
            freq_hz,
            v1_failed_proxy: 0,
            with_any_neighbor: 0,
            with_marginal_neighbor: 0,
            multi_neighbor: 0,
            neighbor_count_hist: HashMap::new(),
            neighbor_snr_samples: Vec::new(),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let ws = workspace_root()?;
    let manifest_path = ws.join("research/corpus/synth/manifests/synth_pair_200.manifest.json");
    let manifest = SynthPairManifest::load(&manifest_path).with_context(|| {
        format!(
            "load synth-pair manifest at {} — run `cargo run --release -p pancetta-research --bin gen-synth-pair -- --config research/corpus/synth/manifests/synth_pair_200.config.json --output research/corpus/synth/manifests/synth_pair_200.manifest.json` first",
            manifest_path.display()
        )
    })?;

    let strong_base_hz: f64 = 1500.0;
    let strong_t_s = manifest.config.slot_lead_in_s;

    eprintln!(
        "Decoding {} synth-pair-200 WAVs (production config; V1 ON)...",
        manifest.entries.len()
    );

    let cfg = Ft8Config::default();
    let mut strict = WindowAgg::new("strict (±25 Hz overlap)", STRICT_FREQ_HZ);
    let mut relaxed = WindowAgg::new("relaxed (±50 Hz adjacent)", RELAXED_FREQ_HZ);

    let mut total_weak_missed = 0usize;
    let mut total_weak_decoded = 0usize;
    let mut total_strong_decoded = 0usize;

    // Bucket strict-window marginal-SNR rate by (ΔSNR, Δf) — drops Δt to
    // keep the table small but preserves the regime structure.
    type BucketKey = (i64, i64); // (dSNR*10, dF*10)
    type BucketCounts = (u32, u32, u32, u32); // (wavs, weak_missed, v1_failed_strict, marginal_strict)
    let mut buckets: std::collections::BTreeMap<BucketKey, BucketCounts> =
        std::collections::BTreeMap::new();

    for (idx, entry) in manifest.entries.iter().enumerate() {
        let wav_path = ws.join(&entry.wav_path);
        let samples = match load_wav(&wav_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [{idx:3}] WAV load failed: {e}");
                continue;
            }
        };

        let mut decoder = Ft8Decoder::new(cfg.clone())
            .map_err(|e| anyhow::anyhow!("Ft8Decoder::new failed: {e}"))?;
        let decodes = decoder
            .decode_window(&samples)
            .map_err(|e| anyhow::anyhow!("decode_window: {e}"))?;

        let pancetta: Vec<(f64, f64, String, f32)> = decodes
            .iter()
            .map(|d| {
                (
                    d.frequency_offset,
                    d.time_offset,
                    d.text.trim().to_string(),
                    d.snr_db,
                )
            })
            .collect();

        let got_strong = pancetta
            .iter()
            .any(|(_, _, m, _)| m.contains(&entry.message_strong));
        let got_weak = pancetta
            .iter()
            .any(|(_, _, m, _)| m.contains(&entry.message_weak));

        if got_strong {
            total_strong_decoded += 1;
        }
        if got_weak {
            total_weak_decoded += 1;
        }

        let bucket_key = (
            (entry.delta_snr_db * 10.0).round() as i64,
            (entry.delta_freq_hz * 10.0).round() as i64,
        );
        let b = buckets.entry(bucket_key).or_insert((0, 0, 0, 0));
        b.0 += 1;

        // V1-failed-candidate analog on synth-pair: the WEAK truth was
        // missed. The weak signal is at (1500 + Δf Hz, lead_in + Δt s).
        if got_weak {
            continue;
        }
        total_weak_missed += 1;
        b.1 += 1;

        let weak_f = strong_base_hz + entry.delta_freq_hz;
        let weak_t = strong_t_s + entry.delta_time_s;

        // Count nearby decoded neighbors at strict + relaxed windows.
        // We EXCLUDE any pancetta decode whose text matches the weak
        // truth itself (it wasn't decoded, so this is moot — but defensive).
        for agg in [&mut strict, &mut relaxed] {
            let neighbors: Vec<&(f64, f64, String, f32)> = pancetta
                .iter()
                .filter(|(pf, pdt, m, _)| {
                    if m.contains(&entry.message_weak) {
                        return false;
                    }
                    let df = (pf - weak_f).abs();
                    let dd = (pdt - weak_t).abs();
                    df <= agg.freq_hz && dd <= NEIGHBOR_DT_S
                })
                .collect();
            let n = neighbors.len();
            *agg.neighbor_count_hist.entry(n).or_insert(0) += 1;

            if n >= 1 {
                agg.with_any_neighbor += 1;
                agg.v1_failed_proxy += 1;
                if n >= 2 {
                    agg.multi_neighbor += 1;
                }
                let any_marginal = neighbors
                    .iter()
                    .any(|(_, _, _, snr)| *snr < MARGINAL_SNR_DB);
                if any_marginal {
                    agg.with_marginal_neighbor += 1;
                    if agg.freq_hz == STRICT_FREQ_HZ {
                        b.3 += 1;
                    }
                }
                for (_, _, _, snr) in &neighbors {
                    agg.neighbor_snr_samples.push(*snr);
                }
                if agg.freq_hz == STRICT_FREQ_HZ {
                    b.2 += 1;
                }
            }
        }

        if idx < 24 || idx % 30 == 0 {
            eprintln!(
                "  [{idx:3}] dSNR={:+4.1} dF={:+5.1} dT={:+4.2} strong={} weak={} weak_truth=({:.1}Hz,{:.2}s) decodes={}",
                entry.delta_snr_db,
                entry.delta_freq_hz,
                entry.delta_time_s,
                if got_strong { "Y" } else { "N" },
                if got_weak { "Y" } else { "N" },
                weak_f,
                weak_t,
                pancetta.len(),
            );
        }
    }

    println!("\n=== hb-086 V2 synth-pair-200 retest diagnostic ===\n");
    println!(
        "Corpus: synth-pair-200 (hb-146), {} WAVs, two truths each.",
        manifest.entries.len()
    );
    println!(
        "Strong baseline: {}/{} decoded ({:.1}%)",
        total_strong_decoded,
        manifest.entries.len(),
        100.0 * total_strong_decoded as f64 / manifest.entries.len() as f64
    );
    println!(
        "Weak baseline:   {}/{} decoded ({:.1}%)",
        total_weak_decoded,
        manifest.entries.len(),
        100.0 * total_weak_decoded as f64 / manifest.entries.len() as f64
    );
    println!(
        "V1-failed-proxy population (weak truth missed): {}\n",
        total_weak_missed
    );

    for agg in [&strict, &relaxed] {
        println!("--- Window: {} ---", agg.label);
        if agg.v1_failed_proxy == 0 {
            println!("  (no V1-failed-proxy candidates with neighbors in this window)\n");
            continue;
        }
        let pct_any = 100.0 * agg.with_any_neighbor as f64 / total_weak_missed as f64;
        let pct_multi_of_v1 = 100.0 * agg.multi_neighbor as f64 / agg.v1_failed_proxy as f64;
        let pct_marginal_of_v1 =
            100.0 * agg.with_marginal_neighbor as f64 / agg.v1_failed_proxy as f64;
        let pct_marginal_of_missed =
            100.0 * agg.with_marginal_neighbor as f64 / total_weak_missed as f64;
        println!(
            "  Missed-weak with ≥1 decoded neighbor (V1-failed proxy): {} ({:.1}% of weak-missed)",
            agg.v1_failed_proxy, pct_any
        );
        println!(
            "  ├─ with 2+ decoded neighbors:                              {} ({:.1}% of v1-fail)",
            agg.multi_neighbor, pct_multi_of_v1
        );
        println!(
            "  └─ AND ≥1 marginal-SNR neighbor (< {:.0} dB) (PRIMARY V2): {} ({:.1}% of v1-fail, {:.1}% of weak-missed)",
            MARGINAL_SNR_DB, agg.with_marginal_neighbor, pct_marginal_of_v1, pct_marginal_of_missed
        );

        let mut counts: Vec<(usize, usize)> = agg
            .neighbor_count_hist
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        counts.sort_by_key(|&(k, _)| k);
        println!(
            "\n  Nearby-neighbor count distribution (over {} weak-missed):",
            total_weak_missed
        );
        for (k, v) in &counts {
            let pct = 100.0 * *v as f64 / total_weak_missed as f64;
            println!(
                "    {:>2} neighbors:  {:>4} weak-missed ({:.1}%)",
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
            let p90 = sorted[(n * 9) / 10];
            let n_marginal = sorted.iter().filter(|s| **s < MARGINAL_SNR_DB).count();
            let pct_marginal_samples = 100.0 * n_marginal as f64 / n as f64;
            println!(
                "\n  Neighbor-decode SNR distribution (n={}): p10={:.1} p25={:.1} median={:.1} p90={:.1} dB",
                n, p10, p25, median, p90
            );
            println!(
                "    marginal (< {:.0} dB): {} samples ({:.1}%)\n",
                MARGINAL_SNR_DB, n_marginal, pct_marginal_samples
            );
        } else {
            println!();
        }
    }

    // ----- Per-bucket strict-window marginal-SNR rate (regime map) -----
    println!("--- Strict-window per-bucket regime (ΔSNR × Δf, aggregated over Δt) ---");
    println!(
        "  {:>6} {:>6} {:>6} {:>10} {:>10} {:>14}",
        "dSNR", "dF", "wavs", "weak_miss", "v1_fail", "marg/v1_fail"
    );
    for ((dsnr_k, df_k), (wavs, weak_miss, v1_fail, marg)) in &buckets {
        let dsnr = (*dsnr_k as f64) / 10.0;
        let df = (*df_k as f64) / 10.0;
        let pct_marg = if *v1_fail > 0 {
            100.0 * *marg as f64 / *v1_fail as f64
        } else {
            0.0
        };
        println!(
            "  {:>6.1} {:>6.1} {:>6} {:>10} {:>10} {:>13.1}%",
            dsnr, df, wavs, weak_miss, v1_fail, pct_marg
        );
    }

    // ----- 3-corpus comparison -----
    let v2_strict_marg_pct = if strict.v1_failed_proxy > 0 {
        100.0 * strict.with_marginal_neighbor as f64 / strict.v1_failed_proxy as f64
    } else {
        0.0
    };
    let v2_relaxed_marg_pct = if relaxed.v1_failed_proxy > 0 {
        100.0 * relaxed.with_marginal_neighbor as f64 / relaxed.v1_failed_proxy as f64
    } else {
        0.0
    };
    let v2_strict_multi_pct = if strict.v1_failed_proxy > 0 {
        100.0 * strict.multi_neighbor as f64 / strict.v1_failed_proxy as f64
    } else {
        0.0
    };
    let v2_relaxed_multi_pct = if relaxed.v1_failed_proxy > 0 {
        100.0 * relaxed.multi_neighbor as f64 / relaxed.v1_failed_proxy as f64
    } else {
        0.0
    };

    let (v2_relaxed_median, v2_relaxed_p10): (f64, f64) =
        if !relaxed.neighbor_snr_samples.is_empty() {
            let mut s = relaxed.neighbor_snr_samples.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            (s[s.len() / 2] as f64, s[s.len() / 10] as f64)
        } else {
            (f64::NAN, f64::NAN)
        };

    println!("\n=== 3-corpus comparison ===");
    println!(
        "{:>30} {:>14} {:>14} {:>14}",
        "metric", "OLD hard-200", "REFRESHED h200", "synth-pair-200"
    );
    println!(
        "{:>30} {:>13.1}% {:>13.1}% {:>13.1}%",
        "multi-neighbor strict",
        HARD200_OLD_MULTI_STRICT_PCT,
        HARD200_REFRESHED_MULTI_STRICT_PCT,
        v2_strict_multi_pct
    );
    println!(
        "{:>30} {:>13.1}% {:>13.1}% {:>13.1}%",
        "multi-neighbor relaxed",
        HARD200_OLD_MULTI_RELAXED_PCT,
        HARD200_REFRESHED_MULTI_RELAXED_PCT,
        v2_relaxed_multi_pct
    );
    println!(
        "{:>30} {:>13.1}% {:>13.1}% {:>13.1}%",
        "marginal-SNR strict (PRIMARY)",
        HARD200_OLD_MARGINAL_STRICT_PCT,
        HARD200_REFRESHED_MARGINAL_STRICT_PCT,
        v2_strict_marg_pct
    );
    println!(
        "{:>30} {:>13.1}% {:>13.1}% {:>13.1}%",
        "marginal-SNR relaxed (PRIMARY)",
        HARD200_OLD_MARGINAL_RELAXED_PCT,
        HARD200_REFRESHED_MARGINAL_RELAXED_PCT,
        v2_relaxed_marg_pct
    );
    println!(
        "{:>30} {:>13.1} {:>13.1} {:>13.1}",
        "nbr SNR median relaxed (dB)",
        HARD200_OLD_NBR_MEDIAN_RELAXED,
        HARD200_REFRESHED_NBR_MEDIAN_RELAXED,
        v2_relaxed_median
    );
    println!(
        "{:>30} {:>13.1} {:>13.1} {:>13.1}",
        "nbr SNR p10 relaxed (dB)",
        HARD200_OLD_NBR_P10_RELAXED,
        HARD200_REFRESHED_NBR_P10_RELAXED,
        v2_relaxed_p10
    );

    // ----- Decision per Phase A label convention -----
    let primary_best = v2_strict_marg_pct.max(v2_relaxed_marg_pct);
    let verdict = if primary_best >= MULTI_NEIGHBOR_THRESHOLD_PCT {
        "PROCEED to Session 2 — V2's mechanism has substrate on synth-pair-200. Implement lightweight soft cancellation pass; sweep on synth-pair-200 + hard-200; graduate only if hard-200 holds."
    } else if primary_best >= 10.0 {
        "MARGINAL — primary criterion partially clears. Document partial result; do not invest implementation effort without a follow-up diagnostic refinement."
    } else {
        "SHELVE on 3rd corpus — synth-pair-200's marginal-SNR rate remains below the 20% gate. The 0% finding on hard-200 was not an artifact of corpus saturation; pancetta's hard-decision pipeline produces uniformly sharp LLRs even when the underlying truth is marginal. Retain V2 as closed pending a fundamentally new mechanism variant."
    };

    println!(
        "\n  strict  primary (marginal-SNR among v1-fail):   {:.1}%",
        v2_strict_marg_pct
    );
    println!(
        "  relaxed primary (marginal-SNR among v1-fail):   {:.1}%",
        v2_relaxed_marg_pct
    );
    println!(
        "  gate: ≥{MULTI_NEIGHBOR_THRESHOLD_PCT:.0}% → PROCEED, 10–20% → MARGINAL, <10% → SHELVE\n"
    );
    println!("Verdict: {verdict}\n");

    Ok(())
}
