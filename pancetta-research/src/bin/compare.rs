//! compare — diff two scorecards into a focused wins/regressions report.

use anyhow::Context;
use pancetta_research::scorecard::Scorecard;
use std::path::PathBuf;

#[derive(Debug)]
struct Args {
    a: PathBuf,
    b: PathBuf,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut args = std::env::args().skip(1);
        let a = args.next().context("usage: compare A.json B.json")?.into();
        let b = args.next().context("usage: compare A.json B.json")?.into();
        Ok(Self { a, b })
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

    let diffs = config_diff(&a.config.decoder, &b.config.decoder);
    if !diffs.is_empty() {
        println!("CONFIG DIFF:");
        for (k, av, bv) in diffs {
            println!("  decoder.{k:<40} {av} → {bv}");
        }
    }

    Ok(())
}
