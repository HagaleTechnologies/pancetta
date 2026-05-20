//! eval — runs a DecoderUnderTest against requested corpus tiers and emits a
//! scorecard. Plan 1 supports the fixtures tier only.

use anyhow::Context;
use chrono::Utc;
use pancetta_research::corpus::load_ft8_fixtures;
use pancetta_research::decoder::{DecoderUnderTest, Ft8Decoder};
use pancetta_research::metrics::{default_weights, populate_composite};
use pancetta_research::scorecard::{
    BuildInfo, ConfigInfo, GitInfo, HarnessInfo, RegressionFlags, Scorecard, TierResult,
};
use pancetta_research::Mode;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug)]
struct Args {
    tiers: Vec<String>,
    mode: Mode,
    output: PathBuf,
    seed: u64,
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut tiers: Option<Vec<String>> = None;
        let mut mode: Option<Mode> = None;
        let mut output: Option<PathBuf> = None;
        let mut seed: u64 = 42;
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--tier" | "--tiers" => {
                    tiers = Some(
                        iter.next()
                            .context("--tier needs a value")?
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect(),
                    );
                }
                "--mode" => {
                    mode = Some(
                        iter.next()
                            .context("--mode needs a value")?
                            .parse::<Mode>()
                            .map_err(|e| anyhow::anyhow!("{e}"))?,
                    );
                }
                "--output" => {
                    output = Some(iter.next().context("--output needs a value")?.into());
                }
                "--seed" => {
                    seed = iter.next().context("--seed needs a value")?.parse()?;
                }
                "-h" | "--help" => {
                    eprintln!("usage: eval --tier <tiers,...> --mode <mode> --output <path> [--seed N]");
                    eprintln!("  plan 1: only --tier fixtures is supported");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            tiers: tiers.context("--tier required")?,
            mode: mode.context("--mode required")?,
            output: output.context("--output required")?,
            seed,
        })
    }
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn run_fixtures_tier(
    decoder: &dyn DecoderUnderTest,
    workspace: &std::path::Path,
) -> anyhow::Result<TierResult> {
    let fixtures = load_ft8_fixtures(workspace)?;
    let total = fixtures.len() as u32;
    let mut passed = 0u32;
    let mut failures = Vec::new();
    for f in &fixtures {
        match decoder.decode_wav(&f.wav_path) {
            Ok(decodes) if !decodes.is_empty() => {
                // Plan 1: "pass" means "decoded ≥ 1 message and did not error."
                // Plan 2 will compare against truth.json for exact-message match.
                passed += 1;
            }
            Ok(_) => failures.push(pancetta_research::scorecard::FixtureFailure {
                wav: f.display_name.clone(),
                expected: vec!["any decode".into()],
                got: vec![],
            }),
            Err(e) => failures.push(pancetta_research::scorecard::FixtureFailure {
                wav: f.display_name.clone(),
                expected: vec!["any decode".into()],
                got: vec![format!("error: {e}")],
            }),
        }
    }
    let failed = total - passed;
    let pass_rate = if total == 0 { 0.0 } else { passed as f64 / total as f64 };
    Ok(TierResult {
        wavs_processed: total,
        fixtures_total: Some(total),
        fixtures_passed: Some(passed),
        fixtures_failed: Some(failed),
        failures,
        pass_rate: Some(pass_rate),
        ..Default::default()
    })
}

fn git_info(workspace: &std::path::Path) -> GitInfo {
    let run = |args: &[&str]| -> String {
        std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let sha = run(&["rev-parse", "HEAD"]);
    let merge_base = run(&["merge-base", "main", "HEAD"]);
    let dirty = !run(&["status", "--porcelain"]).is_empty();
    GitInfo {
        branch,
        head_sha: sha,
        main_merge_base: merge_base,
        dirty,
    }
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn main() -> anyhow::Result<()> {
    // Preflight gate. If --preflight refuses, the binary refuses too.
    let preflight = std::process::Command::new("./scripts/research-env.sh")
        .arg("--preflight")
        .current_dir(workspace_root()?)
        .status();
    match preflight {
        Ok(status) if !status.success() => {
            anyhow::bail!("preflight failed; aborting eval");
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "warn: preflight script not found or not executable ({e}); skipping disk check",
            );
        }
    }

    let args = Args::parse()?;
    let workspace = workspace_root()?;
    let started = Instant::now();
    let decoder: Box<dyn DecoderUnderTest> = match args.mode {
        Mode::Ft8 => Box::new(Ft8Decoder::with_default_config()),
    };

    let mut tiers = BTreeMap::new();
    for tier_name in &args.tiers {
        match tier_name.as_str() {
            "fixtures" => {
                let result = run_fixtures_tier(decoder.as_ref(), &workspace)?;
                tiers.insert("fixtures".to_string(), result);
            }
            other => anyhow::bail!(
                "tier '{other}' not supported in plan 1. Only 'fixtures' is wired today; \
                 curated + synth land in plan 2."
            ),
        }
    }

    let mut card = Scorecard {
        schema_version: Scorecard::CURRENT_SCHEMA_VERSION,
        generated_at: Utc::now(),
        mode: args.mode,
        git: git_info(&workspace),
        build: BuildInfo {
            rustc_version: rustc_version(),
            release: cfg!(not(debug_assertions)),
            features: vec!["research-eval".into()],
        },
        harness: HarnessInfo {
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
            host: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            cores_used: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            elapsed_seconds: 0.0,
        },
        config: ConfigInfo {
            decoder: decoder.config_snapshot(),
            seed: args.seed,
            tiers_run: args.tiers.clone(),
        },
        tiers,
        composite: pancetta_research::scorecard::CompositeInfo {
            weights: default_weights(),
            score: 0.0,
            main_baseline_score: None,
            delta_vs_main: None,
        },
        regressions: RegressionFlags::default(),
        notes: format!("Decoder under test: {}", decoder.identity()),
    };
    populate_composite(&mut card, default_weights());
    card.harness.elapsed_seconds = started.elapsed().as_secs_f64();

    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    card.save(&args.output)?;
    println!(
        "wrote scorecard: {} (composite {:.4}, {} tier(s), {:.1}s)",
        args.output.display(),
        card.composite.score,
        args.tiers.len(),
        card.harness.elapsed_seconds,
    );
    Ok(())
}
