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
//!   Accepts decimal or `0x`-prefixed hexadecimal.
//! - `--max-elapsed-regression-pct P` sets the elapsed-time budget (default
//!   20 %). Enforced as a hard gate when both runs share a host and core
//!   count; reported but not enforced otherwise.

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
    max_elapsed_regression_pct: f64,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut bootstrap = true;
        let mut bootstrap_n: usize = 1000;
        let mut bootstrap_seed: u64 = 0xb007;
        let mut max_elapsed_regression_pct: f64 = 20.0;
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
                    let raw = iter.next().context("--bootstrap-seed requires a value")?;
                    bootstrap_seed = parse_u64_auto(&raw)
                        .with_context(|| format!("--bootstrap-seed must be a u64 (got {raw:?})"))?;
                }
                "--max-elapsed-regression-pct" => {
                    let raw = iter
                        .next()
                        .context("--max-elapsed-regression-pct requires a value")?;
                    max_elapsed_regression_pct = raw.parse().with_context(|| {
                        format!("--max-elapsed-regression-pct must be a number (got {raw:?})")
                    })?;
                    anyhow::ensure!(
                        max_elapsed_regression_pct.is_finite() && max_elapsed_regression_pct >= 0.0,
                        "--max-elapsed-regression-pct must be a finite, non-negative percentage"
                    );
                }
                "-h" | "--help" => {
                    println!(
                        "usage: compare A.json B.json [--no-bootstrap] [--bootstrap-n N] \
                         [--bootstrap-seed S] [--max-elapsed-regression-pct P]"
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
            max_elapsed_regression_pct,
        })
    }
}

/// Parse a u64 in either decimal or `0x`-prefixed hexadecimal.
///
/// The seed's own default is documented as `0xb007`, so an operator copying
/// that literal back onto `--bootstrap-seed` must not be rejected by a
/// decimal-only parser. Underscores are permitted as digit separators.
fn parse_u64_auto(raw: &str) -> anyhow::Result<u64> {
    let trimmed = raw.trim();
    let cleaned: String = trimmed.replace('_', "");
    let parsed = match cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        Some(hex) => {
            anyhow::ensure!(!hex.is_empty(), "hexadecimal literal has no digits");
            u64::from_str_radix(hex, 16)?
        }
        None => cleaned.parse::<u64>()?,
    };
    Ok(parsed)
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

/// Workstream 0 (2026-07-06) — the FP-on-noise hard gate. Unlike every
/// other metric in this binary (advisory: printed as a WIN/REGRESSION but
/// never fails the process), ANY increase in `false_positives_total` or
/// `noise_files_decoded` on ANY shared tier is a hard failure: the harness
/// exists specifically so a hallucinating decoder can no longer score
/// identically to a correct one (design spec §2, decision D0). There is
/// no threshold — a +1 is exactly as disqualifying as +7000.
///
/// Returns one report line per regressing tier; empty means the gate
/// passed (A had no FP tier, or B's count never exceeded A's).
fn fp_on_noise_hard_gate(a: &Scorecard, b: &Scorecard) -> Vec<String> {
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
        let a_fp = at.false_positives_total.unwrap_or(0);
        let b_fp = bt.false_positives_total.unwrap_or(0);
        if b_fp > a_fp {
            out.push(format!(
                "  {tier:<20}  false_positives_total   {a_fp} → {b_fp}  (+{})",
                b_fp - a_fp
            ));
        }
        let a_ndec = at.noise_files_decoded.unwrap_or(0);
        let b_ndec = bt.noise_files_decoded.unwrap_or(0);
        if b_ndec > a_ndec {
            out.push(format!(
                "  {tier:<20}  noise_files_decoded     {a_ndec} → {b_ndec}  (+{})",
                b_ndec - a_ndec
            ));
        }
    }
    out
}

/// Task W0.3 (2026-07-06) — the unverified-novel standing-gate term
/// (design spec §2, decision D0(c); plan Global Constraints / the
/// standing TP gate: "unverified-novel count increase ≤ 2× verified-TP
/// increase"). Unlike the zero-tolerance FP-on-noise gate above, this one
/// is proportional: recall improvements structurally tend to pick up a
/// few more borderline decodes alongside genuine gains, so SOME growth in
/// unverified novels is expected. Growth disproportionate to the
/// verified-TP gain (more than double) is the "hallucinating harder"
/// failure mode this term exists to catch.
///
/// ΔTP is the aggregate `truth_decodes_recovered` delta and
/// Δunverified-novels is the aggregate `novels_unverified` delta, summed
/// across every shared tier where at least one side reports
/// `novels_unverified` (tiers that never ran novel classification, e.g.
/// fixtures/synth/noise, are excluded from the sum rather than silently
/// contributing zero on both sides).
///
/// Returns `Vec::new()` when the gate passes (including when neither
/// scorecard carries any `novels_unverified` data — nothing to gate on).
/// Outcome of the elapsed-time gate (PAN-9 A/B runbook).
#[derive(Debug, PartialEq)]
enum ElapsedGate {
    /// Not comparable — different host or core count, or a non-positive
    /// baseline. `elapsed_seconds` is wall-clock and host-bound, so comparing
    /// across machines would be meaningless; report and do not enforce.
    Skipped(String),
    /// Comparable and within budget.
    Passed(String),
    /// Comparable and over budget — a hard failure.
    Failed(String),
}

/// PAN-9 (2026-08-11) — the elapsed-time gate the depth-1 A/B runbook cites.
///
/// The runbook makes "clears the elapsed gate" a ship condition, so the
/// threshold behind it has to be defined and enforced rather than left to the
/// operator's eye. A candidate that recovers more messages but costs a large
/// runtime regression is not automatically shippable.
///
/// Enforcement is conditional on comparability: `HarnessInfo::elapsed_seconds`
/// is wall-clock, so it is only meaningful between runs on the same host with
/// the same core count.
fn elapsed_regression_gate(a: &Scorecard, b: &Scorecard, max_pct: f64) -> ElapsedGate {
    let (base, cand) = (a.harness.elapsed_seconds, b.harness.elapsed_seconds);
    if a.harness.host != b.harness.host {
        return ElapsedGate::Skipped(format!(
            "  (skipped: host differs — {} vs {}; wall-clock is not comparable across machines)",
            a.harness.host, b.harness.host
        ));
    }
    if a.harness.cores_used != b.harness.cores_used {
        return ElapsedGate::Skipped(format!(
            "  (skipped: cores_used differs — {} vs {})",
            a.harness.cores_used, b.harness.cores_used
        ));
    }
    // Host/cores alone don't prove the two runs did the same WORK — a
    // candidate that accidentally omits a tier (or a WAV within a shared
    // tier) can clear the 20% budget simply by doing less of it. Require
    // the same tier set and, for every shared tier, the same
    // wavs_processed count before trusting the elapsed comparison.
    if a.config.tiers_run != b.config.tiers_run {
        return ElapsedGate::Skipped(format!(
            "  (skipped: tiers_run differs — {:?} vs {:?})",
            a.config.tiers_run, b.config.tiers_run
        ));
    }
    for (tier_name, a_result) in &a.tiers {
        if let Some(b_result) = b.tiers.get(tier_name) {
            if a_result.wavs_processed != b_result.wavs_processed {
                return ElapsedGate::Skipped(format!(
                    "  (skipped: tier '{tier_name}' wavs_processed differs — {} vs {})",
                    a_result.wavs_processed, b_result.wavs_processed
                ));
            }
        }
    }
    if !base.is_finite() || !cand.is_finite() || base <= 0.0 {
        return ElapsedGate::Skipped(format!(
            "  (skipped: baseline elapsed_seconds is {base}; nothing to compare against)"
        ));
    }
    let delta_pct = (cand - base) / base * 100.0;
    let line = format!(
        "  elapsed_seconds        {base:.1} → {cand:.1}  ({delta_pct:+.1}%, budget +{max_pct:.1}%)"
    );
    if delta_pct > max_pct {
        ElapsedGate::Failed(line)
    } else {
        ElapsedGate::Passed(line)
    }
}

fn unverified_novel_standing_gate(a: &Scorecard, b: &Scorecard) -> Vec<String> {
    let tier_keys: Vec<&String> = a
        .tiers
        .keys()
        .chain(b.tiers.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut delta_tp: i64 = 0;
    let mut delta_unverified: i64 = 0;
    let mut any_data = false;
    let mut per_tier: Vec<(String, i64, i64)> = Vec::new();
    for tier in tier_keys {
        let (at, bt) = match (a.tiers.get(tier), b.tiers.get(tier)) {
            (Some(at), Some(bt)) => (at, bt),
            _ => continue,
        };
        if at.novels_unverified.is_none() && bt.novels_unverified.is_none() {
            continue;
        }
        any_data = true;
        let tier_delta_tp = bt.truth_decodes_recovered.unwrap_or(0) as i64
            - at.truth_decodes_recovered.unwrap_or(0) as i64;
        let tier_delta_unverified =
            bt.novels_unverified.unwrap_or(0) as i64 - at.novels_unverified.unwrap_or(0) as i64;
        delta_tp += tier_delta_tp;
        delta_unverified += tier_delta_unverified;
        per_tier.push((tier.clone(), tier_delta_tp, tier_delta_unverified));
    }
    if !any_data {
        return Vec::new();
    }
    let allowance = 2 * delta_tp.max(0);
    if delta_unverified <= allowance {
        return Vec::new();
    }
    let mut out = vec![format!(
        "  aggregate: unverified-novels Δ={delta_unverified:+} exceeds allowance {allowance:+} \
         (2×ΔTP, ΔTP={delta_tp:+})"
    )];
    for (tier, tp, unverified) in per_tier {
        if tp != 0 || unverified != 0 {
            out.push(format!(
                "  {tier:<20}  verified-TP Δ={tp:+}   unverified-novels Δ={unverified:+}"
            ));
        }
    }
    out
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

    // Workstream 0 (2026-07-06) — HARD GATE. Any FP-on-noise increase
    // fails the comparison outright, regardless of every other metric
    // above. Printed last (most prominent — the thing the operator's eye
    // lands on) and enforced via a nonzero exit code so this gate is
    // usable in scripts/CI-adjacent tooling, not just human review.
    let fp_gate_failures = fp_on_noise_hard_gate(&a, &b);
    if !fp_gate_failures.is_empty() {
        println!();
        println!("############################################################");
        println!("# HARD GATE FAILURE — FALSE POSITIVES INCREASED ON NOISE TIER");
        println!("# ANY increase disqualifies this change. See design spec §2 (D0).");
        println!("############################################################");
        for line in &fp_gate_failures {
            println!("{line}");
        }
        println!();
        std::process::exit(1);
    }

    // PAN-9 (2026-08-11) — the elapsed-time gate. Always reported so the
    // runbook's "clears the elapsed gate" step has a defined pass/fail, and
    // enforced with a nonzero exit when the two runs are actually comparable.
    let elapsed_gate = elapsed_regression_gate(&a, &b, args.max_elapsed_regression_pct);
    println!("ELAPSED GATE:");
    match &elapsed_gate {
        ElapsedGate::Skipped(line) | ElapsedGate::Passed(line) => println!("{line}"),
        ElapsedGate::Failed(line) => println!("{line}"),
    }
    println!();
    if let ElapsedGate::Failed(line) = &elapsed_gate {
        println!("############################################################");
        println!("# HARD GATE FAILURE — ELAPSED TIME REGRESSED BEYOND BUDGET");
        println!("# Raise --max-elapsed-regression-pct only with an explicit rationale.");
        println!("############################################################");
        println!("{line}");
        println!();
        std::process::exit(1);
    }

    // Task W0.3 (2026-07-06) — the unverified-novel standing-gate term.
    // Same enforcement style as the FP-on-noise gate above (prominent
    // banner + nonzero exit), extending the standing A/B gate per the
    // plan's Global Constraints: "unverified-novel count increase ≤ 2×
    // verified-TP increase".
    let novel_gate_failures = unverified_novel_standing_gate(&a, &b);
    if !novel_gate_failures.is_empty() {
        println!();
        println!("############################################################");
        println!("# HARD GATE FAILURE — UNVERIFIED-NOVEL GROWTH EXCEEDS 2×ΔTP");
        println!("# See design spec §2 (D0) / plan Global Constraints (standing gate).");
        println!("############################################################");
        for line in &novel_gate_failures {
            println!("{line}");
        }
        println!();
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_research::scorecard::{
        BuildInfo, CompositeInfo, ConfigInfo, GitInfo, HarnessInfo, RegressionFlags,
    };

    fn card(host: &str, cores: usize, elapsed: f64) -> Scorecard {
        Scorecard {
            schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
            generated_at: chrono::Utc::now(),
            mode: pancetta_research::Mode::Ft8,
            git: GitInfo {
                branch: "test".into(),
                head_sha: "0000000".into(),
                main_merge_base: "0000000".into(),
                dirty: false,
            },
            build: BuildInfo {
                rustc_version: "1.85.0".into(),
                release: true,
                features: vec![],
            },
            harness: HarnessInfo {
                harness_version: "test".into(),
                host: host.into(),
                cores_used: cores,
                elapsed_seconds: elapsed,
            },
            config: ConfigInfo {
                decoder: serde_json::json!({}),
                seed: 0,
                tiers_run: vec![],
                fp_filter_active: false,
            },
            tiers: BTreeMap::new(),
            composite: CompositeInfo {
                weights: BTreeMap::new(),
                score: 0.0,
                main_baseline_score: None,
                delta_vs_main: None,
            },
            regressions: RegressionFlags::default(),
            notes: String::new(),
        }
    }

    #[test]
    fn bootstrap_seed_accepts_the_hex_literal_the_docs_advertise() {
        // The documented default is 0xb007; an operator passing it back must
        // not be rejected by a decimal-only parser.
        assert_eq!(parse_u64_auto("0xb007").unwrap(), 0xb007);
        assert_eq!(parse_u64_auto("0XB007").unwrap(), 0xb007);
        assert_eq!(parse_u64_auto("45063").unwrap(), 0xb007);
        assert_eq!(parse_u64_auto("0xb_007").unwrap(), 0xb007);
    }

    #[test]
    fn bootstrap_seed_rejects_garbage() {
        assert!(parse_u64_auto("0x").is_err());
        assert!(parse_u64_auto("zzz").is_err());
        assert!(parse_u64_auto("-1").is_err());
    }

    #[test]
    fn elapsed_gate_fails_when_regression_exceeds_budget() {
        let a = card("h", 8, 100.0);
        let b = card("h", 8, 130.0); // +30 % against a 20 % budget
        assert!(matches!(
            elapsed_regression_gate(&a, &b, 20.0),
            ElapsedGate::Failed(_)
        ));
    }

    #[test]
    fn elapsed_gate_passes_within_budget_and_on_improvement() {
        let a = card("h", 8, 100.0);
        assert!(matches!(
            elapsed_regression_gate(&a, &card("h", 8, 110.0), 20.0),
            ElapsedGate::Passed(_)
        ));
        assert!(matches!(
            elapsed_regression_gate(&a, &card("h", 8, 80.0), 20.0),
            ElapsedGate::Passed(_)
        ));
    }

    #[test]
    fn elapsed_gate_skips_when_runs_are_not_comparable() {
        let a = card("host-a", 8, 100.0);
        // Wall-clock across different machines or core counts is meaningless.
        assert!(matches!(
            elapsed_regression_gate(&a, &card("host-b", 8, 500.0), 20.0),
            ElapsedGate::Skipped(_)
        ));
        assert!(matches!(
            elapsed_regression_gate(&a, &card("host-a", 4, 500.0), 20.0),
            ElapsedGate::Skipped(_)
        ));
        // A zero baseline has no meaningful percentage.
        assert!(matches!(
            elapsed_regression_gate(&card("host-a", 8, 0.0), &card("host-a", 8, 5.0), 20.0),
            ElapsedGate::Skipped(_)
        ));
    }

    #[test]
    fn elapsed_gate_skips_when_tiers_run_differs() {
        let mut a = card("host-a", 8, 100.0);
        let mut b = card("host-a", 8, 105.0);
        a.config.tiers_run = vec!["hard-200".into(), "hard-1000".into()];
        b.config.tiers_run = vec!["hard-200".into()];
        assert!(matches!(
            elapsed_regression_gate(&a, &b, 20.0),
            ElapsedGate::Skipped(_)
        ));
    }

    #[test]
    fn elapsed_gate_skips_when_a_shared_tier_processed_different_wav_counts() {
        let mut a = card("host-a", 8, 100.0);
        let mut b = card("host-a", 8, 105.0);
        a.config.tiers_run = vec!["hard-1000".into()];
        b.config.tiers_run = vec!["hard-1000".into()];
        a.tiers.insert(
            "hard-1000".into(),
            TierResult {
                wavs_processed: 1000,
                ..Default::default()
            },
        );
        // Candidate accidentally processed fewer WAVs in the same tier —
        // doing less work must not let it clear the gate on that basis.
        b.tiers.insert(
            "hard-1000".into(),
            TierResult {
                wavs_processed: 400,
                ..Default::default()
            },
        );
        assert!(matches!(
            elapsed_regression_gate(&a, &b, 20.0),
            ElapsedGate::Skipped(_)
        ));
    }
}
