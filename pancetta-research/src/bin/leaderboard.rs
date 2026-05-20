//! leaderboard — rank all scorecards in research/scorecards/history/ by composite
//! score. Reads main.json as the current baseline reference.
//!
//! Output: a markdown table sorted by composite score descending.

use anyhow::Context;
use pancetta_research::scorecard::Scorecard;
use std::path::PathBuf;

fn workspace_root() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

fn load_all_scorecards(workspace: &PathBuf) -> anyhow::Result<Vec<(PathBuf, Scorecard)>> {
    let mut out = Vec::new();
    let history = workspace.join("research/scorecards/history");
    if history.exists() {
        for entry in std::fs::read_dir(&history)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "json") {
                match Scorecard::load(&path) {
                    Ok(c) => out.push((path, c)),
                    Err(e) => eprintln!("warn: skipping {}: {e}", path.display()),
                }
            }
        }
    }
    let main_path = workspace.join("research/scorecards/main.json");
    if main_path.exists() {
        match Scorecard::load(&main_path) {
            Ok(c) => out.push((main_path, c)),
            Err(e) => eprintln!("warn: main.json: {e}"),
        }
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let workspace = workspace_root()?;
    let mut all = load_all_scorecards(&workspace)?;
    if all.is_empty() {
        println!("no scorecards found in research/scorecards/");
        return Ok(());
    }
    all.sort_by(|a, b| {
        b.1.composite
            .score
            .partial_cmp(&a.1.composite.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("# Decoder Research Leaderboard");
    println!();
    println!(
        "| Rank | Score | Slug | Branch | Date | Pass | SNR@50 |"
    );
    println!(
        "|------|-------|------|--------|------|------|--------|"
    );
    for (i, (path, card)) in all.iter().enumerate() {
        let slug = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace(".json", "");
        let fixtures_pass = card
            .tiers
            .get("fixtures")
            .and_then(|t| t.pass_rate)
            .map(|p| format!("{:.3}", p))
            .unwrap_or_else(|| "-".into());
        let snr50 = card
            .tiers
            .get("synth-clean")
            .and_then(|t| t.snr_at_50pct_recovery_db)
            .map(|s| format!("{:+.1}", s))
            .unwrap_or_else(|| "-".into());
        let date = card.generated_at.to_rfc3339()[..10].to_string();
        println!(
            "| {} | {:.4} | {} | {} | {} | {} | {} |",
            i + 1,
            card.composite.score,
            slug,
            card.git.branch,
            date,
            fixtures_pass,
            snr50,
        );
    }
    Ok(())
}
