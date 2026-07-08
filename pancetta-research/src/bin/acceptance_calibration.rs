//! acceptance_calibration — Task W2.1 (decoder-tp-sensitivity plan) research
//! script. Decodes the hard_200 curated corpus + the W0.1 noise_1000 tier
//! with a RESEARCH-ONLY config (`osd_depth: Some(2)` — NOT the production
//! default, which stays `Some(0)`), and for every CRC-valid decode that
//! carries a `pancetta_ft8::acceptance::AcceptanceScore`, writes one CSV row:
//!
//! ```text
//! tier,wav_hash,soft_distance,hard_errors,coherence,is_verified
//! ```
//!
//! `is_verified` (the brief's `is_jt9_verified`) means: for hard_200, the
//! decode's exact message text appears in that WAV's cached jt9 baseline
//! (`research/baselines/ft8/<sha256>.json`) — i.e. an independent decoder
//! confirms this is a real signal, not a CRC-14 coincidence. For noise_1000,
//! EVERY decode is by construction unverified (false positive) — there is no
//! FT8 signal in that corpus at all.
//!
//! This binary does not touch `Ft8Config::default()` — the `osd_depth`
//! override is applied only to the local `Ft8Decoder` wrapper instance it
//! constructs, exactly like `eval`'s `--osd-depth` flag.
//!
//! Also prints a threshold sweep on `soft_distance`: for each candidate
//! threshold t (ascending over the observed values), the FDR of "accept
//! every decode with soft_distance <= t" — i.e. what fraction of ACCEPTED
//! decodes at that threshold are unverified. Reports the largest threshold
//! whose FDR stays <= a target (default 1%).

use anyhow::Context;
use pancetta_research::curated::load_curated_corpus;
use pancetta_research::decoder::{Decode, DecoderUnderTest, Ft8Decoder};
use pancetta_research::gen_noise::load_noise_corpus;
use std::io::Write;
use std::path::PathBuf;

struct Args {
    hard200_manifest: PathBuf,
    noise_manifest: PathBuf,
    /// Cap on how many hard_200 entries to decode. `None` = full corpus.
    hard200_limit: Option<usize>,
    /// Cap on how many noise_1000 entries to decode (osd_depth=2 is far
    /// more expensive than production's osd_depth=0 on signal-free audio,
    /// where BP never converges and OSD always runs its full trial
    /// budget). `None` = full corpus.
    noise_limit: Option<usize>,
    osd_depth: u8,
    /// Task W2.3 [A/B]: which LLR array drives OSD's search. One of
    /// `bp-posterior` (default), `channel`, or `offset:<f32>`.
    osd_input: pancetta_ft8::OsdInput,
    /// Task W2.4 [A/B]: WSJT-X mainline-style npre2 warm-start
    /// preprocessing (active only at `osd_depth >= 3`).
    npre2: bool,
    output_csv: PathBuf,
    target_fdr: f64,
}

fn parse_osd_input(s: &str) -> anyhow::Result<pancetta_ft8::OsdInput> {
    if s == "bp-posterior" {
        Ok(pancetta_ft8::OsdInput::BpPosterior)
    } else if s == "channel" {
        Ok(pancetta_ft8::OsdInput::Channel)
    } else if let Some(v) = s.strip_prefix("offset:") {
        Ok(pancetta_ft8::OsdInput::OffsetSubtracted(v.parse::<f32>()?))
    } else {
        anyhow::bail!("--osd-input must be one of bp-posterior|channel|offset:<f32>, got {s}")
    }
}

fn parse_args() -> anyhow::Result<Args> {
    let workspace = workspace_root()?;
    let mut hard200_manifest = workspace.join("research/corpus/curated/ft8/hard_200.manifest.json");
    let mut noise_manifest =
        workspace.join("research/corpus/curated/noise/noise_1000.manifest.json");
    let mut hard200_limit = None;
    let mut noise_limit = None;
    let mut osd_depth = 2u8;
    let mut osd_input = pancetta_ft8::OsdInput::BpPosterior;
    let mut npre2 = false;
    let mut output_csv = workspace.join("research/scorecards/acceptance_calibration.csv");
    let mut target_fdr = 0.01;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--hard200-manifest" => {
                hard200_manifest =
                    PathBuf::from(it.next().context("--hard200-manifest needs a value")?)
            }
            "--noise-manifest" => {
                noise_manifest = PathBuf::from(it.next().context("--noise-manifest needs a value")?)
            }
            "--hard200-limit" => {
                hard200_limit = Some(
                    it.next()
                        .context("--hard200-limit needs a value")?
                        .parse::<usize>()?,
                )
            }
            "--noise-limit" => {
                noise_limit = Some(
                    it.next()
                        .context("--noise-limit needs a value")?
                        .parse::<usize>()?,
                )
            }
            "--osd-depth" => {
                osd_depth = it
                    .next()
                    .context("--osd-depth needs a value")?
                    .parse::<u8>()?
            }
            "--osd-input" => {
                osd_input = parse_osd_input(&it.next().context("--osd-input needs a value")?)?
            }
            "--npre2" => npre2 = true,
            "--output" => output_csv = PathBuf::from(it.next().context("--output needs a value")?),
            "--target-fdr" => {
                target_fdr = it
                    .next()
                    .context("--target-fdr needs a value")?
                    .parse::<f64>()?
            }
            other => anyhow::bail!("unknown arg {other}"),
        }
    }

    Ok(Args {
        hard200_manifest,
        noise_manifest,
        hard200_limit,
        noise_limit,
        osd_depth,
        osd_input,
        npre2,
        output_csv,
        target_fdr,
    })
}

fn workspace_root() -> anyhow::Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("CARGO_MANIFEST_DIR has no parent")?
        .to_path_buf())
}

/// One CSV row of calibration data.
#[derive(Clone)]
struct Row {
    tier: &'static str,
    wav_hash: String,
    soft_distance: f32,
    hard_errors: u16,
    coherence: Option<f32>,
    is_verified: bool,
}

/// Load the cached jt9 baseline decode texts for a hard_200 WAV (by sha256),
/// mirroring `eval.rs::run_curated_tier`'s baseline lookup. Empty if no
/// baseline is cached for this WAV.
fn load_baseline_texts(workspace: &std::path::Path, wav_sha256: &str) -> Vec<String> {
    let baseline_path = workspace
        .join("research/baselines/ft8")
        .join(format!("{wav_sha256}.json"));
    if !baseline_path.exists() {
        return Vec::new();
    }
    let Ok(s) = std::fs::read_to_string(&baseline_path) else {
        return Vec::new();
    };
    let Ok(cache) = serde_json::from_str::<serde_json::Value>(&s) else {
        return Vec::new();
    };
    cache
        .get("decodes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn rows_from_decodes(
    tier: &'static str,
    wav_hash: &str,
    decodes: &[Decode],
    baseline_texts: &[String],
) -> Vec<Row> {
    decodes
        .iter()
        .filter_map(|d| {
            let (Some(soft_distance), Some(hard_errors)) = (d.soft_distance, d.hard_errors) else {
                // No acceptance metric computed for this decode's code
                // path (e.g. the a7 template-match paths) — not part of
                // this calibration.
                return None;
            };
            let is_verified = baseline_texts.iter().any(|t| t.trim() == d.message.trim());
            Some(Row {
                tier,
                wav_hash: wav_hash.to_string(),
                soft_distance,
                hard_errors,
                coherence: d.coherence,
                is_verified,
            })
        })
        .collect()
}

fn write_csv(path: &std::path::Path, rows: &[Row]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(path)?;
    writeln!(
        f,
        "tier,wav_hash,soft_distance,hard_errors,coherence,is_verified"
    )?;
    for r in rows {
        writeln!(
            f,
            "{},{},{},{},{},{}",
            r.tier,
            r.wav_hash,
            r.soft_distance,
            r.hard_errors,
            r.coherence
                .map(|c| c.to_string())
                .unwrap_or_else(|| "".to_string()),
            r.is_verified,
        )?;
    }
    Ok(())
}

/// One sweep row: (threshold, n_accepted, n_false_positive, fdr).
type SweepPoint = (f32, u32, u32, f64);

/// FDR-threshold sweep: for each distinct observed `soft_distance` value
/// (ascending), compute the FDR of "accept everything with soft_distance <=
/// t" (fraction of accepted rows that are unverified). Returns the largest
/// threshold whose FDR <= target, plus the full sweep for reporting.
fn fdr_sweep(rows: &[Row], target_fdr: f64) -> (Option<f32>, Vec<SweepPoint>) {
    let mut thresholds: Vec<f32> = rows.iter().map(|r| r.soft_distance).collect();
    thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    thresholds.dedup();

    let mut sweep = Vec::with_capacity(thresholds.len());
    let mut best: Option<f32> = None;
    for &t in &thresholds {
        let accepted: Vec<&Row> = rows.iter().filter(|r| r.soft_distance <= t).collect();
        let n_accepted = accepted.len() as u32;
        let n_fp = accepted.iter().filter(|r| !r.is_verified).count() as u32;
        let fdr = if n_accepted == 0 {
            0.0
        } else {
            n_fp as f64 / n_accepted as f64
        };
        sweep.push((t, n_accepted, n_fp, fdr));
        if fdr <= target_fdr {
            best = Some(t);
        }
    }
    (best, sweep)
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let workspace = workspace_root()?;

    eprintln!(
        "acceptance_calibration: osd_depth={} osd_input={:?} npre2={} (RESEARCH-ONLY override; production default stays Some(0)/BpPosterior/npre2=false)",
        args.osd_depth, args.osd_input, args.npre2
    );

    let decoder = Ft8Decoder::with_default_config()
        .with_osd_depth(Some(args.osd_depth))
        .with_osd_input(args.osd_input)
        .with_npre2_enabled(args.npre2);

    let mut all_rows: Vec<Row> = Vec::new();

    // --- hard_200 tier ---
    let mut hard200_entries = load_curated_corpus(&args.hard200_manifest)
        .with_context(|| format!("loading {}", args.hard200_manifest.display()))?;
    if let Some(limit) = args.hard200_limit {
        hard200_entries.truncate(limit);
    }
    eprintln!(
        "hard_200: {} WAVs (limit applied if set)",
        hard200_entries.len()
    );
    let started = std::time::Instant::now();
    for (i, entry) in hard200_entries.iter().enumerate() {
        let decodes = decoder.decode_wav(&entry.wav_path).unwrap_or_default();
        let baseline_texts = load_baseline_texts(&workspace, &entry.wav_sha256);
        all_rows.extend(rows_from_decodes(
            "hard_200",
            &entry.wav_sha256,
            &decodes,
            &baseline_texts,
        ));
        if (i + 1) % 50 == 0 {
            eprintln!(
                "  hard_200: {}/{} ({:.1}s elapsed)",
                i + 1,
                hard200_entries.len(),
                started.elapsed().as_secs_f64()
            );
        }
    }
    eprintln!(
        "hard_200 done: {:.1}s elapsed, {} acceptance rows so far",
        started.elapsed().as_secs_f64(),
        all_rows.len()
    );

    // --- noise_1000 tier ---
    let mut noise_entries = load_noise_corpus(&args.noise_manifest)
        .with_context(|| format!("loading {}", args.noise_manifest.display()))?;
    if let Some(limit) = args.noise_limit {
        noise_entries.truncate(limit);
    }
    eprintln!(
        "noise_1000: {} WAVs (limit applied if set)",
        noise_entries.len()
    );
    let started = std::time::Instant::now();
    for (i, entry) in noise_entries.iter().enumerate() {
        let decodes = decoder.decode_wav(&entry.wav_path).unwrap_or_default();
        // Every decode against pure noise is, by construction, a false
        // positive — no baseline lookup needed (baseline_texts = empty).
        all_rows.extend(rows_from_decodes(
            "noise_1000",
            &entry.wav_sha256,
            &decodes,
            &[],
        ));
        if (i + 1) % 100 == 0 {
            eprintln!(
                "  noise_1000: {}/{} ({:.1}s elapsed)",
                i + 1,
                noise_entries.len(),
                started.elapsed().as_secs_f64()
            );
        }
    }
    eprintln!(
        "noise_1000 done: {:.1}s elapsed, {} total acceptance rows",
        started.elapsed().as_secs_f64(),
        all_rows.len()
    );

    write_csv(&args.output_csv, &all_rows)?;
    eprintln!(
        "wrote {} rows to {}",
        all_rows.len(),
        args.output_csv.display()
    );

    let n_hard200 = all_rows.iter().filter(|r| r.tier == "hard_200").count();
    let n_hard200_verified = all_rows
        .iter()
        .filter(|r| r.tier == "hard_200" && r.is_verified)
        .count();
    let n_noise = all_rows.iter().filter(|r| r.tier == "noise_1000").count();
    eprintln!(
        "hard_200 rows: {n_hard200} ({n_hard200_verified} jt9-verified, {} novel/unverified — \
         jt9 itself misses weak signals on this hard-curated corpus, so 'unverified' here means \
         'no independent confirmation', NOT necessarily a hallucination)",
        n_hard200 - n_hard200_verified
    );
    eprintln!(
        "noise_1000 rows: {n_noise} (100% false positive by construction — no signal exists in this corpus)"
    );

    // Naive sweep: treat EVERY unverified row (including hard_200 novel
    // decodes jt9 simply missed) as a false positive. This is a
    // conservative UPPER BOUND on the true FDR — many hard_200 "novel"
    // decodes are real signals jt9's own recall gap missed, not
    // hallucinations, so this sweep is pessimistic and reported only for
    // transparency/comparison.
    let (naive_best, naive_sweep) = fdr_sweep(&all_rows, args.target_fdr);
    eprintln!(
        "\n=== NAIVE sweep (all rows; hard_200-novel counted as FP — pessimistic upper bound) ==="
    );
    print_sweep(&naive_sweep);
    print_chosen(naive_best, args.target_fdr);

    // Definitive sweep: only rows with an UNAMBIGUOUS ground-truth label —
    // noise_1000 decodes (always FP; no signal exists) and hard_200 decodes
    // an independent decoder (jt9) also found (always TP; independently
    // confirmed real signal). hard_200 rows jt9 didn't confirm are EXCLUDED
    // here rather than being force-labeled either way, because jt9 recall
    // gaps on this hard-curated corpus are well documented (see
    // `eval.rs::run_curated_tier`'s own novel-decode classifier) — lumping
    // them in as FP would make the acceptance metric look artificially
    // worse than it is, and lumping them in as TP would do the opposite.
    let definitive_owned: Vec<Row> = all_rows
        .iter()
        .filter(|r| r.tier == "noise_1000" || (r.tier == "hard_200" && r.is_verified))
        .cloned()
        .collect();
    let (definitive_best, definitive_sweep) = fdr_sweep(&definitive_owned, args.target_fdr);
    eprintln!(
        "\n=== DEFINITIVE sweep ({} rows: noise_1000 FP + hard_200-jt9-verified TP only; \
         hard_200-novel EXCLUDED) — this is the recommended basis for threshold selection ===",
        definitive_owned.len()
    );
    print_sweep(&definitive_sweep);
    print_chosen(definitive_best, args.target_fdr);

    Ok(())
}

fn print_sweep(sweep: &[SweepPoint]) {
    eprintln!(
        "{:>10} {:>10} {:>10} {:>10}",
        "threshold", "n_accept", "n_fp", "fdr"
    );
    for (t, n_accept, n_fp, fdr) in sweep {
        eprintln!("{t:>10.4} {n_accept:>10} {n_fp:>10} {fdr:>10.4}");
    }
}

fn print_chosen(best: Option<f32>, target_fdr: f64) {
    match best {
        Some(t) => eprintln!(
            "chosen threshold at FDR <= {:.2}%: soft_distance <= {:.4}",
            target_fdr * 100.0,
            t
        ),
        None => eprintln!(
            "NO threshold achieves FDR <= {:.2}% on this data (even the smallest observed soft_distance exceeds it)",
            target_fdr * 100.0
        ),
    }
}
