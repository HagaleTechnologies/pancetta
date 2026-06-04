//! hb-216 — Hardware-tier probe CLI.
//!
//! Runs `pancetta_ft8::tier_probe::probe_hardware_tier` and reports
//! the host's tier classification + recommended runtime actions.
//!
//! Intended uses:
//! - Operator-side: run once at install time on a new machine to learn
//!   whether to flip `PANCETTA_SCOPED_FAST_PATH=1` (e.g., Windows 11
//!   MiniPC characterization).
//! - Future integration: pancetta's startup logs the same classification
//!   automatically; this CLI exists for ad-hoc / pre-flight runs.
//!
//! Run:
//!   cargo run --release -p pancetta-research --example tier_probe
//!
//! Override iteration count:
//!   cargo run --release -p pancetta-research --example tier_probe -- --iterations 20

use anyhow::{Context, Result};
use pancetta_ft8::tier_probe::{probe_hardware_tier, HardwareTier};
use std::process::Command;
use std::time::Duration;

const DEFAULT_ITERATIONS: usize = 10;

fn cpu_model() -> String {
    if cfg!(target_os = "macos") {
        Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    } else if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with("model name")).map(|l| {
                    l.trim_start_matches("model name")
                        .trim_start_matches(": ")
                        .trim()
                        .to_string()
                })
            })
            .unwrap_or_else(|| "unknown".to_string())
    } else {
        "unknown".to_string()
    }
}

fn cpu_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn tier_label(t: HardwareTier) -> &'static str {
    match t {
        HardwareTier::Fast => "FAST",
        HardwareTier::Moderate => "MODERATE",
        HardwareTier::Slow => "SLOW",
    }
}

fn main() -> Result<()> {
    let mut iterations = DEFAULT_ITERATIONS;
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--iterations" => {
                iterations = iter
                    .next()
                    .context("--iterations needs a value")?
                    .parse()
                    .context("--iterations not a number")?;
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }

    println!("== Pancetta Hardware-Tier Probe ==");
    println!("  CPU model:     {}", cpu_model());
    println!("  Logical cores: {}", cpu_cores());
    println!("  Target arch:   {}", std::env::consts::ARCH);
    println!("  OS:            {}", std::env::consts::OS);
    println!("  Iterations:    {iterations}");
    println!();
    println!(
        "Probing... (this takes ~{}s on M4-class hardware)",
        iterations
    );
    println!();

    let result =
        probe_hardware_tier(iterations).map_err(|e| anyhow::anyhow!("probe failed: {e}"))?;

    println!("== Results ==");
    println!(
        "  Wall-clock:    p50={:.0}ms  p95={:.0}ms  p99={:.0}ms  max={:.0}ms",
        ms(result.p50),
        ms(result.p95),
        ms(result.p99),
        ms(result.max),
    );
    println!("  Tier:          {}", tier_label(result.tier));
    println!();

    if result.recommendations.is_empty() {
        println!("== Recommendations ==");
        println!("  No tuning needed — defaults are fine on this hardware.");
    } else {
        println!("== Recommendations ==");
        for (i, rec) in result.recommendations.iter().enumerate() {
            println!("  {}. [{}]", i + 1, rec.key);
            println!("     {}", rec.message);
        }
    }
    println!();

    Ok(())
}
