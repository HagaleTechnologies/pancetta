//! PAN-162 — rank capture days by impulsive (lightning-static) interference.
//!
//! Measures every slot with `impulsiveness::slot_impulsiveness` (no
//! decoding), groups slots by capture day, ranks the days, and optionally
//! writes a manifest of the stormiest days' impulsive slots for a targeted
//! A/B (`batch104_impulse_llr_storm_remeasure --manifest`).
//!
//!   impulse-survey --dir ~/.pancetta/recordings --output survey.json \
//!     [--emit-manifest storm_top3.manifest.json --top-days 3 --min-fraction 0.02]
use anyhow::{bail, Context, Result};
use pancetta_research::impulsiveness::{storm_manifest, survey, wavs_in_manifest, wavs_under};
use std::path::PathBuf;

const USAGE: &str =
    "usage: impulse-survey (--dir <path> | --manifest <path>)... --output <report.json>
       [--emit-manifest <path>] [--top-days N (default 3)] [--min-fraction F (default 0.02)]
       [--label <label>]";

fn main() -> Result<()> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut manifests: Vec<PathBuf> = Vec::new();
    let mut output: Option<PathBuf> = None;
    let mut emit: Option<PathBuf> = None;
    let mut top_days = 3usize;
    let mut min_fraction = 0.02f64;
    let mut label = String::from("impulse_storm_days");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--dir" => dirs.push(value()?.into()),
            "--manifest" => manifests.push(value()?.into()),
            "--output" => output = Some(value()?.into()),
            "--emit-manifest" => emit = Some(value()?.into()),
            "--top-days" => top_days = value()?.parse().context("--top-days")?,
            "--min-fraction" => {
                min_fraction = value()?.parse().context("--min-fraction")?;
                if !min_fraction.is_finite() || min_fraction < 0.0 {
                    bail!("--min-fraction must be a finite value >= 0");
                }
            }
            "--label" => label = value()?,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other}\n{USAGE}"),
        }
    }
    let output = output.with_context(|| format!("--output is required\n{USAGE}"))?;
    if dirs.is_empty() && manifests.is_empty() {
        bail!("give at least one --dir or --manifest\n{USAGE}");
    }
    let mut paths = Vec::new();
    for d in &dirs {
        paths.extend(wavs_under(d)?);
    }
    for m in &manifests {
        paths.extend(wavs_in_manifest(m)?);
    }
    let report = survey(&paths);
    report.save(&output)?;
    print!("{}", report.markdown_table());
    println!(
        "\n{} WAVs: {} undated, {} wrong rate, {} unreadable, {} silent → {}",
        paths.len(),
        report.undated_slots,
        report.skipped_wrong_rate,
        report.unreadable,
        report.silent_slots,
        output.display()
    );
    if let Some(path) = emit {
        let manifest = storm_manifest(&report, &label, top_days, min_fraction)?;
        std::fs::write(&path, serde_json::to_string_pretty(&manifest)? + "\n")
            .with_context(|| format!("write {}", path.display()))?;
        println!(
            "{} storm slots → {}",
            manifest.entries.len(),
            path.display()
        );
    }
    Ok(())
}
