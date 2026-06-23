//! compare — diff two scorecards into a focused wins/regressions report.
//!
//! Phase B (2026-06-01): when both scorecards expose full per-WAV records
//! (the `per_wav_records` field, populated by eval ≥ 2026-06-01), compare
//! also emits a nonparametric bootstrap 95 % CI on the per-tier recall
//! and novel deltas. If 0 ∈ CI, the headline delta is reported as
//! "NOT significant" — useful for distinguishing real wins from
//! single-run rayon/OSD/corpus noise.
//!
//! Default knobs:
//! - `--bootstrap` enables the CI (default on).
//! - `--no-bootstrap` disables (e.g. for legacy scorecards).
//! - `--bootstrap-n N` sets the number of resamples (default 1000).
//! - `--bootstrap-seed S` sets the deterministic seed (default 0xb007).

use anyhow::Context;
use pancetta_research::bootstrap_ci::{bootstrap_novel_delta, bootstrap_recall_delta, DeltaCi};
use pancetta_research::scorecard::{PerWavRecord, Scorecard, TierResult};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug)]
struct Args {
    a: PathBuf,
    b: PathBuf,
    bootstrap: bool,
    bootstrap_n: usize,
    bootstrap_seed: u64,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut bootstrap = true;
        let mut bootstrap_n: usize = 1000;
        let mut bootstrap_seed: u64 = 0xb007;
        let mut positional: Vec<PathBuf> = Vec::new();
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--bootstrap" => bootstrap = true,
                "--no-bootstrap" => bootstrap = false,
                "--bootstrap-n" => {
                    bootstrap_n = iter
                        .next()
                        .context("--bootstrap-n requires a value")?
                        .parse()
                        .context("--bootstrap-n must be a positive integer")?;
                }
                "--bootstrap-seed" => {
                    bootstrap_seed = iter
                        .next()
                        .context("--bootstrap-seed requires a value")?
                        .parse()
                        .context("--bootstrap-seed must be a u64")?;
                }
                "-h" | "--help" => {
                    println!(
                        "usage: compare A.json B.json [--no-bootstrap] [--bootstrap-n N] [--bootstrap-seed S]"
                    );
                    std::process::exit(0);
                }
                other if other.starts_with("--") => {
                    anyhow::bail!("unknown flag: {other}");
                }
                _ => positional.push(arg.into()),
            }
        }
        anyhow::ensure!(positional.len() == 2, "usage: compare A.json B.json");
        Ok(Self {
            a: positional[0].clone(),
            b: positional[1].clone(),
            bootstrap,
            bootstrap_n,
            bootstrap_seed,
        })
    }
}

fn fmt_pct(x: f64) -> String {
    format!("{:.4}", x)
}

fn fmt_snr(x: Option<f64>) -> String {
    match x {
        Some(v) => format!("{:+.1} dB", v),
        None => "n/a".to_string(),
    }
}

fn fmt_ci_int(ci: &DeltaCi) -> String {
    let sig = if ci.significant {
        "significant"
    } else {
        "NOT significant"
    };
    format!(
        "(95% CI [{:+.1}, {:+.1}], n_bootstrap={}) — {}",
        ci.ci_low, ci.ci_high, ci.n_bootstrap, sig,
    )
}

fn config_diff(a: &serde_json::Value, b: &serde_json::Value) -> Vec<(String, String, String)> {
    let mut diffs = Vec::new();
    diff_recursive("decoder", a, b, &mut diffs);
    diffs
}

fn diff_recursive(
    prefix: &str,
    a: &serde_json::Value,
    b: &serde_json::Value,
    out: &mut Vec<(String, String, String)>,
) {
    match (a, b) {
        (serde_json::Value::Object(am), serde_json::Value::Object(bm)) => {
            let mut keys: Vec<&String> = am.keys().chain(bm.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let next_prefix = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                match (am.get(k), bm.get(k)) {
                    (Some(av), Some(bv)) => diff_recursive(&next_prefix, av, bv, out),
                    (Some(av), None) => {
                        out.push((next_prefix, value_to_string(av), "<unset>".into()))
                    }
                    (None, Some(bv)) => {
                        out.push((next_prefix, "<unset>".into(), value_to_string(bv)))
                    }
                    (None, None) => {}
                }
            }
        }
        (av, bv) if av != bv => {
            out.push((prefix.to_string(), value_to_string(av), value_to_string(bv)));
        }
        _ => {}
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Phase B: align A's and B's per-WAV records by `wav_hash` so the
/// bootstrap sees the same WAV order on both sides. WAVs present in
/// only one scorecard are dropped (with a count returned to the caller
/// so it can be reported as a caveat).
///
/// Returns `(a_aligned, b_aligned, dropped_only_a, dropped_only_b)`
/// where the aligned vectors are `(recovered, truth)` and `(novel, _)`
/// pairs in the same WAV order — suitable inputs for
/// `bootstrap_recall_delta` / `bootstrap_novel_delta`.
// rationale: the 6-tuple return is documented field-by-field inline below; a
// type alias would hoist the names away from these comments and read worse.
#[allow(clippy::type_complexity)]
fn align_per_wav(
    a: &[PerWavRecord],
    b: &[PerWavRecord],
) -> (
    Vec<(u32, u32)>, // a recall = (recovered, truth)
    Vec<(u32, u32)>, // b recall
    Vec<(u32, u32)>, // a novel = (novel, truth)
    Vec<(u32, u32)>, // b novel
    usize,           // dropped_only_a
    usize,           // dropped_only_b
) {
    let a_map: BTreeMap<&str, &PerWavRecord> = a.iter().map(|r| (r.wav_hash.as_str(), r)).collect();
    let b_map: BTreeMap<&str, &PerWavRecord> = b.iter().map(|r| (r.wav_hash.as_str(), r)).collect();
    let mut a_recall = Vec::new();
    let mut b_recall = Vec::new();
    let mut a_novel = Vec::new();
    let mut b_novel = Vec::new();
    let mut common_hashes: Vec<&str> = a_map
        .keys()
        .filter(|k| b_map.contains_key(*k))
        .copied()
        .collect();
    common_hashes.sort();
    for hash in &common_hashes {
        let ar = a_map[hash];
        let br = b_map[hash];
        a_recall.push((ar.recovered, ar.truth));
        b_recall.push((br.recovered, br.truth));
        a_novel.push((ar.novel, ar.truth));
        b_novel.push((br.novel, br.truth));
    }
    let dropped_only_a = a_map.keys().filter(|k| !b_map.contains_key(*k)).count();
    let dropped_only_b = b_map.keys().filter(|k| !a_map.contains_key(*k)).count();
    (
        a_recall,
        b_recall,
        a_novel,
        b_novel,
        dropped_only_a,
        dropped_only_b,
    )
}

/// Render per-tier bootstrap CIs. Skips tiers where either side lacks
/// full `per_wav_records`. Returns a vector of report lines.
fn render_bootstrap_section(
    a: &Scorecard,
    b: &Scorecard,
    n_bootstrap: usize,
    seed: u64,
) -> Vec<String> {
    let mut out = Vec::new();
    let tier_keys: Vec<&String> = a
        .tiers
        .keys()
        .chain(b.tiers.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for tier in tier_keys {
        let (at, bt) = match (a.tiers.get(tier), b.tiers.get(tier)) {
            (Some(at), Some(bt)) => (at, bt),
            _ => continue,
        };
        if at.per_wav_records.is_empty() || bt.per_wav_records.is_empty() {
            // Skip tiers we can't bootstrap — typically synth, fixtures,
            // or any scorecard predating Phase B.
            continue;
        }
        let (a_rec, b_rec, a_nov, b_nov, dropped_a, dropped_b) =
            align_per_wav(&at.per_wav_records, &bt.per_wav_records);
        if a_rec.is_empty() {
            out.push(format!(
                "  {tier:<24}  bootstrap: no overlapping WAVs between A and B (skipped)"
            ));
            continue;
        }
        let n_common = a_rec.len();
        let rec_ci = bootstrap_recall_delta(&a_rec, &b_rec, n_bootstrap, seed);
        let nov_ci = bootstrap_novel_delta(&a_nov, &b_nov, n_bootstrap, seed.wrapping_add(1));
        let delta_rec: i64 = b_rec.iter().map(|(r, _)| *r as i64).sum::<i64>()
            - a_rec.iter().map(|(r, _)| *r as i64).sum::<i64>();
        let delta_nov: i64 = b_nov.iter().map(|(r, _)| *r as i64).sum::<i64>()
            - a_nov.iter().map(|(r, _)| *r as i64).sum::<i64>();
        out.push(format!(
            "  {tier:<24}  rec Δ={:+}  {}",
            delta_rec,
            fmt_ci_int(&rec_ci),
        ));
        out.push(format!(
            "  {tier:<24}  novel Δ={:+}  {}",
            delta_nov,
            fmt_ci_int(&nov_ci),
        ));
        if dropped_a > 0 || dropped_b > 0 {
            out.push(format!(
                "  {tier:<24}    (caveat: aligned over {n_common} common WAVs; dropped {dropped_a} A-only / {dropped_b} B-only)"
            ));
        }
    }
    out
}

/// Phase B fallback: if neither A nor B carries `per_wav_records` for
/// any tier (older scorecards), emit a single banner explaining why no
/// CI ran. Avoids the silent-skip footgun where a stale scorecard makes
/// the CI section look "fine — no CI lines printed".
fn any_tier_has_per_wav_records(card: &Scorecard) -> bool {
    card.tiers
        .values()
        .any(|t: &TierResult| !t.per_wav_records.is_empty())
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    let a = Scorecard::load(&args.a).with_context(|| format!("loading A: {}", args.a.display()))?;
    let b = Scorecard::load(&args.b).with_context(|| format!("loading B: {}", args.b.display()))?;

    println!(
        "A: {} (sha {}, score {})",
        args.a.display(),
        &a.git.head_sha[..8.min(a.git.head_sha.len())],
        fmt_pct(a.composite.score)
    );
    println!(
        "B: {} (sha {}, score {} {}{})",
        args.b.display(),
        &b.git.head_sha[..8.min(b.git.head_sha.len())],
        fmt_pct(b.composite.score),
        if b.composite.score >= a.composite.score {
            "+"
        } else {
            ""
        },
        fmt_pct(b.composite.score - a.composite.score),
    );
    println!();

    let mut wins: Vec<String> = Vec::new();
    let mut regressions: Vec<String> = Vec::new();

    // Walk each tier present in both.
    let tier_keys: Vec<&String> = a
        .tiers
        .keys()
        .chain(b.tiers.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for tier in tier_keys {
        match (a.tiers.get(tier), b.tiers.get(tier)) {
            (Some(at), Some(bt)) => {
                // SNR @ 50% — lower is better.
                if at.snr_at_50pct_recovery_db != bt.snr_at_50pct_recovery_db {
                    let delta = bt.snr_at_50pct_recovery_db.unwrap_or(0.0)
                        - at.snr_at_50pct_recovery_db.unwrap_or(0.0);
                    let bucket = if delta < 0.0 {
                        &mut wins
                    } else {
                        &mut regressions
                    };
                    bucket.push(format!(
                        "  {tier:<20}  SNR@50%       {} → {}  ({:+.1} dB)",
                        fmt_snr(at.snr_at_50pct_recovery_db),
                        fmt_snr(bt.snr_at_50pct_recovery_db),
                        delta,
                    ));
                }
                // TODO(plan-3): surface fixtures_skipped delta if it changed
                // so promotions Skip → AnyDecode/Exact are visible in the report.

                // Pass rate — higher is better.
                if at.pass_rate != bt.pass_rate {
                    let delta = bt.pass_rate.unwrap_or(0.0) - at.pass_rate.unwrap_or(0.0);
                    let bucket = if delta > 0.0 {
                        &mut wins
                    } else {
                        &mut regressions
                    };
                    bucket.push(format!(
                        "  {tier:<20}  pass_rate     {:.4} → {:.4}  ({:+.4})",
                        at.pass_rate.unwrap_or(0.0),
                        bt.pass_rate.unwrap_or(0.0),
                        delta,
                    ));
                }
                // Decode rate — higher is better.
                if at.decode_rate != bt.decode_rate {
                    let delta = bt.decode_rate.unwrap_or(0.0) - at.decode_rate.unwrap_or(0.0);
                    let bucket = if delta > 0.0 {
                        &mut wins
                    } else {
                        &mut regressions
                    };
                    bucket.push(format!(
                        "  {tier:<20}  decode_rate   {:.4} → {:.4}  ({:+.4})",
                        at.decode_rate.unwrap_or(0.0),
                        bt.decode_rate.unwrap_or(0.0),
                        delta,
                    ));
                }
            }
            (Some(_), None) => regressions.push(format!("  {tier:<20}  removed in B")),
            (None, Some(_)) => wins.push(format!("  {tier:<20}  added in B")),
            (None, None) => {}
        }
    }

    if !wins.is_empty() {
        println!("WINS:");
        for w in &wins {
            println!("{w}");
        }
        println!();
    }
    if !regressions.is_empty() {
        println!("REGRESSIONS:");
        for r in &regressions {
            println!("{r}");
        }
        println!();
    } else {
        println!("REGRESSIONS:\n  (none)\n");
    }

    // Phase B: nonparametric bootstrap CIs on per-tier recall/novel
    // deltas. Distinguishes real wins from same-config rayon/OSD noise.
    if args.bootstrap {
        let lines = render_bootstrap_section(&a, &b, args.bootstrap_n, args.bootstrap_seed);
        if !lines.is_empty() {
            println!(
                "BOOTSTRAP CI (n_bootstrap={}, seed=0x{:x}):",
                args.bootstrap_n, args.bootstrap_seed
            );
            for line in &lines {
                println!("{line}");
            }
            println!();
        } else if !any_tier_has_per_wav_records(&a) && !any_tier_has_per_wav_records(&b) {
            println!(
                "BOOTSTRAP CI:\n  (skipped: neither scorecard carries `per_wav_records`. \
                 Re-eval with the Phase-B build to enable.)\n"
            );
        } else {
            println!(
                "BOOTSTRAP CI:\n  (skipped: one side lacks per_wav_records for the overlapping tiers)\n"
            );
        }
    }

    let diffs = config_diff(&a.config.decoder, &b.config.decoder);
    if !diffs.is_empty() {
        println!("CONFIG DIFF:");
        for (k, av, bv) in diffs {
            println!("  decoder.{k:<40} {av} → {bv}");
        }
    }

    Ok(())
}
