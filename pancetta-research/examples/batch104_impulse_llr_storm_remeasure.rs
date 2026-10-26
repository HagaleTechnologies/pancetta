//! Batch 104 — PAN-162: pre-registered re-measurement of hb-256's
//! impulse-robust LLR knee on stormier synthetic plants, plus the off-air
//! do-no-harm check (c′). All decision logic lives in
//! `pancetta_research::storm_remeasure`; this is the I/O + decode driver.
//!
//! Modes (unknown or mode-irrelevant arguments are refused):
//!
//! ```text
//! # one decode shard: <shard-dir>/<plant>-<config>.json (resumable: decodes
//! # only grid points the shard does not hold yet; rayon over trials)
//! batch104 --plant <awgn|base|storm|severe> --config <off|k3|k6> --shard-dir DIR
//!          [--grid-lo X] [--grid-hi Y]          # extension points (0.5 dB multiples)
//!          [--smoke [--trials N]]               # marked smoke; refused by --summarize
//!
//! # off-air (c′): 12 kHz mono WAVs with a sibling .txt reference list
//! batch104 --offair --output PATH [--wav-dir pancetta-ft8/vendor/ft8_lib/test/wav]
//!
//! # summarize shards + off-air into the storm scorecard
//! batch104 --summarize --shard-dir DIR --offair PATH --output PATH
//!
//! # operator-side real-day A/B against research/baselines/ft8/<sha>.ft8lib.json
//! batch104 --manifest PATH [--limit N] --output PATH
//! ```

use anyhow::{bail, ensure, Context, Result};
use pancetta_research::metrics::hash_normalize_message;
use pancetta_research::storm_remeasure::{
    bits_to_string, build_storm_scorecard, clean_signals, config_knee, decode_success_bits,
    decode_texts, extended_grid, grid_points, impulsive_block_fraction, parse_reference_txt,
    plant_by_name, preregistered_grid, rates_of, received_and_noise, score_decodes, trials_for,
    OffairScorecard, OffairWav, PlantInput, Shard, StormRemeasureScorecard, CONFIGS,
    GRID_EXT_MAX_DB, GRID_EXT_MIN_DB, MESSAGES, PLANTS, SCHEMA_VERSION,
};
use rayon::prelude::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

const DEFAULT_WAV_DIR: &str = "pancetta-ft8/vendor/ft8_lib/test/wav";
const DEFAULT_SMOKE_TRIALS: usize = 2;

#[derive(Debug, Default)]
struct Args {
    plant: Option<String>,
    config: Option<String>,
    shard_dir: Option<PathBuf>,
    smoke: bool,
    trials: Option<usize>,
    grid_lo: Option<f64>,
    grid_hi: Option<f64>,
    offair_mode: bool,
    offair_path: Option<PathBuf>,
    output: Option<PathBuf>,
    wav_dir: Option<String>,
    summarize: bool,
    manifest: Option<PathBuf>,
    limit: Option<usize>,
}

fn parse_args(raw: impl Iterator<Item = String>) -> Result<Args> {
    let mut a = Args::default();
    let mut it = raw.peekable();
    let mut seen = HashSet::new();
    while let Some(flag) = it.next() {
        ensure!(seen.insert(flag.clone()), "{flag} given twice");
        let mut val = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--plant" => a.plant = Some(val()?),
            "--config" => a.config = Some(val()?),
            "--shard-dir" => a.shard_dir = Some(PathBuf::from(val()?)),
            "--smoke" => a.smoke = true,
            "--trials" => a.trials = Some(val()?.parse().context("--trials")?),
            "--grid-lo" => a.grid_lo = Some(val()?.parse().context("--grid-lo")?),
            "--grid-hi" => a.grid_hi = Some(val()?.parse().context("--grid-hi")?),
            // `--offair` is a mode switch in --offair mode and a path in --summarize mode.
            "--offair" => {
                if it.peek().is_some_and(|v| !v.starts_with("--")) {
                    a.offair_path = Some(PathBuf::from(it.next().unwrap()));
                } else {
                    a.offair_mode = true;
                }
            }
            "--output" => a.output = Some(PathBuf::from(val()?)),
            "--wav-dir" => a.wav_dir = Some(val()?),
            "--summarize" => a.summarize = true,
            "--manifest" => a.manifest = Some(PathBuf::from(val()?)),
            "--limit" => a.limit = Some(val()?.parse().context("--limit")?),
            "-h" | "--help" => {
                println!("{}", USAGE);
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(a)
}

const USAGE: &str = "usage:
  batch104 --plant <awgn|base|storm|severe> --config <off|k3|k6> --shard-dir DIR [--grid-lo X] [--grid-hi Y] [--smoke [--trials N]]
  batch104 --offair --output PATH [--wav-dir DIR]
  batch104 --summarize --shard-dir DIR --offair PATH --output PATH
  batch104 --manifest PATH [--limit N] --output PATH";

fn main() -> Result<()> {
    let a = parse_args(std::env::args().skip(1))?;
    let shard_mode = a.plant.is_some() || a.config.is_some();
    let modes = [shard_mode, a.offair_mode, a.summarize, a.manifest.is_some()];
    ensure!(
        modes.iter().filter(|&&m| m).count() == 1,
        "pick exactly one mode\n{USAGE}"
    );
    // Fail closed on arguments that do not belong to the chosen mode.
    let shard_only = a.smoke || a.trials.is_some() || a.grid_lo.is_some() || a.grid_hi.is_some();
    if shard_mode {
        ensure!(
            a.output.is_none() && a.wav_dir.is_none() && a.offair_path.is_none() && a.limit.is_none(),
            "shard mode takes only --plant --config --shard-dir [--grid-lo --grid-hi --smoke --trials]"
        );
        let plant = a.plant.context("--plant is required")?;
        let config = a.config.context("--config is required")?;
        let dir = a.shard_dir.context("--shard-dir is required")?;
        return run_shard(
            &plant, &config, &dir, a.smoke, a.trials, a.grid_lo, a.grid_hi,
        );
    }
    ensure!(
        !shard_only,
        "--smoke/--trials/--grid-lo/--grid-hi belong to shard mode"
    );
    if a.offair_mode {
        ensure!(
            a.shard_dir.is_none() && a.offair_path.is_none() && a.limit.is_none(),
            "--offair mode takes only --output [--wav-dir]"
        );
        let output = a.output.context("--output is required")?;
        let wav_dir = a.wav_dir.unwrap_or_else(|| DEFAULT_WAV_DIR.to_string());
        return run_offair(&wav_dir, &output);
    }
    ensure!(a.wav_dir.is_none(), "--wav-dir belongs to --offair mode");
    if a.summarize {
        ensure!(a.limit.is_none(), "--limit belongs to --manifest mode");
        let dir = a.shard_dir.context("--shard-dir is required")?;
        let offair = a.offair_path.context("--offair <path> is required")?;
        let output = a.output.context("--output is required")?;
        return run_summarize(&dir, &offair, &output);
    }
    ensure!(
        a.shard_dir.is_none() && a.offair_path.is_none(),
        "unexpected arguments for --manifest"
    );
    let manifest = a.manifest.context("--manifest")?;
    let output = a.output.context("--output is required")?;
    run_manifest(&manifest, a.limit, &output)
}

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("no parent")?
        .to_path_buf())
}

fn is_half_db(x: f64) -> bool {
    x.is_finite() && (x * 2.0).fract() == 0.0
}

// ---------------------------------------------------------------------------
// Shard mode
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_shard(
    plant: &str,
    config: &str,
    dir: &Path,
    smoke: bool,
    trials_arg: Option<usize>,
    grid_lo: Option<f64>,
    grid_hi: Option<f64>,
) -> Result<()> {
    let plant_params = plant_by_name(plant)?;
    let knee = config_knee(config)?;
    let trials = if smoke {
        trials_arg.unwrap_or(DEFAULT_SMOKE_TRIALS)
    } else {
        ensure!(
            trials_arg.is_none(),
            "--trials is only allowed with --smoke (trials are pre-registered)"
        );
        trials_for(plant)?
    };
    ensure!(trials > 0, "--trials must be > 0");
    let pre = preregistered_grid(plant)?;
    let lo = grid_lo.unwrap_or(pre[0]).min(pre[0]);
    let hi = grid_hi
        .unwrap_or(*pre.last().unwrap())
        .max(*pre.last().unwrap());
    ensure!(
        is_half_db(lo) && is_half_db(hi),
        "--grid-lo/--grid-hi must be multiples of 0.5 dB"
    );
    ensure!(
        lo >= GRID_EXT_MIN_DB && hi <= GRID_EXT_MAX_DB,
        "grid must stay within [{GRID_EXT_MIN_DB}, {GRID_EXT_MAX_DB}] dB"
    );
    let grid = grid_points(lo, hi);

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = Shard::path_in(dir, plant, config);
    let mut shard = if path.exists() {
        let s = Shard::load(&path)?;
        s.validate(plant, config, true)?;
        ensure!(
            s.smoke == smoke && s.trials == trials,
            "{} exists with smoke={} trials={}; refusing to mix with smoke={smoke} trials={trials}",
            path.display(),
            s.smoke,
            s.trials
        );
        ensure!(
            s.grid.iter().all(|g| grid.contains(g)),
            "{} holds points outside the requested grid",
            path.display()
        );
        s
    } else {
        Shard {
            schema_version: SCHEMA_VERSION,
            plant: plant.to_string(),
            config: config.to_string(),
            knee,
            trials,
            smoke,
            grid: Vec::new(),
            outcomes: Vec::new(),
            ibf_by_point: Vec::new(),
            mean_impulsive_block_fraction: 0.0,
        }
    };
    let todo: Vec<f64> = grid
        .iter()
        .copied()
        .filter(|g| !shard.grid.contains(g))
        .collect();
    if todo.is_empty() {
        println!(
            "skip: {} already covers the grid ({} points)",
            path.display(),
            shard.grid.len()
        );
        return Ok(());
    }
    println!(
        "# batch104 shard {plant}-{config}: knee={knee:?} trials={trials} smoke={smoke} points={} rayon_threads={}",
        todo.len(),
        rayon::current_num_threads()
    );
    let cleans = clean_signals()?;
    for snr in todo {
        let t0 = Instant::now();
        let generated: Vec<((Vec<f32>, usize), f64)> = (0..trials)
            .into_par_iter()
            .map(|t| {
                let (recv, noise) = received_and_noise(plant_params.as_ref(), snr, t, &cleans);
                ((recv, t % MESSAGES.len()), impulsive_block_fraction(&noise))
            })
            .collect();
        let (windows, ibfs): (Vec<_>, Vec<f64>) = generated.into_iter().unzip();
        let bits = decode_success_bits(&windows, knee, true)?;
        let ok = bits.iter().filter(|&&b| b).count();
        let ibf = ibfs.iter().sum::<f64>() / ibfs.len() as f64;
        // Insert in SNR order and save after every point (resumable).
        let at = shard.grid.partition_point(|&g| g < snr);
        shard.grid.insert(at, snr);
        shard.outcomes.insert(at, bits_to_string(&bits));
        shard.ibf_by_point.insert(at, ibf);
        shard.mean_impulsive_block_fraction =
            shard.ibf_by_point.iter().sum::<f64>() / shard.ibf_by_point.len() as f64;
        shard.save(&path)?;
        println!(
            "  {snr:+5.1} dB: {ok:>3}/{trials} ({:5.1}%)  IBF {ibf:.4}  {:.1}s",
            100.0 * ok as f64 / trials as f64,
            t0.elapsed().as_secs_f64()
        );
    }
    shard.validate(plant, config, true)?;
    println!("wrote {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Summarize mode
// ---------------------------------------------------------------------------

fn shard_cmd(dir: &Path, plant: &str, config: &str, grid: Option<(f64, f64)>) -> String {
    let exe = std::env::args().next().unwrap_or_else(|| "batch104".into());
    let mut cmd = format!(
        "{exe} --plant {plant} --config {config} --shard-dir {}",
        dir.display()
    );
    if let Some((lo, hi)) = grid {
        cmd.push_str(&format!(" --grid-lo {lo:.1} --grid-hi {hi:.1}"));
    }
    cmd
}

fn run_summarize(dir: &Path, offair_path: &Path, output: &Path) -> Result<()> {
    let offair = OffairScorecard::load(offair_path)?;
    offair
        .check_invariants()
        .context("off-air scorecard invariants")?;
    let mut missing = Vec::new();
    let mut extension = Vec::new();
    let mut inputs = Vec::new();
    for (pname, _) in PLANTS {
        let mut shards = Vec::new();
        for (cname, _) in CONFIGS {
            let p = Shard::path_in(dir, pname, cname);
            if !p.exists() {
                missing.push(shard_cmd(dir, pname, cname, None));
                continue;
            }
            let s = Shard::load(&p)?;
            s.validate(pname, cname, false)?;
            shards.push(s);
        }
        if shards.len() != CONFIGS.len() {
            continue;
        }
        let pre = preregistered_grid(pname)?;
        // Union of every config's grid and the pre-registered grid; any
        // shard short of it (interrupted run, extension run for only some
        // configs) needs its command re-run.
        let lo = shards.iter().map(|s| s.grid[0]).fold(pre[0], f64::min);
        let hi = shards
            .iter()
            .map(|s| *s.grid.last().unwrap())
            .fold(*pre.last().unwrap(), f64::max);
        let union = grid_points(lo, hi);
        let range = (lo != pre[0] || hi != *pre.last().unwrap()).then_some((lo, hi));
        let mut short = false;
        for (s, (cname, _)) in shards.iter().zip(CONFIGS) {
            if s.grid != union {
                short = true;
                missing.push(shard_cmd(dir, pname, cname, range));
            }
        }
        if short {
            continue;
        }
        let rows: Vec<Vec<Vec<bool>>> = shards.iter().map(|s| s.rows()).collect::<Result<_>>()?;
        let curves: Vec<Vec<f64>> = rows.iter().map(|r| rates_of(r)).collect();
        let want = extended_grid(&union, &curves);
        if want != union {
            let range = Some((want[0], *want.last().unwrap()));
            for (cname, _) in CONFIGS {
                extension.push(shard_cmd(dir, pname, cname, range));
            }
            continue;
        }
        let ibf = shards[0].mean_impulsive_block_fraction;
        ensure!(
            shards
                .iter()
                .all(|s| (s.mean_impulsive_block_fraction - ibf).abs() < 1e-12),
            "{pname}: shards disagree on the noise-only impulsive-block fraction (stimulus drift?)"
        );
        inputs.push(PlantInput {
            name: pname.to_string(),
            grid: union,
            mean_impulsive_block_fraction: ibf,
            configs: CONFIGS
                .iter()
                .map(|(c, _)| c.to_string())
                .zip(rows)
                .collect(),
        });
    }
    if !missing.is_empty() {
        eprintln!("missing or incomplete shards; run:");
        for c in &missing {
            eprintln!("  {c}");
        }
        bail!("{} shard(s) missing or incomplete", missing.len());
    }
    if !extension.is_empty() {
        eprintln!("the pre-registered grid extension rule applies; run exactly:");
        for c in &extension {
            eprintln!("  {c}");
        }
        bail!("grid extension needed; run the commands above, then --summarize again");
    }
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace_root()?)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let card = build_storm_scorecard(
        "pan162-impulse-llr-storm",
        inputs,
        &offair,
        &commit,
        rayon::current_num_threads(),
    )?;
    card.check_invariants()?;
    card.save(output)?;
    print_markdown(&card, &offair);
    println!("\nwrote {}\n", output.display());
    println!("{}", card.verdict_line());
    Ok(())
}

fn opt(x: Option<f64>, prec: usize) -> String {
    x.map_or("undefined".into(), |v| format!("{v:+.prec$}"))
}

fn print_markdown(card: &StormRemeasureScorecard, offair: &OffairScorecard) {
    println!(
        "# PAN-162 storm re-measure (commit {}, {} rayon threads)",
        card.commit, card.rayon_threads
    );
    for p in &card.plants {
        println!(
            "\n## {} — {} trials/point, mean impulsive-block fraction {:.4}\n",
            p.name, p.trials, p.mean_impulsive_block_fraction
        );
        let names: Vec<&str> = p.configs.iter().map(|c| c.name.as_str()).collect();
        println!("| SNR (dB) | {} |", names.join(" | "));
        println!("|---|{}", "---|".repeat(names.len()));
        for (i, snr) in p.grid.iter().enumerate() {
            let cells: Vec<String> = p
                .configs
                .iter()
                .map(|c| format!("{:.1}%", 100.0 * c.rates[i]))
                .collect();
            println!("| {snr:+.1} | {} |", cells.join(" | "));
        }
        let thr: Vec<String> = p.configs.iter().map(|c| opt(c.threshold_db, 2)).collect();
        println!("| threshold | {} |", thr.join(" | "));
        println!();
        for s in &p.shifts {
            println!(
                "- shift off → k={}: {} dB, 95% CI [{}, {}], significant={}, undefined resamples {}/{}",
                s.k,
                opt(s.ci.point, 3),
                opt(s.ci.ci_low, 3),
                opt(s.ci.ci_high, 3),
                s.ci.significant,
                s.ci.undefined_resamples,
                s.ci.n_bootstrap
            );
        }
    }
    print_offair(offair);
}

fn print_offair(o: &OffairScorecard) {
    let sum = |f: fn(&OffairWav) -> usize| o.wavs.iter().map(f).sum::<usize>();
    println!(
        "\n## Off-air ({}, {} WAVs, {} missing truth)\n",
        o.label,
        o.wavs.len(),
        o.missing_truth
    );
    println!("| config | ΣTP | ΣFP |\n|---|---|---|");
    println!("| off | {} | {} |", sum(|w| w.tp_off), sum(|w| w.fp_off));
    println!("| k3 | {} | {} |", sum(|w| w.tp_k3), sum(|w| w.fp_k3));
    println!("| k6 | {} | {} |", sum(|w| w.tp_k6), sum(|w| w.fp_k6));
    println!("\ntruth: {}", sum(|w| w.truth));
    for (name, d, ci) in [
        ("k3", o.delta_tp_k3, &o.ci_k3),
        ("k6", o.delta_tp_k6, &o.ci_k6),
    ] {
        println!(
            "ΔTP {name}: {d:+} (paired bootstrap 95% CI [{:+.1}, {:+.1}], significant={})",
            ci.ci_low, ci.ci_high, ci.significant
        );
    }
}

// ---------------------------------------------------------------------------
// Off-air and manifest modes
// ---------------------------------------------------------------------------

fn wav_is_mono_12k(path: &Path) -> bool {
    hound::WavReader::open(path)
        .map(|r| r.spec().channels == 1 && r.spec().sample_rate == 12000)
        .unwrap_or(false)
}

fn load_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    ensure!(
        spec.channels == 1 && spec.sample_rate == 12000,
        "{}: not 12 kHz mono",
        path.display()
    );
    Ok(match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

fn collect_wavs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = entry?.path();
        if p.is_dir() {
            collect_wavs(&p, out)?;
        } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"))
            && p.with_extension("txt").is_file()
            && wav_is_mono_12k(&p)
        {
            out.push(p);
        }
    }
    Ok(())
}

/// Workspace-relative path with `/` separators; paths outside the
/// workspace are kept as given.
fn rel_path(ws: &Path, p: &Path) -> String {
    match p.strip_prefix(ws) {
        Ok(r) => r
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => p.display().to_string(),
    }
}

/// Decodes one WAV with every config and scores it against `truth`.
fn score_wav(
    path_label: String,
    sha256: String,
    samples: &[f32],
    truth: &HashSet<String>,
) -> Result<OffairWav> {
    let mut counts = Vec::new();
    for (_, knee) in CONFIGS {
        let texts = decode_texts(samples, knee)?;
        counts.push(score_decodes(truth, texts.iter().map(String::as_str)));
    }
    Ok(OffairWav {
        path: path_label,
        sha256,
        truth: truth.len(),
        tp_off: counts[0].0,
        fp_off: counts[0].1,
        tp_k3: counts[1].0,
        fp_k3: counts[1].1,
        tp_k6: counts[2].0,
        fp_k6: counts[2].1,
    })
}

fn run_offair(wav_dir: &str, output: &Path) -> Result<()> {
    let ws = workspace_root()?;
    let dir = ws.join(wav_dir);
    let mut paths = Vec::new();
    collect_wavs(&dir, &mut paths)?;
    paths.sort();
    ensure!(
        !paths.is_empty(),
        "no 12 kHz mono WAVs with a .txt list under {}",
        dir.display()
    );
    println!(
        "# batch104 off-air: {} WAVs under {}",
        paths.len(),
        dir.display()
    );
    let t0 = Instant::now();
    let wavs: Vec<OffairWav> = paths
        .par_iter()
        .map(|p| {
            let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
            let sha = format!("{:x}", Sha256::digest(&bytes));
            let txt = std::fs::read_to_string(p.with_extension("txt"))?;
            let truth: HashSet<String> = parse_reference_txt(&txt)
                .iter()
                .map(|m| hash_normalize_message(m))
                .collect();
            let samples = load_wav(p)?;
            score_wav(rel_path(&ws, p), sha, &samples, &truth)
        })
        .collect::<Result<_>>()?;
    let card = OffairScorecard::build("pan162-impulse-llr-offair", &rel_path(&ws, &dir), wavs, 0);
    card.check_invariants()?;
    card.save(output)?;
    print_offair(&card);
    println!(
        "\nwrote {} ({:.1}s)",
        output.display(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn load_ft8lib_truth(ws: &Path, sha: &str) -> Option<HashSet<String>> {
    let path = ws
        .join("research/baselines/ft8")
        .join(format!("{sha}.ft8lib.json"));
    let txt = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    Some(
        v["decodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["message"].as_str())
            .map(hash_normalize_message)
            .collect(),
    )
}

fn run_manifest(manifest: &Path, limit: Option<usize>, output: &Path) -> Result<()> {
    let ws = workspace_root()?;
    let m: Value = serde_json::from_str(
        &std::fs::read_to_string(manifest)
            .with_context(|| format!("reading {}", manifest.display()))?,
    )?;
    let entries: Vec<&Value> = m["entries"]
        .as_array()
        .context("manifest has no entries[]")?
        .iter()
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    let mut todo = Vec::new();
    let mut missing_truth = 0usize;
    for e in entries {
        let wav = e["wav_path"]
            .as_str()
            .context("entry wav_path")?
            .to_string();
        let sha = e["wav_sha256"]
            .as_str()
            .context("entry wav_sha256")?
            .to_string();
        match load_ft8lib_truth(&ws, &sha) {
            Some(t) => todo.push((wav, sha, t)),
            None => missing_truth += 1,
        }
    }
    println!(
        "# batch104 manifest A/B: {} WAVs with truth, {missing_truth} missing truth",
        todo.len()
    );
    let t0 = Instant::now();
    let wavs: Vec<OffairWav> = todo
        .par_iter()
        .map(|(wav, sha, truth)| {
            // Truth is keyed by the manifest's hash; refuse a WAV whose bytes
            // no longer match it rather than score it against the wrong truth.
            let bytes = std::fs::read(wav).with_context(|| format!("read {wav}"))?;
            let actual = format!("{:x}", Sha256::digest(&bytes));
            ensure!(
                actual == *sha,
                "{wav}: sha256 {actual} != manifest wav_sha256 {sha}"
            );
            let samples = load_wav(Path::new(wav))?;
            score_wav(wav.clone(), sha.clone(), &samples, truth)
        })
        .collect::<Result<_>>()?;
    let label = format!("pan162-manifest:{}", manifest.display());
    let card = OffairScorecard::build(&label, &manifest.display().to_string(), wavs, missing_truth);
    card.check_invariants()?;
    card.save(output)?;
    print_offair(&card);
    println!(
        "\nwrote {} ({:.1}s)",
        output.display(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(xs: &[&str]) -> Result<Args> {
        parse_args(xs.iter().map(|s| s.to_string()))
    }

    #[test]
    fn offair_flag_is_mode_or_path() {
        let a = parse(&["--offair", "--output", "x.json"]).unwrap();
        assert!(a.offair_mode && a.offair_path.is_none());
        let a = parse(&["--summarize", "--offair", "o.json", "--shard-dir", "d"]).unwrap();
        assert!(!a.offair_mode && a.offair_path.is_some());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--plant", "storm", "--plant", "awgn"]).is_err());
    }
}
